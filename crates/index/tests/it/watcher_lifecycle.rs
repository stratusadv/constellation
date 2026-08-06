//! The watcher's lifecycle: nothing indexes after the handle is gone.
//!
//! A long-running MCP server must be able to put the watcher down
//! deterministically. Left to a detached thread, shutdown races the database
//! close and the process can exit with a re-index still writing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::common::{Workspace, module_source};

/// A stop must not outlive a generous bound, or it is a hang rather than a join.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn stop_joins_the_watch_thread() {
    let workspace = Workspace::new("lifecycle");
    let mut handle = workspace.watch();

    workspace.write("app/models.py", &module_source("Article"));
    workspace.wait_for_convergence().require();

    let started = Instant::now();
    handle.stop();

    assert!(started.elapsed() < STOP_TIMEOUT, "stop joined rather than hanging");
    assert!(handle.is_stopping(), "the handle reports itself stopped");
}

#[test]
fn a_second_stop_is_a_no_op() {
    let workspace = Workspace::new("double-stop");
    let mut handle = workspace.watch();

    handle.stop();
    handle.stop();
    handle.stop();

    assert!(handle.is_stopping(), "repeated stops leave it stopped, not wedged");
}

#[test]
fn dropping_the_handle_joins_the_watch_thread() {
    let workspace = Workspace::new("drop-joins");

    let started = Instant::now();

    {
        let _handle = workspace.watch();

        workspace.write("app/models.py", &module_source("Comment"));
    }

    assert!(started.elapsed() < STOP_TIMEOUT, "the drop joined rather than hanging");

    // Nothing may index after the handle is gone. Write once more and confirm
    // the store stays where the drop left it.
    let after_drop = workspace.observed();

    workspace.write("app/late.py", "def late():\n    return 1\n");
    std::thread::sleep(Duration::from_millis(1_500));

    assert_eq!(
        workspace.observed(),
        after_drop,
        "no indexing outlives the handle",
    );
}

#[test]
fn stopping_during_a_burst_leaves_a_readable_database() {
    let workspace = Workspace::new("stop-mid-burst");
    let changes = Arc::new(AtomicU32::new(0));

    let mut handle = workspace.watch_counting(Arc::clone(&changes));

    // Enough files to make a re-index take real time, then stop while it is
    // plausibly still running.
    for index in 0..80 {
        workspace.write(&format!("app/module{index}.py"), &module_source(&format!("Model{index}")));
    }

    std::thread::sleep(Duration::from_millis(600));

    let started = Instant::now();
    handle.stop();

    assert!(started.elapsed() < STOP_TIMEOUT, "stop waited for the in-flight re-index, not forever");

    // The store must still open and answer. A half-written transaction would
    // fail here rather than merely look odd.
    let store = workspace.store();

    assert!(
        store.count_nodes(&workspace.project).is_ok(),
        "the database is readable after a mid-burst stop",
    );
    assert!(store.count_edges().is_ok(), "and its edge table is intact");
}

#[test]
fn a_quiet_watcher_reports_no_changes() {
    let workspace = Workspace::new("quiet");
    let changes = Arc::new(AtomicU32::new(0));

    let mut handle = workspace.watch_counting(Arc::clone(&changes));

    std::thread::sleep(Duration::from_millis(1_500));
    handle.stop();

    assert_eq!(
        changes.load(Ordering::SeqCst),
        0,
        "an unchanged tree fires no change hook, so caches are not dropped for nothing",
    );
}
