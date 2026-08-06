//! The profile boundary end to end: a workspace's `[profile]` selection decides
//! which company conventions apply to it, and the generic profile applies none.
//!
//! Companion discovery is the one company behavior visible from outside the
//! index, so it is what these tests measure: the same tree on disk, indexed under
//! two profiles, discovers a company package under one and nothing under the
//! other.

use std::path::{Path, PathBuf};

use constellation_graph::Profile;
use constellation_index::{discover_companions, load_companion_repositories, load_profile};
use constellation_store::Store;

/// A workspace root holding a `.venv` with `django_spire` installed as a wheel,
/// so companion discovery has something real to find.
fn workspace_with_a_companion(directory: &Path) -> PathBuf {
    let site_packages =
        directory.join(".venv").join("lib").join("python3.13").join("site-packages");

    let package = site_packages.join("django_spire");

    std::fs::create_dir_all(&package).expect("the site-packages tree");
    std::fs::write(package.join("__init__.py"), "").expect("the package initializer");

    std::fs::create_dir_all(directory.join(".constellation")).expect("the index directory");

    directory.to_path_buf()
}

/// The `.constellation/config.toml` of `root` written with `text`.
fn write_config(root: &Path, text: &str) {
    std::fs::write(root.join(".constellation").join("config.toml"), text)
        .expect("writing the workspace config");
}

#[test]
fn the_default_profile_discovers_the_company_companions() {
    let directory = tempfile::tempdir().unwrap();
    let root = workspace_with_a_companion(directory.path());

    let store = Store::open_in_memory().unwrap();
    let targets = discover_companions(&store, &root).expect("discovery runs");

    assert_eq!(load_profile(&root), Profile::default(), "no config file means the default profile");

    // Only django-spire is asserted: an ambient VIRTUAL_ENV is also searched, so
    // whichever other company packages the developer's own environment installs
    // resolve too, and the test must not depend on which.
    let spire = targets
        .iter()
        .find(|target| target.project_id == "django-spire")
        .expect("the default profile names django-spire, and it is installed here");

    assert!(
        spire.package_root.starts_with(&root),
        "the workspace's own .venv copy is the one located, got {:?}",
        spire.package_root,
    );

    assert!(
        load_companion_repositories(&root).contains_key("django-spire"),
        "its history repository comes from the profile with no configuration",
    );
}

#[test]
fn the_generic_profile_indexes_a_workspace_with_zero_company_behavior() {
    let directory = tempfile::tempdir().unwrap();
    let root = workspace_with_a_companion(directory.path());

    write_config(&root, "[profile]\nname = \"generic\"\n");

    let store = Store::open_in_memory().unwrap();
    let targets = discover_companions(&store, &root).expect("discovery runs");
    let profile = load_profile(&root);

    assert_eq!(profile, Profile::generic(), "the config selects the generic profile");
    assert!(profile.hook_names_extra.is_empty(), "and it adds no framework hook names");

    assert!(
        targets.is_empty(),
        "an installed company package is not a companion under a profile that never named it",
    );

    assert!(
        load_companion_repositories(&root).is_empty(),
        "and no repository is configured for one",
    );
}

#[test]
fn a_generic_workspace_can_still_name_its_own_companions() {
    let directory = tempfile::tempdir().unwrap();
    let root = workspace_with_a_companion(directory.path());

    write_config(
        &root,
        "[profile]\nname = \"generic\"\n\n[companions]\npackages = [\"django-spire\"]\n",
    );

    let store = Store::open_in_memory().unwrap();
    let targets = discover_companions(&store, &root).expect("discovery runs");

    assert_eq!(
        targets.len(),
        1,
        "the companion mechanism is generic; only the default list came from the profile",
    );

    assert_eq!(targets[0].project_id, "django-spire");
}

#[test]
fn an_unreadable_profile_name_falls_back_rather_than_failing_the_index() {
    let directory = tempfile::tempdir().unwrap();
    let root = workspace_with_a_companion(directory.path());

    write_config(&root, "[profile]\nname = \"nonesuch\"\n");

    assert_eq!(load_profile(&root), Profile::default(), "an unknown name falls back");

    write_config(&root, "this is not toml at all\n");

    assert_eq!(load_profile(&root), Profile::default(), "and so does a malformed file");
}
