//! The corpus survey: what the real Django repositories actually look like.
//!
//! constellation's test fixtures are Django code someone invented, and invented
//! Django code is Django code as the author imagines it rather than as it is
//! written. The gap is not academic. Real projects here inherit from mixins the
//! extractor has to recognise as models, declare foreign keys as quoted strings
//! rather than imports, route through two levels of namespaced `include`, and
//! reach templates through a helper rather than through `render`. A fixture
//! written from imagination covers none of that, and a parser refactor that
//! broke all of it would pass.
//!
//! So this task reads the graphs constellation has already built of the real
//! repositories and reports what is in them: which bases, which field shapes,
//! which decorators, which module layout, and what fails to resolve. The output
//! is a shopping list for fixtures, and re-running it later says whether the
//! fixtures still resemble the code they stand in for.
//!
//! One thing the survey deliberately does not report: whether a relation field
//! named its target as a quoted `'app.Model'` or as an imported identifier. The
//! extractor normalizes the quotes away before the signature is stored, so the
//! graph cannot answer it and a section claiming to would be reporting on its
//! own heuristic rather than on the corpus. Read that distinction off the
//! source, or teach the store to keep it.
//!
//! **Nothing from a real repository is copied into this tree.** The survey
//! emits counts and shapes, never source. A fixture is then written fresh, in
//! an invented domain, to exhibit a shape this report says is common. That
//! distinction is the whole point: the extractor sees structure, so structure is
//! what a fixture has to reproduce, and structure is the one thing that carries
//! no business meaning with it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::{Result, workspace_root};

/// The machine-local file naming the repositories to mine.
///
/// Gitignored, because a corpus is a set of absolute paths on one developer's
/// disk and committing them would only be committing something wrong for
/// everybody else.
const CONFIG_FILE: &str = "fixtures.toml";

/// The rows a section prints when the caller does not say. A survey is
/// read to find the shapes worth reproducing, and a long tail of one-off shapes
/// is noise; pass a count to see further down it.
const SECTION_ROWS_DEFAULT: usize = 12;

/// A cap on rows one section prints, so a mistyped argument cannot ask for a
/// listing of every distinct shape in three large repositories.
const SECTION_ROWS_MAX: usize = 500;

/// A cap on repositories one survey opens, so a misconfigured root that points
/// at a whole filesystem fails instead of walking it.
const DATABASES_MAX: usize = 64;

/// A cap on directory entries the discovery walk visits.
const WALK_ENTRIES_MAX: u32 = 100_000;

/// The survey's configuration, as read from [`CONFIG_FILE`].
#[derive(Debug, Deserialize)]
struct Config {
    /// The directory the indexed repositories sit in.
    root: String,

    /// The repositories to mine, by directory name. Omitted or empty means
    /// every indexed repository under `root`.
    #[serde(default)]
    projects: Vec<String>,
}

/// The template written when no configuration exists, so the first run explains
/// itself rather than only failing.
const CONFIG_TEMPLATE: &str = "\
# The corpus the fixture survey mines. Machine-local and gitignored: these are
# absolute paths on one disk, and a committed copy would be wrong everywhere
# else.
#
# Only repositories constellation has already indexed are readable, since the
# survey reads .constellation/index.db rather than parsing source. Index one
# with `constellation init <path>`.

root = \"/path/to/the/directory/holding/your/repositories\"

# The repositories to mine, by directory name. Comment the list out to mine
# every indexed repository under `root`.
projects = [
    \"django-spire\",
    \"your-project\",
]
";

/// The corpus surveyed and reported.
pub fn survey(rows: Option<&str>) -> Result {
    let rows = section_rows(rows)?;
    let root = workspace_root()?;
    let config = load_config(&root)?;
    let databases = discover(&config)?;

    assert!(!databases.is_empty(), "a corpus with no databases is refused before here");
    assert!(databases.len() <= DATABASES_MAX, "the corpus stays inside its cap");

    println!("corpus: {} indexed repositor(ies)\n", databases.len());

    let mut report = Report::default();

    for (name, database) in &databases {
        let connection =
            Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| format!("opening {}: {error}", database.display()))?;

        let nodes = scalar(&connection, "SELECT COUNT(*) FROM nodes")?;
        let edges = scalar(&connection, "SELECT COUNT(*) FROM edges")?;

        println!("  {name}: {nodes} nodes, {edges} edges");

        report.absorb(&connection)?;
    }

    println!();
    report.print(rows);

    Ok(())
}

/// The configuration read from the workspace root, writing the template and
/// failing with an explanation when there is none.
fn load_config(root: &Path) -> Result<Config> {
    let path = root.join(CONFIG_FILE);

    if !path.is_file() {
        fs::write(&path, CONFIG_TEMPLATE)?;

        return Err(format!(
            "no {CONFIG_FILE} found, so a template was written to {}. \
             Edit it to name your indexed repositories, then run this again.",
            path.display(),
        )
        .into());
    }

    let text = fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&text)?;

    assert!(!config.root.is_empty(), "the corpus root must not be empty");

    Ok(config)
}

/// The configured repositories, each paired with the index it was built into.
///
/// A named repository that is not indexed is an error rather than a skip: the
/// survey's numbers are only comparable between runs if the same repositories
/// went into both, so silently dropping one would make two reports differ for a
/// reason the reader cannot see.
fn discover(config: &Config) -> Result<Vec<(String, PathBuf)>> {
    let root = PathBuf::from(&config.root);

    if !root.is_dir() {
        return Err(format!("the corpus root {} is not a directory", root.display()).into());
    }

    let names = match config.projects.is_empty() {
        true => discover_names(&root)?,
        false => config.projects.clone(),
    };

    let mut databases: Vec<(String, PathBuf)> = Vec::new();

    for name in names {
        let database = root.join(&name).join(".constellation").join("index.db");

        if !database.is_file() {
            return Err(format!(
                "{name} has no index at {}; run `constellation init` there, \
                 or drop it from {CONFIG_FILE}",
                database.display(),
            )
            .into());
        }

        databases.push((name, database));
    }

    if databases.is_empty() {
        return Err(format!("no indexed repository found under {}", root.display()).into());
    }

    assert!(databases.len() <= DATABASES_MAX, "the corpus stays inside its cap");

    Ok(databases)
}

/// The directory names under `root` that carry a constellation index, sorted.
///
/// Only the root's immediate children are inspected, because a repository holds
/// its index at its own root; descending further would find the same indexes
/// again through nested checkouts.
fn discover_names(root: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut examined: u32 = 0;

    for entry in fs::read_dir(root)? {
        examined += 1;

        assert!(examined <= WALK_ENTRIES_MAX, "the discovery walk stays bounded");

        let path = entry?.path();

        if path.join(".constellation").join("index.db").is_file()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            names.push(name.to_string());
        }
    }

    names.sort();

    Ok(names)
}

/// A counted shape and how often the corpus carries it.
type Tally = BTreeMap<String, i64>;

/// The whole of what the survey counts, accumulated across every repository so a shape
/// common to all of them outranks one peculiar to the largest.
#[derive(Default)]
struct Report {
    bases: Tally,
    base_combinations: Tally,
    decorators: Tally,
    edge_kinds: Tally,
    field_types: Tally,
    field_kwargs: Tally,
    import_sources: Tally,
    module_names: Tally,
    node_kinds: Tally,
    route_depths: Tally,
    unresolved: Tally,

    /// The names the extractor gave the classes declared in a `models.py`, and
    /// what the ones it did not call models inherit from.
    ///
    /// A class in a `models.py` is a model far more often than not, so these two
    /// sections read together are the closest thing the corpus offers to a
    /// measurement of the model heuristic. A large `class` count here is either
    /// a genuine population of managers, querysets, and enums, or the heuristic
    /// missing an in-house base, and the base combinations say which.
    models_module_kinds: Tally,
    models_module_missed: Tally,
    views_module_kinds: Tally,
}

impl Report {
    /// A repository's counts folded into the running totals.
    fn absorb(&mut self, connection: &Connection) -> Result {
        merge(&mut self.node_kinds, grouped(connection, SQL_NODE_KINDS)?);
        merge(&mut self.edge_kinds, grouped(connection, SQL_EDGE_KINDS)?);
        merge(&mut self.bases, grouped(connection, SQL_BASES)?);
        merge(&mut self.base_combinations, grouped(connection, SQL_BASE_COMBINATIONS)?);
        merge(&mut self.field_types, grouped(connection, SQL_FIELD_TYPES)?);
        merge(&mut self.import_sources, grouped(connection, SQL_IMPORT_SOURCES)?);
        merge(&mut self.unresolved, grouped(connection, SQL_UNRESOLVED)?);
        merge(&mut self.models_module_kinds, grouped(connection, SQL_MODELS_MODULE_KINDS)?);
        merge(&mut self.models_module_missed, grouped(connection, SQL_MODELS_MODULE_MISSED)?);
        merge(&mut self.views_module_kinds, grouped(connection, SQL_VIEWS_MODULE_KINDS)?);

        let signatures = column(connection, SQL_FIELD_SIGNATURES)?;

        for signature in &signatures {
            for keyword in keywords_of(signature) {
                *self.field_kwargs.entry(keyword).or_insert(0) += 1;
            }
        }

        for decorators in column(connection, SQL_DECORATORS)? {
            for decorator in decorators.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                *self.decorators.entry(decorator.to_string()).or_insert(0) += 1;
            }
        }

        for path in column(connection, SQL_FILE_PATHS)? {
            let module = path.rsplit('/').next().unwrap_or(&path).to_string();

            *self.module_names.entry(module).or_insert(0) += 1;
        }

        for reverse_name in column(connection, SQL_REVERSE_NAMES)? {
            let depth = reverse_name.matches(':').count();

            *self.route_depths.entry(format!("{depth} namespace segment(s)")).or_insert(0) += 1;
        }

        Ok(())
    }

    /// The whole report printed, section by section.
    fn print(&self, rows: usize) {
        section(rows, "node kinds", &self.node_kinds);
        section(rows, "edge kinds", &self.edge_kinds);
        section(rows, "base classes", &self.bases);
        section(rows, "base combinations (what a class inherits)", &self.base_combinations);
        section(rows, "model field types", &self.field_types);
        section(rows, "model field keyword arguments", &self.field_kwargs);
        section(rows, "decorators", &self.decorators);
        section(rows, "module names", &self.module_names);
        section(rows, "import sources", &self.import_sources);
        section(rows, "route namespace depth", &self.route_depths);
        let missed = &self.models_module_missed;

        section(rows, "classes in a models.py, by extracted kind", &self.models_module_kinds);
        section(rows, "what a models.py class NOT called a model inherits", missed);
        section(rows, "symbols in a views.py, by extracted kind", &self.views_module_kinds);
        section(rows, "most common unresolved references", &self.unresolved);
    }
}

/// A tally folded into another.
fn merge(into: &mut Tally, from: Tally) {
    for (label, count) in from {
        *into.entry(label).or_insert(0) += count;
    }
}

/// A section printed, highest count first, capped at `rows`.
fn section(rows: usize, title: &str, tally: &Tally) {
    let mut ranked: Vec<(&String, &i64)> = tally.iter().collect();

    ranked.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));

    println!("{title}");

    if ranked.is_empty() {
        println!("  (none)\n");

        return;
    }

    for (label, count) in ranked.iter().take(rows) {
        println!("  {count:>8}  {label}");
    }

    if ranked.len() > rows {
        println!("  {:>8}  ({} more shapes)", "", ranked.len() - rows);
    }

    println!();
}

/// The row cap one run prints, parsed from the optional argument.
fn section_rows(rows: Option<&str>) -> Result<usize> {
    let Some(text) = rows else {
        return Ok(SECTION_ROWS_DEFAULT);
    };

    let parsed: usize =
        text.parse().map_err(|_| format!("rows must be a positive number, got {text:?}"))?;

    if parsed == 0 || parsed > SECTION_ROWS_MAX {
        return Err(format!("rows must be between 1 and {SECTION_ROWS_MAX}, got {parsed}").into());
    }

    Ok(parsed)
}

/// An integer read off a single-row, single-column query.
fn scalar(connection: &Connection, sql: &str) -> Result<i64> {
    let value = connection.query_row(sql, [], |row| row.get(0))?;

    Ok(value)
}

/// A `(label, count)` query run and collected.
fn grouped(connection: &Connection, sql: &str) -> Result<Tally> {
    let mut statement = connection.prepare(sql)?;

    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut tally = Tally::new();

    for row in rows {
        let (label, count) = row?;

        if !label.is_empty() {
            tally.insert(label, count);
        }
    }

    Ok(tally)
}

/// A single-column text query run and collected, skipping nulls.
fn column(connection: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, Option<String>>(0))?;

    let mut values: Vec<String> = Vec::new();

    for row in rows {
        if let Some(value) = row? {
            values.push(value);
        }
    }

    Ok(values)
}

/// The keyword argument names in a field signature, in order.
///
/// A scan rather than a parse: the signature is already a truncated rendering,
/// so it may end mid-argument, and every shape this looks for survives that.
fn keywords_of(signature: &str) -> Vec<String> {
    let mut keywords: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut depth: u32 = 0;

    for character in signature.chars() {
        match character {
            '(' | '[' | '{' => {
                depth += 1;
                token.clear();
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                token.clear();
            }
            ',' => token.clear(),
            '=' if depth == 1 && !token.trim().is_empty() => {
                keywords.push(token.trim().to_string());
                token.clear();
            }
            _ => token.push(character),
        }
    }

    keywords
}

const SQL_NODE_KINDS: &str = "SELECT kind, COUNT(*) FROM nodes GROUP BY kind";

const SQL_EDGE_KINDS: &str = "SELECT kind, COUNT(*) FROM edges GROUP BY kind";

const SQL_BASES: &str = "\
    SELECT t.name, COUNT(*)
    FROM edges e
    JOIN nodes t ON t.id = e.target
    WHERE e.kind = 'extends'
    GROUP BY t.name";

const SQL_BASE_COMBINATIONS: &str = "\
    SELECT combo, COUNT(*) FROM (
        SELECT GROUP_CONCAT(name, ' + ') AS combo
        FROM (
            SELECT e.source AS src, t.name AS name
            FROM edges e
            JOIN nodes t ON t.id = e.target
            WHERE e.kind = 'extends'
            ORDER BY e.source, t.name
        )
        GROUP BY src
    )
    GROUP BY combo";

const SQL_FIELD_TYPES: &str = "\
    SELECT substr(signature, 1, instr(signature, '(') - 1), COUNT(*)
    FROM nodes
    WHERE kind = 'field' AND signature IS NOT NULL AND instr(signature, '(') > 1
    GROUP BY 1";

const SQL_FIELD_SIGNATURES: &str =
    "SELECT signature FROM nodes WHERE kind = 'field' AND signature IS NOT NULL";

const SQL_DECORATORS: &str = "SELECT decorators FROM nodes WHERE decorators IS NOT NULL";

const SQL_FILE_PATHS: &str = "SELECT path FROM files";

const SQL_IMPORT_SOURCES: &str =
    "SELECT source, COUNT(*) FROM import_mappings GROUP BY source";

const SQL_REVERSE_NAMES: &str = "SELECT reverse_name FROM route_reverse_name";

const SQL_MODELS_MODULE_KINDS: &str = "\
    SELECT kind, COUNT(*)
    FROM nodes
    WHERE (file_path LIKE '%/models.py' OR file_path = 'models.py')
      AND kind IN ('class', 'model')
    GROUP BY kind";

const SQL_MODELS_MODULE_MISSED: &str = "\
    SELECT combo, COUNT(*) FROM (
        SELECT GROUP_CONCAT(name, ' + ') AS combo
        FROM (
            SELECT e.source AS src, t.name AS name
            FROM edges e
            JOIN nodes t ON t.id = e.target
            JOIN nodes s ON s.id = e.source
            WHERE e.kind = 'extends'
              AND s.kind = 'class'
              AND (s.file_path LIKE '%/models.py' OR s.file_path = 'models.py')
            ORDER BY e.source, t.name
        )
        GROUP BY src
    )
    GROUP BY combo";

const SQL_VIEWS_MODULE_KINDS: &str = "\
    SELECT kind, COUNT(*)
    FROM nodes
    WHERE file_path LIKE '%views.py'
      AND file_path NOT LIKE '%test%'
      AND kind IN ('class', 'function', 'view')
    GROUP BY kind";

const SQL_UNRESOLVED: &str = "\
    SELECT reference_kind || ' ' || reference_name, COUNT(*)
    FROM unresolved_refs
    GROUP BY 1";
