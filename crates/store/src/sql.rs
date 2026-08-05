//! The SQL text the queries are built from.
//!
//! Column lists live here rather than beside each query because the order they
//! declare is a contract with the row mapping in [`crate::mapping`]: changing
//! one without the other silently reads the wrong column.

use std::sync::LazyLock;


/// The flow columns, unqualified, in the order [`flow_row`] expects.
pub(crate) const FLOW_COLUMNS: &str = "id, project_id, name, entry_node_id, entry_kind, depth_max, \
node_count, file_count, app_count, project_count, criticality, truncated";

/// The node columns, qualified with the `n` alias, in the order [`node_row`]
/// expects. Shared by every read that joins nodes against edges or FTS.
pub(crate) const NODE_COLUMNS_PREFIXED: &str =
    "n.id, n.project_id, n.kind, n.name, n.qualified_name, \
n.file_path, n.language, n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, \
n.signature, n.visibility, n.is_exported, n.is_async, n.is_static, n.is_abstract, n.decorators, \
n.updated_at";

/// The node columns, unqualified, in the order [`node_row`] expects. Shared by
/// the scoped single-column lookups that back incremental resolution.
pub(crate) const NODE_COLUMNS: &str =
    "id, project_id, kind, name, qualified_name, file_path, language, \
start_line, end_line, start_column, end_column, docstring, signature, visibility, is_exported, \
is_async, is_static, is_abstract, decorators, updated_at";

/// The hot graph-navigation SQL, built once. These join the full node column set
/// (a ~280-char constant) into the result, so building the string with `format!`
/// on every `callers`/`callees`/`search` call would allocate it afresh each time
/// and force a SQL re-parse. Built once here; the per-call path uses
/// `prepare_cached`, so both the allocation and the parse leave the hot path.
pub(crate) static CALLERS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED}, e.kind FROM edges e JOIN nodes n ON e.source = n.id
         WHERE e.target = ?1",
    )
});

pub(crate) static CALLEES_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED}, e.kind FROM edges e JOIN nodes n ON e.target = n.id
         WHERE e.source = ?1",
    )
});

/// The symbol search, ordered by full-text relevance.
///
/// Ordering here was once absent entirely: the join returned matches in FTS
/// rowid order, so searching a model's own name could truncate that model out of
/// its own result set behind dozens of symbols that merely contain the word.
/// `Inventory` did not rank in the top forty hits for "Inventory" on a real
/// index.
///
/// Relevance is all this query does. Preferring an exact name match is not
/// expressed here, because doing so in SQL costs a sort over every FTS match
/// before the limit applies, which measured eighteen times slower on a
/// 37,000-node graph. [`Store::exact_name_matches`] answers that half through
/// `idx_nodes_lower_name` instead, and [`Store::search_nodes_matching`] merges
/// the two.
pub(crate) static SEARCH_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes_fts JOIN nodes n ON n.id = nodes_fts.id
         WHERE nodes_fts MATCH ?1 ORDER BY nodes_fts.rank LIMIT ?2",
    )
});

/// The unranked variant, for the any-token recall path.
///
/// [`Store::search_nodes_any`] exists to find candidates a strict match would
/// miss, and its only caller re-ranks everything it returns by graph structure,
/// inverse document frequency, and recency. Ordering here would therefore be
/// computed and then discarded: measured, it cost roughly two milliseconds a
/// call and moved `explore`'s mean reciprocal rank slightly the wrong way, by
/// changing which candidates survived the fetch limit into a ranking that had
/// already been tuned against the unordered set.
pub(crate) static SEARCH_ANY_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes_fts JOIN nodes n ON n.id = nodes_fts.id
         WHERE nodes_fts MATCH ?1 LIMIT ?2",
    )
});

/// The index-backed exact-name lookup, served by `idx_nodes_lower_name`.
pub(crate) static EXACT_NAME_SQL: LazyLock<String> = LazyLock::new(|| {
    format!("SELECT {NODE_COLUMNS} FROM nodes WHERE lower(name) = lower(?1) LIMIT ?2")
});

/// The node columns qualified with `alias`, in the order [`node_row`] expects.
/// The runtime equivalent of [`NODE_COLUMNS_PREFIXED`] for a join that aliases
/// the nodes table more than once (source and target endpoints in one row).
pub(crate) fn node_columns(alias: &str) -> String {
    qualify(NODE_COLUMNS, alias)
}

/// The flow columns qualified with `alias`, in the order [`flow_row`] expects.
pub(crate) fn flow_columns(alias: &str) -> String {
    qualify(FLOW_COLUMNS, alias)
}

/// A comma-separated column list with `alias.` prefixed to every column.
///
/// One allocation, sized exactly: the list itself plus `alias.` per column.
/// Columns are counted by splitting on the same `", "` the loop below splits
/// on, so the reservation and the writes cannot disagree; counting commas
/// instead (which they used to) over-reserved by the separator's spaces.
fn qualify(columns: &str, alias: &str) -> String {
    assert!(!alias.is_empty(), "column alias must not be empty");

    let count = columns.split(", ").count();
    let expected = columns.len() + count * (alias.len() + 1);
    let mut out = String::with_capacity(expected);

    for (index, column) in columns.split(", ").enumerate() {
        if index > 0 {
            out.push_str(", ");
        }

        out.push_str(alias);
        out.push('.');
        out.push_str(column);
    }

    assert!(out.len() == expected, "the reservation matched what was written");

    out
}

/// A `?n, ?n+1, ...` placeholder list of `count` positional parameters starting
/// at `first_index`, for an `IN (...)` clause bound through `params_from_iter`.
pub(crate) fn placeholder_list(count: usize, first_index: usize) -> String {
    assert!(count > 0, "a placeholder list is never empty");
    assert!(first_index >= 1, "SQLite parameter indices are 1-based");

    let mut out = String::with_capacity(count * 6);

    for offset in 0..count {
        if offset > 0 {
            out.push_str(", ");
        }

        out.push('?');
        out.push_str(&(first_index + offset).to_string());
    }

    out
}
