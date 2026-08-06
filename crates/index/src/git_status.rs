//! A cached, off-the-query-path view of every indexed project's working tree.
//!
//! "This file is modified right now" is the highest-signal bit constellation
//! otherwise does not show, and mid-task it is usually the file the task is
//! about. It is also the one signal that cannot be read from the graph, so it
//! comes from `git status`.
//!
//! `git status` on a large repository takes tens to hundreds of milliseconds,
//! which is one to three orders of magnitude more than a graph query. It
//! therefore never runs on the query path. A worker thread refreshes a snapshot
//! after each debounced watcher burst and on a [`GIT_STATUS_TTL_SECS`] ceiling;
//! queries read the most recent snapshot with one atomic clone of an `Arc`. A
//! snapshot stale by a few seconds is acceptable; a blocking one is not.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

use crate::git::run_git;

/// The refresh ceiling: a snapshot older than this is rebuilt on the next tick
/// even when no filesystem event arrived, so a `git stash` or an external
/// checkout does not leave a stale view indefinitely.
pub const GIT_STATUS_TTL_SECS: u64 = 5;

/// The fail-fast bound on porcelain entries parsed from one repository. Past
/// this the snapshot is marked truncated rather than growing without limit.
pub const GIT_STATUS_ENTRIES_MAX: usize = 10_000;

/// The fail-fast bound on bytes read from one `git status` invocation.
const GIT_STATUS_BYTES_MAX: usize = 8 * 1024 * 1024;

/// The change the working tree has made to a file since HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkingTreeState {
    /// The state of a file tracked and untouched relative to HEAD, which is every file a
    /// snapshot does not mention.
    Clean,
    /// The state of a file present on disk and unknown to git.
    Untracked,
    /// The state of a file deleted from the working tree or the index.
    Deleted,
    /// The state of a file newly added to the index.
    Added,
    /// The state of a file modified, renamed, copied, or type-changed.
    Modified,
}

impl WorkingTreeState {
    /// The compact marker appended to a rendered symbol line, or an empty
    /// string for a clean file, so the common case costs no bytes.
    pub fn marker(self) -> &'static str {
        match self {
            WorkingTreeState::Added => " [A]",
            WorkingTreeState::Clean => "",
            WorkingTreeState::Deleted => " [D]",
            WorkingTreeState::Modified => " [M]",
            WorkingTreeState::Untracked => " [?]",
        }
    }

    /// The `0.0..=1.0` weight this state contributes to a file's recency score.
    /// A tracked edit outranks an untracked file: an untracked file is often a
    /// scratch script, an edited one is nearly always the task.
    pub fn recency_weight(self) -> f64 {
        let weight = match self {
            WorkingTreeState::Added | WorkingTreeState::Modified => 1.0,
            WorkingTreeState::Deleted => 0.6,
            WorkingTreeState::Untracked => 0.4,
            WorkingTreeState::Clean => 0.0,
        };

        assert!((0.0..=1.0).contains(&weight), "a recency weight lands in 0..=1");

        weight
    }
}

/// A point-in-time view of every indexed project's working tree, keyed by
/// `(project id, project-relative path)`.
#[derive(Debug, Default)]
pub struct GitStatusSnapshot {
    entries: FxHashMap<(String, String), WorkingTreeState>,
    refreshed_at_ms: i64,
    truncated: bool,
}

impl GitStatusSnapshot {
    /// The working-tree state of one file, [`WorkingTreeState::Clean`] when the
    /// snapshot does not mention it (which includes every file in a project that
    /// is not a git repository).
    pub fn state(&self, project_id: &str, file_path: &str) -> WorkingTreeState {
        let key = (project_id.to_string(), file_path.replace('\\', "/"));

        self.entries.get(&key).copied().unwrap_or(WorkingTreeState::Clean)
    }

    /// The number of files the snapshot reports as not clean.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the snapshot reports no working-tree change at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The epoch-millisecond time the snapshot was built, zero for the empty
    /// snapshot a handle starts with.
    pub fn refreshed_at_ms(&self) -> i64 {
        self.refreshed_at_ms
    }

    /// Whether any repository hit [`GIT_STATUS_ENTRIES_MAX`], so the snapshot
    /// under-reports rather than being merely quiet.
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// A shared handle onto the most recent [`GitStatusSnapshot`].
///
/// Reading is one mutex acquisition and one `Arc` clone, never a `git`
/// invocation and never a copy of the entry map, so a query path can hold it
/// without inheriting the worker's latency.
#[derive(Clone, Debug)]
pub struct GitStatusHandle {
    current: Arc<Mutex<Arc<GitStatusSnapshot>>>,
}

impl Default for GitStatusHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl GitStatusHandle {
    /// A handle holding an empty snapshot, which reports every file clean until
    /// the first refresh lands.
    pub fn new() -> Self {
        Self { current: Arc::new(Mutex::new(Arc::new(GitStatusSnapshot::default()))) }
    }

    /// The most recent snapshot.
    pub fn snapshot(&self) -> Arc<GitStatusSnapshot> {
        Arc::clone(&lock_recover(&self.current))
    }

    /// The snapshot replaced wholesale. Readers holding the previous one keep a
    /// consistent view of it; there is no partially updated state.
    fn publish(&self, snapshot: GitStatusSnapshot) {
        *lock_recover(&self.current) = Arc::new(snapshot);
    }

    /// Whether the snapshot is older than the time-to-live, and so due a refresh
    /// even with no filesystem event to prompt one.
    fn is_expired(&self) -> bool {
        let refreshed = self.snapshot().refreshed_at_ms();
        let ttl_ms = i64::try_from(GIT_STATUS_TTL_SECS.saturating_mul(1_000)).unwrap_or(i64::MAX);

        now_ms().saturating_sub(refreshed) >= ttl_ms
    }
}

/// The milliseconds since the Unix epoch, or zero for a clock that predates it
/// (which makes every snapshot look expired rather than panicking).
use constellation_graph::now_unix_millis as now_ms;

/// The lock guard on `mutex`, recovered if a previous holder panicked. The
/// snapshot behind it is immutable, so a panic elsewhere cannot have left it
/// half-written.
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The snapshot rebuilt across every `(project id, root)` pair and published to
/// `handle`. Runs `git status --porcelain=v1` once per repository; a root that
/// is not a repository contributes nothing rather than failing the refresh.
pub fn refresh(handle: &GitStatusHandle, roots: &[(String, String)]) {
    let mut snapshot = GitStatusSnapshot {
        entries: FxHashMap::default(),
        refreshed_at_ms: now_ms(),
        truncated: false,
    };

    for (project_id, root) in roots {
        let Some(output) = run_status(Path::new(root)) else {
            continue;
        };

        let (entries, truncated) = parse_porcelain(&output);

        snapshot.truncated |= truncated;

        for (path, state) in entries {
            snapshot.entries.insert((project_id.clone(), path), state);
        }
    }

    handle.publish(snapshot);
}

/// The refresh skipped unless the snapshot has expired, for the worker's idle
/// tick. Returns whether a refresh ran.
pub fn refresh_if_expired(handle: &GitStatusHandle, roots: &[(String, String)]) -> bool {
    if !handle.is_expired() {
        return false;
    }

    refresh(handle, roots);

    true
}

/// The stdout of `git -C <root> status --porcelain=v1`, or `None` when git is
/// unavailable, the path is not a repository, the call passed its deadline, or
/// the output is implausibly large.
///
/// Bounded in time as well as size: this runs on the watcher's idle tick inside
/// a live server, so a git that parks (a smudge filter, a credential helper with
/// no terminal) would otherwise stall the working-tree snapshot for the session.
fn run_status(root: &Path) -> Option<String> {
    if !root.is_dir() {
        return None;
    }

    let run = run_git(root, &["status", "--porcelain=v1", "--no-renames"])?;

    if run.truncated || run.stdout.len() > GIT_STATUS_BYTES_MAX {
        return None;
    }

    Some(run.stdout)
}

/// The `(path, state)` pairs parsed from porcelain v1 output, plus whether the
/// entry cap was hit.
///
/// Each line is `XY PATH`, where `X` is the index status and `Y` the working
/// tree's. Renames are disabled at the call site (`--no-renames`), so a renamed
/// file appears as a delete plus an add rather than as an arrow form that would
/// need a second path parsed out.
fn parse_porcelain(output: &str) -> (Vec<(String, WorkingTreeState)>, bool) {
    let mut entries: Vec<(String, WorkingTreeState)> = Vec::new();

    for line in output.lines() {
        if entries.len() >= GIT_STATUS_ENTRIES_MAX {
            return (entries, true);
        }

        let Some((codes, path)) = split_porcelain_line(line) else {
            continue;
        };

        entries.push((path, combine_codes(codes)));
    }

    assert!(entries.len() <= GIT_STATUS_ENTRIES_MAX, "parsing respects the entry cap");

    (entries, false)
}

/// The `(status codes, path)` of one porcelain line, or `None` when the line is
/// too short to be one. Quoting (which git applies to paths holding unusual
/// bytes) is stripped, so the path matches what the store holds.
fn split_porcelain_line(line: &str) -> Option<(&str, String)> {
    if line.len() < 4 {
        return None;
    }

    let codes = line.get(..2)?;
    let path = line.get(3..)?.trim();

    if path.is_empty() {
        return None;
    }

    let unquoted = path.trim_matches('"');

    Some((codes, unquoted.replace('\\', "/")))
}

/// The single state a file's index and working-tree codes combine to. An
/// untracked file is only untracked; otherwise a delete outranks an add, which
/// outranks every other change, so the marker names the most consequential
/// thing that happened to the file.
fn combine_codes(codes: &str) -> WorkingTreeState {
    let mut characters = codes.chars();
    let index = characters.next().unwrap_or(' ');
    let worktree = characters.next().unwrap_or(' ');

    if index == '?' || worktree == '?' {
        return WorkingTreeState::Untracked;
    }

    if index == 'D' || worktree == 'D' {
        return WorkingTreeState::Deleted;
    }

    if index == 'A' || worktree == 'A' {
        return WorkingTreeState::Added;
    }

    WorkingTreeState::Modified
}

#[cfg(test)]
mod tests {
    use super::{
        GIT_STATUS_ENTRIES_MAX, GitStatusHandle, WorkingTreeState, combine_codes, parse_porcelain,
    };

    #[test]
    fn porcelain_lines_round_trip_into_states() {
        let output = concat!(
            " M app/models.py\n",
            "M  app/views.py\n",
            "A  app/new_service.py\n",
            " D app/gone.py\n",
            "?? notes.md\n",
            "MM app/both.py\n",
        );

        let (entries, truncated) = parse_porcelain(output);

        assert!(!truncated, "a six-line status is nowhere near the cap");

        let find = |path: &str| {
            entries.iter().find(|(entry, _)| entry == path).map(|(_, state)| *state)
        };

        assert_eq!(find("app/models.py"), Some(WorkingTreeState::Modified));
        assert_eq!(find("app/views.py"), Some(WorkingTreeState::Modified));
        assert_eq!(find("app/new_service.py"), Some(WorkingTreeState::Added));
        assert_eq!(find("app/gone.py"), Some(WorkingTreeState::Deleted));
        assert_eq!(find("notes.md"), Some(WorkingTreeState::Untracked));
        assert_eq!(find("app/both.py"), Some(WorkingTreeState::Modified));
    }

    #[test]
    fn a_quoted_path_is_unquoted_and_normalized() {
        let (entries, _) = parse_porcelain(" M \"app/odd name.py\"\n M app\\win.py\n");

        assert_eq!(entries[0].0, "app/odd name.py", "quotes are stripped");
        assert_eq!(entries[1].0, "app/win.py", "separators normalize to forward slashes");
    }

    #[test]
    fn a_short_or_blank_line_is_skipped_rather_than_panicking() {
        let (entries, _) = parse_porcelain("\n M\nxy\n M app/ok.py\n");

        assert_eq!(entries.len(), 1, "only the well-formed line parses, got {entries:?}");
    }

    #[test]
    fn parsing_stops_at_the_entry_cap_and_says_so() {
        let mut output = String::new();

        for index in 0..(GIT_STATUS_ENTRIES_MAX + 50) {
            output.push_str(&format!(" M app/file{index}.py\n"));
        }

        let (entries, truncated) = parse_porcelain(&output);

        assert_eq!(entries.len(), GIT_STATUS_ENTRIES_MAX, "the cap bounds the parse");
        assert!(truncated, "hitting the cap is reported, never silent");
    }

    #[test]
    fn a_delete_outranks_an_add_which_outranks_a_modify() {
        assert_eq!(combine_codes("AD"), WorkingTreeState::Deleted);
        assert_eq!(combine_codes("AM"), WorkingTreeState::Added);
        assert_eq!(combine_codes("MM"), WorkingTreeState::Modified);
        assert_eq!(combine_codes("??"), WorkingTreeState::Untracked);
    }

    #[test]
    fn markers_are_empty_for_clean_and_distinct_otherwise() {
        assert_eq!(WorkingTreeState::Clean.marker(), "", "a clean file costs no bytes");

        let markers = [
            WorkingTreeState::Added.marker(),
            WorkingTreeState::Deleted.marker(),
            WorkingTreeState::Modified.marker(),
            WorkingTreeState::Untracked.marker(),
        ];

        let mut sorted = markers.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(sorted.len(), markers.len(), "each state renders distinctly");
    }

    #[test]
    fn a_tracked_edit_weighs_more_than_an_untracked_file() {
        assert!(
            WorkingTreeState::Modified.recency_weight() > WorkingTreeState::Untracked.recency_weight(),
            "an edited file is nearly always the task; an untracked one is often a scratch script",
        );

        assert_eq!(WorkingTreeState::Clean.recency_weight(), 0.0, "a clean file adds nothing");
    }

    #[test]
    fn a_fresh_handle_reports_every_file_clean() {
        let handle = GitStatusHandle::new();
        let snapshot = handle.snapshot();

        assert!(snapshot.is_empty(), "no refresh has run yet");
        assert_eq!(snapshot.state("blog", "app/models.py"), WorkingTreeState::Clean);
        assert!(!snapshot.truncated(), "an empty snapshot is not a truncated one");
    }
}
