//! The shared fixture and convergence oracle for the watcher tests.
//!
//! The oracle is the whole point of this suite, so it is defined once here
//! rather than approximated per test: **a watched store must end up holding
//! exactly what a from-scratch index of the final tree would hold.** Everything
//! else (renames, deletes, storms, new directories) is a way of stressing that
//! one invariant.
//!
//! Comparing `(file set, node count per file, edge count)` rather than raw row
//! dumps keeps the oracle stable against extractor changes: a change that adds a
//! node kind moves both sides equally, while a change that leaks or drops a
//! file moves only one.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use constellation_graph::ProjectId;
use constellation_index::{WatchHandle, index_project, watch_constellation};
use constellation_store::Store;

/// The time a convergence wait allows before failing. Generous: a debounced
/// burst plus a re-index plus a slow CI filesystem is still far inside this,
/// and a flaky timeout is worse than a slow test.
pub const CONVERGE_TIMEOUT: Duration = Duration::from_secs(60);

/// The interval at which a convergence wait re-checks.
pub const CONVERGE_POLL: Duration = Duration::from_millis(100);

/// The consecutive unchanged polls that mean the store has settled rather than
/// merely paused. A re-index writes in one transaction, so the store is either
/// mid-burst or done; this is comfortably longer than the gap between a
/// debounced burst and the write it produces.
pub const CONVERGE_SETTLED_POLLS: u32 = 20;

/// The fail-fast bound on convergence polls, derived from the timeout and the
/// poll interval so it cannot drift out of step with them.
pub const CONVERGE_POLLS_MAX: u32 =
    (CONVERGE_TIMEOUT.as_millis() / CONVERGE_POLL.as_millis()) as u32 + 64;

/// A temporary indexed project, its database, and the paths to mutate.
pub struct Workspace {
    pub database: PathBuf,
    pub project: ProjectId,
    directory: tempfile::TempDir,
}

impl Workspace {
    /// A fresh workspace holding one trivial module, indexed once so the store
    /// has a project row for the watcher to pick up.
    pub fn new(name: &str) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path().to_path_buf();

        std::fs::create_dir_all(root.join(".constellation")).expect("the index directory");

        let workspace = Self {
            database: root.join(".constellation").join("index.db"),
            project: ProjectId::new(name),
            directory,
        };

        workspace.write("seed.py", "def seed():\n    return 1\n");
        workspace.index();

        workspace
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        self.directory.path()
    }

    /// The absolute path of a project-relative file.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    /// A file written, creating parent directories as needed.
    pub fn write(&self, relative: &str, source: &str) {
        let path = self.path(relative);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory");
        }

        std::fs::write(&path, source).expect("writing a source file");
    }

    /// A file removed, ignoring an already-absent path.
    pub fn remove(&self, relative: &str) {
        let _ = std::fs::remove_file(self.path(relative));
    }

    /// A directory removed with everything under it.
    pub fn remove_directory(&self, relative: &str) {
        let _ = std::fs::remove_dir_all(self.path(relative));
    }

    /// A file renamed.
    pub fn rename(&self, from: &str, to: &str) {
        let target = self.path(to);

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory");
        }

        let _ = std::fs::rename(self.path(from), target);
    }

    /// A read handle on the watched database.
    pub fn store(&self) -> Store {
        Store::open(&self.database).expect("opening the store")
    }

    /// The workspace indexed once, synchronously.
    pub fn index(&self) {
        let store = self.store();
        let name = self.project.as_str().to_string();

        index_project(&store, &self.project, &name, self.root()).expect("indexing the workspace");
    }

    /// The watcher started against this workspace, with a no-op change hook.
    pub fn watch(&self) -> WatchHandle {
        watch_constellation(&self.database, || {}).expect("starting the watcher")
    }

    /// The watcher started with a change counter, for tests that assert how many
    /// refreshes a burst produced.
    pub fn watch_counting(&self, counter: std::sync::Arc<std::sync::atomic::AtomicU32>) -> WatchHandle {
        watch_constellation(&self.database, move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("starting the watcher")
    }

    /// The state a from-scratch index of the current tree would produce: the
    /// oracle every convergence assertion compares against.
    pub fn oracle(&self) -> Snapshot {
        let store = Store::open_in_memory().expect("an in-memory oracle store");
        let name = self.project.as_str().to_string();

        index_project(&store, &self.project, &name, self.root()).expect("indexing the oracle");

        snapshot(&store, &self.project)
    }

    /// The state the watched store currently holds.
    pub fn observed(&self) -> Snapshot {
        snapshot(&self.store(), &self.project)
    }

    /// The wait until the watched store matches the oracle, the store stops
    /// changing, or the timeout elapses.
    ///
    /// The three outcomes are kept apart deliberately. Returning only "did it
    /// match" made a slow machine and a broken watcher produce the same failure:
    /// a snapshot diff, printed as though the watcher had converged on the wrong
    /// answer. Since this suite exists to hold one invariant, a failure that
    /// misreports *which* invariant broke is worse than no failure at all, and
    /// it is what taught a reader to re-run the suite instead of reading it.
    ///
    /// The discriminator is whether the store had settled by the deadline. A
    /// genuine divergence reaches a wrong answer and stays there; a run that is
    /// merely slow is still moving when time runs out.
    ///
    /// Settling never *shortens* the wait, only labels the failure at the end of
    /// it. A re-index commits in one transaction, so a store can sit unchanged
    /// for seconds while the watcher is very much still working; failing early
    /// on that stillness would turn a slow machine into a confident, wrong
    /// report of divergence, which is the failure this whole method exists to
    /// stop making.
    pub fn wait_for_convergence(&self) -> Convergence {
        let started = Instant::now();
        let deadline = started + CONVERGE_TIMEOUT;

        let mut polls: u32 = 0;
        let mut changes: u32 = 0;
        let mut settled: u32 = 0;
        let mut previous: Option<Snapshot> = None;

        loop {
            polls += 1;

            assert!(polls < CONVERGE_POLLS_MAX, "the convergence poll loop stays bounded");

            let expected = self.oracle();
            let observed = self.observed();

            if observed == expected {
                return Convergence { observed, expected, outcome: Outcome::Converged };
            }

            if previous.as_ref() == Some(&observed) {
                settled += 1;
            } else {
                settled = 0;
                changes += 1;
                previous = Some(observed.clone());
            }

            if Instant::now() >= deadline {
                let outcome = if settled >= CONVERGE_SETTLED_POLLS {
                    Outcome::Diverged { settled_for: settled }
                } else {
                    Outcome::TimedOut { elapsed: started.elapsed(), changes }
                };

                return Convergence { observed, expected, outcome };
            }

            std::thread::sleep(CONVERGE_POLL);
        }
    }
}

/// The way a convergence wait ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The store matched the oracle.
    Converged,
    /// The store stopped changing and still does not match. The watcher reached
    /// a wrong answer and stayed there, which is the failure this suite is for.
    Diverged { settled_for: u32 },
    /// The store was still changing when the deadline passed. The machine was
    /// too slow or too loaded for the budget, which says nothing about the
    /// watcher's correctness.
    TimedOut { elapsed: Duration, changes: u32 },
}

/// The result of one convergence wait: what the store held, what a from-scratch
/// index would hold, and which of the two questions the answer settles.
#[derive(Clone, Debug)]
pub struct Convergence {
    pub observed: Snapshot,
    pub expected: Snapshot,
    pub outcome: Outcome,
}

impl Convergence {
    /// Whether the store matched a from-scratch index.
    pub fn is_converged(&self) -> bool {
        self.outcome == Outcome::Converged
    }

    /// The failure, spelled out: which kind it is, and the difference behind it.
    pub fn describe(&self) -> String {
        match &self.outcome {
            Outcome::Converged => "the store matches a from-scratch index".to_string(),
            Outcome::Diverged { settled_for } => format!(
                "DIVERGED: the store settled for {settled_for} polls on a state that a \
                 from-scratch index does not produce.\n  observed: {:?}\n  expected: {:?}",
                self.observed, self.expected,
            ),
            Outcome::TimedOut { elapsed, changes } => format!(
                "TIMED OUT after {elapsed:?} with the store still changing ({changes} distinct \
                 states seen). This is a slow or loaded machine, not necessarily a watcher bug; \
                 raise CONVERGE_TIMEOUT or run this suite with less contention before reading \
                 the diff below.\n  observed: {:?}\n  expected: {:?}",
                self.observed, self.expected,
            ),
        }
    }

    /// The observed and expected snapshots, after failing the test when the
    /// store did not converge.
    pub fn require(self) -> (Snapshot, Snapshot) {
        assert!(self.is_converged(), "{}", self.describe());

        (self.observed, self.expected)
    }
}

/// The comparable state of one project's graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The total edge count across the constellation.
    pub edges: u32,
    /// The path of each indexed file mapped to its symbol count, ordered so two
    /// snapshots compare and print stably.
    pub files: BTreeMap<String, i64>,
}

impl Snapshot {
    /// The file paths this snapshot holds.
    pub fn paths(&self) -> Vec<&str> {
        self.files.keys().map(String::as_str).collect()
    }
}

/// A project's snapshot read off a store.
pub fn snapshot(store: &Store, project: &ProjectId) -> Snapshot {
    let files = store
        .files_for(project)
        .expect("reading files")
        .into_iter()
        .map(|file| (file.path, file.node_count))
        .collect();

    Snapshot { edges: store.count_edges().expect("counting edges"), files }
}

/// A small but real Django-shaped module, so the graph under test has edges to
/// lose rather than isolated nodes.
pub fn module_source(name: &str) -> String {
    format!(
        "from django.db import models\n\n\n\
         class {name}(models.Model):\n\
         \x20   title = models.CharField(max_length=200)\n\n\
         \x20   def describe(self):\n\
         \x20       return self.title\n\n\n\
         def build_{lower}():\n\
         \x20   return {name}()\n",
        lower = name.to_lowercase(),
    )
}
