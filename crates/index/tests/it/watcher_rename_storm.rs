//! A rename storm: hundreds of files moved in one burst.
//!
//! This is what a package reorganization, a `git mv` sweep, or an IDE refactor
//! looks like to a watcher. The requirement is not speed but convergence: the
//! store's file set must end up equal to the on-disk set, and the burst must
//! collapse into a small number of refreshes rather than one per file.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::common::{Workspace, module_source};

/// The storm size. Large enough to exercise coalescing, small enough that a
/// from-scratch oracle index stays quick.
const STORM_FILES: usize = 500;

/// The most refreshes a single storm may produce. One is ideal; a handful is
/// acceptable on a platform that reports a rename as several events. Anything
/// approaching the file count means coalescing is not working.
const REFRESHES_MAX: u32 = 24;

#[test]
fn renaming_five_hundred_files_converges_and_coalesces() {
    let workspace = Workspace::new("rename-storm");

    for index in 0..STORM_FILES {
        workspace.write(&format!("before/module{index}.py"), &module_source(&format!("Model{index}")));
    }

    workspace.index();

    let refreshes = Arc::new(AtomicU32::new(0));
    let _handle = workspace.watch_counting(Arc::clone(&refreshes));

    let baseline = refreshes.load(Ordering::SeqCst);

    for index in 0..STORM_FILES {
        workspace.rename(
            &format!("before/module{index}.py"),
            &format!("after/module{index}.py"),
        );
    }

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "the storm converges to a from-scratch index of the final tree");

    assert!(
        observed.paths().iter().all(|path| !path.starts_with("before/")),
        "no pre-rename path survives",
    );

    assert_eq!(
        observed.files.iter().filter(|(path, _)| path.starts_with("after/")).count(),
        STORM_FILES,
        "every renamed file is indexed at its new path",
    );

    let produced = refreshes.load(Ordering::SeqCst).saturating_sub(baseline);

    assert!(
        produced <= REFRESHES_MAX,
        "the burst coalesced into {produced} refreshes, which must stay far below {STORM_FILES}",
    );
}

#[test]
fn a_partial_rename_leaves_both_halves_correct() {
    let workspace = Workspace::new("partial-rename");

    for index in 0..40 {
        workspace.write(&format!("app/module{index}.py"), &module_source(&format!("Model{index}")));
    }

    workspace.index();

    let _handle = workspace.watch();

    for index in 0..20 {
        workspace.rename(&format!("app/module{index}.py"), &format!("moved/module{index}.py"));
    }

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "a partial rename converges");

    assert_eq!(
        observed.files.iter().filter(|(path, _)| path.starts_with("moved/")).count(),
        20,
        "the moved half is at its new paths",
    );

    assert_eq!(
        observed.files.iter().filter(|(path, _)| path.starts_with("app/")).count(),
        20,
        "the untouched half stayed where it was",
    );
}
