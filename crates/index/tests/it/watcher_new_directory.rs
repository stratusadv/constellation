//! Directories created after the watch starts.
//!
//! A recursive watch does not uniformly pick up directories created after
//! registration: the behaviour differs by platform and by how the tree was
//! created (one `mkdir -p` versus a level at a time). A Django app added
//! mid-session must not stay invisible until the server restarts.

use crate::common::{Workspace, module_source};

#[test]
fn a_package_created_after_the_watch_starts_is_indexed() {
    let workspace = Workspace::new("new-package");
    let _handle = workspace.watch();

    workspace.write("orders/__init__.py", "");
    workspace.write("orders/models.py", &module_source("Order"));

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "a new package converges to a from-scratch index");
    assert!(
        observed.paths().contains(&"orders/models.py"),
        "the new module is present: {:?}",
        observed.paths(),
    );
}

#[test]
fn a_deeply_nested_tree_created_in_one_shot_is_indexed() {
    let workspace = Workspace::new("nested");
    let _handle = workspace.watch();

    // Created as one `create_dir_all` plus a write, which is the case a
    // per-level watch registration misses.
    workspace.write("a/b/c/views.py", &module_source("Deep"));

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "a nested tree converges");
    assert!(
        observed.paths().contains(&"a/b/c/views.py"),
        "the deeply nested module is present: {:?}",
        observed.paths(),
    );
}

#[test]
fn a_file_added_to_a_directory_created_earlier_in_the_session_is_indexed() {
    let workspace = Workspace::new("second-wave");
    let _handle = workspace.watch();

    workspace.write("billing/models.py", &module_source("Invoice"));
    workspace.wait_for_convergence().require();

    // The second write lands in a directory that only became watchable through
    // the re-registration the first burst triggered.
    workspace.write("billing/services.py", &module_source("InvoiceService"));

    let (observed, expected) = workspace.wait_for_convergence().require();

    assert_eq!(observed, expected, "the second wave converges too");
    assert!(
        observed.paths().contains(&"billing/services.py"),
        "the later file is present: {:?}",
        observed.paths(),
    );
}
