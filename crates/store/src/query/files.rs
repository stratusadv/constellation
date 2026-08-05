//! Files, and the per-file facts the indexer needs to decide what to
//! re-extract: hashes, modification times, import mappings, and events.

use constellation_graph::ProjectId;
use constellation_resolution::{EventRecord, EventRole, ImportMapping};
use rusqlite::params;
use rustc_hash::FxHashMap;

use crate::error::{StoreError, charge};
use crate::limits::ROWS_LOADED_MAX;
use crate::rows::FileRow;
use crate::store::Store;

impl Store {
    /// The recorded file paths mapped to their stored content hashes, for a project.
    /// Used to skip re-extracting unchanged files on re-index.
    pub fn file_hashes(&self, project: &ProjectId) -> Result<FxHashMap<String, String>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT path, content_hash FROM files WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut hashes: FxHashMap<String, String> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "file hash load")?;

            let (path, hash) = row?;
            hashes.insert(path, hash);
        }

        Ok(hashes)
    }

    /// The recorded file paths mapped to their stored modification times (epoch ms),
    /// for a project. The staleness baseline `status` compares the working tree
    /// against, without reading file contents.
    pub fn file_mtimes(&self, project: &ProjectId) -> Result<FxHashMap<String, i64>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT path, modified_at FROM files WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut mtimes: FxHashMap<String, i64> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "file mtime load")?;

            let (path, mtime) = row?;
            mtimes.insert(path, mtime);
        }

        Ok(mtimes)
    }

    /// The import mappings for a project, each paired with the file it was
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
            charge(&mut count, ROWS_LOADED_MAX, "import-mapping load")?;

            mappings.push(row?);
        }

        Ok(mappings)
    }

    /// The indexed files in a project with their language, symbol count, and
    /// size, ordered by path. Backs the `files` listing tool.
    ///
    /// The symbol count is taken from the `nodes` table rather than the
    /// denormalized `files.node_count`, which records how many nodes extraction
    /// *emitted* for the file. Same-id nodes collapse on insert, so that column
    /// runs ahead of what was stored, and per-package sums built from it could
    /// exceed the project total the same response prints from `count_nodes`.
    /// Both numbers now count the same rows.
    pub fn files_for(&self, project: &ProjectId) -> Result<Vec<FileRow>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT f.path, f.language, COALESCE(n.stored, 0), f.size_bytes
             FROM files f
             LEFT JOIN (
                 SELECT file_path, COUNT(*) AS stored FROM nodes
                 WHERE project_id = ?1 GROUP BY file_path
             ) n ON n.file_path = f.path
             WHERE f.project_id = ?1 ORDER BY f.path",
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
            charge(&mut count, ROWS_LOADED_MAX, "file load")?;

            files.push(row?);
        }

        Ok(files)
    }

    /// The distinct file paths that hold at least one node in a project.
    pub fn project_file_paths(&self, project: &ProjectId) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT DISTINCT file_path FROM nodes WHERE project_id = ?1")?;

        let rows = statement.query_map(params![project.as_str()], |row| row.get::<_, String>(0))?;

        let mut paths: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "file-path load")?;

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
            charge(&mut count, ROWS_LOADED_MAX, "scoped import-mapping load")?;

            mappings.push(row?);
        }

        Ok(mappings)
    }

    /// The event-channel observations for a project, for correlating
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
            charge(&mut count, ROWS_LOADED_MAX, "event load")?;

            events.push(row?);
        }

        Ok(events)
    }
}
