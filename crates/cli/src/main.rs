#![forbid(unsafe_code)]

//! constellation CLI: index, watch, link, and serve the cross-project knowledge graph.

mod bootstrap;
mod progress;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_graph::ProjectId;
use constellation_index::{
    discover_companions, index_project_reporting, link_constellation, watch_project,
};
use constellation_store::Store;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A fail-fast bound on directory levels walked while discovering the database.
const DISCOVER_DEPTH_MAX: u32 = 4_096;

/// The dispatch of a subcommand (`init`, `watch`, `link`, `serve`, `install`,
/// `uninstall`). A bare path argument indexes that repository; with no
/// argument, run an in-memory smoke check.
fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.split_first() {
        Some((command, rest)) if command == "init" => init_command(rest),
        Some((command, rest)) if command == "link" => link_command(rest),
        Some((command, rest)) if command == "serve" => serve_command(rest),
        Some((command, rest)) if command == "watch" => watch_command(rest),
        Some((command, _)) if command == "install" => bootstrap::install(),
        Some((command, _)) if command == "uninstall" => bootstrap::uninstall(),
        Some((root, _)) => index_command(root),
        None => smoke_check(),
    }
}

/// The `constellation init [path]` command creates `<path>/.constellation/index.db`
/// (defaulting to the current directory) and indexes the project.
fn init_command(rest: &[String]) -> Result<()> {
    let root_owned = match rest.iter().find(|argument| !argument.starts_with('-')) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };

    let root = root_owned.as_path();

    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }

    // Canonicalize so the stored root is absolute: a relative root (`.`) breaks
    // explore's source loading whenever serve runs from a different directory.
    let canonical = resolve_root(root)?;
    let root = canonical.as_path();

    let database = create_index_directory(root)?;
    let store = Store::open(&database)?;

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());
    let mut progress = progress::Progress::new("indexing");

    let stats = index_project_reporting(&store, &project, &name, root, |phase| {
        progress.on_phase(phase);
    })?;

    progress.finish();

    println!(
        "{NAME} {VERSION} initialized and indexed {}: {} files, {} nodes",
        root.display(),
        stats.files_indexed,
        stats.nodes,
    );

    let companions = index_companions(&store, root)?;

    if companions > 0 {
        let linked = link_constellation(&store)?;

        println!("linked {linked} cross-project edge(s) to {companions} companion package(s)");
    }

    Ok(())
}

/// The `constellation watch <repo>` command indexes the repository, then re-indexes it
/// incrementally whenever its files change, until interrupted.
fn watch_command(rest: &[String]) -> Result<()> {
    let Some(root_argument) = rest.first() else {
        bail!("usage: constellation watch <repo>");
    };

    let root = Path::new(root_argument);

    if !root.is_dir() {
        bail!("not a directory: {root_argument}");
    }

    let canonical = resolve_root(root)?;
    let root = canonical.as_path();

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());

    let database = create_index_directory(root)?;
    let store = Store::open(&database)?;

    println!("{NAME} {VERSION}: watching {} (Ctrl-C to stop)", root.display());

    watch_project(&store, &project, &name, root, |stats| {
        println!(
            "  reindexed: {} changed, {} unchanged, {} removed; {} nodes, {} resolved",
            stats.files_indexed,
            stats.files_unchanged,
            stats.files_removed,
            stats.nodes,
            stats.resolved_edges,
        );
    })?;

    Ok(())
}

/// The `constellation serve [database]` command serves the constellation graph to an agent
/// over MCP (stdio). With no argument it discovers the database from the
/// `CONSTELLATION_DB` environment variable, or by walking up from the working
/// directory for a `.constellation/index.db`, so one registration serves
/// every project.
fn serve_command(rest: &[String]) -> Result<()> {
    let database = match rest.first() {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` in the project",
            database.display(),
        );
    }

    constellation_mcp::serve(&database)?;

    Ok(())
}

/// The database located without an explicit path: the `CONSTELLATION_DB` override,
/// else the nearest `.constellation/index.db` at or above the working directory.
fn discover_database() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CONSTELLATION_DB") {
        return Ok(PathBuf::from(path));
    }

    let mut directory = std::env::current_dir()?;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= DISCOVER_DEPTH_MAX, "directory walk exceeded {DISCOVER_DEPTH_MAX} levels");

        let candidate = directory.join(".constellation").join("index.db");

        if candidate.is_file() {
            return Ok(candidate);
        }

        if !directory.pop() {
            break;
        }
    }

    bail!(
        "no .constellation/index.db found from the working directory; \
         run `constellation init` first, or set CONSTELLATION_DB",
    )
}

/// The index.db path inside a freshly created `<root>/.constellation/`.
fn create_index_directory(root: &Path) -> Result<PathBuf> {
    let index_directory = root.join(".constellation");
    std::fs::create_dir_all(&index_directory)?;

    Ok(index_directory.join("index.db"))
}

/// A project name from a repository root directory. Resolves `.`, `..`, and a
/// trailing-slash root (whose `file_name()` is empty) to the real directory
/// name by canonicalizing, so `constellation .` names the project after the
/// actual folder, not the literal "project". Cross-project linking keys on this
/// name, so a wrong one silently breaks linking.
fn project_name(root: &Path) -> String {
    if let Some(name) = root.file_name().and_then(|name| name.to_str())
        && !name.is_empty()
        && name != "."
        && name != ".."
    {
        return name.to_string();
    }

    let resolved = std::fs::canonicalize(root)
        .ok()
        .and_then(|absolute| absolute.file_name().and_then(|name| name.to_str()).map(str::to_string));

    let name = resolved.unwrap_or_else(|| "project".to_string());

    assert!(!name.is_empty(), "a project name is never empty");

    name
}

/// A root directory resolved to a clean absolute path. Canonicalizes so a relative
/// root (`.`) becomes absolute (a relative `root_path` breaks explore's source
/// loading whenever serve runs from a different directory), then strips the
/// Windows `\\?\` verbatim prefix canonicalize adds, so the stored path stays
/// portable and the file watcher (which dislikes verbatim paths) accepts it.
fn resolve_root(root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(root)?;
    let text = canonical.to_string_lossy();
    let cleaned = text.strip_prefix(r"\\?\").unwrap_or(&text);

    Ok(PathBuf::from(cleaned))
}

/// The companion libraries discovered under `root`'s virtual environment,
/// indexing each new one as its own project, drawing a progress bar per companion.
/// Returns the number of companions indexed this call.
fn index_companions(store: &Store, root: &Path) -> Result<usize> {
    let targets = discover_companions(store, root)?;

    for target in &targets {
        let project = ProjectId::new(target.project_id.as_str());
        let mut progress = progress::Progress::new(&format!("indexing {}", target.project_id));

        let stats = index_project_reporting(
            store,
            &project,
            &target.project_id,
            &target.package_root,
            |phase| progress.on_phase(phase),
        )?;

        progress.finish();

        println!(
            "  + companion {}: {} files, {} nodes from {}",
            target.project_id,
            stats.files_indexed,
            stats.nodes,
            target.package_root.display(),
        );
    }

    Ok(targets.len())
}

/// The `constellation link <database> <repo> [repo ...]` command indexes every repository
/// into one shared constellation database, then links imports across them.
fn link_command(rest: &[String]) -> Result<()> {
    let Some((database, repositories)) = rest.split_first() else {
        bail!("usage: constellation link <database> <repo> [repo ...]");
    };

    if repositories.is_empty() {
        bail!("usage: constellation link <database> <repo> [repo ...]");
    }

    let store = Store::open(Path::new(database))?;

    for repository in repositories {
        let root = Path::new(repository);

        if !root.is_dir() {
            bail!("not a directory: {repository}");
        }

        let canonical = resolve_root(root)?;
        let root = canonical.as_path();

        let name = project_name(root);
        let project = ProjectId::new(name.as_str());

        let mut progress = progress::Progress::new(&format!("indexing {name}"));

        let stats = index_project_reporting(&store, &project, &name, root, |phase| {
            progress.on_phase(phase);
        })?;

        progress.finish();

        println!(
            "  {name}: {} nodes, {} resolved, {} pending",
            stats.nodes, stats.resolved_edges, stats.unresolved_remaining,
        );

        index_companions(&store, root)?;
    }

    let linked = link_constellation(&store)?;

    println!("linked {linked} cross-project edges across {} projects", repositories.len());
    println!("constellation written to {database}");

    Ok(())
}

/// The indexing of a repository given as a positional argument, without creating the index directory first.
fn index_command(root_argument: &str) -> Result<()> {
    let root = Path::new(root_argument);

    if !root.is_dir() {
        bail!("not a directory: {root_argument}");
    }

    let canonical = resolve_root(root)?;
    let root = canonical.as_path();

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());

    let database = create_index_directory(root)?;
    let store = Store::open(&database)?;

    let mut progress = progress::Progress::new("indexing");

    let stats = index_project_reporting(&store, &project, &name, root, |phase| {
        progress.on_phase(phase);
    })?;

    progress.finish();

    println!(
        "{NAME} {VERSION}: indexed {} files ({} unchanged, {} removed, {} skipped): {} nodes, {} structural edges",
        stats.files_indexed,
        stats.files_unchanged,
        stats.files_removed,
        stats.files_skipped,
        stats.nodes,
        stats.edges,
    );
    println!(
        "resolved {} of {} references into edges ({} still pending cross-project linking)",
        stats.resolved_edges, stats.unresolved_refs, stats.unresolved_remaining,
    );
    println!("synthesized {} event edge(s) from JS/Alpine dispatch+listener pairs", stats.synthesized_edges);
    println!("synthesized {} external edge(s) into third-party/stdlib symbols", stats.external_edges);

    let companions = index_companions(&store, root)?;

    if companions > 0 {
        let linked = link_constellation(&store)?;

        println!("linked {linked} cross-project edge(s) to {companions} companion package(s)");
    }

    println!("graph written to {}", database.display());

    Ok(())
}

/// The smoke check that verifies the binary links correctly by opening an in-memory store and printing the schema version.
fn smoke_check() -> Result<()> {
    let store = Store::open_in_memory()?;
    let version = store.schema_version()?;

    assert!(version >= 1, "store must initialize to schema version >= 1");

    println!("{NAME} {VERSION}: in-memory store ready at schema v{version}");
    println!("pass a repository path to index it");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{project_name, resolve_root};

    use std::path::Path;

    #[test]
    fn project_name_takes_the_last_path_segment() {
        assert_eq!(project_name(Path::new("/srv/www/portal")), "portal", "the leaf directory names the project");
        assert_eq!(project_name(Path::new("blog")), "blog", "a bare relative name is taken as-is");
        assert_eq!(project_name(Path::new("a/b/c")), "c", "nested paths use the final segment");
    }

    #[test]
    fn project_name_ignores_a_trailing_separator() {
        assert_eq!(project_name(Path::new("a/b/")), "b", "a trailing slash does not blank the name");
    }

    #[test]
    fn project_name_falls_back_to_the_real_directory_for_dot() {
        // `.` has no file_name, so the fallback canonicalizes to the working
        // directory's actual name, which is always a non-empty segment.
        let name = project_name(Path::new("."));

        assert!(!name.is_empty(), "the dot directory resolves to a real, non-empty name");
        assert_ne!(name, ".", "the literal dot is never returned as the name");
    }

    #[test]
    fn resolve_root_returns_a_clean_absolute_path() {
        let directory = tempfile::tempdir().unwrap();

        let resolved = resolve_root(directory.path()).unwrap();

        assert!(resolved.is_absolute(), "a canonical root is absolute, got {}", resolved.display());
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

        assert!(resolve_root(&missing).is_err(), "canonicalizing an absent path fails");
    }
}
