//! Deletion, and the convergence that must follow it.
//!
//! A leaked node after a delete is the worst kind of index bug: every query
//! keeps answering with a symbol that no longer exists, and nothing about the
//! answer looks wrong.

mod delete_tests {
    use crate::common::{Workspace, module_source};

    #[test]
    fn deleting_a_file_removes_its_nodes() {
        let workspace = Workspace::new("delete-file");
        let _handle = workspace.watch();

        workspace.write("app/models.py", &module_source("Article"));
        workspace.wait_for_convergence().require();

        assert!(
            workspace.observed().paths().contains(&"app/models.py"),
            "the file was indexed before the delete",
        );

        workspace.remove("app/models.py");

        let (observed, expected) = workspace.wait_for_convergence().require();

        assert_eq!(observed, expected, "the delete converges to a from-scratch index");
        assert!(
            !observed.paths().contains(&"app/models.py"),
            "the deleted file leaves no rows behind: {:?}",
            observed.paths(),
        );
    }

    #[test]
    fn deleting_a_whole_app_removes_all_of_it() {
        let workspace = Workspace::new("delete-app");
        let _handle = workspace.watch();

        for index in 0..12 {
            workspace.write(&format!("orders/module{index}.py"), &module_source(&format!("Order{index}")));
        }

        workspace.wait_for_convergence().require();

        let before = workspace.observed();

        assert!(before.files.len() > 12, "the app and the seed are indexed, got {:?}", before.paths());

        workspace.remove_directory("orders");

        let (observed, expected) = workspace.wait_for_convergence().require();

        assert_eq!(observed, expected, "removing a whole app converges");

        assert!(
            observed.paths().iter().all(|path| !path.starts_with("orders/")),
            "no part of the app survives: {:?}",
            observed.paths(),
        );
    }

    #[test]
    fn deleting_and_recreating_within_one_window_converges_to_disk() {
        let workspace = Workspace::new("delete-recreate");
        let _handle = workspace.watch();

        workspace.write("app/models.py", &module_source("Article"));
        workspace.wait_for_convergence().require();

        // Both operations land inside one debounce window, so the watcher sees
        // one burst that ends with the file present but rewritten.
        workspace.remove("app/models.py");
        workspace.write("app/models.py", &module_source("Rewritten"));

        let (observed, expected) = workspace.wait_for_convergence().require();

        assert_eq!(
            observed, expected,
            "a delete followed by a recreate converges to what is on disk, not to either half",
        );

        assert!(
            observed.paths().contains(&"app/models.py"),
            "the recreated file is present: {:?}",
            observed.paths(),
        );
    }

    #[test]
    fn recreating_with_different_content_replaces_rather_than_merges() {
        let workspace = Workspace::new("replace-content");
        let _handle = workspace.watch();

        workspace.write("app/models.py", &module_source("First"));
        workspace.wait_for_convergence().require();

        workspace.write("app/models.py", "def only_this():\n    return 1\n");

        let (observed, expected) = workspace.wait_for_convergence().require();

        assert_eq!(observed, expected, "the rewrite converges");

        let node_count = observed.files.get("app/models.py").copied().unwrap_or(0);
        let oracle_count = expected.files.get("app/models.py").copied().unwrap_or(0);

        assert_eq!(node_count, oracle_count, "the old symbols were replaced, not merged with the new");
    }
}
