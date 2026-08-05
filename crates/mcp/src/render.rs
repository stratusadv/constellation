//! Turning graph values into the text an agent reads.
//!
//! The rendered text is the contract. An agent never sees a `Node`, only the
//! line this module writes for it, so the formatting here is API surface and
//! changing it changes what every tool returns.

use std::fmt::Write as _;
use std::path::Path;

use constellation_graph::{
    EdgeKind, Node, NodeKind, is_generated_path, is_test_path,
};
use rmcp::model::{CallToolResult, Content};
use rustc_hash::FxHashMap;

use crate::limits::{
    EXPLORE_BYTES_MAX, EXPLORE_FULL_FILES_MAX, EXPLORE_LINES_MAX, EXPLORE_NEIGHBOURS_MAX,
    EXPLORE_RANKED_MAX, EXPLORE_SYMBOLS_PER_FILE_MAX, NODE_BODY_LINES_MAX,
    NODE_LINE_SIGNATURE_CHARS_MAX, OUTLINE_DEPTH_MAX,
};
use crate::rank::{
    exact_name_hits, file_has_token, name_token_coverage, path_token_coverage, query_tokens,
    weighted_token_score,
};
use crate::recency;
use crate::server::working_tree_marker;
use crate::symbols::targetable_name;
use crate::text::truncate_at_boundary;

/// The related edges with repeats to the same target collapsed into one row carrying its
/// multiplicity, preserving first-seen order: a view that calls `qs.get()`
/// twelve times lists the target once as `×12`, not twelve identical lines.
pub(crate) fn dedup_related(related: Vec<(EdgeKind, Node)>) -> Vec<(EdgeKind, Node, usize)> {
    let mut order: Vec<(EdgeKind, Node, usize)> = Vec::new();
    let mut index: FxHashMap<(EdgeKind, String), usize> = FxHashMap::default();

    for (kind, node) in related {
        let key = (kind, node.id.as_str().to_string());

        match index.get(&key) {
            Some(&position) => order[position].2 += 1,
            None => {
                index.insert(key, order.len());
                order.push((kind, node, 1));
            }
        }
    }

    order
}

/// A " (5 calls, 4 relates_to)" breakdown of deduped related edges by kind,
/// most common first, or an empty string when there are none. Lets `node` show a
/// symbol's usage shape in one call, consistent with callers/callees (both drop
/// containment and dedup), so the printed count matches the rows those tools list.
pub(crate) fn edge_kind_breakdown(related: &[(EdgeKind, Node, usize)]) -> String {
    if related.is_empty() {
        return String::new();
    }

    let mut counts: FxHashMap<&str, u32> = FxHashMap::default();

    for (kind, _, _) in related {
        *counts.entry(kind.as_str()).or_insert(0) += 1;
    }

    let mut pairs: Vec<(&str, u32)> = counts.into_iter().collect();
    pairs.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));

    let body = pairs
        .iter()
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect::<Vec<String>>()
        .join(", ");

    format!(" ({body})")
}

/// A compact "55 import, 2 method" summary of node kinds, most common first.
pub(crate) fn summarize_kinds(nodes: &[Node]) -> String {
    let mut counts: FxHashMap<&str, u32> = FxHashMap::default();

    for node in nodes {
        *counts.entry(node.kind.as_str()).or_insert(0) += 1;
    }

    let mut pairs: Vec<(&str, u32)> = counts.into_iter().collect();
    pairs.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));

    pairs.iter().map(|(kind, count)| format!("{count} {kind}")).collect::<Vec<String>>().join(", ")
}

/// The knobs one explore render takes, grouped so they cannot be transposed at
/// the call site.
pub(crate) struct RenderRequest<'a> {
    pub(crate) budget: usize,
    pub(crate) max_files: u32,
    pub(crate) outline: bool,
    pub(crate) query: &'a str,
}

/// The source of ranked nodes emitted grouped by file: each file's relevant
/// symbols in source order, with no line printed twice. Container nodes (a
/// whole file or module) are dropped (their members carry the source) and a
/// symbol fully contained in one already emitted for the file is skipped, so a
/// ranked class and its ranked methods render once, not three times over.
/// Bounded by `max_files`, the byte budget, and a hard line cap.
///
/// Returns the rendered text and the positions actually written, so a caller
/// annotating the result (a coverage note, a hint) speaks about what the reader
/// can see rather than about the wider candidate set ranking started from.
pub(crate) fn render_ranked(
    nodes: &[Node],
    ranked: &[usize],
    roots: &FxHashMap<String, String>,
    request: &RenderRequest<'_>,
    recency: impl Fn(&str, &str) -> f64,
) -> (String, Vec<usize>) {
    let budget = request.budget;

    assert!(budget <= EXPLORE_BYTES_MAX, "byte budget stays within the cap");

    let tokens = query_tokens(request.query);
    let (file_order, by_file, named_prefix) =
        group_by_file(nodes, ranked, request.max_files, &tokens, recency);

    // In outline mode no file renders full source; otherwise the top few do and
    // the rest fall through to signature-only outlines. The count is cut to the
    // files that actually name a query word: a file admitted on token mass alone is
    // not worth the response's most expensive budget. When nothing names a query
    // word the query is a pure content match, where a body hit is the only evidence
    // there is, so the plain cap stands.
    let full_files = if request.outline {
        0
    } else if named_prefix == 0 {
        EXPLORE_FULL_FILES_MAX
    } else {
        EXPLORE_FULL_FILES_MAX.min(named_prefix)
    };

    // The most query tokens any single rendered symbol's name covers. When a
    // multi-word query has no symbol tying two of its words together, the result
    // is a scattered content/structure match, worth flagging so the agent can
    // sharpen with a specific identifier.
    let best_coverage = file_order
        .iter()
        .filter_map(|file_key| by_file.get(file_key))
        .map(|positions| name_token_coverage(nodes, positions, &tokens))
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    let mut emitted: Vec<usize> = Vec::new();
    let mut budget = budget;
    let mut lines: usize = 0;

    for (file_index, file_key) in file_order.iter().enumerate() {
        let positions = by_file.get(file_key).expect("every ordered file has a group");

        let Some(node) = positions.first().map(|&position| &nodes[position]) else {
            continue;
        };

        // Mark the transition from full source to signature-only outlines once.
        // Skipped in outline mode, where every file is a signature outline.
        if file_index == full_files && full_files > 0 {
            if file_index == named_prefix {
                out.push_str(
                    "# (below: files matching only part of a query word, e.g. `line` from \
                     `production_line_type`; signatures only. Name one of these to read it.)\n\n",
                );
            } else {
                out.push_str(
                    "# (more relevant files: signatures only; explore or node for full source)\n\n",
                );
            }
        }

        let available = positions.len();
        let focused = focused_positions(nodes, positions, &tokens);
        let positions = focused.as_deref().unwrap_or(positions);

        let within_budget = if file_index < full_files {
            match load_source(roots, node) {
                Some(source) => emit_file_source(
                    &mut out,
                    &source,
                    nodes,
                    positions,
                    &mut emitted,
                    &mut budget,
                    &mut lines,
                ),
                None => emit_file_outline(
                    &mut out,
                    nodes,
                    positions,
                    available,
                    &mut emitted,
                    &mut budget,
                    &mut lines,
                ),
            }
        } else {
            emit_file_outline(
                &mut out,
                nodes,
                positions,
                available,
                &mut emitted,
                &mut budget,
                &mut lines,
            )
        };

        if !within_budget {
            out.push_str("... (output budget reached)\n");
            break;
        }
    }

    if tokens.len() >= 3 && best_coverage <= 1 && !out.is_empty() {
        out.push_str(
            "\n(low confidence: no symbol matches more than one of your query words; these are \
             scattered content/structure matches. For a sharper result, pass an exact \
             class/function/method name, or one or two specific identifiers.)\n",
        );
    }

    (out, emitted)
}

/// The ranked node positions grouped by file, in order of first (most relevant)
/// appearance, dropping container nodes and test files and admitting at most
/// `max_files` distinct files. Members of an already-admitted file are kept
/// past the limit, so a file's whole relevant surface renders together.
fn group_by_file(
    nodes: &[Node],
    ranked: &[usize],
    max_files: u32,
    tokens: &[String],
    recency: impl Fn(&str, &str) -> f64,
) -> (Vec<String>, FxHashMap<String, Vec<usize>>, usize) {
    let mut file_order: Vec<String> = Vec::new();
    let mut by_file: FxHashMap<String, Vec<usize>> = FxHashMap::default();
    let mut rwr_rank: FxHashMap<String, usize> = FxHashMap::default();

    for (rank, &position) in ranked.iter().take(EXPLORE_RANKED_MAX).enumerate() {
        assert!(position < nodes.len(), "ranked position indexes a node");

        let node = &nodes[position];

        if matches!(node.kind, NodeKind::File | NodeKind::Module)
            || is_test_path(&node.file_path)
            || is_generated_path(&node.file_path)
        {
            continue;
        }

        let file_key = file_key(node.project_id.as_str(), &node.file_path);

        // Keep every ranked position per file here (uncapped) so the file-ranking
        // signals below see the file's whole matched surface: a deep method whose
        // name covers the query must still count. The per-file render cap is
        // applied later, at emit time, over these rank-ordered positions.
        match by_file.get_mut(&file_key) {
            Some(positions) => positions.push(position),
            None => {
                rwr_rank.insert(file_key.clone(), rank);
                file_order.push(file_key.clone());
                by_file.insert(file_key, vec![position]);
            }
        }
    }

    let keys = file_sort_keys(nodes, &file_order, &by_file, &rwr_rank, tokens, recency);

    assert!(keys.len() == file_order.len(), "every admitted file carries a sort key");

    // Positions are sorted rather than the keys themselves, so the comparison
    // can read both the key and the file name without either borrowing the
    // vector being reordered.
    //
    // The comparison ends in the file key itself, because every file that no
    // seed reached shares `usize::MAX` for its walk rank and would otherwise be
    // left in the order the store returned them, which is the order the tree was
    // walked in.
    let mut order: Vec<usize> = (0..file_order.len()).collect();

    order.sort_by(|&left, &right| {
        keys[right]
            .path_coverage
            .cmp(&keys[left].path_coverage)
            .then(keys[right].name_coverage.cmp(&keys[left].name_coverage))
            .then(keys[right].exact_names.cmp(&keys[left].exact_names))
            .then(keys[right].weighted.cmp(&keys[left].weighted))
            .then(keys[left].walk_rank.cmp(&keys[right].walk_rank))
            .then_with(|| file_order[left].cmp(&file_order[right]))
    });

    order.truncate(max_files as usize);

    // How many leading files name any query word at all, in a symbol name or in the
    // path. All three name signals sort ahead of the IDF mass, so these are a prefix.
    //
    // The rest reached this listing on token mass alone, which for a compound query
    // word is mass from its *parts*: `production_line_type` splits, and `line` alone
    // pulls in every SalesOrderLineItem in the repo. Full source is the expensive
    // budget in this response, and spending it on a file that matches none of the
    // words asked for is how a 350-line answer ends up 60% about the wrong models.
    let named_prefix =
        order.iter().take_while(|&&position| keys[position].names_a_query_word()).count();

    let ranked_files: Vec<String> =
        order.into_iter().map(|position| file_order[position].clone()).collect();

    assert!(named_prefix <= ranked_files.len(), "the named prefix is part of the listing");

    (ranked_files, by_file, named_prefix)
}

/// The relevance of one file to the query, as the tuple its listing sorts on.
///
/// Named fields rather than a positional tuple because the comparison reads
/// four of them descending and one ascending, and a transposition there is a
/// ranking bug that no test would obviously catch.
#[derive(Clone, Copy, Default)]
struct FileSortKey {
    /// The compound (underscored) query tokens appearing in the file's path.
    path_coverage: usize,
    /// The most query tokens any one symbol name in the file covers.
    name_coverage: usize,
    /// The query tokens that exactly name a symbol defined here.
    exact_names: usize,
    /// The IDF-weighted token relevance, plus the capped recency bonus.
    weighted: u64,
    /// The position the structural walk put this file at, or `usize::MAX` if no seed
    /// reached it.
    walk_rank: usize,
}

impl FileSortKey {
    /// The key for a file nothing matched and no seed reached, which sorts last.
    const NEUTRAL: Self = Self {
        path_coverage: 0,
        name_coverage: 0,
        exact_names: 0,
        weighted: 0,
        walk_rank: usize::MAX,
    };

    /// Whether the query named something in this file, by symbol name or path,
    /// rather than merely accumulating token mass.
    fn names_a_query_word(&self) -> bool {
        self.path_coverage > 0 || self.name_coverage > 0 || self.exact_names > 0
    }
}

/// The relevance key for every admitted file, in `file_order`'s own order.
///
/// The ordering these produce: a file whose path covers the query's compound
/// (underscored) tokens first (`order_line page_views` lands on the file in
/// the `order_line` app named `page_views`, even when its symbols are
/// generic; this key is zero for every ordinary query, so it reshuffles nothing
/// else). Then exact symbol-name matches (the query literally named a symbol
/// defined here: beats any sum of partial hits), then IDF-weighted token
/// relevance (a rare identifier like `subtotal_amount` outweighs a common one
/// like `inventory`/`form_views` that matches dozens of files), then structural
/// rank. Admitting every file before this cut lets an on-the-nose file survive
/// even when the structural walk buried it under common-token mass.
fn file_sort_keys(
    nodes: &[Node],
    file_order: &[String],
    by_file: &FxHashMap<String, Vec<usize>>,
    rwr_rank: &FxHashMap<String, usize>,
    tokens: &[String],
    recency: impl Fn(&str, &str) -> f64,
) -> Vec<FileSortKey> {
    let file_total = file_order.len().max(1);
    let document_frequency_by_term = document_frequencies(nodes, file_order, by_file, tokens);

    // The recency bonus is capped against the median per-token weight in this
    // candidate set, so it breaks ties inside a relevance band and cannot lift a
    // file across one. The median rather than the mean, so a single very rare
    // token does not set the ceiling for the whole query.
    let mut token_weights: Vec<u64> = tokens
        .iter()
        .map(|token| {
            let frequency = document_frequency_by_term.get(token.as_str()).copied().unwrap_or(1).max(1);

            (file_total as u64 * 1000) / frequency as u64
        })
        .collect();

    let median = recency::median_weight(&mut token_weights);

    let mut keys: Vec<FileSortKey> = Vec::with_capacity(file_order.len());

    for key in file_order {
        let positions = by_file.get(key).expect("ordered file has a group");

        // A file is admitted with its first position, so an empty group cannot
        // occur; the neutral key keeps the result parallel to `file_order`
        // rather than silently shortening it if that ever stops being true.
        let Some(&first) = positions.first() else {
            keys.push(FileSortKey::NEUTRAL);

            continue;
        };

        let node = &nodes[first];
        let bonus =
            recency::recency_bonus(median, recency(node.project_id.as_str(), &node.file_path));

        let weighted = weighted_token_score(nodes, positions, tokens, &document_frequency_by_term, file_total)
            .saturating_add(bonus);

        keys.push(FileSortKey {
            path_coverage: path_token_coverage(nodes, positions, tokens),
            name_coverage: name_token_coverage(nodes, positions, tokens),
            exact_names: exact_name_hits(nodes, positions, tokens),
            weighted,
            walk_rank: rwr_rank.get(key).copied().unwrap_or(usize::MAX),
        });
    }

    keys
}

/// The number of candidate files containing each query token, the document
/// frequency the IDF weighting divides by.
fn document_frequencies<'tokens>(
    nodes: &[Node],
    file_order: &[String],
    by_file: &FxHashMap<String, Vec<usize>>,
    tokens: &'tokens [String],
) -> FxHashMap<&'tokens str, usize> {
    let mut document_frequency_by_term: FxHashMap<&str, usize> = FxHashMap::default();

    for key in file_order {
        let positions = by_file.get(key).expect("ordered file has a group");

        for token in tokens {
            if file_has_token(nodes, positions, token) {
                *document_frequency_by_term.entry(token.as_str()).or_insert(0) += 1;
            }
        }
    }

    document_frequency_by_term
}

/// The first symbol name a rendered response mentions, read back off the
/// `[project] kind name @ ...` line shape, so a hint can target the response's
/// leading symbol without every tool threading one out by hand. `None` when the
/// response named nothing.
pub(crate) fn first_named_symbol(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();

        if !trimmed.starts_with('[') {
            continue;
        }

        let after_project = trimmed.split_once("] ")?.1;
        let mut parts = after_project.split_whitespace();

        let _kind = parts.next()?;
        let name = parts.next()?;

        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

/// The `(path:line)` file path a rendered symbol line carries, for recording
/// what a response surfaced against the session. `None` for a line that is not
/// a symbol line.
pub(crate) fn file_path_in_line(line: &str) -> Option<&str> {
    let inside = line.rsplit_once('(')?.1;
    let inside = inside.split_once(')')?.0;
    let (path, line_number) = inside.rsplit_once(':')?;

    if path.is_empty() || line_number.parse::<u32>().is_err() {
        return None;
    }

    Some(path)
}

/// The `project::path` key a file is grouped and looked up by.
pub(crate) fn file_key(project_id: &str, file_path: &str) -> String {
    format!("{project_id}::{file_path}")
}

/// The positions to render for a file when the query named a symbol in it: the
/// named symbols, plus a couple of neighbours for context. A caller who typed an
/// identifier asked about that identifier, so filling the response with the five
/// next-ranked functions in the same file spends their budget on code they did
/// not ask for. `None` when the query named nothing here, leaving the ranked
/// top-N (the right answer for a descriptive query) untouched.
fn focused_positions(nodes: &[Node], positions: &[usize], tokens: &[String]) -> Option<Vec<usize>> {
    if tokens.is_empty() {
        return None;
    }

    let named: Vec<usize> = positions
        .iter()
        .copied()
        .filter(|&position| {
            tokens.iter().any(|token| token.eq_ignore_ascii_case(&nodes[position].name))
        })
        .collect();

    if named.is_empty() {
        return None;
    }

    let mut focused = named.clone();

    for &position in positions {
        if focused.len() >= named.len() + EXPLORE_NEIGHBOURS_MAX {
            break;
        }

        if !focused.contains(&position) {
            focused.push(position);
        }
    }

    assert!(focused.len() <= positions.len(), "focusing never invents positions");

    Some(focused)
}

/// The symbols of one file emitted in source order, skipping any whose span is fully
/// contained in one already emitted for the file (nested symbols render once),
/// charging each against the shared byte and line budgets. Returns whether
/// budget remains for more files.
fn emit_file_source(
    out: &mut String,
    source: &str,
    nodes: &[Node],
    positions: &[usize],
    emitted: &mut Vec<usize>,
    budget: &mut usize,
    lines: &mut usize,
) -> bool {
    // positions arrive in relevance order; render only the most relevant few per
    // file so one large file cannot dump all its symbols, then lay those out in
    // source order.
    let capped = &positions[..positions.len().min(EXPLORE_SYMBOLS_PER_FILE_MAX)];
    let mut ordered: Vec<usize> = capped.to_vec();

    ordered.sort_by(|&left, &right| {
        let by_start = nodes[left].span.start_line.cmp(&nodes[right].span.start_line);

        by_start.then(nodes[right].span.end_line.cmp(&nodes[left].span.end_line))
    });

    let mut covered_line_end: u32 = 0;

    for &position in &ordered {
        let node = &nodes[position];

        if node.span.end_line <= covered_line_end {
            continue;
        }

        let header = format!(
            "# [{}] {} {} ({}:{})\n",
            node.project_id,
            node.kind.as_str(),
            node.name,
            node.file_path,
            node.span.start_line,
        );

        if *budget <= header.len() || *lines >= EXPLORE_LINES_MAX {
            return false;
        }

        let body_lines = node.span.end_line.saturating_sub(node.span.start_line).saturating_add(1);
        let end_line = if body_lines > NODE_BODY_LINES_MAX {
            node.span.start_line.saturating_add(NODE_BODY_LINES_MAX).saturating_sub(1)
        } else {
            node.span.end_line
        };

        let snippet = slice_lines(source, node.span.start_line, end_line, *budget - header.len());

        out.push_str(&header);
        out.push_str(&snippet);

        if end_line < node.span.end_line {
            let _ = write!(out, "\n… ({} more lines)", node.span.end_line - end_line);
        }

        out.push_str("\n\n");

        *budget = budget.saturating_sub(header.len() + snippet.len());
        *lines += snippet.lines().count();
        covered_line_end = node.span.end_line;

        emitted.push(position);
    }

    true
}

/// An outline of one file: the same top-ranked, non-nested symbols `emit_file_source`
/// would render, but as a header and one-line signature each, no bodies (a cheap
/// pointer to less-relevant code). Returns whether budget remains for more files.
///
/// `available` is how many of the file's symbols ranked into the group before
/// query focusing and the per-file cap narrowed it. The difference is reported
/// rather than dropped: an outline is read as "this is what the file holds", so
/// a silent cut invites the reader to conclude a class has three members when it
/// has fifteen.
fn emit_file_outline(
    out: &mut String,
    nodes: &[Node],
    positions: &[usize],
    available: usize,
    emitted: &mut Vec<usize>,
    budget: &mut usize,
    lines: &mut usize,
) -> bool {
    assert!(available >= positions.len(), "focusing and capping never invent symbols");

    let capped = &positions[..positions.len().min(EXPLORE_SYMBOLS_PER_FILE_MAX)];
    let mut ordered: Vec<usize> = capped.to_vec();

    ordered.sort_by(|&left, &right| {
        let by_start = nodes[left].span.start_line.cmp(&nodes[right].span.start_line);

        by_start.then(nodes[right].span.end_line.cmp(&nodes[left].span.end_line))
    });

    // Nested symbols are kept here, unlike in source mode where the container's
    // body already printed them. An outline that drops a class's methods answers
    // a query for one of those methods with the class line alone, which is the
    // one thing a signature-only survey must not do.
    let mut open_ends: Vec<u32> = Vec::new();

    for &position in &ordered {
        let node = &nodes[position];

        while open_ends.last().is_some_and(|&end| node.span.start_line > end) {
            open_ends.pop();
        }

        let depth = open_ends.len().min(OUTLINE_DEPTH_MAX);
        let indent = "  ".repeat(depth);

        // Nesting here is inferred from line spans, so a member whose owner did not
        // rank into this outline lands at the root: a second model's `Meta` prints
        // as a bare top-level `class Meta`, which reads as a class of its own. The
        // dotted tail of the qualified name says whose it is.
        let tail = node.qualified_name.rsplit("::").next().unwrap_or(node.name.as_str());
        let display = if depth == 0 && tail.contains('.') { tail } else { node.name.as_str() };

        let mut line = format!(
            "# {indent}[{}] {} {} ({}:{})",
            node.project_id,
            node.kind.as_str(),
            display,
            node.file_path,
            node.span.start_line,
        );

        if let Some(signature) = &node.signature {
            line.push_str("  ");
            line.push_str(&signature.replace('\n', " "));
        }

        line.push('\n');

        if *budget <= line.len() || *lines >= EXPLORE_LINES_MAX {
            return false;
        }

        out.push_str(&line);

        *budget = budget.saturating_sub(line.len());
        *lines += 1;

        emitted.push(position);
        open_ends.push(node.span.end_line);

        assert!(open_ends.len() <= EXPLORE_SYMBOLS_PER_FILE_MAX, "nesting stack stays bounded");
    }

    let omitted = available.saturating_sub(capped.len());

    if omitted > 0 {
        let note = format!(
            "#   (+{omitted} more symbol(s) here, not shown; `node` or `model` for the rest)\n"
        );

        if *budget <= note.len() || *lines >= EXPLORE_LINES_MAX {
            return false;
        }

        out.push_str(&note);

        *budget = budget.saturating_sub(note.len());
        *lines += 1;
    }

    out.push('\n');

    true
}

/// A node's source file read via its project root, or `None` when unavailable.
pub(crate) fn load_source(roots: &FxHashMap<String, String>, node: &Node) -> Option<String> {
    assert!(!node.file_path.is_empty(), "node file_path must not be empty");

    let root = roots.get(node.project_id.as_str())?;
    let path = Path::new(root).join(&node.file_path);

    std::fs::read_to_string(path).ok()
}

/// The 1-based line range `[start, end]` extracted from `source`, truncated to
/// the byte budget.
fn slice_lines(source: &str, start: u32, end: u32, budget: usize) -> String {
    assert!(start >= 1, "source lines are 1-based");
    assert!(end >= start, "the line range is well-formed");

    let first = start.saturating_sub(1) as usize;
    let count = (end as usize).saturating_sub(first).max(1);

    // Pre-size to the byte budget (the result is truncated to it anyway) and
    // write each line directly into the buffer, instead of allocating a temporary
    // String per line via `format!`.
    let mut snippet = String::with_capacity(budget.min(64 * 1024));
    let mut line_number = start;

    for line in source.lines().skip(first).take(count) {
        let _ = writeln!(snippet, "{line_number}\t{line}");
        line_number = line_number.saturating_add(1);
    }

    let body = snippet.strip_suffix('\n').unwrap_or(&snippet);

    truncate_at_boundary(body, budget).into_owned()
}

/// The single-line render of one node: project, kind, name, qualified name,
/// file location, and a working-tree marker (` [M]` modified, ` [A]` added,
/// ` [D]` deleted, ` [?]` untracked). A clean file gets no marker, so the common
/// case costs no bytes.
pub(crate) fn node_line(node: &Node) -> String {
    // The qualified name is `file_path::Owner.member`, so printing it whole beside
    // the location repeated the path on every row: in a search listing roughly a
    // third of the characters said the same thing twice. `targetable_name` drops the
    // prefix only when the tail is still a name these tools accept back as an
    // argument (`CompanyService.save_model_obj`); a free function has no owner to
    // disambiguate by, so it keeps the full `file::name` form.
    format!(
        "[{}] {} {} @ {} ({}:{}){}",
        node.project_id,
        node.kind.as_str(),
        node.name,
        targetable_name(node),
        node.file_path,
        node.span.start_line,
        working_tree_marker(node),
    )
}

/// The render of several nodes, one per line, each followed by a compact,
/// whitespace-collapsed signature when the extractor captured one, so a search
/// shows a symbol's call shape inline (the way codegraph's search does) without a
/// second `node` lookup.
pub(crate) fn node_lines(nodes: &[Node]) -> String {
    let mut out = String::new();

    for node in nodes {
        out.push_str(&node_line(node));

        if let Some(signature) = node.signature.as_deref() {
            push_compact_signature(&mut out, signature);
        }

        out.push('\n');
    }

    out
}

/// A symbol's signature appended with its whitespace collapsed to single
/// spaces, cut to [`NODE_LINE_SIGNATURE_CHARS_MAX`] characters.
///
/// Written straight into the listing buffer. Collapsing through a `Vec<&str>`
/// and a `join`, then a `chars().collect()`, then a `format!` cost four
/// allocations for every row of every listing, none of which outlived the line.
fn push_compact_signature(out: &mut String, signature: &str) {
    let start = out.len();
    let mut characters: usize = 0;
    let mut first = true;

    for word in signature.split_whitespace() {
        if characters >= NODE_LINE_SIGNATURE_CHARS_MAX {
            out.push('…');

            return;
        }

        if first {
            out.push_str("  ");
            first = false;
        } else {
            out.push(' ');
            characters += 1;
        }

        for character in word.chars() {
            if characters >= NODE_LINE_SIGNATURE_CHARS_MAX {
                out.push('…');

                return;
            }

            out.push(character);
            characters += 1;
        }
    }

    debug_assert!(out.len() >= start, "appending a signature never shortens the line");
}

/// The given text wrapped as a successful tool result.
pub(crate) fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text)])
}

/// A response body joined to its follow-up hint on a line of its own. A listing
/// ends in a newline and an empty-result sentence does not, so appending the
/// hint blindly produced `no symbols matching "X"next: explore ...`.
///
/// A body that found nothing takes no hint at all. The hints name a follow-up
/// against what the response returned, so on an empty result they instruct the
/// reader to run `callers` on a symbol the previous line just said does not
/// exist, which reads as though the tool found something.
pub(crate) fn with_hint(text: String, hint: &str) -> String {
    if hint.is_empty() || found_nothing(&text) {
        return text;
    }

    if text.is_empty() || text.ends_with('\n') {
        return text + hint;
    }

    text + "\n" + hint
}

/// Whether a response body reports that nothing matched.
///
/// Every tool opens such a body with `no ` (`no symbol named "X"`, `no routes
/// matching "X"`, `no flows contain ...`), while a body with results opens with a
/// project tag or a count header. A new empty-result message must keep to that
/// opening for the hint to stay suppressed.
fn found_nothing(text: &str) -> bool {
    text.lines().next().is_some_and(|first| first.starts_with("no "))
}
