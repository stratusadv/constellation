use std::path::Path;
use std::sync::LazyLock;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span, Visibility,
};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, QUERYSET_BUILTINS, UnresolvedRef};
use rusqlite::{Connection, OptionalExtension, params};
use rustc_hash::FxHashMap;

use crate::error::StoreError;
use crate::time::now_ms;

/// The fail-fast bound on the rows written for a single file in one call.
const ROWS_PER_FILE_MAX: u32 = 5_000_000;

/// The fail-fast bound on the rows materialized by a single read.
const ROWS_LOADED_MAX: u32 = 50_000_000;

/// The full schema and single source of truth; a change here changes the
/// fingerprint, which rebuilds any database built under the old schema.
const SCHEMA: &str = include_str!("schema.sql");

/// A compile-time fingerprint of [`SCHEMA`] (FNV-1a, reduced to a positive 31-bit
/// value for SQLite's `PRAGMA user_version`). It records the schema a database was
/// built under; a mismatch on open means the schema changed, so the database is
/// discarded and rebuilt from scratch rather than migrated. constellation
/// re-indexes quickly, so the cost of a schema change is a re-index, not a
/// hand-written migration.
const SCHEMA_FINGERPRINT: i32 = schema_fingerprint();

/// The FNV-1a hash of the schema, evaluated at compile time.
const fn schema_fingerprint() -> i32 {
    let bytes = SCHEMA.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut index = 0;

    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }

    (hash & 0x7fff_ffff) as i32
}

/// Whether an existing database at `path` was built under a different schema than
/// this binary's, and so must be discarded and rebuilt. A missing database is not
/// stale: the next open creates it fresh.
fn schema_is_stale(path: &Path) -> Result<bool, StoreError> {
    if !path.exists() {
        return Ok(false);
    }

    let connection = Connection::open(path)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;

    Ok(version != i64::from(SCHEMA_FINGERPRINT))
}

/// The stale database file and its WAL sidecars removed, so the next open rebuilds
/// from the current schema. Best-effort: a failed delete leaves the next open to
/// fail clearly rather than silently use an incompatible file.
fn discard_database(path: &Path) {
    let _ = std::fs::remove_file(path);

    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();

        sidecar.push(suffix);

        let _ = std::fs::remove_file(sidecar);
    }
}

/// The cap on how many rows a `LIMIT`-bounded read pre-allocates for, so a caller
/// passing an enormous limit cannot reserve a huge Vec for a tiny result set.
const PREALLOC_ROWS_MAX: usize = 4_096;

/// The node columns, qualified with the `n` alias, in the order [`node_row`]
/// expects. Shared by every read that joins nodes against edges or FTS.
const NODE_COLUMNS_PREFIXED: &str = "n.id, n.project_id, n.kind, n.name, n.qualified_name, \
n.file_path, n.language, n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, \
n.signature, n.visibility, n.is_exported, n.is_async, n.is_static, n.is_abstract, n.decorators, \
n.updated_at";

/// The node columns, unqualified, in the order [`node_row`] expects. Shared by
/// the scoped single-column lookups that back incremental resolution.
const NODE_COLUMNS: &str = "id, project_id, kind, name, qualified_name, file_path, language, \
start_line, end_line, start_column, end_column, docstring, signature, visibility, is_exported, \
is_async, is_static, is_abstract, decorators, updated_at";

/// The hot graph-navigation SQL, built once. These join the full node column set
/// (a ~280-char constant) into the result, so building the string with `format!`
/// on every `callers`/`callees`/`search` call would allocate it afresh each time
/// and force a SQL re-parse. Built once here; the per-call path uses
/// `prepare_cached`, so both the allocation and the parse leave the hot path.
static CALLERS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED}, e.kind FROM edges e JOIN nodes n ON e.source = n.id
         WHERE e.target = ?1",
    )
});

static CALLEES_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED}, e.kind FROM edges e JOIN nodes n ON e.target = n.id
         WHERE e.source = ?1",
    )
});

static SEARCH_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes_fts f JOIN nodes n ON n.id = f.id
         WHERE nodes_fts MATCH ?1 LIMIT ?2",
    )
});

/// The node columns qualified with `alias`, in the order [`node_row`] expects.
/// The runtime equivalent of [`NODE_COLUMNS_PREFIXED`] for a join that aliases
/// the nodes table more than once (source and target endpoints in one row).
fn node_columns(alias: &str) -> String {
    assert!(!alias.is_empty(), "column alias must not be empty");

    // One allocation: every column gets the `alias.` prefix, so the result is the
    // column list plus `alias.` per column. Avoids the per-column temporary
    // strings a split/map/collect/join would allocate.
    let mut out = String::with_capacity(NODE_COLUMNS.len() + NODE_COLUMNS.matches(',').count() * (alias.len() + 1) + alias.len() + 1);

    for (index, column) in NODE_COLUMNS.split(", ").enumerate() {
        if index > 0 {
            out.push_str(", ");
        }

        out.push_str(alias);
        out.push('.');
        out.push_str(column);
    }

    out
}

/// A project row: its id, display name, filesystem root, the epoch-ms timestamp
/// of its last index, and whether it is reference-only.
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub root_path: String,
    pub indexed_at: i64,
    /// Whether this project is withheld from cross-project link targets: a
    /// reference-only version copy, queryable but never linked into.
    pub reference_only: bool,
}

/// One commit from a project's git history: its hash, author name, committer
/// timestamp (epoch seconds), subject line, and the files it touched.
pub struct CommitRecord {
    pub commit_hash: String,
    pub author: String,
    pub committed_at: i64,
    pub summary: String,
    pub files: Vec<CommitFile>,
}

/// One file a commit touched, with its line churn (both zero for a binary file
/// or a pure rename, which git reports as `-`).
pub struct CommitFile {
    pub file_path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// One row of a history timeline: a commit that touched the queried path, with
/// its churn aggregated over only the files matching that path.
pub struct HistoryHit {
    pub project_id: String,
    pub commit_hash: String,
    pub author: String,
    pub committed_at: i64,
    pub summary: String,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// The kind of change a symbol underwent between two consecutive revisions of its
/// file, as recorded in `git_symbol_revision`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolChange {
    Added,
    Modified,
    Removed,
}

impl SymbolChange {
    /// The lowercase label stored for this change.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolChange::Added => "added",
            SymbolChange::Modified => "modified",
            SymbolChange::Removed => "removed",
        }
    }
}

/// One symbol-level change to record: a trackable symbol added, modified, or
/// removed in a commit, identified within its file by qualified name.
pub struct SymbolRevision {
    pub commit_hash: String,
    pub file_path: String,
    pub qualified_name: String,
    pub name: String,
    pub kind: String,
    pub change: SymbolChange,
    pub signature: Option<String>,
}

/// One commit that touched a file, used to drive symbol diffing in commit order.
pub struct FileTouch {
    pub file_path: String,
    pub commit_hash: String,
}

/// One row of a symbol's change history: when it changed (commit, time, subject),
/// how (`added`/`modified`/`removed`), and its kind and signature at that point.
pub struct SymbolHistoryHit {
    pub project_id: String,
    pub commit_hash: String,
    pub committed_at: i64,
    pub qualified_name: String,
    pub kind: String,
    pub change: String,
    pub signature: Option<String>,
    pub summary: String,
}

/// One symbol alive at a reconstructed point in time: its file, qualified name,
/// kind, and the signature in effect then.
pub struct AsOfSymbol {
    pub project_id: String,
    pub file_path: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: Option<String>,
}

/// A file row for the `files` listing: its path, language, symbol (node) count,
/// and size in bytes.
pub struct FileRow {
    pub path: String,
    pub language: String,
    pub node_count: i64,
    pub size_bytes: i64,
}

/// A cross-project link edge with both endpoints hydrated: an import in
/// `source`'s repo resolved to the `target` definition in another repo, tagged
/// with the linker's `link:<from>-><to>` provenance.
pub struct LinkEdge {
    pub source: Node,
    pub target: Node,
    pub kind: EdgeKind,
    pub provenance: String,
}

/// The metadata for one indexed file, written alongside its extracted graph.
pub struct FileIndex<'a> {
    pub path: &'a str,
    pub content_hash: &'a str,
    pub language: Language,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
    /// The file's full source, full-text indexed so `explore` can rank from
    /// body content. Empty when content indexing is not wanted (e.g. tests).
    pub source: &'a str,
}

/// A handle to the constellation database: every project's graph and the
/// cross-project edges between them, in one SQLite file.
pub struct Store {
    connection: Connection,
}

impl Store {
    /// The store at `path`, created if absent. A database built under a different
    /// schema is discarded and rebuilt (see [`SCHEMA_FINGERPRINT`]); the pragmas
    /// and the schema are then applied before returning.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        assert!(!path.as_os_str().is_empty(), "store path must not be empty");

        if schema_is_stale(path)? {
            discard_database(path);
        }

        let connection = Connection::open(path)?;

        Self::init(connection)
    }

    /// An ephemeral in-memory database, fully initialized. Intended for tests and
    /// smoke checks.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;

        Self::init(connection)
    }

    fn init(connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "recursive_triggers", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;

        connection.execute_batch(SCHEMA)?;
        connection.pragma_update(None, "user_version", SCHEMA_FINGERPRINT)?;

        Ok(Self { connection })
    }

    /// The schema fingerprint stamped into this database, identifying the schema it
    /// was built under.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        let version: i64 =
            self.connection.pragma_query_value(None, "user_version", |row| row.get(0))?;

        assert!(version >= 0, "user_version is non-negative");

        Ok(u32::try_from(version).unwrap_or(0))
    }

    /// A project row, recorded or refreshed. Must run before any of the project's
    /// files are persisted, as nodes and files reference it by foreign key.
    pub fn upsert_project(
        &self,
        id: &ProjectId,
        name: &str,
        root_path: &str,
    ) -> Result<(), StoreError> {
        assert!(!name.is_empty(), "project name must not be empty");
        assert!(!root_path.is_empty(), "project root_path must not be empty");

        self.connection.execute(
            "INSERT INTO projects (id, name, root_path, indexed_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 root_path = excluded.root_path,
                 indexed_at = excluded.indexed_at",
            params![id.as_str(), name, root_path, now_ms()?],
        )?;

        Ok(())
    }

    /// The extractor version stamp recorded for a project: the binary
    /// fingerprint that last fully indexed it, or empty when never stamped (a
    /// fresh project, or one indexed before stamping existed). A mismatch with
    /// the running binary tells the indexer to re-extract every file rather than
    /// trust the per-file content-hash skip, so an extractor change lands without
    /// a manual rebuild of the database.
    pub fn index_version(&self, project: &ProjectId) -> Result<String, StoreError> {
        let version: Option<String> = self
            .connection
            .query_row(
                "SELECT index_version FROM projects WHERE id = ?1",
                params![project.as_str()],
                |row| row.get(0),
            )
            .optional()?;

        Ok(version.unwrap_or_default())
    }

    /// A project stamped with the binary fingerprint that just fully indexed it,
    /// recorded after a successful index so a later run with the same binary can
    /// trust the content-hash skip again.
    pub fn set_index_version(&self, project: &ProjectId, version: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE projects SET index_version = ?2 WHERE id = ?1",
            params![project.as_str(), version],
        )?;

        Ok(())
    }

    /// The stamp recorded the last time `project`'s git history was ingested (its
    /// HEAD commit plus the extractor fingerprint), or `None` when never ingested.
    /// A caller compares it to the current state to skip re-ingesting unchanged
    /// history.
    pub fn git_ingest_stamp(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        let key = format!("git_ingest:{}", project.as_str());

        let stamp: Option<String> = self
            .connection
            .query_row("SELECT value FROM project_metadata WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?;

        Ok(stamp)
    }

    /// The git-history ingest stamp for `project` recorded, so the next run can
    /// detect that nothing changed and skip re-ingesting.
    pub fn set_git_ingest_stamp(&self, project: &ProjectId, stamp: &str) -> Result<(), StoreError> {
        assert!(!stamp.is_empty(), "ingest stamp must not be empty");

        let key = format!("git_ingest:{}", project.as_str());

        self.connection.execute(
            "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![key, stamp, now_ms()?],
        )?;

        Ok(())
    }

    /// A project marked reference-only (or not): its symbols are withheld from
    /// cross-project link targets while it stays fully queryable. Set after a
    /// version copy is indexed, from its config `reference` flag.
    pub fn set_reference_only(&self, project: &ProjectId, reference_only: bool) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE projects SET reference_only = ?2 WHERE id = ?1",
            params![project.as_str(), i64::from(reference_only)],
        )?;

        Ok(())
    }

    /// The ids of every reference-only project, the set the constellation linker
    /// excludes from cross-project link targets.
    pub fn reference_only_project_ids(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM projects WHERE reference_only != 0")?;

        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;

        let mut ids: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "reference-only load exceeded {ROWS_LOADED_MAX}");

            ids.push(row?);
        }

        Ok(ids)
    }

    /// The atomic persist of one file's extracted graph: clear the file's prior
    /// rows, then write the file row, its nodes, structural edges, and
    /// unresolved references in a single transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn persist_file(
        &self,
        project: &ProjectId,
        file: &FileIndex<'_>,
        nodes: &[Node],
        edges: &[Edge],
        references: &[UnresolvedRef],
        mappings: &[ImportMapping],
        events: &[EventRecord],
    ) -> Result<(), StoreError> {
        assert!(!file.path.is_empty(), "file path must not be empty");

        let node_count = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
        let indexed_at = now_ms()?;

        if self.connection.is_autocommit() {
            let transaction = self.connection.unchecked_transaction()?;

            persist_file_rows(
                &transaction, project, file, nodes, edges, references, mappings, events, node_count, indexed_at,
            )?;

            transaction.commit()?;
        } else {
            persist_file_rows(
                &self.connection, project, file, nodes, edges, references, mappings, events, node_count,
                indexed_at,
            )?;
        }

        Ok(())
    }

    /// The start of a bulk write transaction spanning many [`Store::persist_file`]
    /// calls, so the whole index commits once instead of fsyncing per file.
    /// Paired with [`Store::bulk_commit`] / [`Store::bulk_rollback`]; a no-op
    /// when a transaction is already open.
    pub fn bulk_begin(&self) -> Result<(), StoreError> {
        if self.connection.is_autocommit() {
            self.connection.execute_batch("BEGIN")?;
        }

        Ok(())
    }

    /// The commit of the bulk transaction opened by [`Store::bulk_begin`].
    pub fn bulk_commit(&self) -> Result<(), StoreError> {
        if !self.connection.is_autocommit() {
            self.connection.execute_batch("COMMIT")?;
        }

        Ok(())
    }

    /// The rollback of the bulk transaction after an error. Best-effort.
    pub fn bulk_rollback(&self) {
        if !self.connection.is_autocommit() {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }

    /// The number of nodes recorded for a project.
    pub fn count_nodes(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(&self.connection, "SELECT COUNT(*) FROM nodes WHERE project_id = ?1", project)
    }

    /// The number of files recorded for a project.
    pub fn count_files(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(&self.connection, "SELECT COUNT(*) FROM files WHERE project_id = ?1", project)
    }

    /// The number of references still awaiting resolution for a project.
    pub fn count_unresolved(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(
            &self.connection,
            "SELECT COUNT(*) FROM unresolved_refs WHERE project_id = ?1",
            project,
        )
    }

    /// The number of edges in the database (edges are not project-scoped: a single
    /// edge may cross a project boundary).
    pub fn count_edges(&self) -> Result<u32, StoreError> {
        let total: i64 = self.connection.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;

        assert!(total >= 0, "edge count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// Every project's id, name, root path, and last-indexed timestamp.
    pub fn all_projects(&self) -> Result<Vec<ProjectRow>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, name, root_path, indexed_at, reference_only FROM projects")?;

        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        let mut projects: Vec<ProjectRow> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "project load exceeded {ROWS_LOADED_MAX}");

            let (id, name, root_path, indexed_at, reference_only) = row?;

            if id.is_empty() || id.contains("::") {
                continue;
            }

            projects.push(ProjectRow {
                id: ProjectId::new(id),
                name,
                root_path,
                indexed_at,
                reference_only: reference_only != 0,
            });
        }

        Ok(projects)
    }

    /// A project's git history, replacing any previously recorded for it: the
    /// commit rows and the per-file churn each touched, written in one
    /// transaction (so a failed ingest leaves the prior history intact). Returns
    /// the number of commits stored.
    pub fn replace_history(
        &self,
        project: &ProjectId,
        commits: &[CommitRecord],
    ) -> Result<u32, StoreError> {
        assert!(!project.as_str().is_empty(), "project id must not be empty");

        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM git_commit WHERE project_id = ?1",
            params![project.as_str()],
        )?;

        let mut stored: u32 = 0;

        for commit in commits {
            assert!(stored < u32::MAX, "commit count must not overflow");

            transaction.execute(
                "INSERT OR REPLACE INTO git_commit
                     (project_id, commit_hash, author, committed_at, summary)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.as_str(),
                    commit.commit_hash,
                    commit.author,
                    commit.committed_at,
                    commit.summary,
                ],
            )?;

            for file in &commit.files {
                transaction.execute(
                    "INSERT INTO git_commit_file
                         (project_id, commit_hash, file_path, insertions, deletions)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        project.as_str(),
                        commit.commit_hash,
                        file.file_path,
                        file.insertions,
                        file.deletions,
                    ],
                )?;
            }

            stored += 1;
        }

        transaction.commit()?;

        Ok(stored)
    }

    /// The number of commits recorded in `project`'s history, zero until
    /// [`Store::replace_history`] has run for it.
    pub fn count_history_commits(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(
            &self.connection,
            "SELECT COUNT(*) FROM git_commit WHERE project_id = ?1",
            project,
        )
    }

    /// The commits touching files whose path matches `path_like` (a SQL `LIKE`
    /// pattern), newest first, with churn aggregated over only the matching
    /// files. `project` scopes the search to one project when given. The timeline
    /// behind `constellation_history`.
    pub fn history_for_path(
        &self,
        project: Option<&ProjectId>,
        path_like: &str,
        limit: u32,
    ) -> Result<Vec<HistoryHit>, StoreError> {
        assert!(!path_like.is_empty(), "path pattern must not be empty");

        let project_filter = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare(
            "SELECT c.project_id, c.commit_hash, c.author, c.committed_at, c.summary,
                    COUNT(f.file_path), COALESCE(SUM(f.insertions), 0), COALESCE(SUM(f.deletions), 0)
             FROM git_commit c
             JOIN git_commit_file f
                 ON f.project_id = c.project_id AND f.commit_hash = c.commit_hash
             WHERE f.file_path LIKE ?1
               AND (?2 IS NULL OR c.project_id = ?2)
             GROUP BY c.project_id, c.commit_hash
             ORDER BY c.committed_at DESC, c.commit_hash
             LIMIT ?3",
        )?;

        let rows = statement.query_map(params![path_like, project_filter, limit], |row| {
            Ok(HistoryHit {
                project_id: row.get(0)?,
                commit_hash: row.get(1)?,
                author: row.get(2)?,
                committed_at: row.get(3)?,
                summary: row.get(4)?,
                files_changed: u32::try_from(row.get::<_, i64>(5)?).unwrap_or(u32::MAX),
                insertions: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(u32::MAX),
                deletions: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(u32::MAX),
            })
        })?;

        let mut hits: Vec<HistoryHit> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "history load exceeded {ROWS_LOADED_MAX}");

            hits.push(row?);
        }

        Ok(hits)
    }

    /// A project's symbol-level history, replacing any previously recorded for it:
    /// the per-commit added/modified/removed rows from diffing each file's
    /// trackable symbols across revisions, written in one transaction. Returns the
    /// number of rows stored. Requires the commit rows ([`Store::replace_history`])
    /// to exist; the rows cascade away with their commit.
    pub fn replace_symbol_revisions(
        &self,
        project: &ProjectId,
        revisions: &[SymbolRevision],
    ) -> Result<u32, StoreError> {
        assert!(!project.as_str().is_empty(), "project id must not be empty");

        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM git_symbol_revision WHERE project_id = ?1",
            params![project.as_str()],
        )?;

        let mut stored: u32 = 0;

        for revision in revisions {
            assert!(stored < u32::MAX, "revision count must not overflow");

            transaction.execute(
                "INSERT INTO git_symbol_revision
                     (project_id, commit_hash, file_path, qualified_name, name, kind, change_kind, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    project.as_str(),
                    revision.commit_hash,
                    revision.file_path,
                    revision.qualified_name,
                    revision.name,
                    revision.kind,
                    revision.change.as_str(),
                    revision.signature,
                ],
            )?;

            stored += 1;
        }

        transaction.commit()?;

        Ok(stored)
    }

    /// The number of symbol-change rows recorded for `project`.
    pub fn count_symbol_revisions(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(
            &self.connection,
            "SELECT COUNT(*) FROM git_symbol_revision WHERE project_id = ?1",
            project,
        )
    }

    /// Whether any symbol-revision rows exist (optionally scoped to one project):
    /// whether the `history --symbols` pass has populated the timeline at all. Lets
    /// the empty-result hint tell "the symbol pass never ran" apart from "it ran but
    /// nothing matches this query".
    pub fn has_symbol_revisions(&self, project: Option<&ProjectId>) -> Result<bool, StoreError> {
        let project_filter = project.map(|project| project.as_str().to_string());

        let present: i64 = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM git_symbol_revision WHERE (?1 IS NULL OR project_id = ?1))",
            params![project_filter],
            |row| row.get(0),
        )?;

        Ok(present != 0)
    }

    /// Every commit that touched a file in `project`, ordered by file then commit
    /// time, so a caller can diff each file's revisions in chronological order.
    /// Capped at `max` touches.
    pub fn history_file_touches(
        &self,
        project: &ProjectId,
        max: u32,
    ) -> Result<Vec<FileTouch>, StoreError> {
        assert!(max > 0, "touch cap must be positive");

        let mut statement = self.connection.prepare(
            "SELECT f.file_path, f.commit_hash
             FROM git_commit_file f
             JOIN git_commit c
                 ON c.project_id = f.project_id AND c.commit_hash = f.commit_hash
             WHERE f.project_id = ?1
             ORDER BY f.file_path, c.committed_at, f.commit_hash
             LIMIT ?2",
        )?;

        let rows = statement.query_map(params![project.as_str(), max], |row| {
            Ok(FileTouch { file_path: row.get(0)?, commit_hash: row.get(1)? })
        })?;

        let mut touches: Vec<FileTouch> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= max, "touch load exceeded the cap {max}");

            touches.push(row?);
        }

        Ok(touches)
    }

    /// A symbol's recorded change history, newest first: the commits where a
    /// definition matching `symbol` (by exact name, exact qualified name, a longer
    /// qualified name ending in `.symbol`, or an `Owner.member` path sitting just
    /// past the `file_path::` prefix) was added, modified, or removed. The `.`-suffix
    /// match targets a nested member (`Order.total` finds `shipping.Order.total`); the
    /// `::`-suffix match targets a member of a top-level owner (`Order.total` finds
    /// `models.py::Order.total`, and `Order` finds `models.py::Order`) without matching
    /// a deeper same-named member. `project` scopes it. The timeline behind
    /// `constellation_symbol_history`.
    pub fn symbol_history(
        &self,
        project: Option<&ProjectId>,
        symbol: &str,
        limit: u32,
    ) -> Result<Vec<SymbolHistoryHit>, StoreError> {
        assert!(!symbol.is_empty(), "symbol must not be empty");

        let project_filter = project.map(|project| project.as_str().to_string());
        let member_suffix = format!("%.{symbol}");
        let owner_suffix = format!("%::{symbol}");

        let mut statement = self.connection.prepare(
            "SELECT s.project_id, s.commit_hash, c.committed_at, s.qualified_name, s.kind,
                    s.change_kind, s.signature, c.summary
             FROM git_symbol_revision s
             JOIN git_commit c
                 ON c.project_id = s.project_id AND c.commit_hash = s.commit_hash
             WHERE (s.qualified_name = ?1 OR s.name = ?1
                    OR s.qualified_name LIKE ?2 OR s.qualified_name LIKE ?3)
               AND (?4 IS NULL OR s.project_id = ?4)
             ORDER BY c.committed_at DESC, s.commit_hash, s.qualified_name
             LIMIT ?5",
        )?;

        let rows = statement.query_map(
            params![symbol, member_suffix, owner_suffix, project_filter, limit],
            |row| {
            Ok(SymbolHistoryHit {
                project_id: row.get(0)?,
                commit_hash: row.get(1)?,
                committed_at: row.get(2)?,
                qualified_name: row.get(3)?,
                kind: row.get(4)?,
                change: row.get(5)?,
                signature: row.get(6)?,
                summary: row.get(7)?,
            })
        })?;

        let mut hits: Vec<SymbolHistoryHit> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "symbol history load exceeded {ROWS_LOADED_MAX}");

            hits.push(row?);
        }

        Ok(hits)
    }

    /// The symbols alive as of `at_committed_at` (epoch seconds), reconstructed
    /// from the symbol-revision log: a symbol counts as alive when its latest
    /// change at or before that time was an add or a modify (not a removal), and
    /// the signature returned is the one in effect then. `path_like` (a SQL `LIKE`
    /// pattern) scopes to matching files, `project` to one project. The state
    /// behind `constellation_as_of`. Only symbols that changed within the indexed
    /// history window appear: one added before the earliest indexed commit, and
    /// untouched since, is not in the log and so is not reported.
    pub fn symbols_as_of(
        &self,
        project: Option<&ProjectId>,
        at_committed_at: i64,
        path_like: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AsOfSymbol>, StoreError> {
        let project_filter = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare(
            "WITH events AS (
                 SELECT s.project_id, s.file_path, s.qualified_name, s.kind, s.change_kind, s.signature,
                        ROW_NUMBER() OVER (
                            PARTITION BY s.project_id, s.file_path, s.qualified_name
                            ORDER BY c.committed_at DESC, s.commit_hash DESC
                        ) AS rank
                 FROM git_symbol_revision s
                 JOIN git_commit c
                     ON c.project_id = s.project_id AND c.commit_hash = s.commit_hash
                 WHERE c.committed_at <= ?1
                   AND (?2 IS NULL OR s.project_id = ?2)
                   AND (?3 IS NULL OR s.file_path LIKE ?3)
             )
             SELECT project_id, file_path, qualified_name, kind, signature
             FROM events
             WHERE rank = 1 AND change_kind <> 'removed'
             ORDER BY project_id, file_path, qualified_name
             LIMIT ?4",
        )?;

        let rows = statement.query_map(
            params![at_committed_at, project_filter, path_like, limit],
            |row| {
                Ok(AsOfSymbol {
                    project_id: row.get(0)?,
                    file_path: row.get(1)?,
                    qualified_name: row.get(2)?,
                    kind: row.get(3)?,
                    signature: row.get(4)?,
                })
            },
        )?;

        let mut symbols: Vec<AsOfSymbol> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "as-of load exceeded {ROWS_LOADED_MAX}");

            symbols.push(row?);
        }

        assert!(symbols.len() as u32 <= limit, "as-of result respects the limit");

        Ok(symbols)
    }

    /// The committer time (epoch seconds) of the commit whose hash matches
    /// `hash_prefix` (the newest, if a short prefix is ambiguous), or `None` when
    /// none matches. `project` scopes the lookup. Resolves an as-of point given a
    /// commit hash rather than a date.
    pub fn commit_committed_at(
        &self,
        project: Option<&ProjectId>,
        hash_prefix: &str,
    ) -> Result<Option<i64>, StoreError> {
        assert!(!hash_prefix.is_empty(), "hash prefix must not be empty");

        let project_filter = project.map(|project| project.as_str().to_string());
        let prefix = format!("{hash_prefix}%");

        let result = self.connection.query_row(
            "SELECT committed_at FROM git_commit
             WHERE commit_hash LIKE ?1 AND (?2 IS NULL OR project_id = ?2)
             ORDER BY committed_at DESC
             LIMIT 1",
            params![prefix, project_filter],
            |row| row.get::<_, i64>(0),
        );

        match result {
            Ok(time) => Ok(Some(time)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Every recorded file path mapped to its stored content hash, for a project.
    /// Used to skip re-extracting unchanged files on re-index.
    pub fn file_hashes(&self, project: &ProjectId) -> Result<FxHashMap<String, String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT path, content_hash FROM files WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut hashes: FxHashMap<String, String> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "file hash load exceeded {ROWS_LOADED_MAX}");

            let (path, hash) = row?;
            hashes.insert(path, hash);
        }

        Ok(hashes)
    }

    /// Every recorded file path mapped to its stored modification time (epoch ms),
    /// for a project. The staleness baseline `status` compares the working tree
    /// against, without reading file contents.
    pub fn file_mtimes(&self, project: &ProjectId) -> Result<FxHashMap<String, i64>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT path, modified_at FROM files WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut mtimes: FxHashMap<String, i64> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "file mtime load exceeded {ROWS_LOADED_MAX}");

            let (path, mtime) = row?;
            mtimes.insert(path, mtime);
        }

        Ok(mtimes)
    }

    /// The removal of a file and its graph (nodes cascade to edges and references).
    /// Used when a file disappears from disk between indexes.
    pub fn remove_file(&self, project: &ProjectId, path: &str) -> Result<(), StoreError> {
        assert!(!path.is_empty(), "file path must not be empty");

        if self.connection.is_autocommit() {
            let transaction = self.connection.unchecked_transaction()?;

            clear_file(&transaction, project, path)?;

            transaction.commit()?;
        } else {
            clear_file(&self.connection, project, path)?;
        }

        Ok(())
    }

    /// Every edge as a (source id, target id) pair, for building the in-memory
    /// adjacency that structural ranking walks.
    pub fn all_edges(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached("SELECT source, target FROM edges")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "edge load exceeded {ROWS_LOADED_MAX}");

            edges.push(row?);
        }

        Ok(edges)
    }

    /// Every edge as `(source, target, kind)`, the directed, kinded form the
    /// explore cache needs to trace a call path between named symbols (`all_edges`
    /// drops the kind for the undirected random-walk adjacency).
    pub fn all_edges_kinded(&self) -> Result<Vec<(String, String, EdgeKind)>, StoreError> {
        let mut statement = self.connection.prepare_cached("SELECT source, target, kind FROM edges")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut edges: Vec<(String, String, EdgeKind)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "edge load exceeded {ROWS_LOADED_MAX}");

            let (source, target, kind) = row?;

            if let Some(edge_kind) = EdgeKind::from_str_label(&kind) {
                edges.push((source, target, edge_kind));
            }
        }

        Ok(edges)
    }

    /// Every import mapping for a project, paired with the file it was
    /// declared in.
    pub fn all_import_mappings(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(String, ImportMapping)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT file_path, local_name, exported_name, source, is_default, is_namespace
             FROM import_mappings WHERE project_id = ?1",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ImportMapping {
                    local_name: row.get(1)?,
                    exported_name: row.get(2)?,
                    source: row.get(3)?,
                    is_default: row.get::<_, i64>(4)? != 0,
                    is_namespace: row.get::<_, i64>(5)? != 0,
                    resolved_path: None,
                },
            ))
        })?;

        let mut mappings: Vec<(String, ImportMapping)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "import-mapping load exceeded {ROWS_LOADED_MAX}");

            mappings.push(row?);
        }

        Ok(mappings)
    }

    /// The filesystem root recorded for a project, if it exists.
    pub fn project_root(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        let root = self
            .connection
            .query_row(
                "SELECT root_path FROM projects WHERE id = ?1",
                params![project.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(root)
    }

    /// The nodes, scoped to one project or (with `None`) across every
    /// project in the database. Rows whose stored enums no longer parse are
    /// skipped rather than aborting the load.
    pub fn all_nodes(&self, project: Option<&ProjectId>) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE (?1 IS NULL OR project_id = ?1)",
        )?;

        let rows = statement.query_map(params![project.map(ProjectId::as_str)], node_row)?;

        let mut nodes: Vec<Node> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "node load exceeded {ROWS_LOADED_MAX}");

            if let Some(node) = node_from_row(row?) {
                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// The nodes in a project matching a fixed single-column predicate (`name = ?2`,
    /// `lower(name) = ?2`, ...) bound to `value`. The predicate is a constant
    /// fragment (never user input), so interpolating it is safe. Backs the
    /// scoped lookups that resolve references without loading the whole graph.
    fn nodes_filtered(
        &self,
        project: &ProjectId,
        predicate: &str,
        value: &str,
    ) -> Result<Vec<Node>, StoreError> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE project_id = ?1 AND {predicate}");
        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![project.as_str(), value], node_row)?;

        let mut nodes: Vec<Node> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "scoped node load exceeded {ROWS_LOADED_MAX}");

            if let Some(node) = node_from_row(row?) {
                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// The nodes in a project with an exact name.
    pub fn nodes_named_in(&self, project: &ProjectId, name: &str) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "name = ?2", name)
    }

    /// The nodes in a project whose lower-cased name matches `lower_name`.
    pub fn nodes_lower_named_in(
        &self,
        project: &ProjectId,
        lower_name: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "lower(name) = ?2", lower_name)
    }

    /// The nodes in a project with an exact qualified name.
    pub fn nodes_qualified_in(
        &self,
        project: &ProjectId,
        qualified_name: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "qualified_name = ?2", qualified_name)
    }

    /// The nodes of a given kind in a project.
    pub fn nodes_kind_in(
        &self,
        project: &ProjectId,
        kind: NodeKind,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "kind = ?2", kind.as_str())
    }

    /// The nodes declared in one file of a project.
    pub fn nodes_file_in(
        &self,
        project: &ProjectId,
        file_path: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "file_path = ?2", file_path)
    }

    /// Every indexed file in a project with its language, symbol count, and
    /// size, ordered by path. Backs the `files` listing tool.
    pub fn files_for(&self, project: &ProjectId) -> Result<Vec<FileRow>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT path, language, node_count, size_bytes FROM files
             WHERE project_id = ?1 ORDER BY path",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok(FileRow {
                path: row.get::<_, String>(0)?,
                language: row.get::<_, String>(1)?,
                node_count: row.get::<_, i64>(2)?,
                size_bytes: row.get::<_, i64>(3)?,
            })
        })?;

        let mut files: Vec<FileRow> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "file load exceeded {ROWS_LOADED_MAX}");

            files.push(row?);
        }

        Ok(files)
    }

    /// The distinct file paths that hold at least one node in a project.
    pub fn project_file_paths(&self, project: &ProjectId) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT file_path FROM nodes WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| row.get::<_, String>(0))?;

        let mut paths: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "file-path load exceeded {ROWS_LOADED_MAX}");

            paths.push(row?);
        }

        Ok(paths)
    }

    /// The import mappings declared in one file of a project.
    pub fn import_mappings_in(
        &self,
        project: &ProjectId,
        file_path: &str,
    ) -> Result<Vec<ImportMapping>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT local_name, exported_name, source, is_default, is_namespace
             FROM import_mappings WHERE project_id = ?1 AND file_path = ?2",
        )?;

        let rows = statement.query_map(params![project.as_str(), file_path], |row| {
            Ok(ImportMapping {
                local_name: row.get(0)?,
                exported_name: row.get(1)?,
                source: row.get(2)?,
                is_default: row.get::<_, i64>(3)? != 0,
                is_namespace: row.get::<_, i64>(4)? != 0,
                resolved_path: None,
            })
        })?;

        let mut mappings: Vec<ImportMapping> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "scoped import-mapping load exceeded {ROWS_LOADED_MAX}");

            mappings.push(row?);
        }

        Ok(mappings)
    }

    /// Every event-channel observation for a project, for correlating
    /// dispatch sites with listener registrations during edge synthesis.
    pub fn events_for(&self, project: &ProjectId) -> Result<Vec<EventRecord>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT role, event_name, symbol, line, column FROM events WHERE project_id = ?1",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            let role: String = row.get(0)?;

            Ok(EventRecord {
                role: if role == "dispatch" { EventRole::Dispatch } else { EventRole::Listen },
                event: row.get(1)?,
                symbol: row.get(2)?,
                line: row.get(3)?,
                column: row.get(4)?,
            })
        })?;

        let mut events: Vec<EventRecord> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "event load exceeded {ROWS_LOADED_MAX}");

            events.push(row?);
        }

        Ok(events)
    }

    /// The replacement of a project's synthesized edges (those tagged with a `synthesis:`
    /// provenance) with `edges`, atomically. The synthesis pass re-derives them
    /// from scratch each index, so the prior set is cleared first (scoped to
    /// this project's nodes) to avoid duplicates. Returns the number written.
    pub fn replace_synthesized_edges(
        &self,
        project: &ProjectId,
        prefix: &str,
        edges: &[Edge],
    ) -> Result<u32, StoreError> {
        assert!(prefix.starts_with("synthesis:"), "synthesized provenance is namespaced under synthesis:");

        let pattern = format!("{prefix}%");
        let transaction = self.connection.unchecked_transaction()?;

        // Scope the clear to this provenance prefix so independent synthesis
        // passes (events, reverse relations) do not clobber one another's edges.
        transaction.execute(
            "DELETE FROM edges WHERE provenance LIKE ?2
             AND source IN (SELECT id FROM nodes WHERE project_id = ?1)",
            params![project.as_str(), pattern],
        )?;

        insert_edges(&transaction, edges)?;
        transaction.commit()?;

        Ok(u32::try_from(edges.len()).unwrap_or(u32::MAX))
    }

    /// The genuine forward `relates_to` relations whose source is in `project`, as
    /// `(source_id, target_id)`. Excludes the reverse relations a prior synthesis
    /// pass added (provenance `synthesis:reverse%`), so re-deriving reverses from
    /// this set is idempotent: it never feeds its own output back in.
    pub fn relation_edges(&self, project: &ProjectId) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT e.source, e.target FROM edges e
             JOIN nodes s ON e.source = s.id
             WHERE e.kind = 'relates_to' AND s.project_id = ?1
               AND (e.provenance IS NULL OR e.provenance NOT LIKE 'synthesis:reverse%')",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut relations: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "relation load exceeded {ROWS_LOADED_MAX}");

            relations.push(row?);
        }

        Ok(relations)
    }

    /// The resolved `extends` edges whose source is in `project`, as
    /// `(subclass_id, base_id)`, the in-project class hierarchy the override
    /// synthesis walks. A base resolved to a third-party `External` node is
    /// included; the override pass simply finds no method to bind under it.
    pub fn extends_edges(&self, project: &ProjectId) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT e.source, e.target FROM edges e
             JOIN nodes s ON e.source = s.id
             WHERE e.kind = 'extends' AND s.project_id = ?1",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "extends load exceeded {ROWS_LOADED_MAX}");

            edges.push(row?);
        }

        Ok(edges)
    }

    /// The `(id, name)` of every method node in `project`, the symbols the
    /// override synthesis matches by name against each class's ancestors.
    pub fn class_methods(&self, project: &ProjectId) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT id, name FROM nodes WHERE project_id = ?1 AND kind = 'method'")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut methods: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "method load exceeded {ROWS_LOADED_MAX}");

            methods.push(row?);
        }

        Ok(methods)
    }

    /// The deletion of a project's still-pending references of one edge kind, returning
    /// how many were removed. Backs the styles gate: a `styles` reference that
    /// matched no indexed selector can never resolve (the project's CSS is fully
    /// known by the time resolution finishes), so persisting it is dead weight.
    pub fn delete_unresolved_kind(
        &self,
        project: &ProjectId,
        kind: EdgeKind,
    ) -> Result<u32, StoreError> {
        let removed = self.connection.execute(
            "DELETE FROM unresolved_refs WHERE project_id = ?1 AND reference_kind = ?2",
            params![project.as_str(), kind.as_str()],
        )?;

        Ok(u32::try_from(removed).unwrap_or(u32::MAX))
    }

    /// The replacement of a project's route reverse-name index: every prior row for
    /// the project is cleared, then the freshly computed `(reverse_name, route_id)`
    /// pairs are written, so each index re-derives the project's reverse names from
    /// scratch. Read across projects by [`Store::route_reverse_names`] for
    /// cross-project `{% url %}`/reverse resolution.
    pub fn replace_route_reverse_names(
        &self,
        project: &ProjectId,
        names: &[(String, String)],
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM route_reverse_name WHERE project_id = ?1",
            params![project.as_str()],
        )?;

        {
            let mut insert = transaction.prepare(
                "INSERT INTO route_reverse_name (project_id, reverse_name, route_id) VALUES (?1, ?2, ?3)",
            )?;

            for (reverse_name, route_id) in names {
                insert.execute(params![project.as_str(), reverse_name, route_id])?;
            }
        }

        transaction.commit()?;

        Ok(u32::try_from(names.len()).unwrap_or(u32::MAX))
    }

    /// Every route reverse name across all projects, as `(project_id, reverse_name,
    /// route_id)`, for the cross-project linker to resolve a namespaced reverse into
    /// the route another project defines.
    pub fn route_reverse_names(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT project_id, reverse_name, route_id FROM route_reverse_name")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut names: Vec<(String, String, String)> = Vec::new();

        for row in rows {
            names.push(row?);
        }

        Ok(names)
    }

    /// The replacement of the project's synthesized External symbol nodes and the edges into
    /// them. External nodes (kind `external`) are cleared first (deleting a node
    /// cascades to its edges) then re-created, so each index re-derives the
    /// library-boundary layer from scratch (like [`Store::replace_synthesized_edges`]).
    pub fn replace_external(
        &self,
        project: &ProjectId,
        nodes: &[Node],
        edges: &[Edge],
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM nodes WHERE project_id = ?1 AND kind = 'external'",
            params![project.as_str()],
        )?;

        insert_nodes(&transaction, nodes, now_ms()?)?;
        insert_edges(&transaction, edges)?;

        transaction.commit()?;

        Ok(u32::try_from(edges.len()).unwrap_or(u32::MAX))
    }

    /// The pending references, each paired with its row id so a resolved
    /// reference can be deleted by id once its edge is written. Scoped to one
    /// project, or (with `None`) across every project.
    pub fn load_unresolved(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<(i64, UnresolvedRef)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, from_node_id, reference_name, reference_kind, line, column, file_path, language, candidates
             FROM unresolved_refs WHERE (?1 IS NULL OR project_id = ?1)",
        )?;

        let rows = statement.query_map(params![project.map(ProjectId::as_str)], reference_row)?;

        let mut references: Vec<(i64, UnresolvedRef)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "reference load exceeded {ROWS_LOADED_MAX}");

            if let Some(pair) = reference_from_row(row?) {
                references.push(pair);
            }
        }

        Ok(references)
    }

    /// The atomic write of each resolved edge, deleting the reference it
    /// resolved. The two arrays move in lockstep: `resolved[i]` is the row id
    /// of the reference that produced `edges[i]`.
    pub fn commit_resolved(&self, resolved: &[(i64, Edge)]) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut written: u32 = 0;

        {
            let mut insert = transaction.prepare(
                "INSERT INTO edges (source, target, kind, line, column, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;

            let mut delete = transaction.prepare("DELETE FROM unresolved_refs WHERE id = ?1")?;

            for (reference_id, edge) in resolved {
                written += 1;

                assert!(written <= ROWS_LOADED_MAX, "resolved commit exceeded {ROWS_LOADED_MAX}");

                insert.execute(params![
                    edge.source.as_str(),
                    edge.target.as_str(),
                    edge.kind.as_str(),
                    edge.line,
                    edge.column,
                    edge.provenance,
                ])?;

                delete.execute(params![reference_id])?;
            }
        }

        transaction.commit()?;

        Ok(written)
    }

    /// The deletion of every pending reference that now has a satisfying edge.
    /// External synthesis and the synthesis passes write their edges in bulk
    /// (`replace_external`, `replace_synthesized_edges`) rather than through
    /// [`Store::commit_resolved`], so the reference rows they satisfy are never
    /// deleted by id and linger as false pending rows. Run once after every
    /// resolution, linking, and synthesis pass has emitted its edges, this clears
    /// those now-resolved rows so the pending table holds only references that bind
    /// to nothing. Returns the number of rows removed.
    ///
    /// Two clauses, both safe against deleting a genuinely-unresolved reference:
    /// - A location match (same source, kind, line, column): the precise key, so a
    ///   reference is never dropped because a same-named sibling on its line
    ///   resolved (`f(g(x))`).
    /// - A name match to a template or external target (same source and kind, target
    ///   node named the reference). External synthesis deduplicates its edges by
    ///   (source, target, kind), so an `{% include %}` repeated down a card, or a
    ///   library symbol called on many lines, yields one edge at the first line and
    ///   leaves the later locations unmatched by the first clause. Template names are
    ///   globally namespaced and an external node is named for the very symbol the
    ///   reference imports/calls, so a name match to one of those kinds is the same
    ///   resolution, not a collision.
    pub fn delete_satisfied_unresolved(&self) -> Result<u32, StoreError> {
        let by_location = self.connection.execute(
            "DELETE FROM unresolved_refs
             WHERE EXISTS (
                 SELECT 1 FROM edges e
                 WHERE e.source = unresolved_refs.from_node_id
                   AND e.kind = unresolved_refs.reference_kind
                   AND e.line = unresolved_refs.line
                   AND e.column = unresolved_refs.column
             )",
            [],
        )?;

        let by_name = self.connection.execute(
            "DELETE FROM unresolved_refs
             WHERE EXISTS (
                 SELECT 1 FROM edges e
                 JOIN nodes n ON n.id = e.target
                 WHERE e.source = unresolved_refs.from_node_id
                   AND e.kind = unresolved_refs.reference_kind
                   AND n.name = unresolved_refs.reference_name
                   AND n.kind IN ('template', 'external')
             )",
            [],
        )?;

        Ok(u32::try_from(by_location + by_name).unwrap_or(u32::MAX))
    }

    /// The collapse of each external stub into the real cross-project definition it
    /// shadows: redirect every edge pointing at the stub to the definition, then
    /// delete the stub. Retargets *before* deleting so the `ON DELETE CASCADE` on
    /// `edges.target` cannot drop the just-redirected edges. After this a model
    /// that "extends an external mixin" extends the real, indexed class across the
    /// project boundary, and `node`/`search` show one definition, not a definition
    /// plus per-project import stubs. Returns the number of stubs unified.
    pub fn unify_externals(&self, redirects: &[(NodeId, NodeId)]) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut unified: u32 = 0;

        {
            let mut retarget = transaction.prepare("UPDATE edges SET target = ?1 WHERE target = ?2")?;
            let mut delete = transaction.prepare("DELETE FROM nodes WHERE id = ?1")?;

            for (stub, definition) in redirects {
                unified += 1;

                assert!(unified <= ROWS_LOADED_MAX, "unify exceeded {ROWS_LOADED_MAX}");

                retarget.execute(params![definition.as_str(), stub.as_str()])?;
                delete.execute(params![stub.as_str()])?;
            }
        }

        transaction.commit()?;

        Ok(unified)
    }

    /// The number of cross-project edges (those the linker tagged with a `link:`
    /// provenance).
    pub fn count_links(&self) -> Result<u32, StoreError> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM edges WHERE provenance LIKE 'link:%'",
            [],
            |row| row.get(0),
        )?;

        assert!(total >= 0, "link count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The cross-project link edges, both endpoints hydrated, newest first by
    /// edge id. These are the constellation's connective tissue: an import in one
    /// repo resolved to a definition in another, the links no single-repo index holds.
    /// Bounded by `limit`.
    pub fn link_edges(&self, project: Option<&str>, limit: u32) -> Result<Vec<LinkEdge>, StoreError> {
        let source_columns = node_columns("s");
        let target_columns = node_columns("t");

        let sql = format!(
            "SELECT {source_columns}, e.kind, e.provenance, {target_columns}
             FROM edges e
             JOIN nodes s ON e.source = s.id
             JOIN nodes t ON e.target = t.id
             WHERE e.provenance LIKE 'link:%'
               AND (?2 IS NULL OR s.project_id = ?2 OR t.project_id = ?2)
             ORDER BY e.id DESC LIMIT ?1",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![limit, project], |row| {
            let source = node_row_at(row, 0)?;
            let kind = row.get::<_, String>(20)?;
            let provenance = row.get::<_, Option<String>>(21)?;
            let target = node_row_at(row, 22)?;

            Ok((source, kind, provenance, target))
        })?;

        let mut links: Vec<LinkEdge> = Vec::with_capacity((limit as usize).min(PREALLOC_ROWS_MAX));
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "link load exceeded {ROWS_LOADED_MAX}");

            let (source, kind, provenance, target) = row?;

            if let (Some(source), Some(target), Some(kind)) =
                (node_from_row(source), node_from_row(target), EdgeKind::from_str_label(&kind))
            {
                links.push(LinkEdge { source, target, kind, provenance: provenance.unwrap_or_default() });
            }
        }

        Ok(links)
    }

    /// The number of still-unresolved references naming `symbol`, across all projects.
    /// These are call sites that named the symbol but never bound to an edge:
    /// dynamic dispatch (a chained queryset, `get_queryset`, a template or admin
    /// reference) or a missing import. A non-zero count means a symbol's real
    /// callers are likely undercounted by the resolved edges alone: a trust signal
    /// the agent needs before treating a low caller count as "safe to change".
    pub fn count_unresolved_named(&self, symbol: &str) -> Result<u32, StoreError> {
        assert!(!symbol.is_empty(), "symbol must not be empty");

        // The template member-access pipeline (`accesses_member` for a
        // `{{ var.attr }}`, `context_type` for a view's `var -> model` binding)
        // leaves its references pending by design: a synthesis pass consumes
        // them, so they are not unresolved callers and must not inflate the
        // dark-caller trust signal for a member or model name.
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM unresolved_refs
             WHERE reference_name = ?1
               AND reference_kind NOT IN
                   ('accesses_member', 'context_type', 'loop_binding', 'reverse_accessor',
                    'derived_collection')",
            params![symbol],
            |row| row.get(0),
        )?;

        assert!(total >= 0, "unresolved count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The per-kind node counts for one project, as `(kind, count)`, the cheap GROUP
    /// BY that backs a project overview without loading every node.
    pub fn kind_counts(&self, project: &ProjectId) -> Result<Vec<(NodeKind, u32)>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT kind, COUNT(*) FROM nodes WHERE project_id = ?1 GROUP BY kind")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut counts: Vec<(NodeKind, u32)> = Vec::new();

        for row in rows {
            let (label, count) = row?;

            if let Some(kind) = NodeKind::from_str_label(&label) {
                counts.push((kind, u32::try_from(count.max(0)).unwrap_or(u32::MAX)));
            }
        }

        Ok(counts)
    }

    /// The nodes matching a prefix full-text search over their names, qualified
    /// names, docstrings, and signatures.
    pub fn search_nodes(&self, query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        self.search_nodes_matching(&fts_prefix_query(query), limit)
    }

    /// The any-token variant of [`Store::search_nodes`]: matches ANY query token (OR), not all.
    /// The forgiving fallback for multi-word or natural-language explore queries
    /// an all-tokens prefix match would miss.
    pub fn search_nodes_any(&self, query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        self.search_nodes_matching(&fts_any_query(query), limit)
    }

    /// The files whose source content matches `query`, ranked by full-text relevance
    /// (bm25 over the porter-stemmed body index), as `(project, file_path)`.
    /// Explore seeds its structural ranking from the definitions in these files,
    /// so a method found only by an identifier in its body still surfaces. Empty
    /// for a database indexed before content was captured.
    pub fn search_content(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<(ProjectId, String)>, StoreError> {
        let match_query = fts_any_query(query);

        if match_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = self.connection.prepare_cached(
            "SELECT fc.project_id, fc.file_path FROM file_content_fts f
             JOIN file_content fc ON fc.rowid = f.rowid
             WHERE file_content_fts MATCH ?1 ORDER BY bm25(file_content_fts) LIMIT ?2",
        )?;

        let rows = statement.query_map(params![match_query, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut hits: Vec<(ProjectId, String)> =
            Vec::with_capacity((limit as usize).min(PREALLOC_ROWS_MAX));

        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "content hit load exceeded {ROWS_LOADED_MAX}");

            let (project_id, file_path) = row?;
            hits.push((ProjectId::new(project_id), file_path));
        }

        Ok(hits)
    }

    fn search_nodes_matching(&self, match_query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        if match_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = self.connection.prepare_cached(&SEARCH_SQL)?;
        let rows = statement.query_map(params![match_query, limit], node_row)?;

        collect_nodes_capacity(rows, limit)
    }

    /// Every node with the given simple name, across all projects.
    pub fn nodes_named(&self, name: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE name = ?1",
        )?;

        let rows = statement.query_map(params![name], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes whose name equals `suffix` or ends with `/suffix`, addressing a node
    /// by its basename, chiefly a template by filename (`research_page.html` finds
    /// `partner/page/research_page.html`). Bounded; a fallback for when an
    /// exact-name lookup found nothing.
    pub fn nodes_named_suffix(&self, suffix: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE name = ?1 OR name LIKE '%/' || ?1 LIMIT 50",
        )?;

        let rows = statement.query_map(params![suffix], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes with an exact qualified name, across all projects. Lets a tool
    /// target the precise node it printed (`file.py::Owner.member`, or a route's
    /// `file.py::route::<url>`) regardless of how its display name collides.
    pub fn nodes_qualified(&self, qualified_name: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE qualified_name = ?1",
        )?;

        let rows = statement.query_map(params![qualified_name], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes whose file path ends with `file_suffix` and whose span covers
    /// `line`, innermost (smallest span) first. Backs `constellation_at`: a
    /// file:line from a traceback or grep hit mapped to its enclosing symbol. The
    /// suffix match lets a bare `views.py` or a longer `app/views.py` both hit.
    pub fn nodes_at(&self, file_suffix: &str, line: u32) -> Result<Vec<Node>, StoreError> {
        assert!(line >= 1, "line is 1-based");

        let pattern = format!("%{}", file_suffix.replace('\\', "/"));

        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes
             WHERE file_path LIKE ?1 AND start_line <= ?2 AND end_line >= ?2
             ORDER BY (end_line - start_line) ASC LIMIT 8",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params![pattern, line], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes (and edge kinds) that reference the target node.
    pub fn callers(&self, target: &NodeId) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        self.edges_join(&CALLERS_SQL, target)
    }

    /// The nodes (and edge kinds) the source node references.
    pub fn callees(&self, source: &NodeId) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        self.edges_join(&CALLEES_SQL, source)
    }

    /// The unbound call sites naming `name`: `Calls` references the resolver could
    /// not tie to a definition (an overloaded or base service method reached through
    /// a `Model.services.x()` descriptor, a builtin like `save_model_obj`, or any
    /// untyped receiver), joined to the enclosing node they sit in, with the call
    /// line. Dropped from the precise edge set to avoid a false edge; surfaced here by
    /// name so a caller listing can show them as unproven, the recall a text search
    /// gives without inventing an edge. Bounded by `limit`.
    pub fn unresolved_callers_of(&self, name: &str, limit: u32) -> Result<Vec<(Node, u32)>, StoreError> {
        assert!(!name.is_empty(), "name must not be empty");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, u.line
             FROM unresolved_refs u
             JOIN nodes n ON n.id = u.from_node_id
             WHERE u.reference_name = ?1 AND u.reference_kind = 'calls'
             ORDER BY n.project_id, n.file_path, u.line
             LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![name, limit], |row| {
            let raw = node_row(row)?;
            let line: i64 = row.get(20)?;

            Ok((raw, line))
        })?;

        let mut out: Vec<(Node, u32)> = Vec::new();

        for row in rows {
            let (raw, line) = row?;

            if let Some(node) = node_from_row(raw) {
                out.push((node, line.max(1) as u32));
            }
        }

        assert!(out.len() as u32 <= limit, "unresolved caller load respects its limit");

        Ok(out)
    }

    /// The unbound callee names a definition invokes: `Calls` references originating
    /// in `from_id` that the resolver could not tie to a target, each with its call
    /// line. The callee counterpart of [`unresolved_callers_of`]; disjoint from the
    /// resolved callees, since a bound call leaves this table. The Django
    /// QuerySet/Manager builtins (`all`, `filter`, `select_related`, ...) are excluded:
    /// dynamically dispatched with no project definition to bind to, they are incidental
    /// noise in this view, not a dropped project call. Excluded in SQL so `limit` still
    /// yields real callees. Bounded by `limit`.
    pub fn unresolved_callees_of(
        &self,
        from_id: &NodeId,
        limit: u32,
    ) -> Result<Vec<(String, u32)>, StoreError> {
        // The builtin names are compile-time constant identifiers (no quotes or
        // separators), so formatting them into the `NOT IN` list cannot inject.
        let exclusions =
            QUERYSET_BUILTINS.iter().map(|name| format!("'{name}'")).collect::<Vec<_>>().join(", ");

        let sql = format!(
            "SELECT reference_name, line FROM unresolved_refs
             WHERE from_node_id = ?1 AND reference_kind = 'calls'
               AND reference_name NOT IN ({exclusions})
             ORDER BY line LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![from_id.as_str(), limit], |row| {
            let name: String = row.get(0)?;
            let line: i64 = row.get(1)?;

            Ok((name, line))
        })?;

        let mut out: Vec<(String, u32)> = Vec::new();

        for row in rows {
            let (name, line) = row?;

            out.push((name, line.max(1) as u32));
        }

        assert!(out.len() as u32 <= limit, "unresolved callee load respects its limit");

        Ok(out)
    }

    /// The definition nodes in `project` with no incoming edge other than structural
    /// containment: nothing calls, imports, instantiates, tests, relates to, or
    /// extends them. Candidate dead code (an LLM should verify: a symbol reached only
    /// by a framework convention - a management command's `handle`, a signal receiver,
    /// a serialized name - has no static edge and surfaces here too, so the caller
    /// filters those by path/name). Functions, methods, classes, and models only;
    /// ordered by location, bounded by `limit`.
    pub fn orphan_definitions(&self, project: &ProjectId, limit: u32) -> Result<Vec<Node>, StoreError> {
        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes n
             WHERE n.project_id = ?1
               AND n.kind IN ('function', 'method', 'class', 'model')
               AND NOT EXISTS (
                   SELECT 1 FROM edges e WHERE e.target = n.id AND e.kind != 'contains'
               )
             ORDER BY n.file_path, n.start_line
             LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![project.as_str(), limit], node_row)?;

        collect_nodes(rows)
    }

    /// The definition nodes in `project`'s `file_path` whose source span overlaps the
    /// 1-based line range `[start_line, end_line]`: the symbols a diff hunk touched.
    /// Innermost (smallest span) first, so the tightest enclosing definition leads.
    /// Functions, methods, classes, models, and properties, the editable units.
    pub fn nodes_in_range(
        &self,
        project: &ProjectId,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<Node>, StoreError> {
        assert!(start_line >= 1, "start_line is 1-based");
        assert!(start_line <= end_line, "start_line must not exceed end_line");

        let normalized = file_path.replace('\\', "/");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes n
             WHERE n.project_id = ?1 AND n.file_path = ?2
               AND n.start_line <= ?4 AND n.end_line >= ?3
               AND n.kind IN ('function', 'method', 'class', 'model', 'property')
             ORDER BY (n.end_line - n.start_line) ASC",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement
            .query_map(params![project.as_str(), normalized, start_line, end_line], node_row)?;

        collect_nodes(rows)
    }

    /// The target ids this node relates to through a *reverse* relation: the
    /// back-edges the reverse-relation synthesis pass adds (provenance
    /// `synthesis:reverse%`), the models that declare a ForeignKey/M2M *to* this
    /// one, reachable here as Django's reverse accessor. Lets `model` label a
    /// relation's direction (a forward FK/M2M this model declares vs. a reverse
    /// accessor onto it), which `callees` alone cannot tell apart, both being
    /// outgoing `relates_to` edges.
    pub fn reverse_relation_targets(&self, source: &NodeId) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT target FROM edges
             WHERE source = ?1 AND kind = 'relates_to' AND provenance LIKE 'synthesis:reverse%'",
        )?;

        let rows = statement.query_map(params![source.as_str()], |row| row.get::<_, String>(0))?;

        let mut targets: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "reverse-relation load exceeded {ROWS_LOADED_MAX}");

            targets.push(row?);
        }

        Ok(targets)
    }

    /// The [`Store::callers`] result plus the 1-based source line of each
    /// reference edge (the call site), so a caller listing can quote the line of
    /// source where the reference happens. A 0 line means none was recorded.
    pub fn callers_located(
        &self,
        target: &NodeId,
    ) -> Result<Vec<(EdgeKind, Node, u32)>, StoreError> {
        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, e.kind, e.line FROM edges e JOIN nodes n ON e.source = n.id
             WHERE e.target = ?1",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![target.as_str()], |row| {
            Ok((node_row(row)?, row.get::<_, String>(20)?, row.get::<_, Option<u32>>(21)?))
        })?;

        let mut located: Vec<(EdgeKind, Node, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "caller load exceeded {ROWS_LOADED_MAX}");

            let (raw, kind, line) = row?;

            if let (Some(node), Some(edge_kind)) =
                (node_from_row(raw), EdgeKind::from_str_label(&kind))
            {
                located.push((edge_kind, node, line.unwrap_or(0)));
            }
        }

        Ok(located)
    }

    fn edges_join(
        &self,
        sql: &str,
        node_id: &NodeId,
    ) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        let mut statement = self.connection.prepare_cached(sql)?;

        let rows = statement.query_map(params![node_id.as_str()], |row| {
            Ok((node_row(row)?, row.get::<_, String>(20)?))
        })?;

        let mut edges: Vec<(EdgeKind, Node)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            count += 1;

            assert!(count <= ROWS_LOADED_MAX, "edge join exceeded {ROWS_LOADED_MAX}");

            let (raw, kind) = row?;

            if let (Some(node), Some(edge_kind)) = (node_from_row(raw), EdgeKind::from_str_label(&kind)) {
                edges.push((edge_kind, node));
            }
        }

        Ok(edges)
    }
}

/// The write of one file's rows (clearing its prior rows first) on the given
/// connection or transaction. The caller owns the transaction boundary, so this
/// works inside a per-file transaction or a bulk one spanning the whole index.
#[allow(clippy::too_many_arguments)]
fn persist_file_rows(
    connection: &Connection,
    project: &ProjectId,
    file: &FileIndex<'_>,
    nodes: &[Node],
    edges: &[Edge],
    references: &[UnresolvedRef],
    mappings: &[ImportMapping],
    events: &[EventRecord],
    node_count: u32,
    indexed_at: i64,
) -> Result<(), StoreError> {
    clear_file(connection, project, file.path)?;
    write_file_row(connection, project, file, node_count, indexed_at)?;
    insert_nodes(connection, nodes, indexed_at)?;
    insert_edges(connection, edges)?;
    insert_references(connection, project, references)?;
    insert_import_mappings(connection, project, file.path, mappings)?;
    insert_events(connection, project, file.path, events)?;
    insert_file_content(connection, project, file.path, file.source)?;

    Ok(())
}

/// A file's source stored for full-text content search. Skipped when empty so a
/// caller that does not index content writes no row.
fn insert_file_content(
    connection: &Connection,
    project: &ProjectId,
    path: &str,
    source: &str,
) -> Result<(), StoreError> {
    if source.is_empty() {
        return Ok(());
    }

    connection.execute(
        "INSERT INTO file_content (project_id, file_path, content) VALUES (?1, ?2, ?3)",
        params![project.as_str(), path, source],
    )?;

    Ok(())
}

/// The deletion of a file's prior rows so re-indexing is idempotent. Deleting the nodes
/// cascades to their edges and unresolved references via foreign keys.
fn clear_file(connection: &Connection, project: &ProjectId, path: &str) -> Result<(), StoreError> {
    connection.execute(
        "DELETE FROM nodes WHERE project_id = ?1 AND file_path = ?2",
        params![project.as_str(), path],
    )?;

    connection.execute(
        "DELETE FROM files WHERE project_id = ?1 AND path = ?2",
        params![project.as_str(), path],
    )?;

    connection.execute(
        "DELETE FROM import_mappings WHERE project_id = ?1 AND file_path = ?2",
        params![project.as_str(), path],
    )?;

    connection.execute(
        "DELETE FROM events WHERE project_id = ?1 AND file_path = ?2",
        params![project.as_str(), path],
    )?;

    connection.execute(
        "DELETE FROM file_content WHERE project_id = ?1 AND file_path = ?2",
        params![project.as_str(), path],
    )?;

    Ok(())
}

fn insert_events(
    connection: &Connection,
    project: &ProjectId,
    file_path: &str,
    events: &[EventRecord],
) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO events (project_id, file_path, role, event_name, symbol, line, column)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;

    let mut count: u32 = 0;

    for event in events {
        count += 1;

        assert!(count <= ROWS_PER_FILE_MAX, "event insert exceeded {ROWS_PER_FILE_MAX}");

        statement.execute(params![
            project.as_str(),
            file_path,
            event_role_label(event.role),
            event.event,
            event.symbol,
            event.line,
            event.column,
        ])?;
    }

    Ok(())
}

/// The stored label for an event role.
fn event_role_label(role: EventRole) -> &'static str {
    match role {
        EventRole::Dispatch => "dispatch",
        EventRole::Listen => "listen",
    }
}

fn insert_import_mappings(
    connection: &Connection,
    project: &ProjectId,
    file_path: &str,
    mappings: &[ImportMapping],
) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO import_mappings
            (project_id, file_path, local_name, exported_name, source, is_default, is_namespace)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;

    let mut count: u32 = 0;

    for mapping in mappings {
        count += 1;

        assert!(count <= ROWS_PER_FILE_MAX, "import-mapping insert exceeded {ROWS_PER_FILE_MAX}");

        statement.execute(params![
            project.as_str(),
            file_path,
            mapping.local_name,
            mapping.exported_name,
            mapping.source,
            i64::from(mapping.is_default),
            i64::from(mapping.is_namespace),
        ])?;
    }

    Ok(())
}

fn write_file_row(
    connection: &Connection,
    project: &ProjectId,
    file: &FileIndex<'_>,
    node_count: u32,
    indexed_at: i64,
) -> Result<(), StoreError> {
    let size_bytes = i64::try_from(file.size_bytes).unwrap_or(i64::MAX);

    connection.execute(
        "INSERT INTO files
            (path, project_id, content_hash, language, size_bytes, modified_at, indexed_at, node_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            file.path,
            project.as_str(),
            file.content_hash,
            file.language.as_str(),
            size_bytes,
            file.modified_at_ms,
            indexed_at,
            node_count,
        ],
    )?;

    Ok(())
}

fn insert_nodes(connection: &Connection, nodes: &[Node], updated_at: i64) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO nodes
            (id, project_id, kind, name, qualified_name, file_path, language,
             start_line, end_line, start_column, end_column,
             docstring, signature, visibility,
             is_exported, is_async, is_static, is_abstract, decorators, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
         ON CONFLICT(id) DO UPDATE SET
             project_id = excluded.project_id,
             kind = excluded.kind,
             name = excluded.name,
             qualified_name = excluded.qualified_name,
             file_path = excluded.file_path,
             language = excluded.language,
             start_line = excluded.start_line,
             end_line = excluded.end_line,
             start_column = excluded.start_column,
             end_column = excluded.end_column,
             docstring = excluded.docstring,
             signature = excluded.signature,
             visibility = excluded.visibility,
             is_exported = excluded.is_exported,
             is_async = excluded.is_async,
             is_static = excluded.is_static,
             is_abstract = excluded.is_abstract,
             decorators = excluded.decorators,
             updated_at = excluded.updated_at",
    )?;

    let mut count: u32 = 0;

    for node in nodes {
        count += 1;

        assert!(count <= ROWS_PER_FILE_MAX, "node insert exceeded {ROWS_PER_FILE_MAX}");

        let decorators = json_string_array(&node.decorators);

        statement.execute(params![
            node.id.as_str(),
            node.project_id.as_str(),
            node.kind.as_str(),
            node.name,
            node.qualified_name,
            node.file_path,
            node.language.as_str(),
            node.span.start_line,
            node.span.end_line,
            node.span.start_column,
            node.span.end_column,
            node.docstring,
            node.signature,
            node.visibility.map(|visibility| visibility.as_str()),
            i64::from(node.is_exported),
            i64::from(node.is_async),
            i64::from(node.is_static),
            i64::from(node.is_abstract),
            decorators,
            updated_at,
        ])?;
    }

    Ok(())
}

fn insert_edges(connection: &Connection, edges: &[Edge]) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO edges (source, target, kind, line, column, provenance)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;

    let mut count: u32 = 0;

    for edge in edges {
        count += 1;

        assert!(count <= ROWS_PER_FILE_MAX, "edge insert exceeded {ROWS_PER_FILE_MAX}");

        statement.execute(params![
            edge.source.as_str(),
            edge.target.as_str(),
            edge.kind.as_str(),
            edge.line,
            edge.column,
            edge.provenance,
        ])?;
    }

    Ok(())
}

fn insert_references(
    connection: &Connection,
    project: &ProjectId,
    references: &[UnresolvedRef],
) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO unresolved_refs
            (project_id, from_node_id, reference_name, reference_kind, line, column, file_path, language, candidates)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;

    let mut count: u32 = 0;

    for reference in references {
        count += 1;

        assert!(count <= ROWS_PER_FILE_MAX, "reference insert exceeded {ROWS_PER_FILE_MAX}");

        let candidates = json_string_array(&reference.candidates);

        statement.execute(params![
            project.as_str(),
            reference.from_node_id.as_str(),
            reference.reference_name,
            reference.reference_kind.as_str(),
            reference.line,
            reference.column,
            reference.file_path,
            reference.language.as_str(),
            candidates,
        ])?;
    }

    Ok(())
}

/// A string list serialized to a JSON array, fast-pathing the dominant empty case
/// (most symbols carry no decorators and most references no candidates) to the
/// literal `"[]"` without invoking serde or growing a buffer.
fn json_string_array(values: &[String]) -> String {
    if values.is_empty() {
        return "[]".to_string();
    }

    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

fn count(connection: &Connection, sql: &str, project: &ProjectId) -> Result<u32, StoreError> {
    let total: i64 = connection.query_row(sql, params![project.as_str()], |row| row.get(0))?;

    assert!(total >= 0, "row count must be non-negative");

    Ok(u32::try_from(total).unwrap_or(u32::MAX))
}

/// A raw `nodes` row, before its stored strings are parsed back into typed enums.
struct NodeRow {
    id: String,
    project_id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    language: String,
    start_line: i64,
    end_line: i64,
    start_column: i64,
    end_column: i64,
    docstring: Option<String>,
    signature: Option<String>,
    visibility: Option<String>,
    is_exported: i64,
    is_async: i64,
    is_static: i64,
    is_abstract: i64,
    decorators: Option<String>,
    updated_at: i64,
}

fn node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
    node_row_at(row, 0)
}

/// A [`NodeRow`] read from twenty consecutive columns starting at `base`, in the
/// order [`NODE_COLUMNS`] lists. The offset lets one query hydrate two nodes per
/// row (the source block at one base and the target block at another) for the
/// edge joins that return both endpoints.
fn node_row_at(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: row.get(base)?,
        project_id: row.get(base + 1)?,
        kind: row.get(base + 2)?,
        name: row.get(base + 3)?,
        qualified_name: row.get(base + 4)?,
        file_path: row.get(base + 5)?,
        language: row.get(base + 6)?,
        start_line: row.get(base + 7)?,
        end_line: row.get(base + 8)?,
        start_column: row.get(base + 9)?,
        end_column: row.get(base + 10)?,
        docstring: row.get(base + 11)?,
        signature: row.get(base + 12)?,
        visibility: row.get(base + 13)?,
        is_exported: row.get(base + 14)?,
        is_async: row.get(base + 15)?,
        is_static: row.get(base + 16)?,
        is_abstract: row.get(base + 17)?,
        decorators: row.get(base + 18)?,
        updated_at: row.get(base + 19)?,
    })
}

/// A [`Node`] rebuilt from its row, returning `None` for a row whose enums or
/// span no longer parse, keeping a single corrupt row from aborting a load.
fn node_from_row(raw: NodeRow) -> Option<Node> {
    let kind = NodeKind::from_str_label(&raw.kind)?;
    let language = Language::from_str_label(&raw.language)?;

    if raw.start_line < 1 || raw.end_line < raw.start_line {
        return None;
    }

    let span = Span::new(
        line_u32(raw.start_line),
        line_u32(raw.end_line),
        column_u32(raw.start_column),
        column_u32(raw.end_column),
    );

    let identity = NodeIdentity {
        name: raw.name,
        qualified_name: raw.qualified_name,
        file_path: raw.file_path,
        language,
    };

    let mut node = Node::new(
        NodeId::from_raw(raw.id),
        ProjectId::new(raw.project_id),
        kind,
        identity,
        span,
        raw.updated_at.max(0),
    );

    node.docstring = raw.docstring;
    node.signature = raw.signature;
    node.visibility = raw.visibility.as_deref().and_then(Visibility::from_str_label);
    node.is_exported = raw.is_exported != 0;
    node.is_async = raw.is_async != 0;
    node.is_static = raw.is_static != 0;
    node.is_abstract = raw.is_abstract != 0;

    node.decorators = raw
        .decorators
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    Some(node)
}

/// The nodes drained from a mapped-row iterator of [`NodeRow`]s, skipping any that
/// fail to parse and bounding the total.
fn collect_nodes<I>(rows: I) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    collect_nodes_with(rows, 0)
}

/// The [`collect_nodes`] variant that pre-sizes the result to a `LIMIT`-bounded read's
/// row count (capped), so a search that returns up to `limit` rows allocates
/// once instead of regrowing 0→4→8→… as rows arrive.
fn collect_nodes_capacity<I>(rows: I, limit: u32) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    collect_nodes_with(rows, (limit as usize).min(PREALLOC_ROWS_MAX))
}

fn collect_nodes_with<I>(rows: I, capacity: usize) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    let mut nodes: Vec<Node> = Vec::with_capacity(capacity);
    let mut count: u32 = 0;

    for row in rows {
        count += 1;

        assert!(count <= ROWS_LOADED_MAX, "node load exceeded {ROWS_LOADED_MAX}");

        if let Some(node) = node_from_row(row?) {
            nodes.push(node);
        }
    }

    Ok(nodes)
}

/// A raw `unresolved_refs` row, before parsing.
struct RefRow {
    id: i64,
    from_node_id: String,
    reference_name: String,
    reference_kind: String,
    line: i64,
    column: i64,
    file_path: String,
    language: String,
    candidates: Option<String>,
}

fn reference_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        id: row.get(0)?,
        from_node_id: row.get(1)?,
        reference_name: row.get(2)?,
        reference_kind: row.get(3)?,
        line: row.get(4)?,
        column: row.get(5)?,
        file_path: row.get(6)?,
        language: row.get(7)?,
        candidates: row.get(8)?,
    })
}

/// An [`UnresolvedRef`] and its row id rebuilt, returning `None` for a row that
/// no longer satisfies the type's invariants.
fn reference_from_row(raw: RefRow) -> Option<(i64, UnresolvedRef)> {
    let kind = EdgeKind::from_str_label(&raw.reference_kind)?;
    let language = Language::from_str_label(&raw.language)?;

    if raw.reference_name.is_empty() || raw.file_path.is_empty() || raw.line < 1 {
        return None;
    }

    let mut reference = UnresolvedRef::new(
        NodeId::from_raw(raw.from_node_id),
        raw.reference_name,
        kind,
        line_u32(raw.line),
        column_u32(raw.column),
        raw.file_path,
        language,
    );

    reference.candidates = raw
        .candidates
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    Some((raw.id, reference))
}

/// A stored 1-based line as a `u32`, clamped to the valid range.
fn line_u32(value: i64) -> u32 {
    let line = u32::try_from(value.max(1)).unwrap_or(u32::MAX);

    assert!(line >= 1, "a stored line is 1-based");

    line
}

/// A stored 0-based column as a `u32`, clamped to the valid range.
fn column_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

/// A safe FTS5 prefix query: split the free text on non-word characters,
/// append `*` to each token (so `ArticleList` matches `ArticleListView`), and
/// join the tokens with `separator` directly into one `String`, with no
/// intermediate `Vec<String>` or per-token `format!`. Empty when the query has no
/// word characters.
fn fts_query(query: &str, separator: &str) -> String {
    let mut out = String::with_capacity(query.len() + separator.len());
    let mut count: u32 = 0;

    for token in query.split(|character: char| !(character.is_alphanumeric() || character == '_')) {
        count += 1;

        assert!(count <= ROWS_LOADED_MAX, "query token split exceeded {ROWS_LOADED_MAX}");

        if token.is_empty() {
            continue;
        }

        if !out.is_empty() {
            out.push_str(separator);
        }

        out.push_str(token);
        out.push('*');
    }

    out
}

/// The all-terms (AND) prefix match: every token must be present. The precise form
/// `search` and explore's first pass use.
fn fts_prefix_query(query: &str) -> String {
    fts_query(query, " ")
}

/// The any-term (OR) prefix match: one token suffices. Explore's forgiving fallback
/// for multi-word, natural-language queries an AND match would miss entirely.
fn fts_any_query(query: &str) -> String {
    fts_query(query, " OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_under_a_different_schema_is_rebuilt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.db");
        let project = ProjectId::new("blog");

        {
            let store = Store::open(&path).unwrap();

            store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

            assert_eq!(store.all_projects().unwrap().len(), 1, "the project is seeded");
        }

        // Stamp a foreign fingerprint into the file header, as a database built by
        // a different schema would carry.
        {
            let connection = Connection::open(&path).unwrap();

            connection.pragma_update(None, "user_version", 1_i32).unwrap();
        }

        let store = Store::open(&path).unwrap();

        assert!(
            store.all_projects().unwrap().is_empty(),
            "a database under a different schema is discarded and rebuilt empty",
        );
        assert_eq!(
            store.schema_version().unwrap(),
            u32::try_from(SCHEMA_FINGERPRINT).unwrap(),
            "the rebuilt database carries the current fingerprint",
        );
    }
}
