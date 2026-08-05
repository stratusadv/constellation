//! The bidirectional invariant, checked after arbitrary bursts.
//!
//! Every stored `files` row must have a live on-disk counterpart, and every
//! indexable file on disk must have a stored row. Convergence to the oracle
//! implies this, but stating it separately catches the asymmetric failure the
//! oracle comparison can mask: an extractor change that moves both sides
//! equally still leaves this invariant intact, while a leak breaks only this
//! one.

use std::collections::BTreeSet;

use crate::common::{Workspace, module_source};

#[test]
fn every_stored_file_exists_on_disk_and_the_reverse() {
    let workspace = Workspace::new("consistency");
    let _handle = workspace.watch();

    for index in 0..30 {
        workspace.write(&format!("app/module{index}.py"), &module_source(&format!("Model{index}")));
    }

    workspace.wait_for_convergence().require();

    for index in 0..10 {
        workspace.remove(&format!("app/module{index}.py"));
    }

    for index in 30..40 {
        workspace.write(&format!("app/module{index}.py"), &module_source(&format!("Model{index}")));
    }

    workspace.wait_for_convergence().require();

    let stored: BTreeSet<String> = workspace.observed().files.into_keys().collect();
    let on_disk = python_files(&workspace);

    let leaked: Vec<&String> = stored.difference(&on_disk).collect();
    let missing: Vec<&String> = on_disk.difference(&stored).collect();

    assert!(leaked.is_empty(), "every stored file still exists on disk; leaked {leaked:?}");
    assert!(missing.is_empty(), "every on-disk file is stored; missing {missing:?}");
}

#[test]
fn the_invariant_survives_an_empty_tree() {
    let workspace = Workspace::new("emptied");
    let _handle = workspace.watch();

    workspace.write("app/models.py", &module_source("Article"));
    workspace.wait_for_convergence().require();

    workspace.remove_directory("app");
    workspace.remove("seed.py");

    workspace.wait_for_convergence().require();

    let stored: BTreeSet<String> = workspace.observed().files.into_keys().collect();

    assert_eq!(
        stored,
        python_files(&workspace),
        "emptying the tree empties the index, rather than leaving orphans",
    );
}

/// The indexable Python files under the workspace, project-relative, with
/// forward slashes, matching the form the store keeps.
fn python_files(workspace: &Workspace) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<std::path::PathBuf> = vec![workspace.root().to_path_buf()];
    let mut visited: u32 = 0;

    while let Some(directory) = stack.pop() {
        visited += 1;

        assert!(visited < 100_000, "the walk stays bounded");

        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with('.') || name == "__pycache__");

                if !skip {
                    stack.push(path);
                }

                continue;
            }

            if path.extension().and_then(|extension| extension.to_str()) != Some("py") {
                continue;
            }

            if let Ok(relative) = path.strip_prefix(workspace.root()) {
                found.insert(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    found
}
