//! `constellation_search` and the seeding behind `constellation_explore`.

use std::fmt::Write;

use constellation_graph::{
    Node, is_covering_ref, is_test_path,
};
use constellation_store::{Store, StoreError};
use rustc_hash::FxHashSet;

use crate::limits::{
    CONTENT_FILES_MAX, CONTENT_SEED_NODES_MAX, EXPLORE_COVERAGE_CHECK_MAX, SEARCH_FETCH_MAX,
    SEARCH_FETCH_MIN, SEARCH_FUZZY_SCAN_MAX, SEARCH_FUZZY_SLACK_MAX, SEARCH_OVERFETCH,
};
use crate::rank::{kind_rank, listing_order, listing_rank, path_penalty};
use crate::render::node_lines;
use crate::symbols::{is_coverage_checkable, is_definition_kind, qualified_name_ends_with};
use crate::cursor;

/// The symbols matching `query`, ranked and rendered, one page at a time. Over-fetches
/// past `limit` so ranking sees more than the window it fills, and folds in exact-name
/// hits the full-text index ranked out, so a symbol named exactly as asked is never
/// missing from its own search.
#[doc(hidden)]
pub fn search_text(
    store: &Store,
    query: &str,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let fetch = limit
        .saturating_mul(SEARCH_OVERFETCH)
        .clamp(SEARCH_FETCH_MIN, SEARCH_FETCH_MAX)
        .max(limit);

    assert!(fetch >= limit, "over-fetch is at least the requested limit");

    let needle = query.trim();
    let mut nodes = store.search_nodes(query, fetch)?;

    let mut seen: FxHashSet<String> = nodes.iter().map(|node| node.id.as_str().to_string()).collect();

    for node in store.nodes_named(needle)? {
        if seen.insert(node.id.as_str().to_string()) {
            nodes.push(node);
        }
    }

    let mut misspelled = false;

    if nodes.is_empty() {
        nodes = fuzzy_nodes(store, needle)?;
        misspelled = !nodes.is_empty();
    }

    if nodes.is_empty() {
        return Ok(format!("no symbols matching {query:?}\n"));
    }

    let needle_lower = needle.to_lowercase();

    // Source rank first (tests and generated files sink below hand-written
    // code, so a search never leads with a `test_*` method), then kind (a
    // definition outranks a field/variable of the same name: "Inventory"
    // wants `model Inventory`, not a form's `field inventory`), then match
    // quality. So an exact field still beats partial defs once tests are out
    // of the way: "order_number" surfaces the field, not the tests that name it.
    let match_rank = |node: &Node| {
        let exact = u8::from(!node.name.eq_ignore_ascii_case(needle));
        let prefix = u8::from(!node.name.to_lowercase().starts_with(&needle_lower));

        (path_penalty(&node.file_path), kind_rank(node.kind), exact, prefix)
    };

    // A query matching several files (`Order` matches every file in the app)
    // ties them on every component above, so the listing order finishes the sort.
    nodes.sort_by(|left, right| {
        match_rank(left).cmp(&match_rank(right)).then_with(|| listing_order(left, right))
    });

    let matched = nodes.len();
    let window = cursor::slice(&nodes, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    if misspelled {
        let _ = writeln!(out, "no exact match for {query:?}; closest names:");
    }

    out.push_str(&node_lines(window));

    if let Some(next) = cursor::next_line(page.offset, window.len(), matched, generation) {
        out.push_str(&next);
        out.push('\n');
    }

    Ok(out)
}

/// The definitions whose names read as a misspelling of `needle`: every query
/// character appears in the name in order, and the name is not much longer than
/// the query. Runs only after an exact and a substring search both came back
/// empty, so the cost of one scan buys a "did you mean" instead of a dead end.
fn fuzzy_nodes(store: &Store, needle: &str) -> Result<Vec<Node>, StoreError> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    let needle_lower = needle.to_lowercase();
    let mut nodes: Vec<Node> = Vec::new();

    // The cap goes to SQL, not to the iterator. Taking it here after
    // `all_nodes` meant loading every node in the constellation, with all of
    // its strings, in order to look at the first two hundred thousand.
    let scanned = u32::try_from(SEARCH_FUZZY_SCAN_MAX).unwrap_or(u32::MAX);

    for node in store.nodes_capped(scanned)? {
        if !is_definition_kind(node.kind) {
            continue;
        }

        let name_lower = node.name.to_lowercase();

        if name_lower.len() > needle_lower.len() + SEARCH_FUZZY_SLACK_MAX {
            continue;
        }

        if is_subsequence(&needle_lower, &name_lower) {
            nodes.push(node);
        }
    }

    // Closest by length first: of the names that contain the query's letters in
    // order, the one padding them least is the one the caller meant to type.
    let rank = |node: &Node| (node.name.len(), path_penalty(&node.file_path), listing_rank(node));

    nodes.sort_by(|left, right| {
        rank(left).cmp(&rank(right)).then_with(|| listing_order(left, right))
    });

    Ok(nodes)
}

/// Whether every character of `needle` appears in `haystack` in order, the
/// standard fuzzy-finder match. Both are expected lowercased by the caller.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut characters = haystack.chars();

    needle.chars().all(|wanted| characters.any(|available| available == wanted))
}

/// The nodes a tool's `symbol` argument names, sorted so
/// definitions lead references. A dotted argument (`Class.method`,
/// `Outer.Inner.method`) targets one overload: qualified names are
/// `file_path::Owner.member`, so the member name fetches candidates and the
/// dotted tail filters them to the owner the caller meant, disambiguating a
/// method that exists on many classes. A bare name matches every such node.
pub(crate) fn seed_nodes(store: &Store, symbol: &str) -> Result<Vec<Node>, StoreError> {
    assert!(!symbol.is_empty(), "symbol must not be empty");

    if symbol.contains("::") {
        let mut qualified = store.nodes_qualified(symbol)?;

        if !qualified.is_empty() {
            qualified.sort_by(listing_order);

            return Ok(qualified);
        }
    }

    // A namespaced reverse name (`production:line:schedule:page:forecast`). This is
    // the name `routes` prints and the one `reverse()` and `{% url %}` take, so it is
    // what a reader has to hand; a tool that prints a name and then rejects it as an
    // argument sends them back to guessing. Colons without `::` cannot collide with a
    // qualified name, which always carries the doubled separator.
    if symbol.contains(':') && !symbol.contains("::") {
        let mut routes = store.nodes_by_reverse_name(symbol)?;

        if !routes.is_empty() {
            routes.sort_by(listing_order);

            return Ok(routes);
        }
    }

    let mut nodes = match symbol.rsplit_once('.') {
        Some((_, member)) if !member.is_empty() => {
            let mut qualified = store.nodes_named(member)?;
            qualified.retain(|node| qualified_name_ends_with(&node.qualified_name, symbol));

            if qualified.is_empty() {
                store.nodes_named(symbol)?
            } else {
                qualified
            }
        }
        _ => store.nodes_named(symbol)?,
    };

    // Fallback: address a node by its basename, chiefly a template by filename
    // (`research_page.html` -> `partner/page/research_page.html`), which has no
    // exact name node since template names are full load-paths.
    if nodes.is_empty() {
        nodes = store.nodes_named_suffix(symbol)?;
    }

    nodes.sort_by(listing_order);

    Ok(nodes)
}

/// The definition nodes drawn from the files whose body content matches `query`, to
/// seed explore's structural ranking. These surface a symbol found only by an
/// identifier in its body (`obj.order_number = …`), which a name or docstring
/// search never matches. Only behavior-defining kinds are seeded.
pub(crate) fn content_seed_nodes(store: &Store, query: &str) -> Result<Vec<Node>, StoreError> {
    let files = store.search_content(query, CONTENT_FILES_MAX)?;

    let mut nodes: Vec<Node> = Vec::new();

    for (project, path) in files {
        for node in store.nodes_file_in(&project, &path)? {
            if is_definition_kind(node.kind) {
                nodes.push(node);

                if nodes.len() >= CONTENT_SEED_NODES_MAX {
                    return Ok(nodes);
                }
            }
        }
    }

    Ok(nodes)
}

/// The definition seeds an explore landed on that have zero test coverage, as
/// `(node id, display name)` pairs. The actionable half of codegraph's
/// blast-radius digest: which symbols you are about to read or edit are
/// unguarded. A store error counts a seed as covered (never a false alarm).
/// Bounded to a handful of seeds.
pub(crate) fn explore_uncovered_seeds(store: &Store, seeds: &[Node]) -> Vec<(String, String)> {
    let mut uncovered: Vec<(String, String)> = Vec::new();

    let checkable = seeds.iter().filter(|node| is_coverage_checkable(node));

    for node in checkable.take(EXPLORE_COVERAGE_CHECK_MAX) {
        if is_test_path(&node.file_path) {
            continue;
        }

        let covered = store.callers(&node.id).map_or(true, |callers| {
            callers.iter().any(|(kind, caller)| is_covering_ref(*kind, &caller.file_path))
        });

        if !covered {
            uncovered.push((node.id.as_str().to_string(), node.name.clone()));
        }
    }

    uncovered
}

/// A one-line "no covering tests" flag naming only the uncovered seeds the render
/// actually emitted.
///
/// Seeds are gathered before ranking, so most of them never reach the response.
/// Naming those anyway describes symbols the reader cannot see, and reads as a
/// claim about the code that *was* returned. Restricting to `emitted` node ids
/// keeps the note and the body talking about the same symbols.
pub(crate) fn explore_coverage_note(uncovered: &[(String, String)], emitted: &[&str]) -> String {
    let mut names: Vec<&str> = uncovered
        .iter()
        .filter(|(id, _)| emitted.contains(&id.as_str()))
        .map(|(_, name)| name.as_str())
        .collect();

    names.dedup();

    if names.is_empty() {
        return String::new();
    }

    format!("note: no covering tests for: {} (verify before editing)\n\n", names.join(", "))
}
