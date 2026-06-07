#![forbid(unsafe_code)]

//! constellation CLI: index, sync, link, and serve the cross-project knowledge graph.

mod bootstrap;
mod progress;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_graph::ProjectId;
use constellation_index::{
    discover_companions, discover_versions, index_project_reporting, link_constellation,
    refresh_constellation,
};
use constellation_store::Store;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const NAME: &str = "constellation";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A fail-fast bound on directory levels walked while discovering the database.
const DISCOVER_DEPTH_MAX: u32 = 4_096;

/// The dispatch of a subcommand (`init`, `sync`, `link`, `serve`, `install`,
/// `uninstall`). A bare path argument indexes that repository; with no argument,
/// run an in-memory smoke check.
fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.split_first() {
        Some((command, rest)) if command == "init" => init_command(rest),
        Some((command, rest)) if command == "sync" => sync_command(rest),
        Some((command, rest)) if command == "link" => link_command(rest),
        Some((command, rest)) if command == "serve" => serve_command(rest),
        Some((command, _)) if command == "install" => bootstrap::install(),
        Some((command, _)) if command == "uninstall" => bootstrap::uninstall(),
        Some((root, _)) => index_command(root),
        None => smoke_check(),
    }
}

/// The `constellation init [path]` command creates `<path>/.constellation/index.db`
/// (defaulting to the current directory), scaffolds a starter config, indexes the
/// project and its companions, and links them.
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
    scaffold_config(root);

    let store = Store::open(&database)?;

    let name = project_name(root);
    let project = ProjectId::new(name.as_str());

    index_and_report(&store, &project, &name, root)
}

/// The `constellation sync [database]` command re-indexes every project in the
/// constellation from disk (incrementally, skipping unchanged files), re-links it,
/// and prints the summary. A one-shot catch-up: a running `serve` already does this
/// in the background on every change, so `sync` is for refreshing the graph when no
/// server is attached. The database is discovered as for `serve`.
fn sync_command(rest: &[String]) -> Result<()> {
    let database = match rest.first() {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` first",
            database.display(),
        );
    }

    let store = Store::open(&database)?;

    refresh_constellation(&store)?;

    // Pick up companions or versions newly added to the config since the last
    // index, then bind them. The portal root is the directory that holds the
    // discovered `.constellation/`.
    if let Some(portal_root) = database.parent().and_then(Path::parent)
        && portal_root.is_dir()
    {
        index_companions(&store, portal_root)?;
    }

    if store.all_projects()?.len() > 1 {
        link_constellation(&store)?;
    }

    print_store_summary(&store, &database)
}

/// The `constellation serve [database]` command serves the constellation graph to an
/// agent over MCP (stdio) and, in the background, watches every indexed project and
/// re-indexes and re-links on each change, so the graph stays current mid-session.
/// With no argument it discovers the database from the `CONSTELLATION_DB`
/// environment variable, or by walking up from the working directory for a
/// `.constellation/index.db`, so one registration serves every project.
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

/// A commented starter `.constellation/config.toml`, written only when none
/// exists, so a developer discovers where to enable companion packages and add
/// extra version sources. Best-effort: a write failure is reported, never fatal,
/// and an existing config is never clobbered.
fn scaffold_config(root: &Path) {
    let path = root.join(".constellation").join("config.toml");

    if path.exists() {
        return;
    }

    let starter = "\
# Constellation configuration.
[companions]
enabled = true
# packages = [\"django-spire\", \"django-glue\", \"robit\"]
# venv = \".venv\"

# Index other git refs of an installed companion to compare side by side while
# refactoring. Each \"package@ref\" becomes its own project (django-spire@refactor/next),
# rooted like the .venv copy; the repository is found from the install itself.
# Clients still link to the installed version.
# versions = [\"django-spire@refactor/next\"]
";

    if let Err(error) = std::fs::write(&path, starter) {
        eprintln!("constellation: could not write starter config: {error}");
    }
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

/// The portal indexed, its companions and versions discovered and indexed, the
/// constellation linked, and the compact summary printed. Shared by `init` and the
/// bare-path index so both produce the same output.
fn index_and_report(store: &Store, project: &ProjectId, name: &str, root: &Path) -> Result<()> {
    let mut progress = progress::Progress::new("indexing");

    index_project_reporting(store, project, name, root, |phase| {
        progress.on_phase(phase);
    })?;

    progress.finish();

    let companions = index_companions(store, root)?;

    // Relink whenever more than the portal is present, so a re-index of changed
    // portal code rebinds to the existing companions, not only when a new one
    // was just added.
    if store.all_projects()?.len() > 1 {
        link_constellation(store)?;
    }

    let mut rows = vec![SummaryRow {
        label: name.to_string(),
        files: store.count_files(project)?,
        nodes: store.count_nodes(project)?,
        source: "",
    }];

    rows.extend(companions);

    print_constellation_summary(&rows, store.count_links()?);

    Ok(())
}

/// The companions and version copies discovered under `portal_root`, each indexed
/// as its own project (drawing a progress bar) and returned as a summary row. A
/// version copy is marked reference-only in the store after indexing.
fn index_companions(store: &Store, portal_root: &Path) -> Result<Vec<SummaryRow>> {
    let mut targets = discover_companions(store, portal_root)?;
    targets.extend(discover_versions(store, portal_root)?);

    let mut rows: Vec<SummaryRow> = Vec::with_capacity(targets.len());

    for target in &targets {
        let project = ProjectId::new(target.project_id.as_str());
        let mut progress = progress::Progress::new(&format!("indexing {}", target.project_id));

        index_project_reporting(store, &project, &target.project_id, &target.package_root, |phase| {
            progress.on_phase(phase);
        })?;

        progress.finish();

        if target.reference_only {
            store.set_reference_only(&project, true)?;
        }

        rows.push(SummaryRow {
            label: target.project_id.clone(),
            files: store.count_files(&project)?,
            nodes: store.count_nodes(&project)?,
            source: project_source(&target.package_root, portal_root, target.reference_only),
        });
    }

    Ok(rows)
}

/// One row of the index summary: a project, its file and node totals, and a short
/// tag for where its source lives.
struct SummaryRow {
    label: String,
    files: u32,
    nodes: u32,
    source: &'static str,
}

/// A short source tag for a project's root, relative to the portal: empty for the
/// portal itself, `.venv` for an installed copy, `ref` for a version checkout, and
/// `local` for a working copy that overrides the install.
fn project_source(root: &Path, portal_root: &Path, reference_only: bool) -> &'static str {
    if root == portal_root {
        return "";
    }

    if root.components().any(|part| part.as_os_str() == "site-packages") {
        return ".venv";
    }

    if reference_only {
        return "ref";
    }

    "local"
}

/// The number of decimal digits in `value`, at least one, for column alignment.
fn digits(value: u32) -> usize {
    value.to_string().len()
}

/// The compact constellation summary: a version header, one aligned row per
/// project (name, file and node totals, source tag), and the cross-project link
/// total. Columns are sized to the rows, so it stays readable on a narrow terminal.
fn print_constellation_summary(rows: &[SummaryRow], links: u32) {
    let name_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let files_width = rows.iter().map(|row| digits(row.files)).max().unwrap_or(1);
    let nodes_width = rows.iter().map(|row| digits(row.nodes)).max().unwrap_or(1);

    println!("{NAME} {VERSION}");

    for row in rows {
        let label = &row.label;
        let files = row.files;
        let nodes = row.nodes;
        let source = row.source;

        let line = format!(
            "  {label:<name_width$}  {files:>files_width$} files  {nodes:>nodes_width$} nodes  {source}",
        );

        println!("{}", line.trim_end());
    }

    if links > 0 {
        println!("  {links} cross-project links");
    }
}

/// The summary built from every project already in `store`, used after a sync or a
/// link. The portal root is inferred from the database location
/// (`<portal>/.constellation/index.db`) to tag the portal row distinctly.
fn print_store_summary(store: &Store, database: &Path) -> Result<()> {
    let portal_root = database.parent().and_then(Path::parent).unwrap_or(database);

    let mut rows: Vec<SummaryRow> = Vec::new();

    for project in store.all_projects()? {
        rows.push(SummaryRow {
            label: project.id.as_str().to_string(),
            files: store.count_files(&project.id)?,
            nodes: store.count_nodes(&project.id)?,
            source: project_source(Path::new(&project.root_path), portal_root, project.reference_only),
        });
    }

    print_constellation_summary(&rows, store.count_links()?);

    Ok(())
}

/// The `constellation link <database> <repo> [repo ...]` command indexes every
/// repository into one shared constellation database, links imports across them,
/// and prints the summary.
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

        index_project_reporting(&store, &project, &name, root, |phase| {
            progress.on_phase(phase);
        })?;

        progress.finish();

        index_companions(&store, root)?;
    }

    link_constellation(&store)?;

    print_store_summary(&store, Path::new(database))
}

/// The indexing of a repository given as a positional argument, reusing the
/// existing `.constellation/` if present.
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

    index_and_report(&store, &project, &name, root)
}

/// The smoke check that verifies the binary links correctly by opening an in-memory store and printing the schema version.
fn smoke_check() -> Result<()> {
    let store = Store::open_in_memory()?;
    let fingerprint = store.schema_version()?;

    assert!(fingerprint != 0, "an initialized store carries a schema fingerprint");

    println!("{NAME} {VERSION}: in-memory store ready (schema {fingerprint:#010x})");
    println!("pass a repository path to index it");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{digits, project_name, project_source, resolve_root};

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

    #[test]
    fn project_source_tags_each_origin() {
        let portal = Path::new("/code/portal");

        assert_eq!(project_source(portal, portal, false), "", "the portal itself has no tag");
        assert_eq!(
            project_source(Path::new("/code/portal/.venv/Lib/site-packages/robit"), portal, false),
            ".venv",
            "a site-packages path is the installed copy",
        );
        assert_eq!(
            project_source(Path::new("/code/.constellation/sources/x/pkg"), portal, true),
            "ref",
            "a reference-only checkout is a version ref",
        );
        assert_eq!(
            project_source(Path::new("/code/django-spire/django_spire"), portal, false),
            "local",
            "a working copy outside the venv is a local override",
        );
    }

    #[test]
    fn digits_counts_decimal_places() {
        assert_eq!(digits(0), 1, "zero is one digit");
        assert_eq!(digits(7), 1);
        assert_eq!(digits(9477), 4);
    }
}
