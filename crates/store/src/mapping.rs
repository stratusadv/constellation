//! Turning a `rusqlite::Row` into a graph value.
//!
//! Every query funnels through here rather than destructuring rows inline,
//! so the column order declared in [`crate::sql`] is honored in exactly one
//! place, and a row that cannot be mapped (an unknown kind, a malformed
//! span) is dropped in exactly one place too.

use constellation_graph::{
    Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span, Visibility,
};
use rusqlite::{Connection, params};

use crate::error::{StoreError, charge};
use crate::limits::{PREALLOC_ROWS_MAX, ROWS_LOADED_MAX};

pub(crate) fn count(
    connection: &Connection,
    sql: &str,
    project: &ProjectId,
) -> Result<u32, StoreError> {
    let total: i64 = connection.query_row(sql, params![project.as_str()], |row| row.get(0))?;

    assert!(total >= 0, "row count must be non-negative");

    Ok(u32::try_from(total).unwrap_or(u32::MAX))
}

/// A raw `nodes` row, before its stored strings are parsed back into typed enums.
pub(crate) struct NodeRow {
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

pub(crate) fn node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
    node_row_at(row, 0)
}

/// A [`NodeRow`] read from twenty consecutive columns starting at `base`, in the
/// order [`NODE_COLUMNS`] lists. The offset lets one query hydrate two nodes per
/// row (the source block at one base and the target block at another) for the
/// edge joins that return both endpoints.
pub(crate) fn node_row_at(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<NodeRow> {
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
pub(crate) fn node_from_row(raw: NodeRow) -> Option<Node> {
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
pub(crate) fn collect_nodes<I>(rows: I) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    collect_nodes_with(rows, 0)
}

/// The [`collect_nodes`] variant that pre-sizes the result to a `LIMIT`-bounded read's
/// row count (capped), so a search that returns up to `limit` rows allocates
/// once instead of regrowing 0→4→8→… as rows arrive.
pub(crate) fn collect_nodes_capacity<I>(rows: I, limit: u32) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    collect_nodes_with(rows, (limit as usize).min(PREALLOC_ROWS_MAX))
}

pub(crate) fn collect_nodes_with<I>(rows: I, capacity: usize) -> Result<Vec<Node>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<NodeRow>>,
{
    let mut nodes: Vec<Node> = Vec::with_capacity(capacity);
    let mut count: u32 = 0;

    for row in rows {
        charge(&mut count, ROWS_LOADED_MAX, "node load")?;

        if let Some(node) = node_from_row(row?) {
            nodes.push(node);
        }
    }

    Ok(nodes)
}

/// A stored 1-based line as a `u32`, clamped to the valid range.
pub(crate) fn line_u32(value: i64) -> u32 {
    let line = u32::try_from(value.max(1)).unwrap_or(u32::MAX);

    assert!(line >= 1, "a stored line is 1-based");

    line
}

/// A stored 0-based column as a `u32`, clamped to the valid range.
pub(crate) fn column_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}
