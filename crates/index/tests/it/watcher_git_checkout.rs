//! Switching branches under a live watcher.
//!
//! `git checkout` is the rewrite storm that actually happens: a thousand files
//! appear, disappear, and change content in one burst, with no per-file event
//! ordering worth relying on. Convergence must hold in both directions, because
//! a leak that only shows up switching back is a leak that survives a whole
//! working day.
//!
//! Skipped, loudly, when git is unavailable: a silently skipped test is a test
//! that stops being run.

use std::path::Path;
use std::process::Command;

use crate::common::{Workspace, module_source};

/// The files each branch differs by.
const BRANCH_FILES: usize = 120;

#[test]
fn switching_between_two_branches_converges_in_both_directions() {
    let workspace = Workspace::new("checkout");

    if !git_available() {
        eprintln!("watcher_git_checkout: git is not on PATH; skipping");

        return;
    }

    if !init_repository(workspace.root()) {
        eprintln!("watcher_git_checkout: could not initialize a repository; skipping");

        return;
    }

    for index in 0..BRANCH_FILES {
        workspace.write(&format!("main_side/module{index}.py"), &module_source(&format!("Main{index}")));
    }

    commit(workspace.root(), "main side");

    if !git(workspace.root(), &["checkout", "-b", "feature"]) {
        eprintln!("watcher_git_checkout: could not branch; skipping");

        return;
    }

    for index in 0..BRANCH_FILES {
        workspace.remove(&format!("main_side/module{index}.py"));
        workspace.write(&format!("feature_side/module{index}.py"), &module_source(&format!("Feature{index}")));
    }

    commit(workspace.root(), "feature side");

    workspace.index();

    let _handle = workspace.watch();

    // Back to the original branch: a thousand-file rewrite in one burst.
    assert!(git(workspace.root(), &["checkout", "-"]), "checking out the original branch");

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "the checkout back converges");

    assert!(
        observed.paths().iter().all(|path| !path.starts_with("feature_side/")),
        "no file from the other branch leaked: {:?}",
        &observed.paths()[..observed.paths().len().min(8)],
    );

    // And forward again, which is where a one-directional bug shows up.
    assert!(git(workspace.root(), &["checkout", "feature"]), "checking out the feature branch");

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "the checkout forward converges too");

    assert!(
        observed.paths().iter().all(|path| !path.starts_with("main_side/")),
        "no file from the original branch leaked: {:?}",
        &observed.paths()[..observed.paths().len().min(8)],
    );
}

/// Whether git can be invoked at all.
fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|output| output.status.success())
}

/// A repository initialized at `root` with an identity, so committing works on
/// a machine with no global git configuration.
fn init_repository(root: &Path) -> bool {
    git(root, &["init", "--initial-branch=main"])
        && git(root, &["config", "user.email", "eval@example.invalid"])
        && git(root, &["config", "user.name", "constellation tests"])
}

/// The working tree staged and committed, ignoring an empty commit.
fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", message, "--no-gpg-sign"]);
}

/// A git invocation, returning whether it succeeded.
fn git(root: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}
