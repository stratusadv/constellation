//! Walking a repository and deciding what is worth parsing.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use constellation_graph::is_minified_path;
use ignore::WalkBuilder;

use crate::IndexError;
use crate::limits::FILE_COUNT_MAX;

/// The directory names skipped wholesale during the walk, alongside their subtrees.
/// `migrations` is Django's auto-generated schema history: thousands of field
/// constructors and `CreateModel`/`AddField` operations that are never navigation
/// targets (the live schema lives in `models.py`), so indexing it only floods the
/// graph with unresolved instantiations. Schema-over-time still comes from git
/// history, not these files.
///
/// `target` is deliberately absent: it is a Rust build directory only when it
/// looks like one, which [`is_rust_build_directory`] decides. A name alone would
/// silently drop a source directory that happens to be called `target`.
const SKIP_DIRECTORIES: &[&str] = &[
    ".constellation",
    ".git",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "migrations",
    "node_modules",
    "venv",
];

/// The marker cargo writes into its build directory, and the manifest that sits
/// beside one. Either is enough to call a `target` directory a build artifact.
const RUST_BUILD_MARKERS: &[&str] = &["CACHEDIR.TAG", ".rustc_info.json"];

/// The deepest path this may reconstruct while testing components, a bound on the
/// per-path work [`is_ignored_path`] does for a watcher event.
const PATH_COMPONENTS_MAX: u32 = 256;

/// The path of every regular file under `root`, collected and bounded by
/// [`FILE_COUNT_MAX`] so a pathological tree fails fast rather than walking
/// unbounded.
pub(crate) fn collect_file_paths(root: &Path) -> Result<Vec<PathBuf>, IndexError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut visited: u32 = 0;

    for entry in walk(root) {
        let entry = entry?;
        visited += 1;

        assert!(visited <= FILE_COUNT_MAX, "walk exceeded {FILE_COUNT_MAX} entries");

        if entry.file_type().is_some_and(|file_type| file_type.is_file()) {
            paths.push(entry.path().to_path_buf());
        }
    }

    Ok(paths)
}

/// Whether a path lies inside any skipped directory, or inside a Rust build
/// directory (a `target` that carries a cargo marker or sits beside a
/// `Cargo.toml`). A `target` directory holding source is walked like any other.
#[doc(hidden)]
pub fn is_ignored_path(path: &Path) -> bool {
    let mut named_target = false;
    let mut depth: u32 = 0;

    for component in path.components() {
        depth += 1;

        assert!(depth <= PATH_COMPONENTS_MAX, "path exceeded {PATH_COMPONENTS_MAX} components");

        let Component::Normal(name) = component else {
            continue;
        };

        let Some(name) = name.to_str() else {
            continue;
        };

        if SKIP_DIRECTORIES.contains(&name) {
            return true;
        }

        named_target |= name == "target";
    }

    // The build-directory test stats the filesystem, so it runs only for a path
    // that could possibly need it. This predicate is on the watcher's per-event
    // path, where an allocation and a stat for every ordinary file would be paid
    // thousands of times to answer a question about a name almost none of them
    // carry.
    if !named_target {
        return false;
    }

    let mut prefix = PathBuf::new();

    for component in path.components() {
        prefix.push(component);

        if is_rust_build_directory(&prefix) {
            return true;
        }
    }

    false
}

/// Whether `directory` is a cargo build directory: named `target`, and carrying
/// a cargo marker file or sitting beside the `Cargo.toml` that produced it. The
/// name alone is not enough - `app/production/line/schedule/target/` is a Django
/// app, and dropping it would make its models invisible to every query.
fn is_rust_build_directory(directory: &Path) -> bool {
    let named_target = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "target");

    if !named_target {
        return false;
    }

    let marked = RUST_BUILD_MARKERS
        .iter()
        .any(|marker| directory.join(marker).exists());

    if marked {
        return true;
    }

    directory
        .parent()
        .is_some_and(|parent| parent.join("Cargo.toml").exists())
}

/// Whether a path is a minified or bundled asset, excluded from the index so it
/// cannot pollute the graph. The `&Path` form of
/// [`constellation_graph::is_minified_path`], which owns the rule.
pub(crate) fn is_minified(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_minified_path)
}

/// Whether a directory's entire subtree should be skipped.
fn is_skipped_directory(entry: &ignore::DirEntry) -> bool {
    if !entry.file_type().is_some_and(|file_type| file_type.is_dir()) {
        return false;
    }

    let skipped_by_name = entry
        .file_name()
        .to_str()
        .is_some_and(|name| SKIP_DIRECTORIES.contains(&name));

    if skipped_by_name {
        return true;
    }

    is_rust_build_directory(entry.path())
}

/// The file walk: a recursive walk of `root` that honors `.gitignore`
/// (root and nested, with negations, even outside a git repo) on top of the
/// always-skipped [`SKIP_DIRECTORIES`]. Hidden files are kept: the skip list and
/// `.gitignore` decide what to drop, not a blanket dotfile rule.
fn walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);

    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|entry| !is_skipped_directory(entry));

    builder
}

/// The indexing walk, in sorted path order.
///
/// Sorted because the order files reach the store is the order they come back
/// out of it, and every listing a tool renders breaks a tie by falling through
/// to that order. Unsorted, it is the order the filesystem happened to return
/// each directory in: two machines indexing one tree build two stores that
/// answer the same question in two different orders, and the snapshots that pin
/// those answers pass on one machine and fail on the other. Sorting here costs
/// one sort per directory and makes the index a function of the source alone.
pub(crate) fn walk(root: &Path) -> ignore::Walk {
    walk_builder(root).sort_by_file_path(Path::cmp).build()
}

/// The same walk, parallelized across worker threads for the read-only stale
/// check, where the gitignore traversal and per-file stat dominate and there is
/// no shared mutable index state to serialize on.
pub(crate) fn walk_parallel(root: &Path) -> ignore::WalkParallel {
    walk_builder(root).build_parallel()
}

/// The path of `file` relative to `root`, with separators normalized to `/`
/// so node ids stay stable across platforms.
pub(crate) fn relative_path(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let normalized = relative.to_string_lossy().replace('\\', "/");

    if normalized.is_empty() {
        return None;
    }

    Some(normalized)
}

/// A short content fingerprint for change detection. Not cryptographic, only
/// stable within one build of the tool, which is all re-index detection needs.
pub(crate) fn hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);

    let hex = format!("{:016x}", hasher.finish());

    assert!(hex.len() == 16, "a content hash is sixteen hex digits");

    hex
}

/// The file modification time in epoch milliseconds, or 0 when unavailable.
pub(crate) fn modified_ms(path: &Path) -> i64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    let Ok(elapsed) = modified.duration_since(UNIX_EPOCH) else {
        return 0;
    };

    i64::try_from(elapsed.as_millis()).unwrap_or(0)
}

/// A saturating `usize` -> `u32` conversion; per-file counts are bounded well under the cap.
pub(crate) fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
