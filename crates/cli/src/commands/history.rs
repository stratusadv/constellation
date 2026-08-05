//! `history`: git commit history, and optionally per-symbol deltas, ingested so
//! the graph can be read over time.
//!
//! Also the entry point `init` and `sync` call once their own work is done,
//! which is why [`index_history_if_enabled`] lives here rather than beside them.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_graph::ProjectId;
use constellation_store::Store;

use crate::progress;
use crate::summary::digits;
use crate::workspace::discover_database;

/// The `constellation history [database] [--symbols]` command ingests each indexed
/// project's git commit history into the database so the graph can be read over
/// time. `--symbols` also runs the Tier-2 symbol-delta pass. Behavior follows the
/// workspace's `[history]` config (enabled, symbols, companions, commits_max); the
/// `--symbols` flag forces the symbol pass on regardless. The database is
/// discovered as for `serve`.
pub(crate) fn history_command(rest: &[String]) -> Result<()> {
    let symbols_flag = rest.iter().any(|argument| argument == "--symbols");
    let explicit = rest.iter().find(|argument| !argument.starts_with("--"));

    let database = match explicit {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` first",
            database.display(),
        );
    }

    let workspace_root = database
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf);

    let config = match &workspace_root {
        Some(root) => constellation_index::load_history_config(root),
        None => constellation_index::HistoryConfig::default(),
    };

    if !config.enabled {
        println!("git history indexing disabled in .constellation/config.toml");

        return Ok(());
    }

    let symbols = symbols_flag || config.symbols;
    let store = Store::open(&database)?;

    ingest_all_history(&store, &config, symbols, workspace_root.as_deref())
}

/// The history ingested for the workspace after a normal index, when `[history] enabled`
/// (the default). Tier 1 always; the Tier-2 symbol pass only when `[history]
/// symbols`; companion/library histories skipped when `[history] companions` is
/// false. A silent no-op when history is disabled, so `init` and `sync` stay fast
/// for workspaces that opt out.
pub(crate) fn index_history_if_enabled(store: &Store, workspace_root: &Path) -> Result<()> {
    let config = constellation_index::load_history_config(workspace_root);

    if !config.enabled {
        return Ok(());
    }

    ingest_all_history(store, &config, config.symbols, Some(workspace_root))
}

/// The history of every indexed project ingested per `config`, printing a line each and
/// a final summary. Companion/library projects are skipped when `history.companions`
/// is false, so only the workspace's own code is indexed then.
fn ingest_all_history(
    store: &Store,
    config: &constellation_index::HistoryConfig,
    symbols: bool,
    workspace_root: Option<&Path>,
) -> Result<()> {
    let projects = store.all_projects()?;
    let repositories = workspace_root
        .map(constellation_index::load_companion_repositories)
        .unwrap_or_default();
    let fingerprint = constellation_index::extractor_fingerprint();

    let mut rows: Vec<HistoryRow> = Vec::new();
    let mut skipped: u32 = 0;

    for project in &projects {
        if !config.companions && !is_workspace_primary(&project.root_path, workspace_root) {
            skipped += 1;

            continue;
        }

        // A companion with a configured repository (its `.venv` copy carries no
        // `.git`) sources history from a full clone of that repo at the tag matching
        // the installed version; everything else reads its own root.
        let history_root = match workspace_root.zip(repositories.get(project.id.as_str())) {
            Some((root, url)) => match constellation_index::fetch_companion_history_repo(
                root,
                project.id.as_str(),
                url,
            ) {
                Some(clone) => clone,
                None => continue,
            },
            None => PathBuf::from(&project.root_path),
        };

        // Skip re-ingesting a repository whose HEAD and extractor are unchanged
        // since the last run: the stored rows are still valid, so reuse their counts.
        let stamp = constellation_index::git_head(&history_root)
            .map(|head| format!("{head}|{fingerprint}|{symbols}"));
        let stored = store.git_ingest_stamp(&project.id)?;

        if let Some(stamp) = &stamp
            && stored.as_ref() == Some(stamp)
        {
            let commits = store.count_history_commits(&project.id)?;

            if commits == 0 {
                continue;
            }

            let changes = if symbols {
                Some(store.count_symbol_revisions(&project.id)?)
            } else {
                None
            };

            rows.push(HistoryRow {
                label: project.id.as_str().to_string(),
                commits,
                changes,
                cached: true,
            });

            continue;
        }

        let commits =
            ingest_history_with_progress(store, &project.id, &history_root, config.commits_max)?;

        // Zero commits means the root is not its own git repository (a `.venv` copy
        // shares the workspace's repo); replace_history already cleared any stale
        // rows, so skip rather than misattribute the workspace's history.
        if commits == 0 {
            continue;
        }

        let changes = if symbols {
            Some(ingest_symbols_with_progress(
                store,
                &project.id,
                &history_root,
            )?)
        } else {
            None
        };

        if let Some(stamp) = stamp {
            store.set_git_ingest_stamp(&project.id, &stamp)?;
        }

        rows.push(HistoryRow {
            label: project.id.as_str().to_string(),
            commits,
            changes,
            cached: false,
        });
    }

    print_history_summary(&rows, skipped);

    Ok(())
}

/// A project's commit history ingested behind the shared gradient progress bar:
/// the Tier-1 read streams `git log` over every commit, so on a large repo it is
/// worth a bar like the index phase.
fn ingest_history_with_progress(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    commits_max: u32,
) -> Result<u32> {
    let mut progress = progress::Progress::new(&format!("history {project}"));

    let commits = constellation_index::ingest_history_reporting(
        store,
        project,
        root,
        commits_max,
        |done, total| {
            progress.on_phase(constellation_index::IndexPhase::Extracting {
                files_done: done,
                files_total: total,
            });
        },
    )?;

    progress.finish();

    Ok(commits)
}

/// A project's symbol history ingested behind the shared gradient progress bar:
/// the Tier-2 pass walks every touched file revision, so it can run long and is
/// worth a bar like the index phase.
fn ingest_symbols_with_progress(store: &Store, project: &ProjectId, root: &Path) -> Result<u32> {
    let mut progress = progress::Progress::new(&format!("symbols {project}"));

    let revisions = constellation_index::ingest_symbol_revisions_reporting(
        store,
        project,
        root,
        |done, total| {
            progress.on_phase(constellation_index::IndexPhase::Extracting {
                files_done: done,
                files_total: total,
            });
        },
    )?;

    progress.finish();

    Ok(revisions)
}

/// Whether `project_root` is the workspace's own root, not a companion or library
/// (which roots under a venv or `.constellation/sources`), compared as absolute
/// paths. A missing workspace root treats every project as primary, so nothing is
/// skipped.
fn is_workspace_primary(project_root: &str, workspace_root: Option<&Path>) -> bool {
    let Some(workspace_root) = workspace_root else {
        return true;
    };

    match (
        std::path::absolute(project_root),
        std::path::absolute(workspace_root),
    ) {
        (Ok(project), Ok(workspace)) => project == workspace,
        _ => false,
    }
}

/// A project's history-ingest result for the summary: its label, commit count,
/// and symbol-change count when the Tier-2 pass ran.
struct HistoryRow {
    label: String,
    commits: u32,
    changes: Option<u32>,
    cached: bool,
}

/// The history-ingest summary, aligned like the index summary: one row per project
/// with its commit count, plus its symbol-change count when the symbol pass ran.
fn print_history_summary(rows: &[HistoryRow], skipped: u32) {
    if rows.is_empty() && skipped == 0 {
        return;
    }

    let name_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let commits_width = rows
        .iter()
        .map(|row| digits(row.commits))
        .max()
        .unwrap_or(1);

    println!("git history");

    for row in rows {
        let label = &row.label;
        let commits = row.commits;

        let changes = match row.changes {
            Some(changes) => format!("  {changes} symbol changes"),
            None => String::new(),
        };

        let cached = if row.cached { "  (cached)" } else { "" };

        let line =
            format!("  {label:<name_width$}  {commits:>commits_width$} commits{changes}{cached}");

        println!("{}", line.trim_end());
    }

    if skipped > 0 {
        println!(
            "  ({skipped} companion/library projects skipped; set history.companions = true to include)"
        );
    }
}
