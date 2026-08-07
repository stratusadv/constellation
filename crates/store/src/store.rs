//! The store handle: opening a database, and the small set of operations that
//! are about the store itself rather than about the graph inside it.
//!
//! The schema is rebuilt rather than migrated. A database written by an older
//! or newer version is discarded and recreated, because the index is derived
//! data and re-deriving it is cheaper than being wrong about it. What survives
//! the discard is each project's identity (its id, name, and root path), so the
//! rebuilt database still knows which repositories it indexed and the next
//! refresh re-derives every graph from disk instead of coming up empty.
//!
//! The query families are `impl Store` blocks in the modules beside this one.

use std::path::Path;

use constellation_graph::ProjectId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{StoreError, charge};
use crate::limits::ROWS_LOADED_MAX;
use crate::mapping::count;
use crate::rows::ProjectRow;
use crate::time::now_ms;

/// The full schema and single source of truth.
///
/// Every statement in it is `CREATE TABLE IF NOT EXISTS` or `CREATE INDEX IF
/// NOT EXISTS`, which is load-bearing rather than incidental: it is what lets
/// the schema be re-applied to an older database to add whatever is missing
/// without touching what is already there. [`SCHEMA_VERSION_MIN`] documents the
/// one kind of change that breaks that property.
const SCHEMA: &str = include_str!("schema.sql");

/// The schema version this build writes into `PRAGMA user_version`.
///
/// Bump this by hand for a schema change an older build cannot read past. It
/// replaces a compile-time hash of the whole schema file, which could not tell
/// an added table from a changed column and so treated both as a reason to
/// delete the database: every schema edit, however additive, silently destroyed
/// every existing index on the next open, including on a read-only path. Adding
/// the flow tables did exactly that, and they are not populated unless
/// `constellation flows` is run, so the cost was paid for tables that were
/// empty.
///
/// Adding a table or an index is not such a change and must not bump this.
/// Every statement in the schema is `CREATE ... IF NOT EXISTS` and the schema is
/// re-applied on every open, so a database written by an older build grows the
/// new table the first time a new build touches it, and a table an older build
/// never queries is invisible to it. Bumping instead makes that older build read
/// a version above its own and *discard the database*, which is the right
/// response to a shape it cannot understand and pure destruction for one it can:
/// a still-installed binary, or a `serve` watcher running from before the
/// rebuild, deletes and re-derives the whole index on every open, in a loop, for
/// a table it does not use.
const SCHEMA_VERSION: i32 = 3;

/// The oldest schema version this build can open without rebuilding.
///
/// Raise this **only** for a change SQLite cannot express against an existing
/// database: a dropped or renamed column, a changed type or constraint, or a
/// table whose rows now mean something different. Adding a table, an index, or
/// a nullable column with a default is not such a change, and must not raise
/// it. Getting this wrong in the safe direction costs a re-index; getting it
/// wrong in the unsafe direction serves wrong answers from a stale table.
const SCHEMA_VERSION_MIN: i32 = 2;

/// The preparation opening an existing database at `path` requires before use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaAction {
    /// The database predates [`SCHEMA_VERSION_MIN`], or carries a version this
    /// build does not recognise (every database written before versioning, which
    /// stamped a schema hash into the same pragma). Delete and rebuild.
    Discard,
    /// The database is readable as it stands; re-applying the schema adds
    /// anything introduced since, and keeps every row already there.
    Upgrade,
}

/// The preparation an already-open `connection` requires before its database is used.
/// A database that does not exist yet needs nothing: opening created it, and its
/// `user_version` reads as zero until the schema is applied.
fn schema_action(connection: &Connection, fresh: bool) -> Result<SchemaAction, StoreError> {
    if fresh {
        return Ok(SchemaAction::Upgrade);
    }

    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;

    // A version above this build's is a database written by a newer
    // constellation. Rebuilding is the honest response: this binary cannot know
    // what changed, and reading it as though nothing had would be a guess.
    if version < i64::from(SCHEMA_VERSION_MIN) || version > i64::from(SCHEMA_VERSION) {
        return Ok(SchemaAction::Discard);
    }

    Ok(SchemaAction::Upgrade)
}

/// The page cache one connection may hold, as the negative kibibyte count
/// SQLite reads as a size rather than a page count.
///
/// SQLite's default is two megabytes, which for a graph query that joins nodes
/// against edges means re-reading the same index pages from the OS on every
/// call. A pool multiplies this by its connection count, so it buys latency at
/// a bounded, stated memory cost rather than an open-ended one.
const CACHE_KIBIBYTES: i32 = -16 * 1024;

/// The window of the database file a connection may memory-map. Reads inside it
/// skip a copy into the page cache entirely; past it they fall back to ordinary
/// reads, so an index larger than this still works, just without the mapping.
const MMAP_BYTES: i64 = 256 * 1024 * 1024;

/// The time a statement waits for a lock before failing with "database is
/// locked".
///
/// SQLite's default is zero: the first contended statement fails immediately.
/// WAL keeps readers and a writer out of each other's way, but not two writers,
/// and this database has two by design (the CLI, and a `serve` watcher
/// re-indexing underneath it). At zero, a re-index that collided with any other
/// write lost that burst outright, which for a path-scoped watcher means losing
/// exactly the changes that burst named. Waiting is the correct response to a
/// lock that is about to be released.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// The pragmas that make a connection fast to read through and patient under
/// contention, applied to every connection whether it will write or not.
fn tune_for_reads(connection: &Connection) -> Result<(), StoreError> {
    connection.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
    connection.pragma_update(None, "cache_size", CACHE_KIBIBYTES)?;
    connection.pragma_update(None, "mmap_size", MMAP_BYTES)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;

    Ok(())
}

/// The most project rows salvaged from a database about to be discarded, a
/// fail-fast bound on a table that should hold a handful of rows.
const SALVAGE_PROJECTS_MAX: usize = 4_096;

/// The identity a project keeps across a schema rebuild: enough to re-seed its
/// row so the next refresh re-derives its graph from disk.
struct ProjectSeed {
    id: String,
    name: String,
    root_path: String,
    reference_only: bool,
}

/// The project identities read out of a database about to be discarded, so the
/// rebuilt database remembers which repositories it indexed. Without them,
/// `sync` and the serve watcher iterate an empty project table and re-derive
/// nothing: the discard would demand a manual `init` (and `link`) to recover.
///
/// Best-effort by design. The schema is by definition one this build does not
/// fully understand, so any failure (a missing table, a renamed column) yields
/// an empty list and the discard proceeds as a plain rebuild. Only identity is
/// read, never graph content: rows that cannot be trusted are the reason the
/// database is being discarded at all.
fn salvage_projects(connection: &Connection) -> Vec<ProjectSeed> {
    const WITH_FLAG: &str = "SELECT id, name, root_path, reference_only FROM projects";
    const BARE: &str = "SELECT id, name, root_path, 0 FROM projects";

    let mut statement = match connection.prepare(WITH_FLAG).or_else(|_| connection.prepare(BARE)) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };

    let rows = statement.query_map([], |row| {
        Ok(ProjectSeed {
            id: row.get(0)?,
            name: row.get(1)?,
            root_path: row.get(2)?,
            reference_only: row.get::<_, i64>(3)? != 0,
        })
    });

    let Ok(rows) = rows else {
        return Vec::new();
    };

    let mut seeds: Vec<ProjectSeed> = Vec::new();

    for row in rows.take(SALVAGE_PROJECTS_MAX) {
        let Ok(seed) = row else {
            continue;
        };

        if seed.id.is_empty() || seed.id.contains("::") {
            continue;
        }

        if seed.name.is_empty() || seed.root_path.is_empty() {
            continue;
        }

        seeds.push(seed);
    }

    assert!(seeds.len() <= SALVAGE_PROJECTS_MAX, "salvage honors its bound");

    seeds
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

/// A handle to the constellation database: every project's graph and the
/// cross-project edges between them, in one SQLite file.
pub struct Store {
    pub(crate) connection: Connection,
}

impl Store {
    /// The store at `path`, created if absent.
    ///
    /// A database written under a schema this build can still read is upgraded
    /// in place: re-applying the schema creates whatever tables and indexes have
    /// been added since and leaves every existing row untouched. Only a database
    /// older than [`SCHEMA_VERSION_MIN`], or newer than this build, is discarded
    /// and rebuilt. Opening is a read path for most callers, so it must not
    /// destroy an index that merely predates a new table.
    ///
    /// A discard keeps each project's identity (see [`salvage_projects`]) and
    /// nothing else, so the rebuilt database starts empty but self-heals: the
    /// next `sync` or serve refresh walks every remembered root and re-derives
    /// the whole graph, no manual `init` required.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        assert!(!path.as_os_str().is_empty(), "store path must not be empty");

        // One connection answers the version question and then does the work.
        // Opening a second just to read `user_version` and dropping it cost an
        // open, a WAL attach, and a close on the startup path of every command.
        let fresh = !path.exists();
        let connection = Connection::open(path)?;

        if schema_action(&connection, fresh)? == SchemaAction::Discard {
            let seeds = salvage_projects(&connection);

            drop(connection);
            discard_database(path);

            let store = Self::init(Connection::open(path)?)?;

            store.reseed_projects(&seeds)?;

            return Ok(store);
        }

        Self::init(connection)
    }

    /// The salvaged project identities re-recorded after a rebuild, so the next
    /// refresh walks every previously indexed root. Graph content is deliberately
    /// not carried over: the empty `index_version` and empty file table make the
    /// next index of each project a full re-extraction.
    fn reseed_projects(&self, seeds: &[ProjectSeed]) -> Result<(), StoreError> {
        assert!(seeds.len() <= SALVAGE_PROJECTS_MAX, "seeds stay within the salvage bound");

        for seed in seeds {
            let id = ProjectId::new(seed.id.clone());

            self.upsert_project(&id, &seed.name, &seed.root_path)?;

            if seed.reference_only {
                self.set_reference_only(&id, true)?;
            }
        }

        Ok(())
    }

    /// An additional read-only connection to an already-initialized database.
    ///
    /// Applies no schema and takes no version decision: [`Store::open`] has
    /// already settled both, and a reader that re-applied the schema would be
    /// writing to a database it is about to declare read-only. Used by
    /// [`crate::StorePool`] to open its connections past the first.
    pub fn open_reader(path: &Path) -> Result<Self, StoreError> {
        assert!(!path.as_os_str().is_empty(), "store path must not be empty");

        let connection = Connection::open(path)?;

        tune_for_reads(&connection)?;

        let store = Self { connection };

        store.set_query_only()?;

        Ok(store)
    }

    /// An ephemeral in-memory database, fully initialized. Intended for tests and
    /// smoke checks.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;

        Self::init(connection)
    }

    /// The connection barred from writing, so a read path that reaches for a
    /// write fails loudly at the statement rather than quietly succeeding.
    /// One-way for the life of the connection, which is what makes it a
    /// guarantee rather than a setting.
    pub fn set_query_only(&self) -> Result<(), StoreError> {
        self.connection.pragma_update(None, "query_only", "ON")?;

        Ok(())
    }

    fn init(connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "recursive_triggers", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;

        tune_for_reads(&connection)?;

        connection.execute_batch(SCHEMA)?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        Ok(Self { connection })
    }

    /// The schema version stamped into this database, identifying the schema it
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
            params![id.as_str(), name, root_path, now_ms()],
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

    /// A project removed outright: its row, everything keyed by it (files, nodes,
    /// edges, history, flows, all cascading through foreign keys with the FTS
    /// triggers firing), and its metadata keys. For dropping a companion or
    /// version copy the workspace config no longer names.
    pub fn delete_project(&self, project: &ProjectId) -> Result<(), StoreError> {
        let removed = self
            .connection
            .execute("DELETE FROM projects WHERE id = ?1", params![project.as_str()])?;

        assert!(removed <= 1, "project ids are unique");

        let key = format!("git_ingest:{}", project.as_str());

        self.connection
            .execute("DELETE FROM project_metadata WHERE key = ?1", params![key])?;

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
            params![key, stamp, now_ms()],
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
            .prepare_cached("SELECT id FROM projects WHERE reference_only != 0")?;

        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;

        let mut ids: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "reference-only load")?;

            ids.push(row?);
        }

        Ok(ids)
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

    /// The id, name, root path, and last-indexed timestamp of every project.
    pub fn all_projects(&self) -> Result<Vec<ProjectRow>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, name, root_path, indexed_at, reference_only FROM projects",
        )?;

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
            charge(&mut count, ROWS_LOADED_MAX, "project load")?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_below_the_readable_schema_version_is_rebuilt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.db");
        let project = ProjectId::new("blog");

        {
            let store = Store::open(&path).unwrap();

            store.upsert_project(&project, "blog", "/tmp/blog").unwrap();
            store.set_index_version(&project, "stamp-of-the-old-binary").unwrap();

            assert_eq!(store.all_projects().unwrap().len(), 1, "the project is seeded");
        }

        // Stamp a version below SCHEMA_VERSION_MIN, as a database built under a
        // schema this build can no longer read would carry.
        {
            let connection = Connection::open(&path).unwrap();

            connection.pragma_update(None, "user_version", SCHEMA_VERSION_MIN - 1).unwrap();
        }

        let store = Store::open(&path).unwrap();

        assert_eq!(
            store.schema_version().unwrap(),
            u32::try_from(SCHEMA_VERSION).unwrap(),
            "the rebuilt database carries the current schema version",
        );
        assert_eq!(store.count_nodes(&project).unwrap(), 0, "the graph itself is rebuilt empty");

        let projects = store.all_projects().unwrap();

        assert_eq!(projects.len(), 1, "the project's identity survives the discard");
        assert_eq!(projects[0].root_path, "/tmp/blog", "with the root the next refresh walks");
        assert_eq!(
            store.index_version(&project).unwrap(),
            "",
            "and no extractor stamp, so that refresh re-extracts every file",
        );
    }

    #[test]
    fn a_reference_only_project_stays_reference_only_across_a_rebuild() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.db");
        let version_copy = ProjectId::new("django-spire@next");

        {
            let store = Store::open(&path).unwrap();

            store.upsert_project(&version_copy, "django-spire@next", "/tmp/spire-next").unwrap();
            store.set_reference_only(&version_copy, true).unwrap();
        }

        {
            let connection = Connection::open(&path).unwrap();

            connection.pragma_update(None, "user_version", SCHEMA_VERSION_MIN - 1).unwrap();
        }

        let store = Store::open(&path).unwrap();

        assert_eq!(
            store.reference_only_project_ids().unwrap(),
            vec![version_copy.as_str().to_string()],
            "a version copy is still excluded from link targets after the rebuild",
        );
    }
}
