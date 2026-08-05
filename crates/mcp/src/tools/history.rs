//! `constellation_history`, `constellation_symbol_history`, and
//! `constellation_as_of`: the graph read over time.

use std::fmt::Write;

use constellation_graph::ProjectId;
use constellation_store::{Store, StoreError};

use crate::dates::{parse_ymd_to_epoch, ymd_from_epoch_secs};
use crate::limits::PAGED_FETCH_MAX;
use crate::cursor;

/// A timeline for one `history` query: the commits touching `target` (a path
/// substring; `None` lists recent activity across the whole constellation),
/// newest first, each stamped with an absolute date, short hash, churn, and
/// author. Reads the history the `history` command ingests; empty until then.
#[doc(hidden)]
pub fn history_text(
    store: &Store,
    target: Option<&str>,
    project: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let pattern = match target {
        Some(target) if !target.is_empty() => format!("%{target}%"),
        _ => "%".to_string(),
    };

    // Fetch one past the page so an offset can be applied here and the tail is
    // detectable; the SQL limit is the only bound the query itself carries. Fetching
    // exactly the page made every response look complete, because `cursor::next_line`
    // compares what was shown against what was loaded and those were equal by
    // construction, so no cursor was ever offered.
    let fetch = u32::try_from(page.offset.saturating_add(limit as usize).saturating_add(1))
        .unwrap_or(u32::MAX)
        .min(PAGED_FETCH_MAX);

    let hits = store.history_for_path(project_id.as_ref(), &pattern, fetch)?;

    if hits.is_empty() {
        return Ok(history_empty_message(target));
    }

    assert!(hits.len() as u32 <= fetch, "history query respects its limit");

    let label = target.filter(|target| !target.is_empty()).unwrap_or("the constellation");
    let window = cursor::slice(&hits, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out, "history of {label}: {} commits, newest first", window.len());

    for hit in window {
        let (year, month, day) = ymd_from_epoch_secs(hit.committed_at);
        let short = &hit.commit_hash[..hit.commit_hash.len().min(8)];

        let _ = writeln!(out,
            "  {year:04}-{month:02}-{day:02} {short} +{}/-{} ({}f) {}: {}",
            hit.insertions, hit.deletions, hit.files_changed, hit.author, hit.summary,
        );
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), hits.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    Ok(out)
}

/// A timeline for one `symbol_history` query: the commits where a definition
/// matching `symbol` was added, modified, or removed, newest first, each stamped
/// with an absolute date, short hash, change kind, qualified name, and the
/// signature at that revision. Reads the symbol history `history --symbols`
/// ingests; empty until then.
#[doc(hidden)]
pub fn symbol_history_text(
    store: &Store,
    symbol: &str,
    project: Option<&str>,
    limit: u32,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let hits = store.symbol_history(project_id.as_ref(), symbol, limit)?;

    if hits.is_empty() {
        if store.has_symbol_revisions(project_id.as_ref())? {
            return Ok(format!(
                "no recorded changes for {symbol:?} \
                 (symbol history is populated, but nothing matches; try the bare name, \
                 or an exact Owner.member like \"OrderLineItem.quantity\")"
            ));
        }

        return Ok(format!(
            "no recorded changes for {symbol:?} \
             (run `constellation history --symbols` to populate symbol history)"
        ));
    }

    assert!(hits.len() as u32 <= limit, "symbol history respects its limit");

    let mut out = format!("history of {symbol}: {} changes, newest first\n", hits.len());

    for hit in &hits {
        let (year, month, day) = ymd_from_epoch_secs(hit.committed_at);
        let short = &hit.commit_hash[..hit.commit_hash.len().min(8)];

        let signature = match hit.signature.as_deref() {
            Some(signature) if !signature.is_empty() => format!("  [{signature}]"),
            _ => String::new(),
        };

        let _ = writeln!(out,
            "  {year:04}-{month:02}-{day:02} {short} {} {} {}{signature}",
            hit.change, hit.kind, hit.qualified_name,
        );
    }

    Ok(out)
}

/// The symbols alive at one point in time for `constellation_as_of`: those
/// recorded as present (added or modified, not since removed) as of `at` (a
/// commit hash or a "YYYY-MM-DD" date), grouped by file, each with its kind and
/// the signature in effect then. Reads the symbol history `history --symbols`
/// ingests.
#[doc(hidden)]
pub fn as_of_text(
    store: &Store,
    at: &str,
    project: Option<&str>,
    path: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let threshold = match resolve_as_of(store, project_id.as_ref(), at)? {
        Some(threshold) => threshold,
        None => {
            return Ok(format!(
                "could not resolve {at:?} to a commit or date (pass a commit hash or YYYY-MM-DD)"
            ));
        }
    };

    let pattern = path.filter(|path| !path.is_empty()).map(|path| format!("%{path}%"));

    // One past the page, so the tail is detectable and a cursor is offered; see the
    // same bound in `history_text`.
    let fetch = u32::try_from(page.offset.saturating_add(limit as usize).saturating_add(1))
        .unwrap_or(u32::MAX)
        .min(PAGED_FETCH_MAX);

    let symbols = store.symbols_as_of(project_id.as_ref(), threshold, pattern.as_deref(), fetch)?;

    if symbols.is_empty() {
        return Ok(format!(
            "no symbols recorded as of {at} \
             (run `constellation history --symbols`, widen the scope, or pick a later point)"
        ));
    }

    let (year, month, day) = ymd_from_epoch_secs(threshold);

    let window = cursor::slice(&symbols, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out,
        "symbols as of {at} ({year:04}-{month:02}-{day:02}): {} alive",
        window.len(),
    );

    let mut current_file = "";

    for symbol in window {
        if symbol.file_path != current_file {
            let _ = writeln!(out, "{}:", symbol.file_path);
            current_file = symbol.file_path.as_str();
        }

        let signature = match symbol.signature.as_deref() {
            Some(signature) if !signature.is_empty() => format!(" [{signature}]"),
            _ => String::new(),
        };

        let _ = writeln!(out, "  {} {}{signature}", symbol.kind, symbol.qualified_name);
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), symbols.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    Ok(out)
}

/// The epoch-second threshold an as-of point resolves to: a "YYYY-MM-DD" date, or
/// else the committer time of the commit whose hash matches `at`. `None` when it
/// is neither a date nor a known commit.
fn resolve_as_of(
    store: &Store,
    project: Option<&ProjectId>,
    at: &str,
) -> Result<Option<i64>, StoreError> {
    if let Some(epoch) = parse_ymd_to_epoch(at) {
        return Ok(Some(epoch));
    }

    store.commit_committed_at(project, at)
}

/// The project id whose id or display name equals `name`, or `None` when no
/// project matches.
pub(crate) fn find_project(store: &Store, name: &str) -> Result<Option<ProjectId>, StoreError> {
    let projects = store.all_projects()?;

    let found = projects
        .into_iter()
        .find(|project| project.id.as_str() == name || project.name == name);

    Ok(found.map(|project| project.id))
}

/// The reply when a history query matches nothing, distinguishing "no history
/// ingested yet" from "history exists but nothing touched this path".
fn history_empty_message(target: Option<&str>) -> String {
    match target.filter(|target| !target.is_empty()) {
        Some(target) => format!(
            "no commits touching {target:?} in the indexed history \
             (run `constellation history` to populate it)"
        ),
        None => "no git history indexed (run `constellation history` to populate it)".to_string(),
    }
}
