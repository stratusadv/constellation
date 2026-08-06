//! `flows`: Django execution flows traced across the resolved graph and stored.

use std::path::PathBuf;

use anyhow::{Result, bail};
use constellation_store::Store;

use crate::args::{flag_value, positional};
use crate::progress;
use crate::summary::digits;
use crate::workspace::discover_database;

/// The `constellation flows [database] [--project <id>] [--depth N]
/// [--include-tests]` command traces every Django execution flow in each indexed
/// project and stores it, so `constellation_flows` and
/// `constellation_affected_flows` answer from the graph instead of an empty
/// table. Rerun after a structural change; the watcher does not maintain flows
/// by default, since tracing walks the whole adjacency.
pub(crate) fn flows_command(rest: &[String]) -> Result<()> {
    let include_tests = rest.iter().any(|argument| argument == "--include-tests");
    let project_filter = flag_value(rest, "--project");

    let depth = match flag_value(rest, "--depth") {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("--depth expects a positive integer, got {value:?}"))?,
        None => constellation_index::FLOW_DEPTH_MAX,
    };

    if depth == 0 {
        bail!("--depth expects a positive integer");
    }

    let database = match positional(rest) {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` first",
            database.display(),
        );
    }

    let store = Store::open(&database)?;
    let options = constellation_index::FlowOptions { depth_max: depth, include_tests };

    let mut rows: Vec<FlowSummaryRow> = Vec::new();

    for project in store.all_projects()? {
        if let Some(filter) = &project_filter
            && project.id.as_str() != filter
            && &project.name != filter
        {
            continue;
        }

        let mut progress = progress::Progress::new(&format!("flows {}", project.id));
        let stats = constellation_index::compute_flows(&store, &project.id, options)?;

        progress.finish();

        rows.push(FlowSummaryRow { label: project.id.as_str().to_string(), stats });
    }

    if rows.is_empty() {
        bail!("no indexed project matches; run `constellation init` first");
    }

    print_flow_summary(&rows);

    Ok(())
}

/// A project's flow-trace result for the summary.
struct FlowSummaryRow {
    label: String,
    stats: constellation_index::FlowStats,
}

/// The flow-trace summary, aligned like the index summary: one row per project
/// with its stored flow count, plus the entry points that reached nothing, the
/// reach sets that were cut short, and the flows dropped past the total cap.
fn print_flow_summary(rows: &[FlowSummaryRow]) {
    let name_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let flows_width = rows.iter().map(|row| digits(row.stats.stored)).max().unwrap_or(1);

    println!("execution flows");

    for row in rows {
        let label = &row.label;
        let stored = row.stats.stored;
        let empty = row.stats.entries.saturating_sub(stored);

        let mut notes: Vec<String> = Vec::new();

        if empty > 0 {
            notes.push(format!("{empty} entry points reached nothing"));
        }

        if row.stats.truncated > 0 {
            notes.push(format!("{} reach sets truncated", row.stats.truncated));
        }

        if row.stats.dropped > 0 {
            notes.push(format!("{} dropped past the cap", row.stats.dropped));
        }

        let suffix = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) };
        let line = format!("  {label:<name_width$}  {stored:>flows_width$} flows{suffix}");

        println!("{}", line.trim_end());
    }
}
