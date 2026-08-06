//! Ordering results, which is most of what makes a response useful.
//!
//! Every tool can return more than fits in a response, so the question is
//! never what matches but what to show first. Structural ranking (a restarted
//! random walk over the graph) answers it for explore; the cheaper per-node
//! ranks answer it for listings.

use std::cmp::Ordering;

use constellation_graph::{
    EdgeKind, Node, NodeKind, is_generated_path, is_test_path,
};
use constellation_store::{Store, StoreError};
use rustc_hash::FxHashMap;

use crate::git::now_unix_secs;
use crate::limits::{
    EXPLORE_BYTES_BASE, EXPLORE_BYTES_MAX, EXPLORE_BYTES_PER_NODE, EXPLORE_RANKED_MAX,
    SECONDS_PER_DAY,
};
use crate::render::file_key;
use crate::recency;

/// The power-iteration rounds for random-walk-with-restart ranking.
const RWR_ITERATIONS: u32 = 20;

/// The restart (damping) factor: probability the walk follows an edge vs. jumps
/// back to a seed. Higher = relevance spreads further from the seeds.
const RWR_DAMPING: f64 = 0.85;

/// The English function words and generic code verbs that carry no ranking signal.
/// An LLM's prose query ("how does the load get unloaded") should rank on `load`
/// and `unload`, not `how`/`does`/`the`/`get`. A snake_case identifier survives
/// whole (the split keeps `_`), so only a bare `get`/`set`/`add` is dropped, never
/// `get_object_or_null_obj`.
const QUERY_STOP_WORDS: &[&str] = &[
    "add",
    "all",
    "also",
    "and",
    "any",
    "are",
    "back",
    "been",
    "being",
    "but",
    "can",
    "could",
    "did",
    "does",
    "for",
    "from",
    "get",
    "gets",
    "got",
    "has",
    "have",
    "how",
    "into",
    "its",
    "make",
    "new",
    "nor",
    "not",
    "onto",
    "set",
    "should",
    "that",
    "the",
    "these",
    "this",
    "those",
    "use",
    "used",
    "uses",
    "via",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "will",
    "with",
    "would",
    "yet",
];

/// The explore output byte budget for a graph of `node_count` nodes: a floor
/// for small projects, growing with size to a hard cap, so a large constellation
/// can surface more context without an unbounded response.
#[doc(hidden)]
pub fn explore_budget(node_count: usize) -> usize {
    let budget = EXPLORE_BYTES_BASE
        .saturating_add(node_count.saturating_mul(EXPLORE_BYTES_PER_NODE))
        .min(EXPLORE_BYTES_MAX);

    assert!(budget >= EXPLORE_BYTES_BASE, "budget never drops below the base");
    assert!(budget <= EXPLORE_BYTES_MAX, "budget never exceeds the cap");

    budget
}

/// The nodes ranked by random-walk-with-restart from the query's seed positions over
/// the (undirected, pre-built) adjacency (personalized PageRank). Returns node
/// indices in descending relevance; only nodes reachable from a seed score
/// above zero. This is the structural relevance signal text search cannot give.
#[doc(hidden)]
pub fn rank_by_structure(seeds: &[usize], adjacency: &[Vec<u32>]) -> Vec<usize> {
    let count = adjacency.len();

    if count == 0 || seeds.is_empty() {
        return Vec::new();
    }

    // The restart mass is spread over the seeds as given, so a position named
    // twice still carries one seed's worth. Collecting the distinct positions
    // keeps that true while letting the restart pass below touch only them.
    let restart = 1.0 / seeds.len() as f64;

    assert!(restart > 0.0, "restart probability is positive");

    let mut is_seed = vec![false; count];
    let mut distinct: Vec<usize> = Vec::with_capacity(seeds.len());

    for &seed in seeds {
        assert!(seed < count, "seed position must index a node");

        if !is_seed[seed] {
            is_seed[seed] = true;
            distinct.push(seed);
        }
    }

    let mut rank = vec![0.0_f64; count];

    for &seed in &distinct {
        rank[seed] = restart;
    }

    // Two reusable buffers swapped each round, instead of allocating a fresh
    // `count`-length Vec on every one of the 20 iterations.
    let mut next = vec![0.0_f64; count];

    for _ in 0..RWR_ITERATIONS {
        next.fill(0.0);

        for node in 0..count {
            let degree = adjacency[node].len();

            if rank[node] == 0.0 || degree == 0 {
                continue;
            }

            let share = RWR_DAMPING * rank[node] / degree as f64;

            for &neighbor in &adjacency[node] {
                next[neighbor as usize] += share;
            }
        }

        // The restart lands on the seeds and nowhere else. Walking all `count`
        // nodes to add zero to every one of them was twenty passes over the
        // whole constellation per query, for a vector that is zero except at a
        // handful of positions.
        for &seed in &distinct {
            next[seed] += (1.0 - RWR_DAMPING) * restart;
        }

        std::mem::swap(&mut rank, &mut next);
    }

    // Seeds first, then descending rank within each group, then position.
    // Position last, because two nodes reached by the same walk carry the same
    // rank often enough that leaving the tie to the store's row order would make
    // explore's file ordering depend on how the tree was walked.
    let by_relevance = |&left: &usize, &right: &usize| {
        is_seed[right]
            .cmp(&is_seed[left])
            .then_with(|| rank[right].total_cmp(&rank[left]))
            .then(left.cmp(&right))
    };

    let mut order: Vec<usize> = (0..count).filter(|&node| rank[node] > 0.0).collect();

    assert!(order.len() <= count, "ranking never yields more nodes than exist");

    // Only the head is ever read: the render groups at most EXPLORE_RANKED_MAX
    // positions into files and drops the rest. On a connected graph the damped
    // walk leaves almost every node with a non-zero score, so sorting the whole
    // set was an n-log-n sort over the constellation to look at four thousand of
    // them. Partition at the cap, then sort only that prefix.
    if order.len() > EXPLORE_RANKED_MAX {
        order.select_nth_unstable_by(EXPLORE_RANKED_MAX, by_relevance);
        order.truncate(EXPLORE_RANKED_MAX);
    }

    order.sort_by(by_relevance);

    assert!(order.len() <= EXPLORE_RANKED_MAX, "the ranking is capped to what is rendered");

    order
}

/// The most recent commit time (epoch seconds) per indexed file, keyed as
/// [`file_key`], across every project. Read once per explore call from indexed
/// history; empty until `constellation history` has run, which degrades recency
/// to its working-tree and session signals rather than failing.
pub(crate) fn commit_times_by_file(store: &Store) -> Result<FxHashMap<String, i64>, StoreError> {
    let since = now_unix_secs()
        .saturating_sub(recency::HISTORY_WINDOW_DAYS.saturating_mul(SECONDS_PER_DAY));

    let mut times: FxHashMap<String, i64> = FxHashMap::default();

    for project in store.all_projects()? {
        for (path, committed_at) in store.file_latest_commits(&project.id, since)? {
            times.insert(file_key(project.id.as_str(), &path), committed_at);
        }
    }

    Ok(times)
}

/// Whether a query names a symbol outright, used to decide whether a follow-up
/// hint about that symbol's kind is warranted.
pub(crate) fn query_names(query: &str, name: &str) -> bool {
    query_tokens(query).iter().any(|token| token.eq_ignore_ascii_case(name))
}

/// The query's content tokens for ranking: lowercased, three or more characters,
/// snake_case kept whole, with stop words dropped so common prose does not dilute
/// the IDF/coverage signal.
pub(crate) fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.len() >= 3)
        .map(str::to_lowercase)
        .filter(|token| !QUERY_STOP_WORDS.contains(&token.as_str()))
        .collect()
}

/// The most query tokens any single symbol in the file matches as a
/// substring of its name (the file's best "this symbol is what you asked for"
/// signal). `order_summary_view` matches two of {order, summary, export}; a field
/// named `order` matches one. Ranking on this first lets a file whose symbol name
/// captures most of the query beat a file that merely matches one token exactly.
pub(crate) fn name_token_coverage(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
    positions
        .iter()
        .map(|&position| {
            let name = nodes[position].name.to_lowercase();

            tokens.iter().filter(|token| name.contains(token.as_str())).count()
        })
        .max()
        .unwrap_or(0)
}

/// The count of *compound* query tokens (those carrying a `_`, so a deliberate
/// multi-word identifier like `order_line` or `page_views`, never a common
/// dictionary word) that appear in the file's full path. This is what lets a
/// query naming an app and a file kind land on the right file even when that
/// file's symbols are generically named: for "order_line page_views",
/// `shop/order_line/views/page_views.py` covers both path tokens while
/// its only views are `dashboard_view`/`detail_view` (no symbol-name signal can
/// reach it). Restricting to underscored tokens keeps this a no-op for ordinary
/// queries (a PascalCase class, a bare method name, a single word like
/// `inventory`), so it never reshuffles them: it activates only for the
/// path-segment tokens that would otherwise scatter across same-named files.
pub(crate) fn path_token_coverage(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
    let Some(&first) = positions.first() else {
        return 0;
    };

    let path = nodes[first].file_path.to_lowercase();

    tokens
        .iter()
        .filter(|token| token.contains('_') && path.contains(token.as_str()))
        .count()
}

/// The number of query tokens that exactly equal a symbol name defined in
/// the file (the strongest relevance signal: the agent named this symbol). A file
/// with any exact-name hit ranks above every file with none.
pub(crate) fn exact_name_hits(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
    let names_it = |token: &String| {
        positions.iter().any(|&position| nodes[position].name.eq_ignore_ascii_case(token))
    };

    tokens.iter().filter(|token| names_it(token)).count()
}

/// Whether a query token appears in the file (in its name or any symbol
/// name as a substring), used to count the token's document frequency for IDF.
pub(crate) fn file_has_token(nodes: &[Node], positions: &[usize], token: &str) -> bool {
    if let Some(&first) = positions.first() {
        let path = &nodes[first].file_path;
        let basename = path.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();

        if basename.contains(token) {
            return true;
        }
    }

    positions.iter().any(|&position| nodes[position].name.to_lowercase().contains(token))
}

/// The IDF-weighted relevance: each query token the file contains contributes more
/// the rarer it is across the candidate files (`file_total / document_frequency_by_term`), so a
/// rare identifier dominates a token that matches dozens of files.
pub(crate) fn weighted_token_score(
    nodes: &[Node],
    positions: &[usize],
    tokens: &[String],
    document_frequency_by_term: &FxHashMap<&str, usize>,
    file_total: usize,
) -> u64 {
    tokens
        .iter()
        .filter(|token| file_has_token(nodes, positions, token))
        .map(|token| {
            let frequency = document_frequency_by_term.get(token.as_str()).copied().unwrap_or(1).max(1);

            (file_total as u64 * 1000) / frequency as u64
        })
        .sum()
}

/// A relevance penalty for a result by its path: hand-written source (0)
/// ranks ahead of test files (1), which rank ahead of generated/minified files
/// (2). A stable sort on this key reorders (never drops) listing-tool results so
/// the code an agent most likely wants surfaces first.
#[doc(hidden)]
pub fn path_penalty(path: &str) -> u8 {
    let penalty = if is_generated_path(path) {
        2
    } else if is_test_path(path) {
        1
    } else {
        0
    };

    assert!(penalty <= 2, "penalty stays within its three-level range");

    penalty
}

/// A relevance penalty by node kind: definitions (a class, model,
/// function, route, etc.) rank ahead of references (an import, a local variable).
/// Without this a search for `Inventory` buries the class under the dozens of
/// `import Inventory` statements that merely name it.
pub(crate) fn kind_rank(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Model
        | NodeKind::Class
        | NodeKind::Function
        | NodeKind::Method
        | NodeKind::View
        | NodeKind::Route
        | NodeKind::Template
        | NodeKind::Selector => 0,
        NodeKind::Field | NodeKind::Property => 1,
        NodeKind::Module | NodeKind::File | NodeKind::External => 2,
        NodeKind::Constant | NodeKind::Variable | NodeKind::Parameter => 3,
        NodeKind::Import => 4,
    }
}

/// The combined listing order for a node: definitions before references,
/// and within each, hand-written source before tests and generated files. A stable
/// sort on this key reorders (never drops) results.
///
/// Not a total order on its own: see [`listing_order`], which is what a listing
/// should sort by.
pub(crate) fn listing_rank(node: &Node) -> (u8, u8) {
    (kind_rank(node.kind), path_penalty(&node.file_path))
}

/// The listing order of two nodes, ties broken by where each one is defined.
///
/// [`listing_rank`] alone leaves ties, and a stable sort resolves a tie by
/// keeping the order the store handed the rows back in. That order is the order
/// the tree was walked in, which is a filesystem detail: the same source
/// indexed on two machines renders in two different orders, and a snapshot of
/// it passes on one and fails on the other. The file path and start line finish
/// the order with something the graph itself knows, so a tie is broken the same
/// way everywhere.
///
/// Every ordering a tool renders ends in this, or in the same pair spelled out
/// (see `sort_winnow`, `changed_text`).
pub(crate) fn listing_order(left: &Node, right: &Node) -> Ordering {
    listing_rank(left)
        .cmp(&listing_rank(right))
        .then_with(|| left.file_path.cmp(&right.file_path))
        .then(left.span.start_line.cmp(&right.span.start_line))
}

/// The edge-kind order for caller/callee listings: relationship and call
/// edges (relates_to, calls, routes_to, renders, etc.) rank ahead of structural
/// containment, so "what does X relate to / call" surfaces above X's own
/// methods and fields. Imports and plain references sit in between.
pub(crate) fn edge_rank(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Contains => 3,
        EdgeKind::Imports | EdgeKind::References => 2,
        // Type-annotation edges are real but weak signal: a queryset method whose
        // return type is `Order` is not a "user" of it the way a call or a
        // foreign key is. Rank them below structural relations so a model's genuine
        // callers and relations are not buried under every method that returns it.
        EdgeKind::Returns | EdgeKind::TypeOf => 1,
        _ => 0,
    }
}

/// The cross-project ordering within an edge tier: a node in a different
/// project than the symbol's home ranks ahead of a same-project one, so the
/// cross-project edges a single-repo index cannot show (this tool's reason for
/// being) surface within the listing limit instead of below a tier full of
/// same-repo rows. Applied below `edge_rank`, so a meaningful same-project edge
/// (an `extends`, a `calls`) still outranks a cross-project `import`; it only
/// reorders within one edge kind. Returns 0 for cross-project, 1 for same-project.
pub(crate) fn cross_project_rank(node: &Node, home_project: &str) -> u8 {
    u8::from(node.project_id.as_str() == home_project)
}
