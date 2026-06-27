//! Companion-library discovery: when a Django workspace is indexed, locate the
//! company packages it installs (`django-spire`, `django-glue`, `robit`, `dandy`) inside
//! its virtual environment and register each as its own project, so the workspace's
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
//! exactly as before. A `.constellation/config.toml` may disable it, override the
//! package list, and name extra versions to index side by side for comparison:
//!
//! ```toml
//! [companions]
//! enabled = true
//! packages = ["django-spire", "django-glue", "robit", "dandy"]
//! # venv = ".venv"
//!
//! # Each package -> ref entry indexes that git ref of a companion as its own
//! # read-only project (id "django-spire@refactor/next"), rooted like the `.venv`
//! # copy so only the ref suffix differs. The repository is the companion's
//! # configured `repositories` url, or its editable/VCS install.
//! versions = { django-spire = "refactor/next", django-glue = "v1.2.0" }
//! ```
//!
//! A local working copy takes precedence over the `.venv`: if the workspace's
//! `pyproject.toml` pins a package to a path under `[tool.uv.sources]`, or a
//! `development.env`/`.env` sets `PYTHONPATH_APPEND` to a directory holding it,
//! that directory is indexed (and is the repository version refs are taken from)
//! in place of the installed copy, because it is what actually runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use constellation_store::Store;
use rustc_hash::FxHashSet;
use serde::Deserialize;

use crate::IndexError;

/// The companion packages registered by default when none are configured. Each is
/// a project id (hyphenated); the import package name is the id with hyphens
/// replaced by underscores (`django-spire` -> `django_spire`).
const COMPANIONS_DEFAULT: &[&str] = &["django-spire", "django-glue", "robit", "dandy"];

/// The default git repository for each default companion, used to fetch its
/// history (at the tag matching the installed version) when `[companions]
/// repositories` is not set, so companion history works with no configuration.
/// The config key overrides this.
const COMPANION_REPOSITORIES_DEFAULT: &[(&str, &str)] = &[
    ("django-spire", "https://github.com/stratusadv/django-spire"),
    ("django-glue", "https://github.com/stratusadv/django-glue"),
    ("robit", "https://github.com/stratusadv/robit"),
    ("dandy", "https://github.com/stratusadv/dandy"),
];

/// The fail-fast bound on companions resolved in one discovery pass.
const COMPANION_COUNT_MAX: u32 = 64;

/// The fail-fast bound on directory entries scanned while locating a package.
const SCAN_ENTRIES_MAX: u32 = 1_000_000;

/// The fail-fast bound on a sanitized directory segment's length: one path
/// component on Windows and most filesystems.
const SEGMENT_LEN_MAX: u32 = 255;

/// The fail-fast bound on ancestor directories walked while locating a git root.
const GIT_ROOT_DEPTH_MAX: u32 = 6;

/// The fail-fast bound on override candidate directories considered per package.
const OVERRIDE_PATHS_MAX: u32 = 4_096;

/// A companion located on disk: the project id to register it as, and the
/// package directory to index as that project's root.
#[derive(Clone, Debug)]
pub struct CompanionTarget {
    pub project_id: String,
    pub package_root: PathBuf,
    /// Whether this target indexes as a reference-only project, excluded from
    /// cross-project link targets. False for a `.venv` companion (the canonical
    /// version), true for a version copy taken at another ref.
    pub reference_only: bool,
}

/// The `[companions]` section of `.constellation/config.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct CompanionsConfig {
    enabled: bool,
    packages: Vec<String>,
    exclude: Vec<String>,
    repositories: BTreeMap<String, String>,
    venv: Option<String>,
    versions: BTreeMap<String, String>,
}

impl Default for CompanionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            packages: COMPANIONS_DEFAULT.iter().map(|name| name.to_string()).collect(),
            exclude: Vec::new(),
            repositories: COMPANION_REPOSITORIES_DEFAULT
                .iter()
                .map(|(package, url)| (package.to_string(), url.to_string()))
                .collect(),
            venv: None,
            versions: BTreeMap::new(),
        }
    }
}

/// The `[history]` section of `.constellation/config.toml`: the knobs
/// `constellation history` reads. The defaults preserve the prior behavior
/// (enabled, the same commit cap, companions included), so a workspace without the
/// section is unaffected.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// Whether `constellation history` indexes this workspace at all.
    pub enabled: bool,
    /// Whether to record per-symbol changes (the Tier-2 pass); on by default. The
    /// `--symbols` flag forces it on for a single run even when this is set false.
    pub symbols: bool,
    /// Whether the companion/library projects get their history indexed too, not
    /// just this workspace's own code. Their histories are large, so set false to
    /// index only your own.
    pub companions: bool,
    /// The per-repository commit cap for the history read.
    pub commits_max: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            symbols: true,
            companions: true,
            commits_max: crate::history::HISTORY_COMMITS_MAX,
        }
    }
}

/// The whole config file: its `[companions]` and `[history]` sections.
#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    companions: CompanionsConfig,
    #[serde(default)]
    history: HistoryConfig,
}

/// The companion packages a workspace uses, located for indexing and returned as a
/// target each. A local override (a pyproject `[tool.uv.sources]` path, or a
/// `PYTHONPATH_APPEND` directory) is preferred over the `.venv` copy, because it
/// is what actually runs. Empty when discovery is disabled, nothing resolves, or
/// every companion is already a project.
///
/// Discovery only: the caller indexes each target as its own project (so it can
/// draw progress), then runs [`crate::link_constellation`] so the workspace's pending
/// imports bind to what was added.
pub fn discover_companions(
    store: &Store,
    workspace_root: &Path,
) -> Result<Vec<CompanionTarget>, IndexError> {
    assert!(workspace_root.is_dir(), "workspace root must be a directory: {workspace_root:?}");

    let config = load_config(workspace_root);

    if !config.enabled {
        return Ok(Vec::new());
    }

    let roots = site_packages_roots(workspace_root, &config);
    let existing = existing_project_ids(store)?;

    let mut targets: Vec<CompanionTarget> = Vec::with_capacity(config.packages.len());
    let mut count: u32 = 0;

    for id in &config.packages {
        count += 1;

        assert!(count <= COMPANION_COUNT_MAX, "companion list exceeded {COMPANION_COUNT_MAX}");

        if id.is_empty() || existing.contains(id) || config.exclude.contains(id) {
            continue;
        }

        // A local override (pyproject `[tool.uv.sources]` or a `PYTHONPATH_APPEND`)
        // is what actually runs, so it is indexed in preference to the `.venv` copy.
        let package_root = resolve_override(workspace_root, id)
            .map(|dir| package_subdir(&dir, id))
            .or_else(|| resolve_package(&roots, &id.replace('-', "_")));

        let Some(package_root) = package_root else {
            continue;
        };

        targets.push(CompanionTarget { project_id: id.clone(), package_root, reference_only: false });
    }

    Ok(targets)
}

/// The `[companions]` section read from `.constellation/config.toml`. A missing file
/// yields the defaults (discovery on, the four company packages); a malformed file
/// also yields the defaults: an optional config must never fail an index.
fn load_config(workspace_root: &Path) -> CompanionsConfig {
    read_config_file(workspace_root).companions
}

/// The `[history]` configuration for the workspace at `workspace_root`, or the defaults
/// when the config file is absent or omits the section.
pub fn load_history_config(workspace_root: &Path) -> HistoryConfig {
    read_config_file(workspace_root).history
}

/// The parsed `.constellation/config.toml`, or the defaults when it is absent or
/// does not parse (a malformed file falls back to defaults rather than failing).
fn read_config_file(workspace_root: &Path) -> ConfigFile {
    let path = workspace_root.join(".constellation").join("config.toml");

    let Ok(text) = std::fs::read_to_string(&path) else {
        return ConfigFile::default();
    };

    toml::from_str::<ConfigFile>(&text).unwrap_or_default()
}

/// The `[companions] repositories` map (package -> git url) for the workspace: the
/// remotes companion git history is fetched from, since a `.venv` wheel records
/// none. Empty when unconfigured.
pub fn load_companion_repositories(workspace_root: &Path) -> BTreeMap<String, String> {
    read_config_file(workspace_root).companions.repositories
}

/// The full-history checkout of `package`'s repository at the installed version,
/// fetched from `url` into `.constellation/sources/` (cached after the first
/// fetch), so its git history can be read without a local clone or a `.git` in the
/// `.venv` install. `None` when the package is not installed, no tag matches the
/// installed version, or the clone fails, in which case the companion's history is
/// skipped rather than shown at a mismatched version.
pub fn fetch_companion_history_repo(
    workspace_root: &Path,
    package: &str,
    url: &str,
) -> Option<PathBuf> {
    assert!(!package.is_empty(), "package must not be empty");
    assert!(!url.is_empty(), "repository url must not be empty");

    let config = load_config(workspace_root);
    let roots = site_packages_roots(workspace_root, &config);

    let version = installed_version(&roots, package)?;

    clone_full_at_tag(workspace_root, package, url, &version)
}

/// The installed version of `package` from its `*.dist-info` directory name among
/// the site-packages `roots` (`django_spire-0.32.3` -> "0.32.3"), or `None` when
/// the package is not installed.
fn installed_version(roots: &[PathBuf], package: &str) -> Option<String> {
    let import = package.replace('-', "_");
    let dist_info = find_dist_info(roots, &import)?;

    let name = dist_info.file_name()?.to_str()?;
    let version = name.strip_suffix(".dist-info")?.rsplit_once('-')?.1;

    if version.is_empty() {
        return None;
    }

    Some(version.to_string())
}

/// A full clone of `url` at the tag matching `version` (trying `v{version}` then
/// `{version}`) under `.constellation/sources/`, returning the checkout root so its
/// whole history is readable. Reused when already present for that version, so the
/// network is hit only the first time. `None` when git is unavailable, neither tag
/// exists, or the clone fails.
fn clone_full_at_tag(
    workspace_root: &Path,
    package: &str,
    url: &str,
    version: &str,
) -> Option<PathBuf> {
    if !git_available() {
        return None;
    }

    let sources = workspace_root.join(".constellation").join("sources");

    std::fs::create_dir_all(&sources).ok()?;

    let dest = sources.join(sanitize_segment(&format!("{package}@{version}")));

    if is_populated(&dest) {
        return Some(dest);
    }

    let destination = dest.to_string_lossy().into_owned();

    for tag in [format!("v{version}"), version.to_string()] {
        if run_git(None, &["clone", "--branch", tag.as_str(), url, destination.as_str()]).is_some() {
            return Some(dest);
        }

        let _ = std::fs::remove_dir_all(&dest);
    }

    eprintln!("constellation: no tag v{version} or {version} in {url} for '{package}'; skipping its history");

    None
}

/// The set of project ids already in the store, so a companion shared across
/// workspaces (or one a user indexed explicitly) is indexed once, not repointed.
fn existing_project_ids(store: &Store) -> Result<FxHashSet<String>, IndexError> {
    let mut ids: FxHashSet<String> = FxHashSet::default();

    for project in store.all_projects()? {
        ids.insert(project.id.as_str().to_string());
    }

    Ok(ids)
}

/// The candidate `site-packages` directories to search: the configured (or default
/// `.venv`) virtual environment, plus an active `VIRTUAL_ENV` if one is set.
fn site_packages_roots(workspace_root: &Path, config: &CompanionsConfig) -> Vec<PathBuf> {
    let mut venvs: Vec<PathBuf> = Vec::new();

    match &config.venv {
        Some(venv) => {
            let path = Path::new(venv);

            venvs.push(if path.is_absolute() { path.to_path_buf() } else { workspace_root.join(path) });
        }
        None => venvs.push(workspace_root.join(".venv")),
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

/// A local source directory that overrides the `.venv` copy of `package`, from
/// the workspace's `pyproject.toml` `[tool.uv.sources]` or a `PYTHONPATH_APPEND` in
/// `development.env`/`.env`. The returned directory actually holds the package
/// (`<import>/__init__.py`, or is the package itself), so a stale entry that no
/// longer contains it does not shadow the install. `None` falls back to the
/// virtual environment.
fn resolve_override(workspace_root: &Path, package: &str) -> Option<PathBuf> {
    let import = package.replace('-', "_");
    let mut count: u32 = 0;

    for candidate in override_candidates(workspace_root, package) {
        count += 1;

        assert!(count <= OVERRIDE_PATHS_MAX, "override candidate scan exceeded {OVERRIDE_PATHS_MAX}");

        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            workspace_root.join(candidate)
        };

        if contains_package(&resolved, &import) {
            return Some(resolved);
        }
    }

    None
}

/// The candidate override directories for `package`, most specific first: a
/// `[tool.uv.sources]` path, then every `PYTHONPATH_APPEND`/`PYTHONPATH` entry.
fn override_candidates(workspace_root: &Path, package: &str) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(path) = uv_source_path(workspace_root, package) {
        candidates.push(path);
    }

    candidates.extend(pythonpath_dirs(workspace_root));

    candidates
}

/// The local path a workspace's `pyproject.toml` pins `package` to under
/// `[tool.uv.sources]` (`{ path = "..." }`), or `None` when the file, the section,
/// the package, or a `path` key is absent. A git or workspace source has no local
/// directory here and yields `None`, leaving the `.venv` copy in force.
fn uv_source_path(workspace_root: &Path, package: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(workspace_root.join("pyproject.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;

    let sources = value.get("tool")?.get("uv")?.get("sources")?;
    let underscore = package.replace('-', "_");
    let entry = sources.get(package).or_else(|| sources.get(underscore.as_str()))?;

    let path = match entry {
        toml::Value::String(path) => path.clone(),
        toml::Value::Table(table) => table.get("path")?.as_str()?.to_string(),
        _ => return None,
    };

    Some(PathBuf::from(path))
}

/// Every directory named by a `PYTHONPATH_APPEND` or `PYTHONPATH` in the workspace's
/// `development.env` or `.env`, split on the platform path separator.
fn pythonpath_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    for file in ["development.env", ".env"] {
        let Ok(text) = std::fs::read_to_string(workspace_root.join(file)) else {
            continue;
        };

        for key in ["PYTHONPATH_APPEND", "PYTHONPATH"] {
            if let Some(value) = env_value(&text, key) {
                dirs.extend(std::env::split_paths(&value));
            }
        }
    }

    dirs
}

/// The value of `key` in a `KEY=VALUE` env file, trimmed of an optional `export`
/// prefix and surrounding quotes, or `None` when the key is absent or empty.
fn env_value(text: &str, key: &str) -> Option<String> {
    let mut scanned: u32 = 0;

    for line in text.lines() {
        scanned += 1;

        assert!(scanned <= SCAN_ENTRIES_MAX, "env scan exceeded {SCAN_ENTRIES_MAX}");

        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);

        let Some((name, value)) = line.split_once('=') else {
            continue;
        };

        if name.trim() != key {
            continue;
        }

        let value = value.trim().trim_matches('"').trim_matches('\'');

        if !value.is_empty() {
            return Some(value.to_string());
        }
    }

    None
}

/// Whether `dir` holds `import` at one of the layouts pip installs: the directory
/// itself is the package, or it contains `<import>/` or `src/<import>/` with an
/// `__init__.py`.
fn contains_package(dir: &Path, import: &str) -> bool {
    dir.join("__init__.py").is_file()
        || dir.join(import).join("__init__.py").is_file()
        || dir.join("src").join(import).join("__init__.py").is_file()
}

/// A git repository to take a version from: a local clone on disk (the common
/// editable-install case, checked out cheaply via a worktree) or a remote URL to
/// clone.
#[derive(Clone, Debug)]
enum RepoSource {
    Local(PathBuf),
    Remote(String),
}

/// The `direct_url.json` (PEP 610) a non-index install records in its dist-info,
/// naming where the package came from.
#[derive(Debug, Deserialize)]
struct DirectUrl {
    url: String,
    #[serde(default)]
    vcs_info: Option<VcsInfo>,
}

/// The VCS section of a `direct_url.json`, present only for a VCS install.
#[derive(Debug, Deserialize)]
struct VcsInfo {
    vcs: String,
}

/// The extra versions configured under `[companions] versions` (a map of package
/// to git ref, the same shape as `repositories`), each checked out on disk and
/// returned as a reference-only target. The ref is taken from the repository the
/// companion resolves to (a local override, the installed checkout, or the
/// configured `repositories` url), rooted like the `.venv` copy so only the project
/// id differs by the `@ref` suffix.
///
/// Best-effort like [`discover_companions`]: an unlocatable repository, a missing
/// `git`, or a failed checkout is skipped with a message, never fatal. A version
/// already in the store is skipped so a re-index neither duplicates nor
/// re-checks-out. The caller indexes each target, marks it reference-only, then
/// runs [`crate::link_constellation`].
pub fn discover_versions(
    store: &Store,
    workspace_root: &Path,
) -> Result<Vec<CompanionTarget>, IndexError> {
    assert!(workspace_root.is_dir(), "workspace root must be a directory: {workspace_root:?}");

    let config = load_config(workspace_root);

    if !config.enabled || config.versions.is_empty() {
        return Ok(Vec::new());
    }

    let roots = site_packages_roots(workspace_root, &config);
    let existing = existing_project_ids(store)?;

    let mut targets: Vec<CompanionTarget> = Vec::with_capacity(config.versions.len());
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut count: u32 = 0;

    for (package, reference) in &config.versions {
        count += 1;

        assert!(count <= COMPANION_COUNT_MAX, "version map exceeded {COMPANION_COUNT_MAX}");

        if package.is_empty() || reference.is_empty() {
            continue;
        }

        let spec = format!("{package}@{reference}");

        if existing.contains(&spec) || !seen.insert(spec.clone()) {
            continue;
        }

        let Some(repo) = locate_repo(workspace_root, &roots, package) else {
            eprintln!(
                "constellation: skipping '{spec}': no repository for '{package}' (set its \
                 [companions] repositories url, or install it editable or from git)",
            );

            continue;
        };

        if let Some(package_root) = materialize_version(workspace_root, &spec, reference, &repo) {
            targets.push(CompanionTarget {
                project_id: spec,
                package_root,
                reference_only: true,
            });
        }
    }

    Ok(targets)
}

/// The git repository the `package` came from. A local override (a pyproject
/// `[tool.uv.sources]` path, or a `PYTHONPATH_APPEND` directory) wins first, so
/// other refs are taken from the working copy. Otherwise the installed copy's
/// checkout on disk (editable or local-path install), else the git url recorded in
/// `direct_url.json` for a VCS install, else the configured (or default)
/// `[companions] repositories` url for the package, the same remote companion
/// history uses, so a plain wheel install can still check out other refs. `None`
/// only when none of those names a repository.
fn locate_repo(workspace_root: &Path, roots: &[PathBuf], package: &str) -> Option<RepoSource> {
    if let Some(dir) = resolve_override(workspace_root, package)
        && let Some(repo) = git_repo_root(&dir)
    {
        return Some(RepoSource::Local(repo));
    }

    let import = package.replace('-', "_");

    if let Some(package_dir) = resolve_package(roots, &import) {
        let in_site_packages = roots.iter().any(|root| package_dir.starts_with(root));

        let from_install = if in_site_packages {
            repo_from_direct_url(roots, &import)
        } else {
            git_repo_root(&package_dir).map(RepoSource::Local)
        };

        if from_install.is_some() {
            return from_install;
        }
    }

    load_config(workspace_root)
        .repositories
        .get(package)
        .map(|url| RepoSource::Remote(url.clone()))
}

/// The repository named by the package's `direct_url.json`: a `Remote` for a git
/// VCS install, or a `Local` when the url is a `file://` path that is a git
/// checkout. `None` when the file is absent (a plain index install) or names no
/// usable repository.
fn repo_from_direct_url(roots: &[PathBuf], import: &str) -> Option<RepoSource> {
    let dist_info = find_dist_info(roots, import)?;

    let text = std::fs::read_to_string(dist_info.join("direct_url.json")).ok()?;
    let parsed: DirectUrl = serde_json::from_str(&text).ok()?;

    if parsed.vcs_info.as_ref().is_some_and(|info| info.vcs == "git") {
        let url = parsed.url.strip_prefix("git+").unwrap_or(&parsed.url);

        return Some(RepoSource::Remote(url.to_string()));
    }

    let path = file_url_to_path(&parsed.url)?;

    git_repo_root(&path).map(RepoSource::Local)
}

/// The `*.dist-info` directory for `import` among the site-packages roots, matched
/// by the `<import>-` prefix, case-insensitively.
fn find_dist_info(roots: &[PathBuf], import: &str) -> Option<PathBuf> {
    let prefix = format!("{}-", import.to_ascii_lowercase());
    let mut scanned: u32 = 0;

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };

        for entry in entries {
            scanned += 1;

            assert!(scanned <= SCAN_ENTRIES_MAX, "dist-info scan exceeded {SCAN_ENTRIES_MAX}");

            let Ok(entry) = entry else {
                continue;
            };

            let name = entry.file_name();
            let name = name.to_string_lossy().to_ascii_lowercase();

            if name.ends_with(".dist-info") && name.starts_with(&prefix) {
                return Some(entry.path());
            }
        }
    }

    None
}

/// The filesystem path of a `file://` url, or `None` for any other scheme. Strips
/// the leading slash before a Windows drive (`file:///C:/x` -> `C:/x`) and decodes
/// `%20` to a space; other percent escapes are left as-is (best-effort).
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;

    let trimmed = match rest.strip_prefix('/') {
        Some(tail) if is_windows_drive(tail) => tail,
        _ => rest,
    };

    Some(PathBuf::from(trimmed.replace("%20", " ")))
}

/// Whether `text` begins with a Windows drive prefix such as `C:`.
fn is_windows_drive(text: &str) -> bool {
    let bytes = text.as_bytes();

    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// The enclosing git repository of `start`: the nearest ancestor (including
/// `start`) holding a `.git` entry, searched up to a bounded depth. `None` when
/// none is found, so a package with no checkout on disk is skipped.
fn git_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    let mut steps: u32 = 0;

    while let Some(dir) = current {
        steps += 1;

        assert!(steps <= GIT_ROOT_DEPTH_MAX, "git root walk exceeded {GIT_ROOT_DEPTH_MAX} levels");

        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }

        if steps == GIT_ROOT_DEPTH_MAX {
            break;
        }

        current = dir.parent();
    }

    None
}

/// The on-disk package directory for a version spec, checked out at `reference`
/// under `<workspace_root>/.constellation/sources/<sanitized-spec>/` with the `git`
/// CLI. `None` when git is unavailable or the checkout fails, so the version is
/// skipped rather than failing the index.
fn materialize_version(
    workspace_root: &Path,
    spec: &str,
    reference: &str,
    repo: &RepoSource,
) -> Option<PathBuf> {
    if !git_available() {
        eprintln!("constellation: skipping '{spec}': git is not available");

        return None;
    }

    let sources = workspace_root.join(".constellation").join("sources");

    std::fs::create_dir_all(&sources).ok()?;

    let dest = sources.join(sanitize_segment(spec));

    let checked_out = match repo {
        RepoSource::Local(root) => checkout_local(root, &dest, reference),
        RepoSource::Remote(url) => checkout_remote(url, &dest, reference),
    };

    if !checked_out {
        eprintln!("constellation: skipping '{spec}': git checkout of '{reference}' failed");

        return None;
    }

    Some(package_subdir(&dest, spec))
}

/// Whether `reference` is checked out at `dest` from the local repository `repo`.
/// A fresh `dest` gets a detached worktree (cheap, sharing the object store and
/// never touching the source working tree); an existing one is re-pointed at the
/// ref. A ref that lives only on the clone's origin (a branch not checked out
/// locally, the common case for `package@ref`) is fetched from origin first, then
/// checked out, so comparing against an unfetched remote branch still works.
fn checkout_local(repo: &Path, dest: &Path, reference: &str) -> bool {
    if is_populated(dest) {
        if run_git(Some(dest), &["checkout", "--force", "--detach", reference]).is_some() {
            return true;
        }

        if run_git(Some(repo), &["fetch", "origin", reference]).is_none() {
            return false;
        }

        return run_git(Some(dest), &["checkout", "--force", "--detach", "FETCH_HEAD"]).is_some();
    }

    let dest_arg = dest.to_string_lossy();

    let direct = run_git(
        Some(repo),
        &["worktree", "add", "--force", "--detach", dest_arg.as_ref(), reference],
    );

    if direct.is_some() {
        return true;
    }

    if run_git(Some(repo), &["fetch", "origin", reference]).is_none() {
        return false;
    }

    run_git(
        Some(repo),
        &["worktree", "add", "--force", "--detach", dest_arg.as_ref(), "FETCH_HEAD"],
    )
    .is_some()
}

/// Whether `reference` is checked out at `dest` from the remote `url`. A fresh
/// `dest` is shallow-cloned at the branch or tag, falling back to a full clone and
/// detached checkout for a commit; an existing one is fetched and re-pointed.
fn checkout_remote(url: &str, dest: &Path, reference: &str) -> bool {
    if is_populated(dest) {
        if run_git(Some(dest), &["fetch", "--depth", "1", "origin", reference]).is_none() {
            return true;
        }

        return run_git(Some(dest), &["checkout", "--force", "--detach", "FETCH_HEAD"]).is_some();
    }

    let dest_arg = dest.to_string_lossy();

    let shallow = run_git(
        None,
        &["clone", "--depth", "1", "--branch", reference, url, dest_arg.as_ref()],
    );

    if shallow.is_some() {
        return true;
    }

    if run_git(None, &["clone", url, dest_arg.as_ref()]).is_none() {
        return false;
    }

    run_git(Some(dest), &["checkout", "--force", "--detach", reference]).is_some()
}

/// Whether a usable `git` is on the PATH.
fn git_available() -> bool {
    run_git(None, &["--version"]).is_some()
}

/// The output of a `git` invocation, or `None` when git fails to spawn or exits
/// non-zero. `current_dir` runs git inside that directory.
fn run_git(current_dir: Option<&Path>, args: &[&str]) -> Option<Output> {
    assert!(!args.is_empty(), "a git invocation needs at least one argument");

    let mut command = Command::new("git");

    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }

    let output = command.args(args).output().ok()?;

    output.status.success().then_some(output)
}

/// Whether `dest` exists and holds at least one entry.
fn is_populated(dest: &Path) -> bool {
    std::fs::read_dir(dest).is_ok_and(|mut entries| entries.next().is_some())
}

/// The directory holding the package's `__init__.py` within `dir`: the import
/// package name (the spec's text before any `@`, hyphens to underscores) probed at
/// `dir`, then `dir/<package>`, then `dir/src/<package>`. Falls back to `dir`
/// itself, so a layout this does not recognize still indexes.
fn package_subdir(dir: &Path, spec: &str) -> PathBuf {
    assert!(!spec.is_empty(), "version spec must not be empty");

    let bare = spec.split('@').next().unwrap_or(spec);
    let package = bare.replace('-', "_");

    assert!(!package.is_empty(), "package name must not be empty");

    if dir.join("__init__.py").is_file() {
        return dir.to_path_buf();
    }

    for candidate in [dir.join(&package), dir.join("src").join(&package)] {
        if candidate.join("__init__.py").is_file() {
            return candidate;
        }
    }

    dir.to_path_buf()
}

/// A version spec reduced to one safe path segment: every character that is not
/// ASCII alphanumeric, `-`, `_`, or `.` becomes `_`, so `django-spire@refactor/next`
/// becomes `django-spire_refactor_next`. The project id keeps the raw spec; only
/// the directory is sanitized.
fn sanitize_segment(spec: &str) -> String {
    assert!(!spec.is_empty(), "version spec must not be empty");

    let mut out = String::with_capacity(spec.len());
    let mut count: u32 = 0;

    for character in spec.chars() {
        count += 1;

        assert!(count <= SEGMENT_LEN_MAX, "version spec exceeded {SEGMENT_LEN_MAX} chars");

        let safe = character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.');

        out.push(if safe { character } else { '_' });
    }

    assert!(!out.is_empty(), "a sanitized segment is never empty");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_the_file_is_empty() {
        let config: ConfigFile = toml::from_str("").unwrap();

        assert!(config.companions.enabled);
        assert_eq!(config.companions.packages.len(), COMPANIONS_DEFAULT.len());
        assert!(config.companions.exclude.is_empty());
        assert_eq!(config.companions.repositories.len(), COMPANION_REPOSITORIES_DEFAULT.len());
        assert!(config.companions.repositories.contains_key("django-spire"));
        assert!(config.history.enabled);
        assert!(config.history.symbols);
        assert!(config.history.companions);
        assert_eq!(config.history.commits_max, crate::history::HISTORY_COMMITS_MAX);
    }

    #[test]
    fn config_parses_exclude_and_history_overrides() {
        let text = "\
[companions]
exclude = [\"robit\"]

[history]
enabled = false
symbols = true
companions = false
commits_max = 5
";
        let config: ConfigFile = toml::from_str(text).unwrap();

        assert_eq!(config.companions.exclude, vec!["robit".to_string()]);
        assert!(config.companions.enabled, "untouched fields keep their defaults");
        assert!(!config.history.enabled);
        assert!(config.history.symbols);
        assert!(!config.history.companions);
        assert_eq!(config.history.commits_max, 5);
    }

    #[test]
    fn config_parses_companion_repositories() {
        let text = "[companions.repositories]\ndjango-spire = \"https://example.com/django-spire\"\n";
        let config: ConfigFile = toml::from_str(text).unwrap();

        assert_eq!(
            config.companions.repositories.get("django-spire").map(String::as_str),
            Some("https://example.com/django-spire"),
        );
    }

    #[test]
    fn installed_version_reads_the_dist_info_name() {
        let site = tempfile::tempdir().unwrap();
        std::fs::create_dir(site.path().join("django_spire-0.32.3.dist-info")).unwrap();

        let roots = vec![site.path().to_path_buf()];

        assert_eq!(installed_version(&roots, "django-spire").as_deref(), Some("0.32.3"));
        assert_eq!(installed_version(&roots, "missing"), None);
    }

    #[test]
    fn locate_repo_falls_back_to_the_configured_repository() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".constellation")).unwrap();
        std::fs::write(
            workspace.path().join(".constellation").join("config.toml"),
            "[companions.repositories]\ndjango-spire = \"https://example.com/spire\"\n",
        )
        .unwrap();

        let repo = locate_repo(workspace.path(), &[], "django-spire");

        assert!(
            matches!(repo, Some(RepoSource::Remote(url)) if url == "https://example.com/spire"),
            "a wheel-only package falls back to its configured repository url",
        );
    }

    #[test]
    fn config_parses_versions_map() {
        let text = "[companions.versions]\ndjango-spire = \"v1/base\"\n";
        let config: ConfigFile = toml::from_str(text).unwrap();

        assert_eq!(config.companions.versions.get("django-spire").map(String::as_str), Some("v1/base"));
    }

    #[test]
    fn sanitize_segment_replaces_unsafe_characters() {
        assert_eq!(sanitize_segment("django-spire@refactor/next"), "django-spire_refactor_next");
        assert_eq!(sanitize_segment("a/b:c"), "a_b_c", "path and drive separators become _");
        assert_eq!(sanitize_segment("keep.dot-1"), "keep.dot-1", "dot, hyphen, and digits are kept");
    }

    #[test]
    fn package_subdir_finds_the_package_init() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        std::fs::create_dir(root.join("django_spire")).unwrap();
        std::fs::write(root.join("django_spire").join("__init__.py"), "").unwrap();

        let found = package_subdir(root, "django-spire@refactor/next");

        assert!(found.ends_with("django_spire"), "the package subdir is located, got {found:?}");
    }

    #[test]
    fn package_subdir_falls_back_to_the_directory() {
        let directory = tempfile::tempdir().unwrap();

        let found = package_subdir(directory.path(), "mystery@v1");

        assert_eq!(found, directory.path(), "an unrecognized layout indexes the directory as-is");
    }

    #[test]
    fn file_url_to_path_handles_windows_and_posix() {
        assert_eq!(file_url_to_path("file:///C:/code/spire"), Some(PathBuf::from("C:/code/spire")));
        assert_eq!(file_url_to_path("file:///home/u/spire"), Some(PathBuf::from("/home/u/spire")));
        assert_eq!(file_url_to_path("file:///a%20b/spire"), Some(PathBuf::from("/a b/spire")), "%20 decodes");
        assert_eq!(file_url_to_path("https://example.com/x"), None, "a non-file url is rejected");
    }

    #[test]
    fn git_repo_root_finds_the_enclosing_dot_git() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src").join("pkg")).unwrap();

        let found = git_repo_root(&root.join("src").join("pkg")).expect("walks up to the repo root");

        assert_eq!(found, root, "the nearest ancestor with .git is the repo root");
    }

    #[test]
    fn repo_from_direct_url_reads_a_git_vcs_install() {
        let directory = tempfile::tempdir().unwrap();
        let roots = vec![directory.path().to_path_buf()];

        let dist = directory.path().join("django_spire-1.0.dist-info");
        std::fs::create_dir(&dist).unwrap();
        std::fs::write(
            dist.join("direct_url.json"),
            "{\"url\": \"git+https://example.com/org/django-spire\", \"vcs_info\": {\"vcs\": \"git\"}}",
        )
        .unwrap();

        match repo_from_direct_url(&roots, "django_spire") {
            Some(RepoSource::Remote(url)) => {
                assert_eq!(url, "https://example.com/org/django-spire", "the git+ prefix is stripped");
            }
            other => panic!("expected a remote repo, got {other:?}"),
        }
    }

    #[test]
    fn locate_repo_finds_an_editable_clone() {
        let clone = tempfile::tempdir().unwrap();
        let clone_root = clone.path();

        std::fs::create_dir(clone_root.join(".git")).unwrap();
        std::fs::create_dir(clone_root.join("django_spire")).unwrap();
        std::fs::write(clone_root.join("django_spire").join("__init__.py"), "").unwrap();

        let site = tempfile::tempdir().unwrap();
        std::fs::write(
            site.path().join("__editable__.django_spire-1.0.pth"),
            clone_root.to_string_lossy().as_ref(),
        )
        .unwrap();

        let roots = vec![site.path().to_path_buf()];

        // A workspace with no override, so resolution falls through to the .venv copy.
        let workspace = tempfile::tempdir().unwrap();

        match locate_repo(workspace.path(), &roots, "django-spire") {
            Some(RepoSource::Local(root)) => assert_eq!(root, clone_root, "the editable clone is the repo root"),
            other => panic!("expected the local editable clone, got {other:?}"),
        }
    }

    #[test]
    fn materialize_version_checks_out_a_local_tag() {
        if !git_available() {
            return;
        }

        let origin = tempfile::tempdir().unwrap();
        let origin_root = origin.path();

        assert!(run_git(Some(origin_root), &["init", "-q"]).is_some(), "git init");
        assert!(run_git(Some(origin_root), &["config", "user.email", "t@t.test"]).is_some(), "git email");
        assert!(run_git(Some(origin_root), &["config", "user.name", "test"]).is_some(), "git name");

        std::fs::create_dir(origin_root.join("django_spire")).unwrap();
        std::fs::write(origin_root.join("django_spire").join("__init__.py"), "value = 1\n").unwrap();

        assert!(run_git(Some(origin_root), &["add", "-A"]).is_some(), "git add");
        assert!(run_git(Some(origin_root), &["commit", "-q", "-m", "init"]).is_some(), "git commit");
        assert!(run_git(Some(origin_root), &["tag", "v1"]).is_some(), "git tag");

        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".constellation")).unwrap();

        let repo = RepoSource::Local(origin_root.to_path_buf());

        let package = materialize_version(workspace.path(), "django-spire@v1", "v1", &repo)
            .expect("a worktree is materialized from the local repo");

        assert!(package.ends_with("django_spire"), "resolves to the package dir, got {package:?}");
        assert!(package.join("__init__.py").is_file(), "the checked-out package init is present");
    }

    #[test]
    fn env_value_reads_a_key_ignoring_quotes_and_export() {
        let text = "# comment\nexport PYTHONPATH_APPEND=\"C:/code/django-spire\"\nOTHER=1\n";

        assert_eq!(env_value(text, "PYTHONPATH_APPEND").as_deref(), Some("C:/code/django-spire"));
        assert_eq!(env_value(text, "MISSING"), None, "an absent key yields nothing");
    }

    #[test]
    fn uv_source_path_reads_a_local_path_dependency() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("pyproject.toml"),
            "[tool.uv.sources]\ndjango-spire = { path = \"../django-spire\", editable = true }\n",
        )
        .unwrap();

        assert_eq!(
            uv_source_path(workspace.path(), "django-spire"),
            Some(PathBuf::from("../django-spire")),
            "the uv.sources path is read",
        );

        assert_eq!(uv_source_path(workspace.path(), "absent-pkg"), None, "an unlisted package has none");
    }

    #[test]
    fn resolve_override_prefers_a_pythonpath_append_directory() {
        let workspace = tempfile::tempdir().unwrap();

        let clone = tempfile::tempdir().unwrap();
        std::fs::create_dir(clone.path().join("django_spire")).unwrap();
        std::fs::write(clone.path().join("django_spire").join("__init__.py"), "").unwrap();

        std::fs::write(
            workspace.path().join("development.env"),
            format!("PYTHONPATH_APPEND={}\n", clone.path().to_string_lossy()),
        )
        .unwrap();

        let resolved = resolve_override(workspace.path(), "django-spire").expect("the override resolves");

        assert_eq!(resolved, clone.path(), "the PYTHONPATH_APPEND directory holding the package wins");
    }

    #[test]
    fn resolve_override_ignores_a_directory_without_the_package() {
        let workspace = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();

        std::fs::write(
            workspace.path().join("development.env"),
            format!("PYTHONPATH_APPEND={}\n", empty.path().to_string_lossy()),
        )
        .unwrap();

        assert!(
            resolve_override(workspace.path(), "django-spire").is_none(),
            "a stale entry that no longer holds the package does not override",
        );
    }

    #[test]
    fn checkout_local_fetches_a_branch_only_on_origin() {
        if !git_available() {
            return;
        }

        let origin = tempfile::tempdir().unwrap();
        let origin_root = origin.path();

        assert!(run_git(Some(origin_root), &["init", "-q"]).is_some(), "git init");
        assert!(run_git(Some(origin_root), &["config", "user.email", "t@t.test"]).is_some(), "git email");
        assert!(run_git(Some(origin_root), &["config", "user.name", "test"]).is_some(), "git name");

        std::fs::write(origin_root.join("seed.txt"), "seed\n").unwrap();
        assert!(run_git(Some(origin_root), &["add", "-A"]).is_some(), "git add seed");
        assert!(run_git(Some(origin_root), &["commit", "-q", "-m", "seed"]).is_some(), "git commit seed");

        // A slash-named branch with its own commit, then back to the default
        // branch so a clone leaves v1/base only on origin.
        assert!(run_git(Some(origin_root), &["checkout", "-q", "-b", "v1/base"]).is_some(), "branch v1/base");
        std::fs::write(origin_root.join("base.txt"), "base\n").unwrap();
        assert!(run_git(Some(origin_root), &["add", "-A"]).is_some(), "git add base");
        assert!(run_git(Some(origin_root), &["commit", "-q", "-m", "base"]).is_some(), "git commit base");
        assert!(run_git(Some(origin_root), &["checkout", "-q", "-"]).is_some(), "back to the default branch");

        let workspace = tempfile::tempdir().unwrap();
        let clone_root = workspace.path().join("repo");

        let origin_arg = origin_root.to_string_lossy();
        let clone_arg = clone_root.to_string_lossy();

        assert!(
            run_git(None, &["clone", "-q", origin_arg.as_ref(), clone_arg.as_ref()]).is_some(),
            "git clone",
        );

        // The clone has no local v1/base, only origin/v1/base.
        let dest = workspace.path().join("worktree");

        assert!(
            checkout_local(&clone_root, &dest, "v1/base"),
            "a remote-only branch is fetched from origin and checked out",
        );

        assert!(dest.join("base.txt").is_file(), "the worktree holds the branch content, got {dest:?}");
    }
}
