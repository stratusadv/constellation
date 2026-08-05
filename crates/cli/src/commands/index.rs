//! `init` and the bare-path index: turning a directory into an indexed project.
//!
//! Both commands do the same work and differ only in what they set up first,
//! so they share [`index_and_report`] rather than each printing their own
//! summary.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_graph::ProjectId;
use constellation_index::{
    discover_companions, discover_versions, index_project_reporting, link_constellation,
};
use constellation_store::Store;

use crate::commands::history::index_history_if_enabled;
use crate::progress;
use crate::summary::{SummaryRow, print_summary_all_projects, project_source};
use crate::workspace::{create_index_directory, project_name, resolve_root, scaffold_config};

/// The `constellation init [path]` command creates `<path>/.constellation/index.db`
/// (defaulting to the current directory), scaffolds a starter config, indexes the
/// project and its companions, links them, traces their execution flows, and
/// ingests git history when `[history]` is enabled (the default).
///
/// Flows are traced here rather than left to `constellation flows` because a
/// derived table nobody knows to populate is a feature nobody has: both
/// `constellation_flows` and `constellation_affected_flows` return an honest
/// empty until something computes them, and so does the flow-participation
/// factor in `constellation_changed`'s risk score. Seeding them at `init` also
/// starts the freshness loop, since [`constellation_index::refresh_constellation`]
/// retraces the affected flows of any project that already has some.
///
/// `--no-flows` skips the pass for a project large enough that the wait matters.
pub(crate) fn init_command(rest: &[String]) -> Result<()> {
    let root_owned = match rest.iter().find(|argument| !argument.starts_with('-')) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };

    let root = root_owned.as_path();

    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }

    // Canonicalize so the stored root is absolute: a relative root (`.`) breaks
    // explore's source loading whenever serve runs from a different directory.
    let canonical = resolve_root(root)?;
    let root = canonical.as_path();

    let database = create_index_directory(root)?;
    scaffold_config(root);

    let store = Store::open(&database)?;

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());

    index_and_report(&store, &project, &name, root)?;

    if rest.iter().any(|argument| argument == "--no-flows") {
        println!("  flows    skipped (--no-flows); run `constellation flows` to trace them");
    } else {
        trace_flows_for_every_project(&store)?;
    }

    // The hook is registered beside the index it reads, not once per machine, so
    // a project constellation has never indexed never spawns one.
    if rest.iter().any(|argument| argument == "--no-hooks") {
        println!("  hook     skipped (--no-hooks)");
    } else {
        crate::bootstrap::install_project_hook(root);
    }

    index_history_if_enabled(&store, root)
}

/// The indexing of a repository given as a positional argument, reusing the
/// existing `.constellation/` if present.
pub(crate) fn index_command(root_argument: &str) -> Result<()> {
    let root = Path::new(root_argument);

    if !root.is_dir() {
        bail!("not a directory: {root_argument}");
    }

    let canonical = resolve_root(root)?;
    let root = canonical.as_path();

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());

    let database = create_index_directory(root)?;
    let store = Store::open(&database)?;

    index_and_report(&store, &project, &name, root)?;
    index_history_if_enabled(&store, root)
}

/// The execution flows of every indexed project traced and stored, reported as one
/// line per project. Errors are reported and swallowed: a failed flow trace
/// leaves the graph itself perfectly usable, so it must not fail `init`.
fn trace_flows_for_every_project(store: &Store) -> Result<()> {
    let options = constellation_index::FlowOptions::default();

    for project in store.all_projects()? {
        let mut progress = progress::Progress::new(&format!("flows {}", project.id));
        let traced = constellation_index::compute_flows(store, &project.id, options);

        progress.finish();

        match traced {
            Ok(stats) => println!("  flows    {:<14} {} flows", project.id.as_str(), stats.stored),
            Err(error) => eprintln!("  flows    {}: {error}", project.id.as_str()),
        }
    }

    Ok(())
}

/// The workspace indexed, its companions and versions discovered and indexed, the
/// constellation linked, and the compact summary printed. Shared by `init` and the
/// bare-path index so both produce the same output.
fn index_and_report(store: &Store, project: &ProjectId, name: &str, root: &Path) -> Result<()> {
    let mut progress = progress::Progress::new("indexing");

    index_project_reporting(store, project, name, root, |phase| {
        progress.on_phase(phase);
    })?;

    progress.finish();

    index_companions(store, root)?;

    // Relink whenever more than the workspace is present, so a re-index of changed
    // workspace code rebinds to the existing companions, not only when a new one
    // was just added.
    if store.all_projects()?.len() > 1 {
        link_constellation(store)?;
    }

    // Summarize every project in the store, not just the ones indexed this run, so
    // a re-index still lists companions that were already present.
    print_summary_all_projects(store, root)
}

/// The companions and version copies discovered under `workspace_root`, each indexed
/// as its own project (drawing a progress bar) and returned as a summary row. A
/// version copy is marked reference-only in the store after indexing.
pub(crate) fn index_companions(store: &Store, workspace_root: &Path) -> Result<Vec<SummaryRow>> {
    let mut targets = discover_companions(store, workspace_root)?;
    targets.extend(discover_versions(store, workspace_root)?);

    let mut rows: Vec<SummaryRow> = Vec::with_capacity(targets.len());

    for target in &targets {
        let project = ProjectId::new(target.project_id.as_str());
        let mut progress = progress::Progress::new(&format!("indexing {}", target.project_id));

        index_project_reporting(
            store,
            &project,
            &target.project_id,
            &target.package_root,
            |phase| {
                progress.on_phase(phase);
            },
        )?;

        progress.finish();

        if target.reference_only {
            store.set_reference_only(&project, true)?;
        }

        rows.push(SummaryRow {
            label: target.project_id.clone(),
            files: store.count_files(&project)?,
            nodes: store.count_nodes(&project)?,
            source: project_source(&target.package_root, workspace_root, target.reference_only),
        });
    }

    Ok(rows)
}
