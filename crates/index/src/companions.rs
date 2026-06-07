//! Companion-library discovery: when a Django portal is indexed, locate the
//! company packages it installs (`django-spire`, `django-glue`, `robit`) inside
//! its virtual environment and register each as its own project, so the portal's
//! imports of them resolve across a project boundary instead of dead-ending at an
//! external stub.
//!
//! A wheel install sits as real source under `.venv/Lib/site-packages/<package>/`;
//! that package directory is indexed as a standalone project rooted at itself. Its
//! file paths then read `history/mixins.py` rather than `django_spire/history/
//! mixins.py`, which still satisfies the suffix comparison in
//! [`constellation_linking::module_matches`], the evidence both the import linker
//! and external-stub unification require, so no walk-restriction or core change is
//! needed.
//!
//! Discovery is on by default and additive: a repository with no virtual
//! environment, or none of these packages installed, registers nothing and indexes
//! exactly as before. A `.constellation/config.toml` may disable it or override the
//! package list:
//!
//! ```toml
//! [companions]
//! enabled = true
//! packages = ["django-spire", "django-glue", "robit"]
//! # venv = ".venv"
//! ```

use std::path::{Path, PathBuf};

use constellation_store::Store;
use rustc_hash::FxHashSet;
use serde::Deserialize;

use crate::IndexError;

/// The companion packages registered by default when none are configured. Each is
/// a project id (hyphenated); the import package name is the id with hyphens
/// replaced by underscores (`django-spire` -> `django_spire`).
const COMPANIONS_DEFAULT: &[&str] = &["django-spire", "django-glue", "robit"];

/// The fail-fast bound on companions resolved in one discovery pass.
const COMPANION_COUNT_MAX: u32 = 64;

/// The fail-fast bound on directory entries scanned while locating a package.
const SCAN_ENTRIES_MAX: u32 = 1_000_000;

/// A companion located on disk: the project id to register it as, and the
/// package directory to index as that project's root.
#[derive(Clone, Debug)]
pub struct CompanionTarget {
    pub project_id: String,
    pub package_root: PathBuf,
}

/// The `[companions]` section of `.constellation/config.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct CompanionsConfig {
    enabled: bool,
    packages: Vec<String>,
    venv: Option<String>,
}

impl Default for CompanionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            packages: COMPANIONS_DEFAULT.iter().map(|name| name.to_string()).collect(),
            venv: None,
        }
    }
}

/// The whole config file; only its `[companions]` section is read here.
#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    companions: CompanionsConfig,
}

/// The companion packages installed under `portal_root`'s virtual
/// environment that are not already indexed, returning a target for each. Empty
/// when discovery is disabled, no virtual environment is found, or every companion
/// is already a project.
///
/// Discovery only: the caller indexes each target as its own project (so it can
/// draw progress), then runs [`crate::link_constellation`] so the portal's pending
/// imports bind to what was added.
pub fn discover_companions(
    store: &Store,
    portal_root: &Path,
) -> Result<Vec<CompanionTarget>, IndexError> {
    assert!(portal_root.is_dir(), "portal root must be a directory: {portal_root:?}");

    let config = load_config(portal_root);

    if !config.enabled {
        return Ok(Vec::new());
    }

    let roots = site_packages_roots(portal_root, &config);

    if roots.is_empty() {
        return Ok(Vec::new());
    }

    let existing = existing_project_ids(store)?;

    let mut targets: Vec<CompanionTarget> = Vec::with_capacity(config.packages.len());
    let mut count: u32 = 0;

    for id in &config.packages {
        count += 1;

        assert!(count <= COMPANION_COUNT_MAX, "companion list exceeded {COMPANION_COUNT_MAX}");

        if id.is_empty() || existing.contains(id) {
            continue;
        }

        let package = id.replace('-', "_");

        let Some(package_root) = resolve_package(&roots, &package) else {
            continue;
        };

        targets.push(CompanionTarget { project_id: id.clone(), package_root });
    }

    Ok(targets)
}

/// The `[companions]` section read from `.constellation/config.toml`. A missing file
/// yields the defaults (discovery on, the three company packages); a malformed file
/// also yields the defaults: an optional config must never fail an index.
fn load_config(portal_root: &Path) -> CompanionsConfig {
    let path = portal_root.join(".constellation").join("config.toml");

    let Ok(text) = std::fs::read_to_string(&path) else {
        return CompanionsConfig::default();
    };

    toml::from_str::<ConfigFile>(&text)
        .map(|file| file.companions)
        .unwrap_or_default()
}

/// The set of project ids already in the store, so a companion shared across
/// portals (or one a user indexed explicitly) is indexed once, not repointed.
fn existing_project_ids(store: &Store) -> Result<FxHashSet<String>, IndexError> {
    let mut ids: FxHashSet<String> = FxHashSet::default();

    for project in store.all_projects()? {
        ids.insert(project.id.as_str().to_string());
    }

    Ok(ids)
}

/// The candidate `site-packages` directories to search: the configured (or default
/// `.venv`) virtual environment, plus an active `VIRTUAL_ENV` if one is set.
fn site_packages_roots(portal_root: &Path, config: &CompanionsConfig) -> Vec<PathBuf> {
    let mut venvs: Vec<PathBuf> = Vec::new();

    match &config.venv {
        Some(venv) => {
            let path = Path::new(venv);

            venvs.push(if path.is_absolute() { path.to_path_buf() } else { portal_root.join(path) });
        }
        None => venvs.push(portal_root.join(".venv")),
    }

    if let Ok(active) = std::env::var("VIRTUAL_ENV")
        && !active.is_empty()
    {
        venvs.push(PathBuf::from(active));
    }

    let mut roots: Vec<PathBuf> = Vec::with_capacity(venvs.len());

    for venv in &venvs {
        if let Some(site_packages) = site_packages_in(venv) {
            roots.push(site_packages);
        }
    }

    roots
}

/// The `site-packages` directory inside a virtual environment: `Lib/site-packages`
/// on Windows, else the first `lib/python*/site-packages` on POSIX.
fn site_packages_in(venv: &Path) -> Option<PathBuf> {
    let windows = venv.join("Lib").join("site-packages");

    if windows.is_dir() {
        return Some(windows);
    }

    let entries = std::fs::read_dir(venv.join("lib")).ok()?;
    let mut scanned: u32 = 0;

    for entry in entries {
        scanned += 1;

        assert!(scanned <= SCAN_ENTRIES_MAX, "site-packages scan exceeded {SCAN_ENTRIES_MAX}");

        let Ok(entry) = entry else {
            continue;
        };

        let candidate = entry.path().join("site-packages");

        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    None
}

/// The on-disk directory `package` resolves to (the one holding `__init__.py`)
/// within one of the `site-packages` roots: a wheel install first, then a
/// best-effort editable install.
fn resolve_package(site_packages_roots: &[PathBuf], package: &str) -> Option<PathBuf> {
    assert!(!package.is_empty(), "package name must not be empty");

    for site_packages in site_packages_roots {
        let wheel = site_packages.join(package);

        if wheel.join("__init__.py").is_file() {
            return Some(wheel);
        }

        if let Some(editable) = resolve_editable(site_packages, package) {
            return Some(editable);
        }
    }

    None
}

/// The best-effort editable-install resolution. An editable package is registered in
/// `site-packages` by a `<dist>.egg-link` (legacy) or `__editable__.<dist>*.pth`
/// (PEP 660) file naming a directory placed on `sys.path`. Read those, and for any
/// line that is an existing directory, accept it when it actually holds the package
/// (`<dir>/<package>/__init__.py`). Finder-based PEP 660 editables (an
/// `__editable___*_finder.py` import hook) are not covered.
fn resolve_editable(site_packages: &Path, package: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(site_packages).ok()?;
    let mut scanned: u32 = 0;

    for entry in entries {
        scanned += 1;

        assert!(scanned <= SCAN_ENTRIES_MAX, "editable scan exceeded {SCAN_ENTRIES_MAX}");

        let Ok(entry) = entry else {
            continue;
        };

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        let is_pointer = file_name.ends_with(".egg-link")
            || (file_name.starts_with("__editable__") && file_name.ends_with(".pth"));

        if !is_pointer {
            continue;
        }

        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };

        for line in text.lines() {
            let line = line.trim();

            if line.is_empty() || line.starts_with("import ") {
                continue;
            }

            let candidate = Path::new(line).join(package);

            if candidate.join("__init__.py").is_file() {
                return Some(candidate);
            }
        }
    }

    None
}
