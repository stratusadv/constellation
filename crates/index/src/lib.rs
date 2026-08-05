#![forbid(unsafe_code)]

//! Project indexing: walk a repository, parse each supported file with the
//! matching extractor, and persist the resulting graph into the store. This
//! is the orchestration layer that turns a directory on disk into one
//! project's slice of the constellation.
//!
//! An index run is a pipeline, and the modules follow it: [`walk`] finds the
//! files, [`extract`] parses and persists them, [`resolve`] binds the
//! references they left behind, [`synthesize`] derives the edges no single file
//! could show, and [`link`] joins this project to the others. [`watch`] runs
//! that pipeline again, incrementally, as the working tree changes.

use std::path::Path;

use constellation_graph::ProjectId;
use constellation_store::{Store, StoreError};
use thiserror::Error;

mod companions;
mod context;
mod extract;
mod fingerprint;
mod flows;
mod git;
mod git_status;
mod history;
mod limits;
mod link;
mod paths;
mod resolve;
mod stale;
mod symbol_history;
mod synthesize;
mod walk;
mod watch;

pub use companions::{
    CompanionTarget, HistoryConfig, discover_companions, discover_versions,
    fetch_companion_history_repo, load_companion_repositories, load_history_config,
};
pub use extract::{index_paths_tracked, index_project_tracked};
pub use fingerprint::{extractor_fingerprint, git_head};
pub use flows::{
    EntryKind, FLOW_DEPTH_MAX, FLOW_REACH_NODES_MAX, FLOW_TRAVERSAL_KINDS, FLOWS_TOTAL_MAX,
    FlowOptions, FlowStats, compute_flows, retrace_flows,
};
pub use git::{GIT_OUTPUT_BYTES_MAX, GIT_TIMEOUT, GitRun, run_git};
pub use git_status::{
    GIT_STATUS_ENTRIES_MAX, GIT_STATUS_TTL_SECS, GitStatusHandle, GitStatusSnapshot,
    WorkingTreeState,
};
pub use limits::FILE_COUNT_MAX;
pub use link::link_constellation;
pub use paths::{module_of, namespace_chain, route_pattern, template_owner, url_prefix_chain};
pub use resolve::use_store_backed;
pub use stale::{StaleFiles, count_stale_files};
pub use walk::is_ignored_path;
pub use watch::{
    RefreshScope, WatchHandle, refresh_constellation, refresh_scoped, watch_constellation,
    watch_project,
};

/// A project's git commit history read from `root` and written to the store,
/// replacing any history previously recorded for it. Returns the number of
/// commits stored; zero when `root` is not a git repository, which is skipped
/// rather than treated as an error, since not every indexed source is a git
/// checkout.
pub fn ingest_history(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    commits_max: u32,
) -> Result<u32, IndexError> {
    ingest_history_reporting(store, project, root, commits_max, |_done, _total| {})
}

/// The [`ingest_history`] ingest, reporting progress as `(done, total)` commits through
/// `on_progress` so a caller can draw a progress bar as the history streams in.
pub fn ingest_history_reporting(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    commits_max: u32,
    on_progress: impl FnMut(u32, u32),
) -> Result<u32, IndexError> {
    assert!(!project.as_str().is_empty(), "project id must not be empty");
    assert!(!root.as_os_str().is_empty(), "project root must not be empty");
    assert!(commits_max > 0, "commit cap must be positive");

    let commits = history::read_history(root, commits_max, on_progress)?;

    let stored = store.replace_history(project, &commits)?;

    Ok(stored)
}

/// A project's symbol-level history (Tier 2) derived from its git blobs and
/// written to the store, replacing any previously recorded for it. For each file
/// that ever changed, its trackable symbols are extracted at each commit that
/// touched it and diffed against the prior revision, yielding added / modified /
/// removed rows. Returns the number of rows stored. Requires [`ingest_history`]
/// to have run first; it reads the commit/file map that left behind.
pub fn ingest_symbol_revisions(
    store: &Store,
    project: &ProjectId,
    root: &Path,
) -> Result<u32, IndexError> {
    ingest_symbol_revisions_reporting(store, project, root, |_done, _total| {})
}

/// The [`ingest_symbol_revisions`] pass, reporting progress as `(done, total)` touched
/// file revisions through `on_progress` so a caller can draw a progress bar over
/// the long Tier-2 pass.
pub fn ingest_symbol_revisions_reporting(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    on_progress: impl FnMut(u32, u32),
) -> Result<u32, IndexError> {
    assert!(!project.as_str().is_empty(), "project id must not be empty");
    assert!(!root.as_os_str().is_empty(), "project root must not be empty");

    let touches = store.history_file_touches(project, symbol_history::TOUCHES_MAX)?;

    if touches.is_empty() {
        return store.replace_symbol_revisions(project, &[]).map_err(IndexError::from);
    }

    let revisions = symbol_history::diff_history(root, project, &touches, on_progress)?;

    let stored = store.replace_symbol_revisions(project, &revisions)?;

    Ok(stored)
}

/// The tallies produced by one indexing run, for progress reporting and diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub files_indexed: u32,
    pub files_unchanged: u32,
    pub files_removed: u32,
    pub files_skipped: u32,
    pub nodes: u32,
    pub edges: u32,
    pub unresolved_refs: u32,
    pub resolved_edges: u32,
    pub unresolved_remaining: u32,
    pub synthesized_edges: u32,
    pub external_edges: u32,
}

/// The errors that can occur during indexing, walking, or watching.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("walk error: {0}")]
    Walk(#[from] ignore::Error),

    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),

    #[error("git error: {0}")]
    Git(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The index of every supported file under `root` into `store` as project `project`.
pub fn index_project(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
) -> Result<IndexStats, IndexError> {
    index_project_reporting(store, project, name, root, |_phase| {})
}

/// A progress event emitted while indexing, for a caller rendering a UI.
#[derive(Clone, Copy, Debug)]
pub enum IndexPhase {
    /// The file walk and extraction: `files_done` of `files_total` visited.
    Extracting { files_done: u32, files_total: u32 },
    /// The reference-resolution and edge-synthesis phase, after extraction: emitted
    /// once when the phase begins, with no per-item granularity.
    Resolving,
}

/// An indexing run's tallies plus the files it actually rewrote or removed, for
/// a caller that must re-derive something (execution flows) from exactly what
/// changed rather than from the whole project.
#[derive(Clone, Debug, Default)]
pub struct IndexOutcome {
    /// The project-relative paths written or removed this run, in no particular
    /// order. Empty when every file's content hash was unchanged.
    pub changed_paths: Vec<String>,
    pub stats: IndexStats,
}

/// The index of every supported file under `root`, reporting progress through
/// `on_phase` as files are extracted and as resolution begins. [`index_project`]
/// is this with a no-op reporter.
pub fn index_project_reporting(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    on_phase: impl FnMut(IndexPhase),
) -> Result<IndexStats, IndexError> {
    Ok(index_project_tracked(store, project, name, root, on_phase)?.stats)
}
