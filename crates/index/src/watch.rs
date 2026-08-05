//! The file watcher, and the re-index it drives.
//!
//! Events arrive faster and messier than they can be acted on (a git checkout
//! is thousands of events in one burst), so they are debounced and coalesced
//! rather than handled one by one. The invariant the tests hold this to: a
//! watched store must converge on exactly what a from-scratch index would hold.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use constellation_graph::{Language, ProjectId};
use constellation_store::Store;
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode};

use crate::{IndexError, IndexStats};
use crate::extract::{index_paths_tracked, index_project_tracked};
use crate::limits::{
    DEBOUNCE, DEBOUNCE_EVENTS_MAX, DEBOUNCE_WINDOW_MS, PENDING_EVENTS_MAX, WATCH_IDLE_TICK,
    WATCH_REREGISTER_MAX,
};
use crate::link::link_constellation;
use crate::walk::is_ignored_path;
use crate::git_status;
use crate::flows::{FlowOptions, retrace_flows};
use crate::git_status::GitStatusHandle;
use crate::index_project;

/// The initial index of `root`, then a watch that re-indexes (incrementally) after each
/// debounced burst of filesystem changes. `on_index` is called with the stats
/// of every index, initial and subsequent. Blocks until the watcher stops.
pub fn watch_project(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    mut on_index: impl FnMut(&IndexStats),
) -> Result<(), IndexError> {
    assert!(!name.is_empty(), "project name must not be empty");
    assert!(root.is_dir(), "project root must be a directory: {root:?}");

    let root_absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_absolute.as_path();

    on_index(&index_project(store, project, name, root)?);

    relink_companions(store)?;

    let (signals, inbox) = std::sync::mpsc::channel::<WatchSignal>();

    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(DEBOUNCE_WINDOW_MS),
        None,
        burst_callback(signals),
    )?;

    debouncer.watch(root, RecursiveMode::Recursive)?;

    loop {
        let Ok(WatchSignal::Burst(paths)) = inbox.recv() else {
            return Ok(());
        };

        let burst = coalesce_burst(&inbox, paths);

        if burst.shutdown {
            return Ok(());
        }

        register_new_directories(&mut debouncer, &burst.paths);

        // Re-index the files the burst named, not the whole tree. A burst that
        // cannot describe itself (see `burst_scope`) widens back to the walk.
        let outcome = match burst_scope(&burst) {
            RefreshScope::Everything => index_project_tracked(store, project, name, root, drop)?,
            RefreshScope::Paths(paths) => {
                index_paths_tracked(store, project, name, root, paths)?
            }
        };

        on_index(&outcome.stats);

        relink_companions(store)?;
    }
}

/// The constellation re-linked after a single project was re-indexed, when there is
/// more than one project to link.
///
/// A re-index re-derives that project's external stubs from scratch, so a class
/// whose base a companion package defines extends a fresh, un-unified stub again.
/// The first link consumed the reference rows that produced it
/// ([`constellation_store::Store::delete_satisfied_unresolved`]), so nothing else
/// will ever rebuild that edge: skipping the re-link does not delay the
/// cross-project edge, it loses it. `model` then reports `bases: (none)` for every
/// re-indexed model and `subclasses` misses it, silently and permanently.
fn relink_companions(store: &Store) -> Result<(), IndexError> {
    if store.all_projects()?.len() > 1 {
        link_constellation(store)?;
    }

    Ok(())
}

/// The scope one refresh should re-examine.
///
/// The distinction is the difference between a watcher that costs the whole
/// constellation per keystroke and one that costs the files that changed. It is
/// stated as a type rather than an `Option<&[PathBuf]>` because choosing
/// [`RefreshScope::Everything`] is a correctness decision (see [`is_scopable`]),
/// not a missing argument.
#[derive(Clone, Copy, Debug)]
pub enum RefreshScope<'paths> {
    /// The scope re-walking every project from its stored root. The startup catch-up, and
    /// any burst whose paths cannot be trusted to describe themselves.
    Everything,
    /// The scope of these absolute paths only, re-indexed in the projects that own them.
    Paths(&'paths [PathBuf]),
}

/// The projects all re-indexed from their stored roots, the whole-tree form of
/// [`refresh_scoped`]. What `sync` and the startup catch-up run.
pub fn refresh_constellation(store: &Store) -> Result<bool, IndexError> {
    refresh_scoped(store, RefreshScope::Everything)
}

/// The constellation brought back in step with disk over `scope`, re-linking a
/// multi-project constellation when anything changed.
///
/// Returns whether the graph changed, so a caller can skip cache invalidation on
/// a no-op refresh.
///
/// A project that already carries execution flows has the affected ones retraced
/// here too. Retracing is not a flag, because the only projects that pay for it
/// are the ones that ran `constellation flows` and therefore asked for flows to
/// exist; for every other project [`Store::count_flows`] is zero and the pass
/// costs one query. Leaving them stale instead would be the worse failure: an
/// absent flow reads as "not computed", a stale one reads as fact.
pub fn refresh_scoped(store: &Store, scope: RefreshScope<'_>) -> Result<bool, IndexError> {
    let projects = store.all_projects()?;

    let mut changed = false;

    for row in &projects {
        let root = Path::new(&row.root_path);

        if !root.is_dir() {
            continue;
        }

        let outcome = match scope {
            RefreshScope::Everything => {
                index_project_tracked(store, &row.id, &row.name, root, |_phase| {})?
            }
            RefreshScope::Paths(paths) => {
                index_paths_tracked(store, &row.id, &row.name, root, paths)?
            }
        };

        if outcome.stats.files_indexed == 0 && outcome.stats.files_removed == 0 {
            continue;
        }

        changed = true;

        if store.count_flows(&row.id)? > 0 {
            retrace_flows(store, &row.id, &outcome.changed_paths, FlowOptions::default())?;
        }
    }

    // Re-linking reads the whole graph, so it is gated on something having
    // actually changed rather than on the refresh having run. It cannot be
    // narrowed further: a re-indexed project re-derives its external stubs from
    // scratch, and the references that produced the old cross-project edges are
    // consumed, so skipping the re-link loses those edges permanently rather
    // than deferring them.
    if changed && projects.len() > 1 {
        link_constellation(store)?;
    }

    Ok(changed)
}

/// Whether a burst path fully describes its own change, and so can be applied
/// without re-walking the project.
///
/// A file does, present or gone: its rows are keyed by exactly this path.
///
/// A directory never does, in either direction, and both directions are traps.
/// A directory that is *gone* took rows with it that nothing in this burst
/// names. A directory that *exists* is worse, because it looks safe: a
/// recursive watch does not cover a directory created after registration until
/// it is registered, so files written into a brand new package between its
/// creation and that registration produce no events at all. That is the race
/// [`register_new_directories`] exists for, and scoping to the directory path
/// alone indexes precisely nothing while looking like it worked.
///
/// So any directory in a burst widens it to the walk, which is the only run
/// that can see a file no event ever mentioned. Directory paths are rare in
/// practice: creating a file reports the file, not its parent.
fn is_scopable(path: &Path) -> bool {
    if path.is_dir() {
        return false;
    }

    if path.is_file() {
        return true;
    }

    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(Language::from_extension)
        .is_some()
}

/// The scope a coalesced burst may be served at.
///
/// Overflow and any path that cannot describe itself both widen to the whole
/// constellation. Widening is always safe; narrowing wrongly is what breaks
/// convergence, so the choice is deliberately biased.
fn burst_scope(burst: &Burst) -> RefreshScope<'_> {
    if burst.overflowed || !burst.paths.iter().all(|path| is_scopable(path)) {
        return RefreshScope::Everything;
    }

    RefreshScope::Paths(&burst.paths)
}

/// The debouncer callback that turns one settled batch of filesystem events
/// into a [`WatchSignal::Burst`] on `signals`.
///
/// Both watchers (the single-project [`watch_project`] and the whole-constellation
/// [`watch_loop`]) need exactly this, and previously each carried its own copy.
/// Two copies of the filter is two places for "reads are not changes" to be
/// true in one watcher and not the other, which is the bug that makes a watcher
/// re-index forever.
fn burst_callback(
    signals: std::sync::mpsc::Sender<WatchSignal>,
) -> impl FnMut(notify_debouncer_full::DebounceEventResult) {
    move |result| {
        let Ok(events) = result else {
            return;
        };

        let paths: Vec<PathBuf> = events
            .into_iter()
            .filter(|event| is_content_change(&event.event.kind))
            .flat_map(|event| event.event.paths.clone())
            .filter(|path| !is_ignored_path(path))
            .collect();

        if !paths.is_empty() {
            let _ = signals.send(WatchSignal::Burst(paths));
        }
    }
}

/// Whether an event describes a change to the tree rather than a read of it.
///
/// inotify reports opens, reads, and access-time updates as events, where
/// `ReadDirectoryChangesW` reports only writes. Acting on a read is not merely
/// wasted work on Linux, it does not terminate: a re-index reads every file in
/// the project, each read raises another event, and that burst starts the next
/// re-index. One editor, linter, or agent reading the tree is enough to keep it
/// running forever, and the real change arriving mid-storm is coalesced into a
/// burst that never settles long enough to be served.
///
/// Metadata changes go with them. A read updates an access time, so treating
/// metadata as content reopens the same loop through a different event.
fn is_content_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)))
}

/// A message the watch loop acts on. Shutdown travels the same channel as a
/// filesystem burst on purpose: stopping the watcher must never mean dropping
/// the debouncer out from under a loop that is still reading from it.
enum WatchSignal {
    /// A debounced burst of changed paths.
    Burst(Vec<PathBuf>),
    /// A request to finish the in-flight re-index and return.
    Shutdown,
}

/// A running constellation watcher.
///
/// Dropping it, or calling [`WatchHandle::stop`], signals the watch thread and
/// blocks until the in-flight re-index has finished, so no indexing outlives the
/// handle and a caller can rely on the store being quiescent once `stop`
/// returns. Both paths are idempotent: a second `stop`, or a drop after one,
/// does nothing.
pub struct WatchHandle {
    git_status: GitStatusHandle,
    signals: Option<std::sync::mpsc::Sender<WatchSignal>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WatchHandle {
    /// The watcher stopped and joined. Returns once the watch thread has
    /// finished whatever re-index it was running; calling it twice is a no-op.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        // Wake the loop through the dedicated signal, never by dropping the
        // debouncer: the loop owns it, and tearing it down under a live
        // borrow is how a watcher shutdown turns into a hang.
        if let Some(signals) = self.signals.take() {
            let _ = signals.send(WatchSignal::Shutdown);
        }

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        assert!(self.thread.is_none(), "a stopped watcher holds no thread");
    }

    /// The shared working-tree snapshot the watcher keeps refreshed, for a
    /// query path that wants working-tree state without running git itself.
    pub fn git_status(&self) -> GitStatusHandle {
        self.git_status.clone()
    }

    /// Whether the watch thread has been asked to stop.
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The catch-up with the working tree, then a watch of every indexed project's
/// root on a named background thread. After each debounced burst of changes the
/// constellation is refreshed and `on_change` invoked (only when the graph
/// actually changed) so a long-running server can drop its caches.
///
/// Returns immediately with a [`WatchHandle`] that owns the thread; the initial
/// catch-up runs on that thread too, so serving can begin before it finishes. A
/// panic in one re-index is contained so the watcher survives it. Progress goes
/// to stderr, never stdout, which the MCP server reserves for its protocol.
pub fn watch_constellation(
    database: &Path,
    on_change: impl FnMut() + Send + 'static,
) -> Result<WatchHandle, IndexError> {
    let database = database.to_path_buf();
    let git_status = GitStatusHandle::new();
    let stop = Arc::new(AtomicBool::new(false));

    let (signals, inbox) = std::sync::mpsc::channel::<WatchSignal>();

    let thread_git_status = git_status.clone();
    let thread_stop = Arc::clone(&stop);
    let thread_signals = signals.clone();

    let thread = std::thread::Builder::new()
        .name("constellation-watch".to_string())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                watch_loop(&database, &thread_git_status, &thread_stop, &thread_signals, inbox, on_change)
            }));

            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("constellation: watcher stopped: {error}"),
                Err(_) => eprintln!("constellation: watcher thread panicked; re-index is disabled"),
            }
        })?;

    Ok(WatchHandle { git_status, signals: Some(signals), stop, thread: Some(thread) })
}

/// The watch thread's body: open the store, catch up, register the debounced
/// watches, then serve bursts until asked to stop.
fn watch_loop(
    database: &Path,
    git_status: &GitStatusHandle,
    stop: &AtomicBool,
    signals: &std::sync::mpsc::Sender<WatchSignal>,
    inbox: std::sync::mpsc::Receiver<WatchSignal>,
    mut on_change: impl FnMut(),
) -> Result<(), IndexError> {
    let store = Store::open(database)?;

    let roots = status_roots(&store)?;

    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(DEBOUNCE_WINDOW_MS),
        None,
        burst_callback(signals.clone()),
    )?;

    let mut watched: u32 = 0;

    for (_, root) in &roots {
        let root = Path::new(root);

        if root.is_dir() {
            debouncer.watch(root, RecursiveMode::Recursive)?;
            watched += 1;
        }
    }

    assert!(watched as usize <= roots.len(), "watched no more roots than projects");

    if watched == 0 {
        return Ok(());
    }

    // The catch-up runs *after* the watches are registered, not before. A large
    // project's initial index takes seconds, and anything that changed during
    // that window would otherwise be missed outright: the events would fire
    // before there was a watcher to receive them, and the catch-up that follows
    // would already have read the tree. Registering first means those events
    // queue up and the first served burst absorbs them.
    git_status::refresh(git_status, &roots);

    // The catch-up is always a full walk: nothing told this process what changed
    // while it was not running, so only looking everywhere can find it. A
    // catch-up that fails is owed to the burst loop, which pays it on its first
    // idle tick.
    let caught_up = run_refresh(
        &store,
        RefreshScope::Everything,
        "caught up with on-disk changes before serving",
        &mut on_change,
    );

    let watch = WatchContext { store: &store, git_status, stop, roots: &roots };

    serve_bursts(
        &watch,
        &inbox,
        &mut debouncer,
        &mut on_change,
        Owed::Nothing.after(RefreshScope::Everything, caught_up),
    );

    Ok(())
}

/// The state the burst loop reads for the life of a watch and never replaces:
/// the database, the working-tree snapshot, the stop flag, and the roots being
/// watched. Grouped so the loop's own arguments are the ones that change.
#[derive(Clone, Copy)]
struct WatchContext<'watch> {
    store: &'watch Store,
    git_status: &'watch GitStatusHandle,
    stop: &'watch AtomicBool,
    roots: &'watch [(String, String)],
}

/// The burst-serving loop. Blocks on the inbox with an idle tick so the
/// git-status snapshot still ages out when nothing changes, coalesces every
/// signal already queued behind the first, re-registers newly created
/// directories, then re-indexes once.
fn serve_bursts<W: notify::Watcher, C: notify_debouncer_full::FileIdCache>(
    watch: &WatchContext<'_>,
    inbox: &std::sync::mpsc::Receiver<WatchSignal>,
    debouncer: &mut notify_debouncer_full::Debouncer<W, C>,
    on_change: &mut impl FnMut(),
    mut owed: Owed,
) {
    let WatchContext { store, git_status, stop, roots } = *watch;

    while !stop.load(Ordering::SeqCst) {
        let first = match inbox.recv_timeout(WATCH_IDLE_TICK) {
            Ok(WatchSignal::Shutdown) => return,
            Ok(WatchSignal::Burst(paths)) => paths,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                git_status::refresh_if_expired(git_status, roots);

                // Pay off a failed refresh even when nothing else happens. A
                // change dropped by a busy database must not wait for the next
                // edit to be noticed, because the next edit may never come.
                if owed == Owed::FullWalk {
                    let completed = run_refresh(
                        store,
                        RefreshScope::Everything,
                        "recovered from a failed re-index",
                        on_change,
                    );

                    owed = owed.after(RefreshScope::Everything, completed);
                }

                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };

        let burst = coalesce_burst(inbox, first);

        if burst.shutdown {
            return;
        }

        register_new_directories(debouncer, &burst.paths);

        let scope = owed.scope(burst_scope(&burst));

        // The git-status refresh runs before the re-index so a query arriving
        // during a long re-index already sees the new working-tree state.
        git_status::refresh(git_status, roots);

        let completed = run_refresh(store, scope, "re-indexed after a change", on_change);

        owed = owed.after(scope, completed);
    }
}

/// A coalesced burst: every path from the signals already queued behind the
/// first, or an empty path list with `overflowed` set when the accumulated count
/// passed [`PENDING_EVENTS_MAX`] and a whole-constellation refresh is cheaper
/// than tracking them.
struct Burst {
    overflowed: bool,
    paths: Vec<PathBuf>,
    shutdown: bool,
}

/// The signals already queued drained into one burst, so a rewrite storm
/// (`git checkout`, a rebase, a formatter pass) collapses into a single refresh
/// rather than one per file.
fn coalesce_burst(
    inbox: &std::sync::mpsc::Receiver<WatchSignal>,
    first: Vec<PathBuf>,
) -> Burst {
    let mut burst = Burst { overflowed: false, paths: first, shutdown: false };
    let mut drained: u32 = 0;

    while let Ok(signal) = inbox.recv_timeout(DEBOUNCE) {
        drained += 1;

        assert!(drained <= DEBOUNCE_EVENTS_MAX, "coalescing drained over {DEBOUNCE_EVENTS_MAX}");

        match signal {
            WatchSignal::Shutdown => {
                burst.shutdown = true;

                return burst;
            }
            WatchSignal::Burst(paths) => {
                if burst.paths.len().saturating_add(paths.len()) > PENDING_EVENTS_MAX {
                    if !burst.overflowed {
                        eprintln!(
                            "constellation: over {PENDING_EVENTS_MAX} paths changed at once; \
                             collapsing to one full refresh",
                        );
                    }

                    burst.overflowed = true;
                    burst.paths.clear();

                    continue;
                }

                burst.paths.extend(paths);
            }
        }
    }

    assert!(
        burst.paths.len() <= PENDING_EVENTS_MAX,
        "a coalesced burst respects its path cap",
    );

    burst
}

/// The directories created in this burst re-registered with the watcher.
///
/// A recursive watch does not uniformly pick up directories created after
/// registration (the behaviour differs by platform and by how the directory tree
/// was created), so a package added mid-session would otherwise stay invisible
/// until the next restart. Bounded per burst, because a checkout can create
/// thousands of directories and re-registering each is pointless once the burst
/// has already collapsed to a full refresh.
fn register_new_directories<W: notify::Watcher, C: notify_debouncer_full::FileIdCache>(
    debouncer: &mut notify_debouncer_full::Debouncer<W, C>,
    paths: &[PathBuf],
) {
    let mut registered: u32 = 0;

    for path in paths {
        if registered >= WATCH_REREGISTER_MAX {
            return;
        }

        if !path.is_dir() || is_ignored_path(path) {
            continue;
        }

        if debouncer.watch(path, RecursiveMode::Recursive).is_ok() {
            registered += 1;
        }
    }

    assert!(registered <= WATCH_REREGISTER_MAX, "re-registration respects its per-burst cap");
}

/// A contained refresh: a panic inside indexing is caught so the watcher
/// survives it, and `on_change` fires only when the graph actually changed.
///
/// Returns whether the refresh completed. A failed one leaves the store behind
/// disk by exactly the changes it was asked to apply, and a path-scoped watcher
/// has no way to rediscover them: the events naming those files are spent. The
/// caller owes a full walk, which is what [`Owed`] tracks.
#[must_use]
fn run_refresh(
    store: &Store,
    scope: RefreshScope<'_>,
    message: &str,
    on_change: &mut impl FnMut(),
) -> bool {
    match catch_unwind(AssertUnwindSafe(|| refresh_scoped(store, scope))) {
        Ok(Ok(true)) => {
            eprintln!("constellation: {message}");
            on_change();

            true
        }
        Ok(Ok(false)) => true,
        Ok(Err(error)) => {
            eprintln!("constellation: re-index failed: {error}");

            false
        }
        Err(_) => {
            eprintln!("constellation: re-index panicked; skipped this change");

            false
        }
    }
}

/// Whether the watcher still owes the store a full walk.
///
/// A refresh can fail for reasons that have nothing to do with the change that
/// triggered it (a database busy past its timeout, a transient read error, a
/// panic in one file's extractor). Before path scoping this healed itself: every
/// burst re-walked every project, so the next one picked up whatever the last
/// had dropped. Scoping removed that accident, so the debt is now tracked on
/// purpose, and paid at the first opportunity: the next burst, or the idle tick
/// if no burst comes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owed {
    Nothing,
    FullWalk,
}

impl Owed {
    /// The scope to actually run, given what this burst asked for.
    fn scope<'paths>(self, asked: RefreshScope<'paths>) -> RefreshScope<'paths> {
        match self {
            Owed::Nothing => asked,
            Owed::FullWalk => RefreshScope::Everything,
        }
    }

    /// The debt after a refresh at `scope` either completed or did not.
    fn after(self, scope: RefreshScope<'_>, completed: bool) -> Self {
        if !completed {
            return Owed::FullWalk;
        }

        match scope {
            RefreshScope::Everything => Owed::Nothing,
            RefreshScope::Paths(_) => self,
        }
    }
}

/// The `(project id, root path)` pairs the git-status worker polls.
fn status_roots(store: &Store) -> Result<Vec<(String, String)>, IndexError> {
    let projects = store.all_projects()?;

    Ok(projects
        .into_iter()
        .map(|project| (project.id.as_str().to_string(), project.root_path))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use notify::EventKind;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };

    use super::{Burst, Owed, RefreshScope, burst_scope, is_content_change, is_scopable};

    /// A burst carrying the given paths, as `coalesce_burst` would build it.
    fn burst(paths: &[PathBuf]) -> Burst {
        Burst { overflowed: false, paths: paths.to_vec(), shutdown: false }
    }

    #[test]
    fn a_vanished_source_file_describes_its_own_removal() {
        let directory = tempfile::tempdir().unwrap();
        let gone = directory.path().join("app").join("views.py");

        assert!(!gone.exists(), "the path is deliberately absent");
        assert!(is_scopable(&gone), "its rows are keyed by exactly this path");
    }

    #[test]
    fn a_vanished_directory_forces_the_full_walk() {
        let directory = tempfile::tempdir().unwrap();
        let gone = directory.path().join("app");

        assert!(!gone.exists(), "the path is deliberately absent");

        assert!(
            !is_scopable(&gone),
            "nothing in the burst names the files that lived under it, so only a walk finds them",
        );
    }

    #[test]
    fn an_existing_file_is_scopable() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("views.py");

        std::fs::write(&file, "def index():\n    pass\n").unwrap();

        assert!(is_scopable(&file), "a file that exists is re-read from its own path");
    }

    #[test]
    fn an_existing_directory_forces_the_full_walk() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("app");

        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join("views.py"), "def index():\n    pass\n").unwrap();

        // The file written above may never produce an event of its own: a
        // recursive watch does not cover a directory created after
        // registration until it is registered, and a package written in one
        // shot fits entirely inside that gap. Scoping to the directory path
        // would index nothing and report success.
        assert!(
            !is_scopable(&package),
            "a new package's contents are exactly what no event names",
        );
    }

    #[test]
    fn a_burst_widens_to_everything_when_any_path_cannot_describe_itself() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("views.py");
        let vanished_directory = directory.path().join("removed_app");

        std::fs::write(&file, "def index():\n    pass\n").unwrap();

        assert!(
            matches!(burst_scope(&burst(std::slice::from_ref(&file))), RefreshScope::Paths(_)),
            "a burst of plain files is served from its paths",
        );

        assert!(
            matches!(
                burst_scope(&burst(&[file, vanished_directory])),
                RefreshScope::Everything,
            ),
            "one path that cannot describe itself widens the whole burst",
        );
    }

    #[test]
    fn a_failed_refresh_owes_a_full_walk_until_one_completes() {
        let paths = RefreshScope::Paths(&[]);

        // A scoped refresh that fails leaves the store behind by exactly the
        // files it was asked for, and their events are spent.
        let owed = Owed::Nothing.after(paths, false);

        assert_eq!(owed, Owed::FullWalk, "a failure is a debt");

        assert!(
            matches!(owed.scope(paths), RefreshScope::Everything),
            "and the debt overrides what the next burst asked for",
        );

        // Another scoped refresh cannot pay it off, even a successful one: it
        // looked only at its own paths.
        assert_eq!(owed.after(paths, true), Owed::FullWalk, "a scoped pass does not settle it");

        assert_eq!(
            owed.after(RefreshScope::Everything, true),
            Owed::Nothing,
            "only a completed walk does",
        );

        assert_eq!(
            owed.after(RefreshScope::Everything, false),
            Owed::FullWalk,
            "and a walk that itself fails keeps the debt",
        );
    }

    #[test]
    fn a_clean_watcher_owes_nothing_and_scopes_what_it_is_given() {
        let paths = RefreshScope::Paths(&[]);

        assert!(
            matches!(Owed::Nothing.scope(paths), RefreshScope::Paths(_)),
            "with no debt the burst decides its own scope",
        );

        assert_eq!(
            Owed::Nothing.after(paths, true),
            Owed::Nothing,
            "and a successful scoped refresh incurs none",
        );
    }

    #[test]
    fn an_overflowed_burst_always_walks() {
        let overflowed = Burst { overflowed: true, paths: Vec::new(), shutdown: false };

        assert!(
            matches!(burst_scope(&overflowed), RefreshScope::Everything),
            "overflow dropped the paths, so there is nothing left to scope to",
        );
    }

    #[test]
    fn a_read_is_never_mistaken_for_a_change() {
        let reads = [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
        ];

        for kind in reads {
            assert!(
                !is_content_change(&kind),
                "a re-index reads every file, so acting on {kind:?} never terminates",
            );
        }
    }

    #[test]
    fn every_kind_of_write_is_a_change() {
        let writes = [
            EventKind::Create(CreateKind::File),
            EventKind::Create(CreateKind::Folder),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Remove(RemoveKind::Folder),
            EventKind::Any,
        ];

        for kind in writes {
            assert!(is_content_change(&kind), "{kind:?} changes the tree and must re-index");
        }
    }
}
