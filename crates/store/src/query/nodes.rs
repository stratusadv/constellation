//! Finding nodes: by name, by qualified name, by kind, by file, by
//! position, and by full-text search.

use constellation_graph::{Node, NodeKind, ProjectId};
use rusqlite::params;
use rustc_hash::FxHashSet;

use crate::error::{StoreError, charge};
use crate::limits::{FTS_QUERY_TOKENS_MAX, PREALLOC_ROWS_MAX, ROWS_LOADED_MAX};
use crate::mapping::{collect_nodes, collect_nodes_capacity, node_from_row, node_row};
use crate::sql::{EXACT_NAME_SQL, NODE_COLUMNS, NODE_COLUMNS_PREFIXED, SEARCH_ANY_SQL, SEARCH_SQL};
use crate::store::Store;

/// A safe FTS5 prefix query: split the free text on non-word characters,
/// append `*` to each token (so `ArticleList` matches `ArticleListView`), and
/// join the tokens with `separator` directly into one `String`, with no
/// intermediate `Vec<String>` or per-token `format!`. Empty when the query has no
/// word characters.
fn fts_query(query: &str, separator: &str) -> String {
    let mut out = String::with_capacity(query.len() + separator.len());
    let mut tokens: usize = 0;

    for token in query.split(|character: char| !(character.is_alphanumeric() || character == '_')) {
        if token.is_empty() {
            continue;
        }

        // A real bound, not a formality: FTS5 plans one sub-query per term, and
        // a query naming more distinct identifiers than this is prose rather
        // than a search. The tail is dropped instead of failing the call, since
        // the leading terms are the ones the caller meant.
        if tokens >= FTS_QUERY_TOKENS_MAX {
            break;
        }

        tokens += 1;

        if !out.is_empty() {
            out.push_str(separator);
        }

        out.push_str(token);
        out.push('*');
    }

    assert!(tokens <= FTS_QUERY_TOKENS_MAX, "an FTS query stays within its term bound");

    out
}

/// The all-terms (AND) prefix match: every token must be present. The precise form
/// `search` and explore's first pass use.
fn fts_prefix_query(query: &str) -> String {
    fts_query(query, " ")
}

/// The any-term (OR) prefix match: one token suffices. Explore's forgiving fallback
/// for multi-word, natural-language queries an AND match would miss entirely.
fn fts_any_query(query: &str) -> String {
    fts_query(query, " OR ")
}

impl Store {
    /// The nodes, scoped to one project or (with `None`) across every
    /// project in the database. Rows whose stored enums no longer parse are
    /// skipped rather than aborting the load.
    pub fn all_nodes(&self, project: Option<&ProjectId>) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE (?1 IS NULL OR project_id = ?1)",
        )?;

        let rows = statement.query_map(params![project.map(ProjectId::as_str)], node_row)?;

        let mut nodes: Vec<Node> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "node load")?;

            if let Some(node) = node_from_row(row?) {
                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// The nodes, at most `limit` of them, across every project, in no particular order.
    ///
    /// The bounded form of [`Store::all_nodes`], for a scan that carries a cap
    /// of its own. Taking the cap after the load instead means the whole node
    /// table is hydrated (six heap strings a row) so that a prefix of it can be
    /// looked at, which is what the fuzzy name search used to do.
    pub fn nodes_capped(&self, limit: u32) -> Result<Vec<Node>, StoreError> {
        assert!(limit > 0, "a capped scan asks for at least one row");

        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes LIMIT ?1");
        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![limit], node_row)?;

        collect_nodes_capacity(rows, limit)
    }

    /// The nodes in a project matching a fixed single-column predicate (`name = ?2`,
    /// `lower(name) = ?2`, ...) bound to `value`. The predicate is a constant
    /// fragment (never user input), so interpolating it is safe. Backs the
    /// scoped lookups that resolve references without loading the whole graph.
    fn nodes_filtered(
        &self,
        project: &ProjectId,
        predicate: &str,
        value: &str,
    ) -> Result<Vec<Node>, StoreError> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE project_id = ?1 AND {predicate}");
        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![project.as_str(), value], node_row)?;

        let mut nodes: Vec<Node> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "scoped node load")?;

            if let Some(node) = node_from_row(row?) {
                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// The nodes in a project with an exact name.
    pub fn nodes_named_in(&self, project: &ProjectId, name: &str) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "name = ?2", name)
    }

    /// The nodes in a project whose lower-cased name matches `lower_name`.
    pub fn nodes_lower_named_in(
        &self,
        project: &ProjectId,
        lower_name: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "lower(name) = ?2", lower_name)
    }

    /// The nodes in a project with an exact qualified name.
    pub fn nodes_qualified_in(
        &self,
        project: &ProjectId,
        qualified_name: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "qualified_name = ?2", qualified_name)
    }

    /// The nodes of a given kind in a project.
    pub fn nodes_kind_in(
        &self,
        project: &ProjectId,
        kind: NodeKind,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "kind = ?2", kind.as_str())
    }

    /// The nodes declared in one file of a project.
    pub fn nodes_file_in(
        &self,
        project: &ProjectId,
        file_path: &str,
    ) -> Result<Vec<Node>, StoreError> {
        self.nodes_filtered(project, "file_path = ?2", file_path)
    }

    /// The per-kind node counts for one project, as `(kind, count)`, the cheap GROUP
    /// BY that backs a project overview without loading every node.
    pub fn kind_counts(&self, project: &ProjectId) -> Result<Vec<(NodeKind, u32)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT kind, COUNT(*) FROM nodes WHERE project_id = ?1 GROUP BY kind",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut counts: Vec<(NodeKind, u32)> = Vec::new();

        for row in rows {
            let (label, count) = row?;

            if let Some(kind) = NodeKind::from_str_label(&label) {
                counts.push((kind, u32::try_from(count.max(0)).unwrap_or(u32::MAX)));
            }
        }

        Ok(counts)
    }

    /// The nodes matching a prefix full-text search over their names, qualified
    /// names, docstrings, and signatures.
    pub fn search_nodes(&self, query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        self.search_nodes_matching(&fts_prefix_query(query), query, limit, true)
    }

    /// The any-token variant of [`Store::search_nodes`]: matches ANY query token (OR), not all.
    /// The forgiving fallback for multi-word or natural-language explore queries
    /// an all-tokens prefix match would miss.
    pub fn search_nodes_any(&self, query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        self.search_nodes_matching(&fts_any_query(query), query, limit, false)
    }

    /// The files whose source content matches `query`, ranked by full-text relevance
    /// (bm25 over the porter-stemmed body index), as `(project, file_path)`.
    /// Explore seeds its structural ranking from the definitions in these files,
    /// so a method found only by an identifier in its body still surfaces. Empty
    /// for a database indexed before content was captured.
    pub fn search_content(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<(ProjectId, String)>, StoreError> {
        let match_query = fts_any_query(query);

        if match_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = self.connection.prepare_cached(
            "SELECT fc.project_id, fc.file_path FROM file_content_fts f
             JOIN file_content fc ON fc.rowid = f.rowid
             WHERE file_content_fts MATCH ?1 ORDER BY bm25(file_content_fts) LIMIT ?2",
        )?;

        let rows = statement.query_map(params![match_query, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut hits: Vec<(ProjectId, String)> =
            Vec::with_capacity((limit as usize).min(PREALLOC_ROWS_MAX));

        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "content hit load")?;

            let (project_id, file_path) = row?;
            hits.push((ProjectId::new(project_id), file_path));
        }

        Ok(hits)
    }

    /// The ranked matches for an FTS query.
    ///
    /// Two reads, merged: the symbols actually *named* `query`, then the
    /// full-text matches by relevance. Asking for `Inventory` means the thing
    /// called `Inventory`, whatever else contains the string, and no relevance
    /// score reliably expresses that on a corpus where a hundred symbols share a
    /// domain word. Within the full-text half, a name that starts with the query
    /// is floated above one that merely contains it, which is the same
    /// preference applied to the rows already fetched rather than to every match
    /// in the index.
    fn search_nodes_matching(
        &self,
        match_query: &str,
        query: &str,
        limit: u32,
        ranked: bool,
    ) -> Result<Vec<Node>, StoreError> {
        if match_query.is_empty() {
            return Ok(Vec::new());
        }

        let query = query.trim();

        let mut merged = self.exact_name_matches(query, limit)?;
        let mut seen: FxHashSet<String> =
            merged.iter().map(|node| node.id.as_str().to_string()).collect();

        assert!(merged.len() <= limit as usize, "the exact half respects the limit");

        let sql: &str = if ranked { &SEARCH_SQL } else { &SEARCH_ANY_SQL };

        let mut statement = self.connection.prepare_cached(sql)?;
        let rows = statement.query_map(params![match_query, limit], node_row)?;
        let matched: Vec<Node> = collect_nodes_capacity(rows, limit)?;

        let lowered = query.to_lowercase();
        let (prefixed, rest): (Vec<Node>, Vec<Node>) = matched
            .into_iter()
            .filter(|node| !seen.contains(node.id.as_str()))
            .partition(|node| node.name.to_lowercase().starts_with(&lowered));

        for node in prefixed.into_iter().chain(rest) {
            if merged.len() >= limit as usize {
                break;
            }

            if seen.insert(node.id.as_str().to_string()) {
                merged.push(node);
            }
        }

        assert!(merged.len() <= limit as usize, "the merged result respects the limit");

        Ok(merged)
    }

    /// The symbols whose name equals `query`, case-insensitively. Index-backed
    /// and cheap, which is why the exact-match preference lives here rather than
    /// in [`SEARCH_SQL`]'s `ORDER BY`.
    fn exact_name_matches(&self, query: &str, limit: u32) -> Result<Vec<Node>, StoreError> {
        // A multi-word query names no single symbol, so the lookup would always
        // miss; skipping it keeps the common natural-language path to one read.
        if query.is_empty() || query.contains(char::is_whitespace) {
            return Ok(Vec::new());
        }

        let mut statement = self.connection.prepare_cached(&EXACT_NAME_SQL)?;
        let rows = statement.query_map(params![query, limit], node_row)?;

        collect_nodes_capacity(rows, limit)
    }

    /// Whether any node's qualified name is, or ends at a name boundary with,
    /// `expected` (so `Order.total` finds `app/models.py::Order.total`).
    ///
    /// An existence check, deliberately not a search: it does not rank and it
    /// does not truncate. Its callers use it to tell "this symbol is not in the
    /// graph" apart from "the ranker did not return it", and a limited search
    /// cannot distinguish those, which is exactly how a ranking defect once got
    /// reported as a stale goldset.
    pub fn node_exists_named(&self, expected: &str) -> Result<bool, StoreError> {
        assert!(!expected.is_empty(), "an expectation must name something");

        let mut statement = self.connection.prepare_cached(
            "SELECT 1 FROM nodes
             WHERE qualified_name = ?1
                OR qualified_name LIKE '%::' || ?1
                OR qualified_name LIKE '%.' || ?1
             LIMIT 1",
        )?;

        let found = statement.exists(params![expected])?;

        Ok(found)
    }

    /// The nodes with the given simple name, across all projects.
    pub fn nodes_named(&self, name: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE name = ?1",
        )?;

        let rows = statement.query_map(params![name], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes whose name equals `suffix` or ends with `/suffix`, addressing a node
    /// by its basename, chiefly a template by filename (`research_page.html` finds
    /// `partner/page/research_page.html`). Bounded; a fallback for when an
    /// exact-name lookup found nothing.
    pub fn nodes_named_suffix(&self, suffix: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE name = ?1 OR name LIKE '%/' || ?1 LIMIT 50",
        )?;

        let rows = statement.query_map(params![suffix], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes with an exact qualified name, across all projects. Lets a tool
    /// target the precise node it printed (`file.py::Owner.member`, or a route's
    /// `file.py::route::<url>`) regardless of how its display name collides.
    pub fn nodes_qualified(&self, qualified_name: &str) -> Result<Vec<Node>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, project_id, kind, name, qualified_name, file_path, language,
                    start_line, end_line, start_column, end_column,
                    docstring, signature, visibility,
                    is_exported, is_async, is_static, is_abstract, decorators, updated_at
             FROM nodes WHERE qualified_name = ?1",
        )?;

        let rows = statement.query_map(params![qualified_name], node_row)?;

        collect_nodes(rows)
    }

    /// The nodes whose file path ends with `file_suffix` and whose span covers
    /// `line`, innermost (smallest span) first. Backs `constellation_at`: a
    /// file:line from a traceback or grep hit mapped to its enclosing symbol. The
    /// suffix match lets a bare `views.py` or a longer `app/views.py` both hit.
    pub fn nodes_at(&self, file_suffix: &str, line: u32) -> Result<Vec<Node>, StoreError> {
        assert!(line >= 1, "line is 1-based");

        let pattern = format!("%{}", file_suffix.replace('\\', "/"));

        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes
             WHERE file_path LIKE ?1 AND start_line <= ?2 AND end_line >= ?2
             ORDER BY (end_line - start_line) ASC LIMIT 8",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params![pattern, line], node_row)?;

        collect_nodes(rows)
    }

    /// The definition nodes in `project` with no incoming edge other than structural
    /// containment: nothing calls, imports, instantiates, tests, relates to, or
    /// extends them. Candidate dead code (an LLM should verify: a symbol reached only
    /// by a framework convention - a management command's `handle`, a signal receiver,
    /// a serialized name - has no static edge and surfaces here too, so the caller
    /// filters those by path/name). Functions, methods, classes, and models only;
    /// ordered by location, bounded by `limit`.
    pub fn orphan_definitions(&self, project: &ProjectId, limit: u32) -> Result<Vec<Node>, StoreError> {
        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes n
             WHERE n.project_id = ?1
               AND n.kind IN ('function', 'method', 'class', 'model')
               AND NOT EXISTS (
                   SELECT 1 FROM edges e WHERE e.target = n.id AND e.kind != 'contains'
               )
             ORDER BY n.file_path, n.start_line
             LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![project.as_str(), limit], node_row)?;

        collect_nodes(rows)
    }

    /// The definition nodes in `project`'s `file_path` whose source span overlaps the
    /// 1-based line range `[start_line, end_line]`: the symbols a diff hunk touched.
    /// Innermost (smallest span) first, so the tightest enclosing definition leads.
    /// Functions, methods, classes, models, and properties, the editable units.
    pub fn nodes_in_range(
        &self,
        project: &ProjectId,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<Node>, StoreError> {
        assert!(start_line >= 1, "start_line is 1-based");
        assert!(start_line <= end_line, "start_line must not exceed end_line");

        let normalized = file_path.replace('\\', "/");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED} FROM nodes n
             WHERE n.project_id = ?1 AND n.file_path = ?2
               AND n.start_line <= ?4 AND n.end_line >= ?3
               AND n.kind IN ('function', 'method', 'class', 'model', 'property')
             ORDER BY (n.end_line - n.start_line) ASC",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement
            .query_map(params![project.as_str(), normalized, start_line, end_line], node_row)?;

        collect_nodes(rows)
    }
}
