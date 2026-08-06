//! The per-file write path.
//!
//! One file is one transaction: its old rows are cleared and its new ones are
//! inserted together, so a re-index of a changed file replaces exactly that
//! file and a crash mid-write leaves the previous version intact.

use constellation_graph::{Edge, Node, ProjectId};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, UnresolvedRef};
use rusqlite::{Connection, params};

use crate::error::{StoreError, charge};
use crate::limits::ROWS_PER_FILE_MAX;
use crate::query::unresolved::requeue_refs_into_file;
use crate::rows::FileIndex;
use crate::store::Store;
use crate::time::now_ms;

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
///
/// That cascade is indiscriminate: it takes the edges *into* this file as well
/// as the ones out of it, and the inbound ones were written by files whose
/// content has not changed and which this run therefore never re-extracts. So
/// the archived references that produced them are requeued first, before the
/// nodes they are found through disappear, and the resolution pass at the end
/// of the run rebuilds each edge. Without it a file's dependents lose their
/// edges into it permanently, one file at a time, every time it is touched.
fn clear_file(connection: &Connection, project: &ProjectId, path: &str) -> Result<(), StoreError> {
    requeue_refs_into_file(connection, project, path)?;

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
        charge(&mut count, ROWS_PER_FILE_MAX, "event insert")?;

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
        charge(&mut count, ROWS_PER_FILE_MAX, "import-mapping insert")?;

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

pub(crate) fn insert_nodes(
    connection: &Connection,
    nodes: &[Node],
    updated_at: i64,
) -> Result<(), StoreError> {
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
        charge(&mut count, ROWS_PER_FILE_MAX, "node insert")?;

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

pub(crate) fn insert_edges(connection: &Connection, edges: &[Edge]) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO edges (source, target, kind, line, column, provenance)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;

    let mut count: u32 = 0;

    for edge in edges {
        charge(&mut count, ROWS_PER_FILE_MAX, "edge insert")?;

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
        charge(&mut count, ROWS_PER_FILE_MAX, "reference insert")?;

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

impl Store {
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
        let indexed_at = now_ms();

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
}
