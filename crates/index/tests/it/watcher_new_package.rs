//! A package written in one shot, which is the shape the operation fuzzer keeps
//! landing on.
//!
//! Every file in these tests goes into a directory that did not exist when the
//! watch was registered. That is the case a path-scoped refresh is most likely
//! to get wrong, because a recursive watch does not cover a new directory until
//! it has been registered, and anything written into the gap produces no event
//! naming it.

use crate::common::Workspace;

#[test]
fn a_package_and_its_files_written_in_one_shot_are_all_indexed() {
    let workspace = Workspace::new("new-package");
    let _handle = workspace.watch();

    for slot in 0..6 {
        workspace.write(
            &format!("app/slot{slot}.py"),
            &crate::common::module_source(&format!("Created{slot}")),
        );
    }

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "a package written in one shot converges");

    for slot in 0..6 {
        assert!(
            observed.paths().contains(&format!("app/slot{slot}.py").as_str()),
            "slot{slot} is indexed: {:?}",
            observed.paths(),
        );
    }
}

#[test]
fn files_added_to_a_new_package_after_it_settles_are_indexed() {
    let workspace = Workspace::new("new-package-later");
    let _handle = workspace.watch();

    workspace.write("app/first.py", &crate::common::module_source("First"));
    workspace.wait_for_convergence().require();

    workspace.write("app/second.py", &crate::common::module_source("Second"));

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "a file added to a settled package converges");

    assert!(
        observed.paths().contains(&"app/second.py"),
        "the later file is indexed: {:?}",
        observed.paths(),
    );
}

#[test]
fn a_package_deleted_whole_leaves_nothing_behind() {
    let workspace = Workspace::new("package-deleted");
    let _handle = workspace.watch();

    for slot in 0..4 {
        workspace.write(
            &format!("app/slot{slot}.py"),
            &crate::common::module_source(&format!("Created{slot}")),
        );
    }

    workspace.wait_for_convergence().require();

    std::fs::remove_dir_all(workspace.root().join("app")).expect("the package is removed");

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "removing a whole package converges");

    assert!(
        observed.paths().iter().all(|path| !path.starts_with("app/")),
        "no part of the package survives: {:?}",
        observed.paths(),
    );
}
