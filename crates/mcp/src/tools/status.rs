//! `constellation_status`: index health and staleness.

use std::fmt::Write;

use std::path::Path;

use constellation_graph::{ProjectId, now_unix_millis};
use constellation_store::{Store, StoreError};

/// The index's health, rendered: every indexed project with its node, commit,
/// symbol-revision, and flow counts, plus the constellation-wide edge and
/// cross-project link totals. The answer to "is the graph built, and how stale".
#[doc(hidden)]
pub fn status_text(store: &Store) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let edges = store.count_edges()?;
    let links = store.count_links()?;

    let mut node_total: u32 = 0;
    let mut history_total: u32 = 0;
    let mut symbol_total: u32 = 0;
    let mut flow_total: u32 = 0;
    let mut lines = String::new();

    for row in &projects {
        let nodes = store.count_nodes(&row.id)?;
        node_total = node_total.saturating_add(nodes);
        history_total = history_total.saturating_add(store.count_history_commits(&row.id)?);
        symbol_total = symbol_total.saturating_add(store.count_symbol_revisions(&row.id)?);
        flow_total = flow_total.saturating_add(store.count_flows(&row.id)?);

        let _ = writeln!(lines,
            "  - {} ({}): {nodes} nodes, indexed {}{}",
            row.id,
            row.name,
            indexed_age(row.indexed_at),
            stale_hint(store, &row.id, Path::new(&row.root_path)),
        );
    }

    Ok(format!(
        "projects: {}\nnodes: {node_total}\nedges: {edges}\ncross-project links: {links}\n\
         history commits: {history_total}\nsymbol revisions: {symbol_total}\n\
         execution flows: {flow_total}{}\n{lines}",
        projects.len(),
        if flow_total == 0 { " (run `constellation flows`)" } else { "" },
    ))
}

/// A working-tree staleness suffix for a project's status line (how many
/// files changed or were removed on disk since the last index), or an empty string
/// when the index is current, the root is gone, or the count is unavailable. With
/// the in-session watcher running this is normally empty; a non-empty hint flags
/// the brief window before a re-index, or a watcher that never started.
fn stale_hint(store: &Store, project: &ProjectId, root: &Path) -> String {
    if !root.is_dir() {
        return String::new();
    }

    match constellation_index::count_stale_files(store, project, root) {
        Ok(stale) if stale.changed > 0 || stale.removed > 0 => {
            format!(" ({} changed, {} removed on disk since)", stale.changed, stale.removed)
        }
        _ => String::new(),
    }
}

/// A human-readable "time since last index" for the staleness hint.
fn indexed_age(indexed_at_ms: i64) -> String {
    let seconds = (now_unix_millis() - indexed_at_ms).max(0) / 1000;

    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}
