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

/// The dispatch of a subcommand (`init`, `sync`, `link`, `serve`, `history`,
/// `install`, `uninstall`). A bare path argument indexes that repository; with no
/// argument, run an in-memory smoke check.
fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.split_first() {
        Some((command, rest)) if command == "init" => init_command(rest),
        Some((command, rest)) if command == "sync" => sync_command(rest),
        Some((command, rest)) if command == "link" => link_command(rest),
        Some((command, rest)) if command == "serve" => serve_command(rest),
        Some((command, rest)) if command == "history" => history_command(rest),
        Some((command, _)) if command == "install" => bootstrap::install(),
        Some((command, _)) if command == "uninstall" => bootstrap::uninstall(),
        Some((root, _)) => index_command(root),
        None => smoke_check(),
    }
}

/// The `constellation init [path]` command creates `<path>/.constellation/index.db`
/// (defaulting to the current directory), scaffolds a starter config, indexes the
/// project and its companions, links them, and ingests git history when
/// `[history]` is enabled (the default).
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

    index_and_report(&store, &project, &name, root)?;
    index_history_if_enabled(&store, root)
}

/// The `constellation sync [database]` command re-indexes every project in the
/// constellation from disk (incrementally, skipping unchanged files), re-links it,
/// and prints the summary. A one-shot catch-up: a running `serve` already does this
/// in the background on every change, so `sync` is for refreshing the graph when no
/// server is attached. Git history is refreshed too when `[history]` is enabled.
/// The database is discovered as for `serve`.
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
    // index, then bind them. The workspace root is the directory that holds the
    // discovered `.constellation/`.
    if let Some(workspace_root) = database.parent().and_then(Path::parent)
        && workspace_root.is_dir()
    {
        index_companions(&store, workspace_root)?;
    }

    if store.all_projects()?.len() > 1 {
        link_constellation(&store)?;
    }

    print_store_summary(&store, &database)?;

    if let Some(workspace_root) = database.parent().and_then(Path::parent) {
        index_history_if_enabled(&store, workspace_root)?;
    }

    Ok(())
}

/// The `constellation serve [database]` command serves the constellation graph to an
/// agent over MCP (stdio) and, in the background, watches every indexed project and
/// re-indexes and re-links on each change, so the graph stays current mid-session.
/// With no argument it discovers the database from the `CONSTELLATION_DB`
/// environment variable, or by walking up from the working directory for a
/// `.constellation/index.db`, so one registration serves every project.
fn serve_command(rest: &[String]) -> Result<()> {
    let database = match rest.first() {
        Some(path) => Some(PathBuf::from(path)),
        None => discover_database_optional()?,
    };

    match database {
        Some(database) => {
            if !database.is_file() {
                bail!(
                    "no constellation database at {}; run `constellation init` in the project",
                    database.display(),
                );
            }

            constellation_mcp::serve(&database)?;
        }
        None => constellation_mcp::serve_unavailable()?,
    }

    Ok(())
}

/// The `constellation history [database] [--symbols]` command ingests each indexed
/// project's git commit history into the database so the graph can be read over
/// time. `--symbols` also runs the Tier-2 symbol-delta pass. Behavior follows the
/// workspace's `[history]` config (enabled, symbols, companions, commits_max); the
/// `--symbols` flag forces the symbol pass on regardless. The database is
/// discovered as for `serve`.
fn history_command(rest: &[String]) -> Result<()> {
    let symbols_flag = rest.iter().any(|argument| argument == "--symbols");
    let explicit = rest.iter().find(|argument| !argument.starts_with("--"));

    let database = match explicit {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` first",
            database.display(),
        );
    }

    let workspace_root = database.parent().and_then(Path::parent).map(Path::to_path_buf);
    let config = match &workspace_root {
        Some(root) => constellation_index::load_history_config(root),
        None => constellation_index::HistoryConfig::default(),
    };

    if !config.enabled {
        println!("git history indexing disabled in .constellation/config.toml");

        return Ok(());
    }

    let symbols = symbols_flag || config.symbols;
    let store = Store::open(&database)?;

    ingest_all_history(&store, &config, symbols, workspace_root.as_deref())
}

/// Every indexed project's history ingested per `config`, printing a line each and
/// a final summary. Companion/library projects are skipped when `history.companions`
/// is false, so only the workspace's own code is indexed then.
fn ingest_all_history(
    store: &Store,
    config: &constellation_index::HistoryConfig,
    symbols: bool,
    workspace_root: Option<&Path>,
) -> Result<()> {
    let projects = store.all_projects()?;
    let repositories = workspace_root.map(constellation_index::load_companion_repositories).unwrap_or_default();
    let fingerprint = constellation_index::extractor_fingerprint();

    let mut rows: Vec<HistoryRow> = Vec::new();
    let mut skipped: u32 = 0;

    for project in &projects {
        if !config.companions && !is_workspace_primary(&project.root_path, workspace_root) {
            skipped += 1;

            continue;
        }

        // A companion with a configured repository (its `.venv` copy carries no
        // `.git`) sources history from a full clone of that repo at the tag matching
        // the installed version; everything else reads its own root.
        let history_root = match workspace_root.zip(repositories.get(project.id.as_str())) {
            Some((root, url)) => match constellation_index::fetch_companion_history_repo(root, project.id.as_str(), url) {
                Some(clone) => clone,
                None => continue,
            },
            None => PathBuf::from(&project.root_path),
        };

        // Skip re-ingesting a repository whose HEAD and extractor are unchanged
        // since the last run: the stored rows are still valid, so reuse their counts.
        let stamp = constellation_index::git_head(&history_root)
            .map(|head| format!("{head}|{fingerprint}|{symbols}"));
        let stored = store.git_ingest_stamp(&project.id)?;

        if let Some(stamp) = &stamp
            && stored.as_ref() == Some(stamp)
        {
            let commits = store.count_history_commits(&project.id)?;

            if commits == 0 {
                continue;
            }

            let changes = if symbols { Some(store.count_symbol_revisions(&project.id)?) } else { None };

            rows.push(HistoryRow { label: project.id.as_str().to_string(), commits, changes, cached: true });

            continue;
        }

        let commits = ingest_history_with_progress(store, &project.id, &history_root, config.commits_max)?;

        // Zero commits means the root is not its own git repository (a `.venv` copy
        // shares the workspace's repo); replace_history already cleared any stale
        // rows, so skip rather than misattribute the workspace's history.
        if commits == 0 {
            continue;
        }

        let changes =
            if symbols { Some(ingest_symbols_with_progress(store, &project.id, &history_root)?) } else { None };

        if let Some(stamp) = stamp {
            store.set_git_ingest_stamp(&project.id, &stamp)?;
        }

        rows.push(HistoryRow { label: project.id.as_str().to_string(), commits, changes, cached: false });
    }

    print_history_summary(&rows, skipped);

    Ok(())
}

/// One project's commit history ingested behind the shared gradient progress bar:
/// the Tier-1 read streams `git log` over every commit, so on a large repo it is
/// worth a bar like the index phase.
fn ingest_history_with_progress(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    commits_max: u32,
) -> Result<u32> {
    let mut progress = progress::Progress::new(&format!("history {project}"));

    let commits = constellation_index::ingest_history_reporting(
        store,
        project,
        root,
        commits_max,
        |done, total| {
            progress.on_phase(constellation_index::IndexPhase::Extracting {
                files_done: done,
                files_total: total,
            });
        },
    )?;

    progress.finish();

    Ok(commits)
}

/// One project's symbol history ingested behind the shared gradient progress bar:
/// the Tier-2 pass walks every touched file revision, so it can run long and is
/// worth a bar like the index phase.
fn ingest_symbols_with_progress(store: &Store, project: &ProjectId, root: &Path) -> Result<u32> {
    let mut progress = progress::Progress::new(&format!("symbols {project}"));

    let revisions = constellation_index::ingest_symbol_revisions_reporting(
        store,
        project,
        root,
        |done, total| {
            progress.on_phase(constellation_index::IndexPhase::Extracting {
                files_done: done,
                files_total: total,
            });
        },
    )?;

    progress.finish();

    Ok(revisions)
}

/// One project's history-ingest result for the summary: its label, commit count,
/// and symbol-change count when the Tier-2 pass ran.
struct HistoryRow {
    label: String,
    commits: u32,
    changes: Option<u32>,
    cached: bool,
}

/// The history-ingest summary, aligned like the index summary: one row per project
/// with its commit count, plus its symbol-change count when the symbol pass ran.
fn print_history_summary(rows: &[HistoryRow], skipped: u32) {
    if rows.is_empty() && skipped == 0 {
        return;
    }

    let name_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let commits_width = rows.iter().map(|row| digits(row.commits)).max().unwrap_or(1);

    println!("git history");

    for row in rows {
        let label = &row.label;
        let commits = row.commits;

        let changes = match row.changes {
            Some(changes) => format!("  {changes} symbol changes"),
            None => String::new(),
        };

        let cached = if row.cached { "  (cached)" } else { "" };

        let line = format!("  {label:<name_width$}  {commits:>commits_width$} commits{changes}{cached}");

        println!("{}", line.trim_end());
    }

    if skipped > 0 {
        println!("  ({skipped} companion/library projects skipped; set history.companions = true to include)");
    }
}

/// Whether `project_root` is the workspace's own root, not a companion or library
/// (which roots under a venv or `.constellation/sources`), compared as absolute
/// paths. A missing workspace root treats every project as primary, so nothing is
/// skipped.
fn is_workspace_primary(project_root: &str, workspace_root: Option<&Path>) -> bool {
    let Some(workspace_root) = workspace_root else {
        return true;
    };

    match (std::path::absolute(project_root), std::path::absolute(workspace_root)) {
        (Ok(project), Ok(workspace)) => project == workspace,
        _ => false,
    }
}

/// History ingested for the workspace after a normal index, when `[history] enabled`
/// (the default). Tier 1 always; the Tier-2 symbol pass only when `[history]
/// symbols`; companion/library histories skipped when `[history] companions` is
/// false. A silent no-op when history is disabled, so `init` and `sync` stay fast
/// for workspaces that opt out.
fn index_history_if_enabled(store: &Store, workspace_root: &Path) -> Result<()> {
    let config = constellation_index::load_history_config(workspace_root);

    if !config.enabled {
        return Ok(());
    }

    ingest_all_history(store, &config, config.symbols, Some(workspace_root))
}

/// The database located without an explicit path: the `CONSTELLATION_DB` override,
/// else the nearest `.constellation/index.db` at or above the working directory.
/// Errors when none is found; `serve` uses [`discover_database_optional`] instead,
/// which treats "none found" as a valid (unavailable) outcome rather than an error.
fn discover_database() -> Result<PathBuf> {
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
fn discover_database_optional() -> Result<Option<PathBuf>> {
    if let Ok(path) = std::env::var("CONSTELLATION_DB") {
        return Ok(Some(PathBuf::from(path)));
    }

    let mut directory = std::env::current_dir()?;
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= DISCOVER_DEPTH_MAX, "directory walk exceeded {DISCOVER_DEPTH_MAX} levels");

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
# Lines starting with # are comments. Uncomment a setting to change it from its
# default value.


[companions]
# Index the libraries this project installs (django-spire, etc.) alongside it, so
# imports into them resolve to real definitions instead of <external> stubs.
# Set to false to index this project only.
enabled = true

# The libraries to index, by name. Defaults to the three below when omitted; give
# your own list, or [] to index none while leaving companions enabled.
# packages = [\"django-spire\", \"django-glue\", \"robit\"]

# Libraries to leave out of the default set, without having to re-list the rest.
# exclude = [\"robit\"]

# The virtual-environment folder the libraries are installed in, relative to this
# project or absolute. Defaults to \".venv\".
# venv = \".venv\"

# Extra versions of a library to index side by side for comparison while
# refactoring. Each package = \"git-ref\" entry checks out that ref as its own
# read-only project, from the library's repository (see repositories below).
# versions = { django-spire = \"v1/base\" }

# The git repository for each library, used to fetch its commit history at the tag
# matching the installed version, no local clone needed. Defaults to the
# django-spire / django-glue / robit repos, so library history works out of the
# box; set this to override those or add your own. Cloned into
# .constellation/sources/ and cached.
# repositories = { django-spire = \"https://github.com/stratusadv/django-spire\" }


[history]
# Index this project's git history, so the graph can be read over time
# (constellation_history, constellation_symbol_history, constellation_as_of).
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

/// The workspace indexed, its companions and versions discovered and indexed, the
/// constellation linked, and the compact summary printed. Shared by `init` and the
/// bare-path index so both produce the same output.
fn index_and_report(store: &Store, project: &ProjectId, name: &str, root: &Path) -> Result<()> {
    let mut progress = progress::Progress::new("indexing");

    index_project_reporting(store, project, name, root, |phase| {
        progress.on_phase(phase);
    })?;

    progress.finish();

    index_companions(store, root)?;

    // Relink whenever more than the workspace is present, so a re-index of changed
    // workspace code rebinds to the existing companions, not only when a new one
    // was just added.
    if store.all_projects()?.len() > 1 {
        link_constellation(store)?;
    }

    // Summarize every project in the store, not just the ones indexed this run, so
    // a re-index still lists companions that were already present.
    print_summary_all_projects(store, root)
}

/// The companions and version copies discovered under `workspace_root`, each indexed
/// as its own project (drawing a progress bar) and returned as a summary row. A
/// version copy is marked reference-only in the store after indexing.
fn index_companions(store: &Store, workspace_root: &Path) -> Result<Vec<SummaryRow>> {
    let mut targets = discover_companions(store, workspace_root)?;
    targets.extend(discover_versions(store, workspace_root)?);

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
            source: project_source(&target.package_root, workspace_root, target.reference_only),
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

/// A short source tag for a project's root, relative to the workspace: empty for the
/// workspace itself, `.venv` for an installed copy, `ref` for a version checkout, and
/// `local` for a working copy that overrides the install.
fn project_source(root: &Path, workspace_root: &Path, reference_only: bool) -> &'static str {
    if root == workspace_root {
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
/// link. The workspace root is inferred from the database location
/// (`<workspace>/.constellation/index.db`) to tag the workspace row distinctly.
fn print_store_summary(store: &Store, database: &Path) -> Result<()> {
    let workspace_root = database.parent().and_then(Path::parent).unwrap_or(database);

    print_summary_all_projects(store, workspace_root)
}

/// Every project in the store summarized (files, nodes, source tag) with the
/// cross-project link total, relative to `workspace_root`. Shared by `init`/index and
/// `sync` so both always list the whole constellation, whether each project was
/// freshly indexed this run or already present (a re-index skips companions that
/// already exist, but they still belong in the summary).
fn print_summary_all_projects(store: &Store, workspace_root: &Path) -> Result<()> {
    let mut rows: Vec<SummaryRow> = Vec::new();

    for project in store.all_projects()? {
        rows.push(SummaryRow {
            label: project.id.as_str().to_string(),
            files: store.count_files(&project.id)?,
            nodes: store.count_nodes(&project.id)?,
            source: project_source(Path::new(&project.root_path), workspace_root, project.reference_only),
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

    index_and_report(&store, &project, &name, root)?;
    index_history_if_enabled(&store, root)
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
        assert_eq!(project_name(Path::new("/srv/www/workspace")), "workspace", "the leaf directory names the project");
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
        let workspace = Path::new("/code/workspace");

        assert_eq!(project_source(workspace, workspace, false), "", "the workspace itself has no tag");
        assert_eq!(
            project_source(Path::new("/code/workspace/.venv/Lib/site-packages/robit"), workspace, false),
            ".venv",
            "a site-packages path is the installed copy",
        );
        assert_eq!(
            project_source(Path::new("/code/.constellation/sources/x/pkg"), workspace, true),
            "ref",
            "a reference-only checkout is a version ref",
        );
        assert_eq!(
            project_source(Path::new("/code/django-spire/django_spire"), workspace, false),
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
