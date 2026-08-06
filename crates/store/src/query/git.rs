//! The graph over time: commit history, per-file churn, per-symbol
//! revisions, and point-in-time lookups.

use constellation_graph::ProjectId;
use rusqlite::{Connection, params};
use rustc_hash::FxHashMap;

use crate::error::{StoreError, charge};
use crate::limits::{
    CHANGED_SINCE_ROWS_MAX, COMMIT_FILES_ROWS_MAX, FILE_CHURN_ROWS_MAX, ROWS_LOADED_MAX,
};
use crate::mapping::count;
use crate::rows::{
    AsOfSymbol, CommitRecord, FileTouch, HistoryHit, SymbolHistoryHit, SymbolRevision,
};
use crate::store::Store;

/// The commit rows and their per-file churn written on the given connection or
/// transaction, one prepared statement reused across every row. The caller owns
/// the transaction boundary, so the delete that precedes a wholesale replace and
/// these inserts commit together. Returns the number of commits written.
fn insert_commits(
    connection: &Connection,
    project: &ProjectId,
    commits: &[CommitRecord],
) -> Result<u32, StoreError> {
    let mut commit_statement = connection.prepare_cached(
        "INSERT OR REPLACE INTO git_commit
             (project_id, commit_hash, author, committed_at, summary)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;

    let mut file_statement = connection.prepare_cached(
        "INSERT INTO git_commit_file
             (project_id, commit_hash, file_path, insertions, deletions)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;

    let mut stored: u32 = 0;

    for commit in commits {
        assert!(stored < u32::MAX, "commit count must not overflow");
        assert!(!commit.commit_hash.is_empty(), "a commit hash must not be empty");

        commit_statement.execute(params![
            project.as_str(),
            commit.commit_hash,
            commit.author,
            commit.committed_at,
            commit.summary,
        ])?;

        let mut written: u32 = 0;

        for file in &commit.files {
            charge(&mut written, COMMIT_FILES_ROWS_MAX, "commit-file insert")?;

            file_statement.execute(params![
                project.as_str(),
                commit.commit_hash,
                file.file_path,
                file.insertions,
                file.deletions,
            ])?;
        }

        stored += 1;
    }

    Ok(stored)
}

/// The symbol-change rows written on the given connection or transaction, one
/// prepared statement reused across every row. Returns the number written.
fn insert_symbol_revisions(
    connection: &Connection,
    project: &ProjectId,
    revisions: &[SymbolRevision],
) -> Result<u32, StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO git_symbol_revision
             (project_id, commit_hash, file_path, qualified_name, name, kind, change_kind, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;

    let mut stored: u32 = 0;

    for revision in revisions {
        assert!(stored < u32::MAX, "revision count must not overflow");
        assert!(!revision.commit_hash.is_empty(), "a commit hash must not be empty");

        statement.execute(params![
            project.as_str(),
            revision.commit_hash,
            revision.file_path,
            revision.qualified_name,
            revision.name,
            revision.kind,
            revision.change.as_str(),
            revision.signature,
        ])?;

        stored += 1;
    }

    Ok(stored)
}

impl Store {
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

        let stored = insert_commits(&transaction, project, commits)?;

        transaction.commit()?;

        assert!(stored as usize <= commits.len(), "no commit is stored twice");

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

        let mut statement = self.connection.prepare_cached(
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
            charge(&mut count, ROWS_LOADED_MAX, "history load")?;

            hits.push(row?);
        }

        Ok(hits)
    }

    /// The distinct commits that touched each of `project`'s files at or after
    /// `since_unix` (epoch seconds), keyed by file path. The churn input to the
    /// review-risk score, read straight from the indexed history so no
    /// `git log --numstat` subprocess runs per query. Empty until
    /// [`Store::replace_history`] has run for the project.
    pub fn file_commit_counts(
        &self,
        project: &ProjectId,
        since_unix: i64,
    ) -> Result<FxHashMap<String, u32>, StoreError> {
        assert!(!project.as_str().is_empty(), "project id must not be empty");

        let mut statement = self.connection.prepare_cached(
            "SELECT f.file_path, COUNT(DISTINCT f.commit_hash)
             FROM git_commit_file f
             JOIN git_commit c
                 ON c.project_id = f.project_id AND c.commit_hash = f.commit_hash
             WHERE f.project_id = ?1 AND c.committed_at >= ?2
             GROUP BY f.file_path
             LIMIT ?3",
        )?;

        let rows = statement.query_map(
            params![project.as_str(), since_unix, FILE_CHURN_ROWS_MAX],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;

        let mut counts: FxHashMap<String, u32> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, FILE_CHURN_ROWS_MAX, "churn load")?;

            let (path, commits) = row?;

            counts.insert(path, u32::try_from(commits).unwrap_or(u32::MAX));
        }

        Ok(counts)
    }

    /// The file paths one commit touched, in the order git reported them. The
    /// co-change ground truth an impact-accuracy benchmark grades against, read
    /// from the indexed history rather than from a `git show` subprocess.
    pub fn files_touched_by(
        &self,
        project: &ProjectId,
        commit_hash: &str,
    ) -> Result<Vec<String>, StoreError> {
        assert!(!commit_hash.is_empty(), "a commit hash must not be empty");

        let mut statement = self.connection.prepare_cached(
            "SELECT file_path FROM git_commit_file
             WHERE project_id = ?1 AND commit_hash = ?2
             ORDER BY file_path
             LIMIT ?3",
        )?;

        let rows = statement.query_map(
            params![project.as_str(), commit_hash, COMMIT_FILES_ROWS_MAX],
            |row| row.get::<_, String>(0),
        )?;

        let mut files: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, COMMIT_FILES_ROWS_MAX, "commit-file load")?;

            files.push(row?);
        }

        Ok(files)
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

        let stored = insert_symbol_revisions(&transaction, project, revisions)?;

        transaction.commit()?;

        assert!(stored as usize <= revisions.len(), "no revision is stored twice");

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

    /// The commits that touched a file in `project`, ordered by file then commit
    /// time, so a caller can diff each file's revisions in chronological order.
    /// Capped at `max` touches.
    pub fn history_file_touches(
        &self,
        project: &ProjectId,
        max: u32,
    ) -> Result<Vec<FileTouch>, StoreError> {
        assert!(max > 0, "touch cap must be positive");

        let mut statement = self.connection.prepare_cached(
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
            charge(&mut count, max, "touch load")?;

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

        let mut statement = self.connection.prepare_cached(
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
            charge(&mut count, ROWS_LOADED_MAX, "symbol history load")?;

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

        let mut statement = self.connection.prepare_cached(
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
            charge(&mut count, limit.min(ROWS_LOADED_MAX), "as-of load")?;

            symbols.push(row?);
        }

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

    /// The qualified names of every symbol in `project` recorded as added,
    /// modified, or removed at or after `since_unix`. Backs a "changed since"
    /// filter without a per-candidate query. Empty until
    /// `constellation history --symbols` has run.
    pub fn qualified_names_changed_since(
        &self,
        project: Option<&ProjectId>,
        since_unix: i64,
    ) -> Result<Vec<String>, StoreError> {
        let project_filter = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare_cached(
            "SELECT DISTINCT r.qualified_name
             FROM git_symbol_revision r
             JOIN git_commit c ON c.project_id = r.project_id AND c.commit_hash = r.commit_hash
             WHERE (?1 IS NULL OR r.project_id = ?1) AND c.committed_at >= ?2
             LIMIT ?3",
        )?;

        let rows = statement.query_map(
            params![project_filter, since_unix, CHANGED_SINCE_ROWS_MAX],
            |row| row.get::<_, String>(0),
        )?;

        let mut names: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, CHANGED_SINCE_ROWS_MAX, "changed-since load")?;

            names.push(row?);
        }

        Ok(names)
    }

    /// The most recent commit time (epoch seconds) touching each of `project`'s
    /// files at or after `since_unix`, keyed by file path. The commit-recency
    /// input to ranking, read from indexed history with no subprocess.
    pub fn file_latest_commits(
        &self,
        project: &ProjectId,
        since_unix: i64,
    ) -> Result<FxHashMap<String, i64>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT f.file_path, MAX(c.committed_at)
             FROM git_commit_file f
             JOIN git_commit c
                 ON c.project_id = f.project_id AND c.commit_hash = f.commit_hash
             WHERE f.project_id = ?1 AND c.committed_at >= ?2
             GROUP BY f.file_path
             LIMIT ?3",
        )?;

        let rows = statement.query_map(
            params![project.as_str(), since_unix, FILE_CHURN_ROWS_MAX],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;

        let mut latest: FxHashMap<String, i64> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, FILE_CHURN_ROWS_MAX, "recency load")?;

            let (path, committed_at) = row?;

            latest.insert(path, committed_at);
        }

        Ok(latest)
    }
}
