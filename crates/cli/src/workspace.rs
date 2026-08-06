//! Locating and preparing a workspace on disk: finding its database, naming its
//! project, and creating the `.constellation/` directory that holds both.
//!
//! Every command that takes an optional path argument resolves it through here,
//! so "no argument" means the same thing everywhere.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_graph::{PROFILE_NAME_DEFAULT, PROFILE_NAMES, Profile};

/// A fail-fast bound on directory levels walked while discovering the database.
const DISCOVER_DEPTH_MAX: u32 = 4_096;

/// The database located without an explicit path: the `CONSTELLATION_DB` override,
/// else the nearest `.constellation/index.db` at or above the working directory.
/// Errors when none is found; `serve` uses [`discover_database_optional`] instead,
/// which treats "none found" as a valid (unavailable) outcome rather than an error.
pub(crate) fn discover_database() -> Result<PathBuf> {
    match discover_database_optional()? {
        Some(database) => Ok(database),
        None => bail!(
            "no .constellation/index.db found from the working directory; \
             run `constellation init` first, or set CONSTELLATION_DB",
        ),
    }
}

/// The database located as for [`discover_database`], but returning `Ok(None)`
/// when none is found rather than an error: the `CONSTELLATION_DB` override, else
/// the nearest `.constellation/index.db` at or above the working directory, else
/// `None`. `serve` maps `None` to an unavailable server, so a global registration
/// launched outside any indexed project stays quiet instead of failing to connect.
pub(crate) fn discover_database_optional() -> Result<Option<PathBuf>> {
    if let Ok(path) = std::env::var("CONSTELLATION_DB") {
        return Ok(Some(PathBuf::from(path)));
    }

    let mut directory = std::env::current_dir()?;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(
            depth <= DISCOVER_DEPTH_MAX,
            "directory walk exceeded {DISCOVER_DEPTH_MAX} levels"
        );

        let candidate = directory.join(".constellation").join("index.db");

        if candidate.is_file() {
            return Ok(Some(candidate));
        }

        if !directory.pop() {
            break;
        }
    }

    Ok(None)
}

/// The index.db path inside a freshly created `<root>/.constellation/`.
pub(crate) fn create_index_directory(root: &Path) -> Result<PathBuf> {
    let index_directory = root.join(".constellation");
    std::fs::create_dir_all(&index_directory)?;

    Ok(index_directory.join("index.db"))
}

/// A project name from a repository root directory. Resolves `.`, `..`, and a
/// trailing-slash root (whose `file_name()` is empty) to the real directory
/// name by canonicalizing, so `constellation .` names the project after the
/// actual folder, not the literal "project". Cross-project linking keys on this
/// name, so a wrong one silently breaks linking.
pub(crate) fn project_name(root: &Path) -> String {
    if let Some(name) = root.file_name().and_then(|name| name.to_str())
        && !name.is_empty()
        && name != "."
        && name != ".."
    {
        return name.to_string();
    }

    let resolved = std::fs::canonicalize(root).ok().and_then(|absolute| {
        absolute
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });

    let name = resolved.unwrap_or_else(|| "project".to_string());

    assert!(!name.is_empty(), "a project name is never empty");

    name
}

/// A root directory resolved to a clean absolute path. Canonicalizes so a relative
/// root (`.`) becomes absolute (a relative `root_path` breaks explore's source
/// loading whenever serve runs from a different directory), then strips the
/// Windows `\\?\` verbatim prefix canonicalize adds, so the stored path stays
/// portable and the file watcher (which dislikes verbatim paths) accepts it.
pub(crate) fn resolve_root(root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(root)?;
    let text = canonical.to_string_lossy();
    let cleaned = text.strip_prefix(r"\\?\").unwrap_or(&text);

    Ok(PathBuf::from(cleaned))
}

/// A commented starter `.constellation/config.toml`, written only when none
/// exists, so a developer discovers where to select a profile, enable companion
/// packages, and add extra version sources. Best-effort: a write failure is
/// reported, never fatal, and an existing config is never clobbered.
pub(crate) fn scaffold_config(root: &Path) {
    let path = root.join(".constellation").join("config.toml");

    if path.exists() {
        return;
    }

    let starter = starter_config(&Profile::default());

    if let Err(error) = std::fs::write(&path, starter) {
        eprintln!("constellation: could not write starter config: {error}");
    }
}

/// The package name the samples use when the profile names none of its own.
const SAMPLE_PACKAGE: &str = "your-library";

/// The repository url the samples use when the profile names none of its own.
const SAMPLE_REPOSITORY: &str = "https://github.com/your-org/your-library";

/// The starter config, with each profile-derived value marked `@name@` for
/// [`starter_config`] to substitute. A constant rather than a literal inside
/// that function, so the text reads as the file it becomes and the function
/// reads as the substitution it performs.
const STARTER_TEMPLATE: &str = "\
# Constellation configuration.
# Lines starting with # are comments. Uncomment a setting to change it from its
# default value.


[profile]
# The set of company conventions this project is indexed and served under: the
# framework hook names its base classes add, and the libraries it installs.
# One of \"@profile_names@\". \"generic\" is plain Django, with no conventions at all.
name = \"@profile_name@\"

# Method and class names the framework calls with no call site in your code, on
# top of Django's own (save, delete, clean, ready, handle, Meta,
# get_absolute_url). Symbols named here are not reported as dead code.
# Defaults to the profile's list; set this to state your own instead.
# hook_names_extra = @hook_names_extra@


[companions]
# Index the libraries this project installs alongside it, so imports into them
# resolve to real definitions instead of <external> stubs.
# Set to false to index this project only.
enabled = true

# The libraries to index, by name. Defaults to the profile's list (below) when
# omitted; give your own list, or [] to index none while leaving companions
# enabled.
# packages = @packages@

# Libraries to leave out of the profile's list, without re-listing the rest.
# exclude = @excluded@

# The virtual-environment folder the libraries are installed in, relative to this
# project or absolute. Defaults to \".venv\".
# venv = \".venv\"

# Extra versions of a library to index side by side for comparison while
# refactoring. Each package = \"git-ref\" entry checks out that ref as its own
# read-only project, from the library's repository (see repositories below).
# versions = { @sample_package@ = \"v1/base\" }

# The git repository for each library, used to fetch its commit history at the tag
# matching the installed version, no local clone needed. Defaults to the profile's
# own repositories, so library history works out of the box; set this to override
# those or add your own. Cloned into .constellation/sources/ and cached.
# repositories = { @repository_package@ = \"@repository_url@\" }


[history]
# Index this project's git history, so the graph can be read over time
# (the history, symbol_history, and as_of tools).
# Set to false to skip history entirely.
enabled = true

# Also record which individual symbols (functions, classes, model fields, ...)
# were added, changed, or removed in each commit, not just file-level churn.
# On by default; it is slower (reads every file at every commit), so set false
# to record only commit-level churn.
# symbols = true

# Also index the git history of companion libraries that have their own
# repository (such as a versions checkout). On by default; set false to
# index this project's history only.
# companions = true

# The most commits to read from any one repository.
# commits_max = 20000
";

/// The starter config text for `profile`: the commented defaults a developer
/// edits, with the profile's own package list and repository substituted into
/// the samples rather than restated here. Naming a library in this file would be
/// a second copy of what `constellation_graph::Profile` already holds, and the
/// two would drift.
fn starter_config(profile: &Profile) -> String {
    let sample = profile.companion_packages.first().map_or(SAMPLE_PACKAGE, String::as_str);

    let (repository_package, repository_url) = profile
        .companion_repositories
        .first()
        .map_or((SAMPLE_PACKAGE, SAMPLE_REPOSITORY), |(package, url)| {
            (package.as_str(), url.as_str())
        });

    let excluded = profile.companion_packages.last().map(std::slice::from_ref);

    let text = STARTER_TEMPLATE
        .replace("@profile_names@", &PROFILE_NAMES.join("\", \""))
        .replace("@profile_name@", PROFILE_NAME_DEFAULT)
        .replace("@hook_names_extra@", &toml_string_array(&profile.hook_names_extra))
        .replace("@packages@", &toml_string_array(&profile.companion_packages))
        .replace("@excluded@", &toml_string_array(excluded.unwrap_or_default()))
        .replace("@repository_package@", repository_package)
        .replace("@repository_url@", repository_url)
        .replace("@sample_package@", sample);

    assert!(!text.contains('@'), "every starter-config placeholder is substituted");

    text
}

/// A list of strings as a TOML array literal (`[\"a\", \"b\"]`), for writing a
/// profile's own values into the sample lines of the starter config.
fn toml_string_array(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();

    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{PROFILE_NAME_DEFAULT, Profile, project_name, resolve_root, starter_config};

    use std::path::Path;

    #[test]
    fn the_starter_config_parses_and_states_only_its_defaults() {
        let text = starter_config(&Profile::default());
        let parsed: toml::Value = toml::from_str(&text).expect("the starter config is valid TOML");

        let selected =
            parsed.get("profile").and_then(|section| section.get("name")).and_then(toml::Value::as_str);

        assert_eq!(
            selected,
            Some(PROFILE_NAME_DEFAULT),
            "the one uncommented profile key states the default, so scaffolding changes nothing",
        );

        assert_eq!(
            parsed.get("companions").and_then(|section| section.get("packages")),
            None,
            "the package list stays commented out, so the profile keeps supplying it",
        );
    }

    #[test]
    fn the_starter_config_names_no_company_under_the_generic_profile() {
        let text = starter_config(&Profile::generic());

        for package in Profile::stratus().companion_packages {
            assert!(
                !text.contains(&package),
                "a generic starter config names no company package, found {package:?}",
            );
        }

        assert!(
            text.contains("your-library"),
            "the samples fall back to a placeholder rather than emptying out",
        );
    }

    #[test]
    fn project_name_takes_the_last_path_segment() {
        assert_eq!(
            project_name(Path::new("/srv/www/workspace")),
            "workspace",
            "the leaf directory names the project"
        );
        assert_eq!(
            project_name(Path::new("blog")),
            "blog",
            "a bare relative name is taken as-is"
        );
        assert_eq!(
            project_name(Path::new("a/b/c")),
            "c",
            "nested paths use the final segment"
        );
    }

    #[test]
    fn project_name_ignores_a_trailing_separator() {
        assert_eq!(
            project_name(Path::new("a/b/")),
            "b",
            "a trailing slash does not blank the name"
        );
    }

    #[test]
    fn project_name_falls_back_to_the_real_directory_for_dot() {
        // `.` has no file_name, so the fallback canonicalizes to the working
        // directory's actual name, which is always a non-empty segment.
        let name = project_name(Path::new("."));

        assert!(
            !name.is_empty(),
            "the dot directory resolves to a real, non-empty name"
        );
        assert_ne!(name, ".", "the literal dot is never returned as the name");
    }

    #[test]
    fn resolve_root_returns_a_clean_absolute_path() {
        let directory = tempfile::tempdir().unwrap();

        let resolved = resolve_root(directory.path()).unwrap();

        assert!(
            resolved.is_absolute(),
            "a canonical root is absolute, got {}",
            resolved.display()
        );
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "the Windows verbatim prefix is stripped, got {}",
            resolved.display(),
        );
    }

    #[test]
    fn resolve_root_errors_on_a_missing_directory() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("does-not-exist");

        assert!(
            resolve_root(&missing).is_err(),
            "canonicalizing an absent path fails"
        );
    }
}
