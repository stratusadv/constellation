#![forbid(unsafe_code)]

//! MCP server: serves the unified, cross-project constellation graph to an
//! agent over stdio using the official `rmcp` SDK. Tools answer structural
//! questions (search, symbol detail, callers, callees, and transitive impact)
//! across every indexed project in one database.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use constellation_graph::{EdgeKind, Language, Node, NodeId, NodeKind, ProjectId};
use constellation_store::{FileRow, LinkEdge, ProjectRow, Store, StoreError};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router, transport::stdio,
};
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Deserialize;
use thiserror::Error;
use tokio::task::block_in_place;

/// The default number of results returned by search.
const SEARCH_LIMIT_DEFAULT: u32 = 20;

/// The search over-fetches by this factor, then reorders so hand-written source
/// outranks test and generated files before truncating to the requested limit.
const SEARCH_OVERFETCH: u32 = 4;

/// A hard cap on rows fetched for one search, regardless of the requested limit.
const SEARCH_FETCH_MAX: u32 = 200;

/// A floor on rows fetched before re-ranking, so a small requested limit still
/// pulls enough candidates for the source/kind/match re-sort to surface the best
/// few. Without it, `limit=6` fetches only 24 rows and a strong prefix match can
/// sit outside that window, never reordered into view.
const SEARCH_FETCH_MIN: u32 = 64;

/// The default number of callers/callees listed per symbol.
const RELATED_LIMIT_DEFAULT: u32 = 25;

/// The breadth-first hop bound when walking incoming `Extends` edges for
/// `constellation_subclasses`, far past any real inheritance depth.
const SUBCLASS_HOPS_MAX: u32 = 16;

/// The number of definition seeds `constellation_explore` checks for test coverage
/// before flagging the uncovered ones, kept small so the note stays cheap.
const EXPLORE_COVERAGE_CHECK_MAX: usize = 8;

/// The innermost-symbol results `constellation_at` shows for one file:line.
const AT_RESULTS_MAX: usize = 5;

/// A cap on a call-site snippet's length, in characters.
const CALL_SITE_SNIPPET_CHARS_MAX: usize = 160;

/// The default number of cross-project link edges `links` lists before truncating.
const LINKS_LIMIT_DEFAULT: u32 = 100;

/// The default number of commits `history` lists (newest first) before truncating.
const HISTORY_LIMIT_DEFAULT: u32 = 40;

/// The default number of symbols `as_of` lists before truncating: a whole app's
/// surface at a point in time, so larger than the per-commit limit.
const AS_OF_LIMIT_DEFAULT: u32 = 200;

/// A hard cap on link edges fetched for one `links` call.
const LINKS_FETCH_MAX: u32 = 2_000;

/// The top packages an `overview` lists per project (enough to convey the shape, not
/// the whole tree; use `files` for that).
const OVERVIEW_PACKAGES_MAX: usize = 6;

/// A hard cap on base-class hops `model` walks up the inheritance chain when
/// assembling a model's effective fields (a bound on the MRO traversal).
const MODEL_MRO_DEPTH_MAX: u32 = 16;

/// A hard cap on nodes one `model` traversal visits across the inheritance chain.
const MODEL_NODES_MAX: usize = 2_000;

/// A cap on how many same-named nodes `node` details in full before summarizing
/// the rest (usually import sites) as a count.
const NODE_DETAIL_MAX: usize = 8;

/// A cap on callers `node` lists inline for an unambiguous symbol, so the common
/// "who uses this" question is answered without a second `callers` call.
const NODE_CALLERS_INLINE_MAX: usize = 5;

/// The default and hard-cap depth for impact traversal.
const IMPACT_DEPTH_DEFAULT: u32 = 2;
const IMPACT_DEPTH_MAX: u32 = 8;

/// A hard cap on nodes visited during one impact traversal.
const IMPACT_NODES_MAX: usize = 5_000;

/// A cap on impact-result lines rendered before truncating with a "(+N more)"
/// note: the blast radius is still counted past this; only the listing stops.
const IMPACT_LINES_MAX: usize = 200;

/// A per-level cap on impact lines. A hub symbol (a base mixin, a shared util) has
/// hundreds of direct callers; without a per-level bound L1 alone consumes the
/// whole budget and the deeper, often more informative levels never print. The
/// traversal still counts every caller past this; only the L1 listing is
/// sampled, with the remainder rolled into the "(+N more)" tail.
const IMPACT_LEVEL_LINES_MAX: usize = 40;

/// The method names dispatched dynamically across the whole codebase (Django
/// queryset/manager builtins, model lifecycle hooks, and dict/list/str methods),
/// for which the name-global dark-caller count is workspace-wide dispatch noise,
/// not hidden callers of any one definition. `qs.filter()` / `obj.save()` /
/// `data.get()` appear thousands of times with no statically-bound receiver, so a
/// model method named `save` would otherwise report every `.save()` in the
/// constellation as its dark callers.
const DISPATCH_METHOD_NAMES: &[&str] = &[
    "add", "aggregate", "all", "annotate", "append", "bulk_create", "bulk_update", "clean",
    "clean_fields", "count", "create", "defer", "delete", "distinct", "exclude", "exists",
    "extend", "filter", "first", "full_clean", "get", "get_or_create", "items", "keys", "last",
    "latest", "none", "only", "order_by", "pop", "prefetch_related", "refresh_from_db", "remove",
    "save", "select_related", "setdefault", "update", "update_or_create", "values", "values_list",
];

/// Whether a symbol name is a codebase-wide dynamic-dispatch method, so its
/// name-global unresolved count is noise rather than a dark-caller signal.
fn is_dispatch_method_name(name: &str) -> bool {
    DISPATCH_METHOD_NAMES.contains(&name)
}

#[cfg(test)]
mod dispatch_name_tests {
    use super::is_dispatch_method_name;

    #[test]
    fn common_dispatch_methods_are_recognized() {
        for name in ["get", "save", "filter", "create", "delete", "all", "values", "refresh_from_db"] {
            assert!(is_dispatch_method_name(name), "{name:?} is a codebase-wide dispatch method");
        }
    }

    #[test]
    fn distinctive_names_are_not_dispatch() {
        for name in ["recalculate_totals", "generate_po_number", "Inventory", "PurchaseOrder"] {
            assert!(!is_dispatch_method_name(name), "{name:?} is distinctive, a real dark-caller signal");
        }
    }
}

/// The default number of distinct files explore includes source from.
const EXPLORE_FILES_DEFAULT: u32 = 8;

/// A hard cap on symbols explore considers for a query.
const EXPLORE_SYMBOLS_MAX: u32 = 200;

/// The files whose body content explore samples for extra structural seeds.
const CONTENT_FILES_MAX: u32 = 5;

/// A cap on definition nodes drawn from content-matched files as explore seeds.
const CONTENT_SEED_NODES_MAX: usize = 30;

/// The output byte budget for one explore call, scaled to the graph size between a
/// floor (small project) and a hard cap (large project). The cap stays small
/// enough for the whole result to come back in-band: an MCP host spills an
/// oversized tool result to a file, where it is far less useful than inline.
#[doc(hidden)]
pub const EXPLORE_BYTES_BASE: usize = 40_000;
const EXPLORE_BYTES_PER_NODE: usize = 12;

/// A hard cap on the explore output byte budget.
#[doc(hidden)]
pub const EXPLORE_BYTES_MAX: usize = 64_000;

/// A hard cap on source lines one explore call emits, independent of the byte
/// budget: a second bound so a few very long symbols cannot dominate.
const EXPLORE_LINES_MAX: usize = 1_500;

/// A cap on lines rendered for a single symbol body: a long class/report renders
/// its head, not its whole 100+ lines, so one symbol cannot crowd out the rest.
const NODE_BODY_LINES_MAX: u32 = 60;

/// A hard cap on ranked positions explore groups into files. The ranking is
/// relevance-descending, so the tail past this bound is near-zero signal; the
/// bound keeps the grouping walk explicitly finite.
const EXPLORE_RANKED_MAX: usize = 4_096;

/// A cap on how many of a file's symbols explore renders. A flat file of 20
/// independent views or routes should not dump all of them because two matched
/// the query; the top few by relevance carry the answer. Members past this are
/// dropped from that file's render (the file still appears), keeping one big file
/// from crowding out the other relevant files.
const EXPLORE_SYMBOLS_PER_FILE_MAX: usize = 6;

/// The number of top-ranked files explore renders in full source. Files past this
/// rank are outlined (their symbols' headers and signatures only, no bodies) so
/// the most relevant code comes back verbatim while less-relevant files stay
/// visible as cheap pointers (an agent can `explore`/`node` them for full source).
const EXPLORE_FULL_FILES_MAX: usize = 4;

/// The power-iteration rounds for random-walk-with-restart ranking.
const RWR_ITERATIONS: u32 = 20;

/// The distinct named symbols (query words that exactly name a symbol) explore treats
/// as call-path endpoints, the hop bound on each traced path, the BFS visit cap,
/// and the number of paths rendered; these bounds keep flow tracing finite.
const FLOW_ENDPOINTS_MAX: usize = 4;
const FLOW_HOPS_MAX: u32 = 6;
const FLOW_NODES_MAX: usize = 8_000;
const FLOW_PATHS_MAX: usize = 6;

/// The restart (damping) factor: probability the walk follows an edge vs. jumps
/// back to a seed. Higher = relevance spreads further from the seeds.
const RWR_DAMPING: f64 = 0.85;

/// The English function words and generic code verbs that carry no ranking signal.
/// An LLM's prose query ("how does the load get unloaded") should rank on `load`
/// and `unload`, not `how`/`does`/`the`/`get`. A snake_case identifier survives
/// whole (the split keeps `_`), so only a bare `get`/`set`/`add` is dropped, never
/// `get_object_or_null_obj`.
const QUERY_STOP_WORDS: &[&str] = &[
    "add", "all", "also", "and", "any", "are", "back", "been", "being", "but", "can", "could",
    "did", "does", "for", "from", "get", "gets", "got", "has", "have", "how", "into", "its",
    "make", "new", "nor", "not", "onto", "set", "should", "that", "the", "these", "this", "those",
    "use", "used", "uses", "via", "was", "were", "what", "when", "where", "which", "who", "why",
    "will", "with", "would", "yet",
];

/// A fail-fast bound on routes listed per project, so the URL map of a large repo
/// stays a readable digest rather than dumping every route.
const ROUTES_PER_PROJECT_MAX: usize = 250;

/// A fail-fast bound on the symbols a feature slice gathers, so a hub model cannot
/// drag in the whole graph.
const FEATURE_NODES_MAX: usize = 60;

/// The depth to which the feature walk follows the structural chain (route→view→template→
/// includes is three hops).
const FEATURE_DEPTH_MAX: u32 = 3;

/// The threshold above which slicing all same-named definitions interleaves
/// unrelated features (every `detail_view` in every app) into one undifferentiated
/// dump, so the slice is replaced by a disambiguation listing (name one with its
/// `file::name`). At or below it, each seed is sliced (a model and its handful of
/// overloads stay sliceable).
const FEATURE_SEED_DISAMBIG_MAX: usize = 3;

/// The labels for the feature-slice groups, indexed by [`feature_category`].
const FEATURE_LABELS: [&str; 7] =
    ["routes", "views", "templates", "models", "classes", "functions", "other"];

/// The errors the MCP server can return at startup or while serving.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serve error: {0}")]
    Serve(String),
}

/// The lock guard on `mutex`, recovered if a previous holder panicked. The
/// server's state stays structurally valid across a caught panic (a rolled-back
/// SQLite transaction, an unchanged cache), so one panicking request must not
/// poison the lock for every request after it.
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The error used when a handler panics, caught so a panic becomes a normal
/// error response instead of an unanswered request (a client hang) or a process
/// abort. The panic message still reaches stderr through the default hook.
#[cold]
#[inline(never)]
fn panic_error() -> ErrorData {
    ErrorData::internal_error(
        "constellation: internal error while handling the request (see server stderr)",
        None,
    )
}

/// A blocking action run without stalling the async runtime. On a
/// multi-threaded runtime this is `block_in_place` (the worker hands its other
/// tasks off while it blocks, so the event loop keeps serving); off a runtime,
/// or on a single-threaded one, it runs the work directly so handlers stay
/// callable outside `serve`.
fn run_blocking<T>(action: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};

    match Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), RuntimeFlavor::MultiThread) => {
            block_in_place(action)
        }
        _ => action(),
    }
}

/// The arguments for `constellation_search`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Symbol name to search for (e.g. "Article", "auth"). Matching is
    /// substring/fuzzy; exact then prefix matches rank first.
    pub query: String,
    /// Maximum results to return.
    pub limit: Option<u32>,
}

/// The arguments for tools that operate on a named symbol.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolArgs {
    /// Symbol name to look up. A bare name (`save_model_obj`) matches every
    /// definition with that name; pass `Owner.member`
    /// (`PurchaseOrderService.save_model_obj`) to target one overload.
    pub symbol: String,
    /// Maximum related symbols to list.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_impact`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactArgs {
    /// Exact symbol name whose blast radius to compute.
    pub symbol: String,
    /// How many caller levels to traverse.
    pub depth: Option<u32>,
}

/// The arguments for `constellation_explore`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreArgs {
    /// Symbol/file names or concrete domain words to explore (e.g. "Article
    /// ArticleService po_number"). Matched against names, docstrings, and source
    /// bodies; use real code identifiers, not abstract prose.
    pub query: String,
    /// Maximum distinct files to include source from.
    pub max_files: Option<u32>,
    /// Outline mode: return signature-only outlines for every file (no bodies), a
    /// cheap wide survey. Default false: the top files come back in full source.
    pub outline: Option<bool>,
}

/// The arguments for `constellation_files`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilesArgs {
    /// Restrict to one project by its id or display name; omit to list every
    /// project in the constellation.
    pub project: Option<String>,
    /// List the files whose path contains this substring (case-insensitive),
    /// instead of the aggregated package summary (e.g. "models.py" for every
    /// models file, "billing/" for one app). Combine with `project` to scope it.
    pub pattern: Option<String>,
}

/// The arguments for `constellation_links`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinksArgs {
    /// Restrict to links whose source or target is this project (its id or
    /// display name); omit to list every cross-project link.
    pub project: Option<String>,
    /// Maximum link edges to list.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_overview`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewArgs {
    /// Restrict the digest to one project (its id or display name); omit to
    /// summarize every project in the constellation.
    pub project: Option<String>,
}

/// The arguments for `constellation_routes`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RoutesArgs {
    /// Restrict to one project by its id or display name; omit to list every
    /// project's routes.
    pub project: Option<String>,
    /// Show only routes whose URL pattern, view name, rendered template, or full
    /// route name contains this substring (case-insensitive), e.g. "detail" for
    /// the detail routes, "inventory/" for one app's. Omit for the whole map.
    pub pattern: Option<String>,
}

/// The arguments for `constellation_path`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathArgs {
    /// The starting symbol (a name, or `Owner.member` to disambiguate).
    pub from: String,
    /// The destination symbol to reach.
    pub to: String,
}

/// The arguments for `constellation_history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryArgs {
    /// The file or app to trace over time, as a path substring (e.g.
    /// "orders/models.py" for one file, "orders/" for an app, "models.py" for
    /// every models file). Omit to list the most recent commits across the
    /// whole constellation.
    pub target: Option<String>,
    /// Restrict to one project by its id or display name.
    pub project: Option<String>,
    /// Maximum commits to list, newest first.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_symbol_history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolHistoryArgs {
    /// The symbol to trace: a bare name ("Order", "list_orders") or a qualified
    /// name ("orders.Order.total"). Matches a definition's name or qualified name,
    /// or a longer qualified name ending in it (so "Order" finds "orders.Order").
    pub symbol: String,
    /// Restrict to one project by its id or display name.
    pub project: Option<String>,
    /// Maximum change rows to list, newest first.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_as_of`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AsOfArgs {
    /// The point in time to reconstruct: a commit hash (full or a prefix) or a
    /// date "YYYY-MM-DD". The symbols alive at that point are returned.
    pub at: String,
    /// Restrict to one project by its id or display name. Recommended with a
    /// commit hash, which is only meaningful within its own repository.
    pub project: Option<String>,
    /// Restrict to files whose path contains this substring (a file or an app).
    pub path: Option<String>,
    /// Maximum symbols to list.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_at`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AtArgs {
    /// File path as constellation prints it; a suffix is enough (`views.py` or
    /// `app/views.py`).
    pub file: String,
    /// 1-based line number (e.g. from a traceback frame or a grep hit).
    pub line: u32,
}

/// The arguments for `constellation_orphans`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OrphansArgs {
    /// The project to scan (its id or display name). Required: dead-code candidates
    /// are scoped to one project so the scan stays bounded and meaningful.
    pub project: Option<String>,
    /// Maximum candidates to list.
    pub limit: Option<u32>,
}

/// The arguments for `constellation_changed`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChangedArgs {
    /// The git base to diff the working tree against. Defaults to `HEAD` (uncommitted
    /// and staged edits); pass a branch or ref (e.g. `main`) for a whole-branch diff.
    pub base: Option<String>,
    /// Maximum changed symbols to list per project.
    pub limit: Option<u32>,
}

/// The in-process cache for `explore`: the node list plus the undirected adjacency
/// that random-walk ranking traverses. Built once and reused while `generation`
/// matches the server's; rebuilt when the graph changes underneath the server
/// (see [`ConstellationServer::invalidate`]).
struct ExploreCache {
    generation: u64,
    nodes: Vec<Node>,
    index: FxHashMap<String, u32>,
    adjacency: Vec<Vec<u32>>,
    out_edges: Vec<Vec<(u32, EdgeKind)>>,
}

impl ExploreCache {
    fn build(store: &Store, generation: u64) -> Result<Self, StoreError> {
        let nodes = store.all_nodes(None)?;
        let edges = store.all_edges_kinded()?;

        let count = nodes.len();

        assert!(count <= u32::MAX as usize, "graph must hold fewer than u32::MAX nodes");

        let mut index: FxHashMap<String, u32> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());

        for (position, node) in nodes.iter().enumerate() {
            index.insert(node.id.as_str().to_string(), position as u32);
        }

        let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); count];
        let mut out_edges: Vec<Vec<(u32, EdgeKind)>> = vec![Vec::new(); count];

        for (source, target, kind) in &edges {
            if let (Some(&from), Some(&to)) =
                (index.get(source.as_str()), index.get(target.as_str()))
            {
                adjacency[from as usize].push(to);
                adjacency[to as usize].push(from);
                out_edges[from as usize].push((to, *kind));
            }
        }

        assert!(adjacency.len() == nodes.len(), "adjacency has one entry per node");
        assert!(out_edges.len() == nodes.len(), "out-edges have one entry per node");
        assert!(index.len() <= nodes.len(), "index maps at most one entry per node");

        Ok(Self { generation, nodes, index, adjacency, out_edges })
    }
}

/// The MCP server. The store sits behind a mutex because its SQLite connection
/// is not `Sync`; rmcp runs each request on its own task, so handlers execute
/// concurrently and serialize only for the short time they hold this lock. The
/// blocking store work runs under `block_in_place`, so a slow query never
/// starves the runtime's event loop, and a panicking handler is caught instead
/// of left to hang the client. The explore cache sits behind its own lock,
/// invalidated by a monotonic generation counter the re-indexing watcher bumps.
/// The reply every tool returns when the server has no database, i.e. it was
/// launched (typically via a global MCP registration) outside any indexed
/// project: a clear "nothing here" message instead of a hard failure to connect.
const NO_INDEX_MESSAGE: &str =
    "no constellation index for this working directory (not an indexed Django project). \
     Run `constellation init` here, or open a project that has a .constellation/index.db.";

#[derive(Clone)]
pub struct ConstellationServer {
    /// The graph database, or `None` when serving outside any indexed project
    /// (an unavailable server): every tool then returns [`NO_INDEX_MESSAGE`].
    store: Arc<Mutex<Option<Store>>>,
    explore_cache: Arc<Mutex<Option<ExploreCache>>>,
    generation: Arc<AtomicU64>,
}

impl ConstellationServer {
    /// A new server wrapping the given store.
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(Some(store))),
            explore_cache: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A server with no database: it completes the MCP handshake and answers
    /// every tool with [`NO_INDEX_MESSAGE`], rather than failing to start. Built by
    /// [`serve_unavailable`] when `serve` is launched outside any indexed project,
    /// so a global registration stays quiet in non-Django repos instead of erroring.
    pub fn unavailable() -> Self {
        Self {
            store: Arc::new(Mutex::new(None)),
            explore_cache: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The generation bump that makes the next `explore` rebuild its cached
    /// adjacency. Call after the graph changes underneath the server (a
    /// mid-session re-index).
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The result text of `action` against the store, contained so a panic becomes
    /// an error response and store work never starves the runtime. Returns
    /// [`NO_INDEX_MESSAGE`] unchanged when the server has no database (an
    /// unavailable server), so every text tool degrades to a clear message rather
    /// than erroring.
    fn with_store(
        &self,
        action: impl FnOnce(&Store) -> Result<String, StoreError>,
    ) -> Result<String, ErrorData> {
        run_blocking(|| {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let store = lock_recover(&self.store);

                match store.as_ref() {
                    Some(store) => action(store),
                    None => Ok(NO_INDEX_MESSAGE.to_string()),
                }
            }));

            match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(ErrorData::internal_error(error.to_string(), None)),
                Err(_) => Err(panic_error()),
            }
        })
    }

    /// The handler for one `explore` query: ranks the graph by structure from the query's
    /// seeds and returns the relevant source. Run under `block_in_place` so its
    /// store work never starves the runtime's event loop, and contained so a
    /// panic becomes an error response rather than an unanswered request.
    fn explore(&self, query: &str, max_files: u32, outline: bool) -> Result<CallToolResult, ErrorData> {
        run_blocking(|| {
            let result = catch_unwind(AssertUnwindSafe(|| self.explore_locked(query, max_files, outline)));

            match result {
                Ok(Ok(text)) => Ok(text_result(text)),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(panic_error()),
            }
        })
    }

    /// The store- and cache-locked phase of [`explore`]. Holds the store lock
    /// only for the seed query and any cache rebuild, then releases it (before
    /// ranking and reading source from disk) so concurrent search, caller, and
    /// impact requests are not blocked behind a large explore. The cache lock is
    /// held across ranking and render, contending only with other explore calls.
    fn explore_locked(&self, query: &str, max_files: u32, outline: bool) -> Result<String, ErrorData> {
        let mut cache_guard = lock_recover(&self.explore_cache);

        let seed_positions: Vec<usize>;
        let roots: FxHashMap<String, String>;
        let node_count: usize;
        let coverage_note: String;

        {
            let store_guard = lock_recover(&self.store);

            let Some(store) = store_guard.as_ref() else {
                return Ok(NO_INDEX_MESSAGE.to_string());
            };

            let mut seeds = store
                .search_nodes(query, EXPLORE_SYMBOLS_MAX)
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;

            if seeds.is_empty() {
                seeds = store
                    .search_nodes_any(query, EXPLORE_SYMBOLS_MAX)
                    .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            }

            let content_seeds =
                content_seed_nodes(store, query).map_err(|error| ErrorData::internal_error(error.to_string(), None))?;

            let mut seed_ids: FxHashSet<String> = seeds.iter().map(|node| node.id.as_str().to_string()).collect();

            for node in content_seeds {
                if seed_ids.insert(node.id.as_str().to_string()) {
                    seeds.push(node);
                }
            }

            if seeds.is_empty() {
                return Ok(format!("no symbols matching {query:?}"));
            }

            coverage_note = explore_coverage_note(store, &seeds);

            let generation = self.generation.load(Ordering::Relaxed);
            let fresh = cache_guard.as_ref().is_some_and(|cached| cached.generation == generation);

            if !fresh {
                let built = ExploreCache::build(store, generation)
                    .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;

                *cache_guard = Some(built);
            }

            let cache = cache_guard.as_ref().expect("explore cache built above");

            seed_positions = seeds
                .iter()
                .filter_map(|seed| cache.index.get(seed.id.as_str()).map(|&position| position as usize))
                .collect();

            node_count = cache.nodes.len();

            roots = project_roots(store)
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        }

        let cache = cache_guard.as_ref().expect("explore cache present after build");

        assert!(seed_positions.len() <= node_count, "seed positions index the cached graph");

        let ranked = rank_by_structure(&seed_positions, &cache.adjacency);
        let budget = explore_budget(node_count);

        let flow = flow_section(&cache.nodes, &cache.out_edges, &seed_positions, query);
        let body = render_ranked(&cache.nodes, &ranked, &roots, max_files, budget, query, outline);

        Ok(format!("{flow}{coverage_note}{body}"))
    }

    /// The handler for one `path` query, contained like [`ConstellationServer::explore`] so
    /// a panic becomes an error response and store work never starves the runtime.
    fn path(&self, from: &str, to: &str) -> Result<CallToolResult, ErrorData> {
        run_blocking(|| {
            let result = catch_unwind(AssertUnwindSafe(|| self.path_locked(from, to)));

            match result {
                Ok(Ok(text)) => Ok(text_result(text)),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(panic_error()),
            }
        })
    }

    /// The shortest flow path between two symbols over the cached directed
    /// graph. Reuses explore's adjacency cache (built/refreshed under the store
    /// lock), then searches both directions so "how does X reach Y" finds the link
    /// regardless of which endpoint the caller named first.
    fn path_locked(&self, from: &str, to: &str) -> Result<String, ErrorData> {
        let mut cache_guard = lock_recover(&self.explore_cache);

        let from_ids: Vec<String>;
        let to_ids: Vec<String>;

        {
            let store_guard = lock_recover(&self.store);

            let Some(store) = store_guard.as_ref() else {
                return Ok(NO_INDEX_MESSAGE.to_string());
            };

            from_ids = seed_ids(store, from)?;
            to_ids = seed_ids(store, to)?;

            let generation = self.generation.load(Ordering::Relaxed);
            let fresh = cache_guard.as_ref().is_some_and(|cached| cached.generation == generation);

            if !fresh {
                let built = ExploreCache::build(store, generation)
                    .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;

                *cache_guard = Some(built);
            }
        }

        if from_ids.is_empty() {
            return Ok(format!("no symbol named {from:?}"));
        }

        if to_ids.is_empty() {
            return Ok(format!("no symbol named {to:?}"));
        }

        let cache = cache_guard.as_ref().expect("explore cache present after build");

        let from_positions = cache_positions(cache, &from_ids);
        let to_positions = cache_positions(cache, &to_ids);

        for &source in &from_positions {
            for &target in &to_positions {
                if let Some(path) = shortest_flow_path(&cache.out_edges, source, target) {
                    let mut out = format!("# path {from} -> {to}:\n");
                    render_flow_path(&mut out, &cache.nodes, source, &path);

                    return Ok(out);
                }

                if let Some(path) = shortest_flow_path(&cache.out_edges, target, source) {
                    let mut out = format!("# path {to} -> {from} (only this direction connects):\n");
                    render_flow_path(&mut out, &cache.nodes, target, &path);

                    return Ok(out);
                }
            }
        }

        Ok(format!(
            "no flow path between {from:?} and {to:?} within {FLOW_HOPS_MAX} hops \
             (call / route / render / instantiate / inherit / template-inherit edges)"
        ))
    }
}

#[tool_router]
impl ConstellationServer {
    #[tool(description = "Index health: project, node, edge, and cross-project link counts, plus working-tree staleness.")]
    fn constellation_status(&self) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(status_text)?;

        Ok(text_result(text))
    }

    #[tool(description = "How a file or app changed over time, from indexed git history: the commits that touched a path (newest first) with per-commit churn (+lines/-lines, files changed) and author. Pass target=<path substring> (a file like \"orders/models.py\", or an app like \"orders/\"); omit target for recent activity across the constellation. project=<id or name> scopes it. Requires `constellation history` to have been run; empty otherwise.")]
    fn constellation_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(HISTORY_LIMIT_DEFAULT);
        let text = self.with_store(|store| {
            history_text(store, args.target.as_deref(), args.project.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }

    #[tool(description = "How a symbol changed over time, from indexed git history: each commit where the symbol (a function, method, class, Django model/view/route, or model field) was added, modified (signature changed), or removed, newest first, with date and signature. Pass symbol=<name or qualified name>. project=<id or name> scopes it. Requires `constellation history --symbols` to have been run; empty otherwise.")]
    fn constellation_symbol_history(
        &self,
        Parameters(args): Parameters<SymbolHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(HISTORY_LIMIT_DEFAULT);
        let text = self.with_store(|store| {
            symbol_history_text(store, &args.symbol, args.project.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }

    #[tool(description = "The symbols that existed at a point in the past, reconstructed from indexed symbol history: pass at=<commit hash or YYYY-MM-DD> to list the functions, methods, classes, Django models/views/routes, and fields alive then (with their signatures at that time), grouped by file. project=<id or name> scopes it (recommended for a commit hash); path=<substring> narrows to a file or app. Requires `constellation history --symbols`; answers \"what did this look like at version X\".")]
    fn constellation_as_of(
        &self,
        Parameters(args): Parameters<AsOfArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(AS_OF_LIMIT_DEFAULT);
        let text = self.with_store(|store| {
            as_of_text(store, &args.at, args.project.as_deref(), args.path.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }

    #[tool(description = "Find symbols by name across all projects (substring/fuzzy; exact then prefix matches first, definitions before references). Returns locations only: use constellation_explore to read the code.")]
    fn constellation_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(SEARCH_LIMIT_DEFAULT);
        let text = self.with_store(|store| search_text(store, &args.query, limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "One symbol's detail: kind, location, signature, docstring, and caller/callee counts.")]
    fn constellation_node(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| node_text(store, &args.symbol))?;

        Ok(text_result(text))
    }

    #[tool(description = "A Django model's effective schema in one call: its own fields plus those inherited up the base-class chain (abstract bases, mixins, cross-project bases), its bases, and its relations (foreign keys / M2M to other models). Django scatters these across the MRO; this assembles them. Pass a model name (Owner.field form not needed).")]
    fn constellation_model(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| model_text(store, &args.symbol))?;

        Ok(text_result(text))
    }

    #[tool(description = "What references a symbol: callers, imports, route->view, view->template, model relations, cross-project imports (edges grep cannot see), each call/instantiation with the source line of the call site, so you see how it is used, not just who uses it.")]
    fn constellation_callers(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| callers_text(store, &args.symbol, limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "What a symbol references: its callees, imports, bases, and Django relations (a model's related models, a view's template).")]
    fn constellation_callees(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| callees_text(store, &args.symbol, limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "The tests that cover a symbol: TestCase classes bound to it by the XTestCase->X naming convention, plus test functions/methods that call it. '(no covering tests)' when none, so before a change you know what to run and whether the symbol is guarded. Pass a symbol name or Owner.member.")]
    fn constellation_tests(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| tests_text(store, &args.symbol, limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "The transitive subclasses of a base class or mixin: every type that extends it, directly or through intermediate bases, across projects (e.g. every model using HistoryModelMixin, every BaseDjangoModelService subclass). Pass the base name.")]
    fn constellation_subclasses(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| subclasses_text(store, &args.symbol, limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "Candidate dead code in one project: definitions (functions, methods, classes, models) nothing calls, imports, instantiates, tests, relates to, or extends. Framework-reached symbols (tests, migrations, __init__, dunder methods, app configs) are filtered out, but verify each before deleting - a symbol reached only by a runtime/string convention can still surface. Pass project=<id or name>.")]
    fn constellation_orphans(
        &self,
        Parameters(args): Parameters<OrphansArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| orphans_text(store, args.project.as_deref(), limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "What changed and its blast radius: the symbols overlapping the working-tree (plus staged) diff against a base (default HEAD; pass base=<ref> like base=main for a whole-branch diff), grouped by project, each with its direct caller count and a [no covering tests] flag. The edit-impact view git diff alone cannot give. Runs git in each indexed repo.")]
    fn constellation_changed(
        &self,
        Parameters(args): Parameters<ChangedArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store(|store| changed_text(store, args.base.as_deref(), limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "Transitive callers of a symbol: its blast radius before a change, breadth-first to a depth.")]
    fn constellation_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let depth = args.depth.unwrap_or(IMPACT_DEPTH_DEFAULT).min(IMPACT_DEPTH_MAX);

        assert!(depth <= IMPACT_DEPTH_MAX, "traversal depth is capped");

        let text = self.with_store(|store| impact_text(store, &args.symbol, depth))?;

        Ok(text_result(text))
    }

    #[tool(description = "PRIMARY: try first. Give ONE or TWO rare, specific identifiers (e.g. \"ArticleForm subtotal_amount\"); avoid generic words like \"inventory\"/\"form_views\" that match dozens of files. Matches names, docstrings, AND source bodies (porter-stemmed); ranks exact symbol-name matches first, then rare tokens (IDF) over common ones, then graph structure. Returns the relevant source grouped by file (Read-equivalent), line-numbered; the top files come back in full, the rest as signature-only outlines. Name TWO symbols (\"order_summary_view Comment\") to also trace the call path between them (how X reaches Y across files). Pass outline=true for a signature-only survey of every matched file (no bodies), cheap when mapping breadth before drilling in.")]
    fn constellation_explore(
        &self,
        Parameters(args): Parameters<ExploreArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let max_files = args.max_files.unwrap_or(EXPLORE_FILES_DEFAULT);
        let outline = args.outline.unwrap_or(false);

        self.explore(&args.query, max_files, outline)
    }

    #[tool(description = "Project file layout. No argument → each project summarized by top-level package with file + symbol counts (aggregated, so a large repo doesn't flood the response). project=<id or name> → that project's package breakdown. pattern=<text> → list the files whose path contains that substring (e.g. \"models.py\", \"billing/\"), source files first. Faster than globbing.")]
    fn constellation_files(
        &self,
        Parameters(args): Parameters<FilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| files_text(store, args.project.as_deref(), args.pattern.as_deref()))?;

        Ok(text_result(text))
    }

    #[tool(description = "Orient in one call: per project, the file and symbol counts, the Django surface (models, views, routes, templates), the largest packages, and the cross-project link total. Read this first when unfamiliar with the constellation, before explore/files. project=<id or name> focuses one project.")]
    fn constellation_overview(
        &self,
        Parameters(args): Parameters<OverviewArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| overview_text(store, args.project.as_deref()))?;

        Ok(text_result(text))
    }

    #[tool(description = "The vertical slice of a feature: from a route, view, template, or model, assemble the whole Django path (route->view->template(s)->includes, model relations, service/queryset instantiation, base mixins, signal handlers) as one grouped digest, instead of chaining callers/callees by hand. Pass a route name, view, model, or template.")]
    fn constellation_feature(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| feature_text(store, &args.symbol))?;

        Ok(text_result(text))
    }

    #[tool(description = "The URL map: every route's pattern -> its view -> the template it renders, grouped by project: the app's external surface as one table, the orientation a pile of urls.py files cannot give at a glance. project=<id or name> restricts it (recommended for a large constellation). pattern=<text> filters to routes whose pattern, view, template, or full name contains that substring (e.g. \"detail\"), so a single-route question need not dump the whole map.")]
    fn constellation_routes(
        &self,
        Parameters(args): Parameters<RoutesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text =
            self.with_store(|store| routes_text(store, args.project.as_deref(), args.pattern.as_deref()))?;

        Ok(text_result(text))
    }

    #[tool(description = "The cross-project links: imports in one repo resolved to a definition in another (the edges that make this a constellation rather than separate indexes). Grouped by repo pair. project=<id or name> filters to links touching that project. Empty when only one repo is indexed.")]
    fn constellation_links(
        &self,
        Parameters(args): Parameters<LinksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(LINKS_LIMIT_DEFAULT);
        let text = self.with_store(|store| links_text(store, args.project.as_deref(), limit))?;

        Ok(text_result(text))
    }

    #[tool(description = "The shortest call/flow path between two symbols: how `from` reaches `to` across files (calls, route->view, view->template, instantiation, inheritance), as one chain instead of manual callers/callees spelunking. Names accept `Owner.member`; both directions are searched.")]
    fn constellation_path(
        &self,
        Parameters(args): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.path(&args.from, &args.to)
    }

    #[tool(description = "The innermost symbol at a file:line: map a traceback frame, a stack line, or a grep hit back to its enclosing function/method/class. Pass the path as constellation prints it (a suffix like \"views.py\" is enough) and the 1-based line.")]
    fn constellation_at(
        &self,
        Parameters(args): Parameters<AtArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store(|store| at_text(store, &args.file, args.line))?;

        Ok(text_result(text))
    }
}

#[tool_handler]
impl ServerHandler for ConstellationServer {
    fn get_info(&self) -> ServerInfo {
        let links = match lock_recover(&self.store).as_ref() {
            Some(store) => store.count_links().unwrap_or(0),
            None => 0,
        };

        let mut instructions = String::from(
            "FIRST: before any Grep, Glob, Read, or other file search, for ANY question about \
             this codebase (where a symbol is, how it works, what calls / renders / extends what, \
             a model's schema, or the blast radius of a change), call a constellation_* tool. The \
             graph is pre-built and sub-millisecond; reach for grep/read only for literal text it \
             cannot index (string contents, comments, log lines), or to confirm one detail a \
             constellation call already located.\n\n\
             Constellation is a pre-built, sub-millisecond code-intelligence graph of these \
             Django projects: every symbol, call, and import, plus Django structure grep can't \
             give (routes->views, views->templates (render() and template= kwargs), template \
             extends/includes, model fields and foreign keys (relates_to), return and attribute \
             types (returns/type_of), signal handlers, and inheritance from third-party bases (an \
             <external> mixin resolves)). Consult it BEFORE grepping or reading files.\n\n\
             Tools by intent:\n\
             - constellation_overview: orientation. Per project: file/symbol counts, the Django \
             surface (models/views/routes/templates), largest packages, cross-project link total. \
             Read FIRST when unfamiliar, before explore/files.\n\
             - constellation_explore: PRIMARY, try first. Give symbol/file names or concrete domain \
             words (e.g. \"PurchaseOrder po_number generate\"), matched against names, docstrings, \
             AND source bodies (stemmed), then ranked by graph structure (exact name/file matches \
             first). Returns their source grouped by file (Read-equivalent), line-numbered. Use \
             real code identifiers, not abstract prose.\n\
             - constellation_search: find a symbol by name (substring/fuzzy) when you only need its \
             location.\n\
             - constellation_node: one symbol's kind, signature, docstring, caller/callee counts; \
             pass Owner.member, or the printed file::name, to disambiguate an overloaded name.\n\
             - constellation_model: a Django model's effective schema (own + inherited fields \
             across its base chain (abstract bases, mixins), bases, and relations). One call for what \
             Django spreads over the MRO.\n\
             - constellation_callers / constellation_callees: what references a symbol / what it \
             references; Django edges grep cannot follow, deduped (xN for repeats).\n\
             - constellation_impact: transitive non-test callers (blast radius) before a change.\n\
             - constellation_path: the shortest call/flow path between two symbols, i.e. how one \
             reaches the other across files (give from + to); the answer to \"how does X get to \
             Y\".\n\
             - constellation_at: the symbol at a file:line; map a traceback frame or grep hit to \
             its enclosing function/method/class.\n\
             - constellation_files: project layout, packages with symbol counts (project=<id> for \
             a directory breakdown).\n\
             - constellation_links: the cross-project links themselves, imports in one repo \
             resolved to a definition in another, grouped by repo pair.\n\
             - constellation_status: index health and working-tree staleness.\n\
             - constellation_history: how a file or app changed over time from git \
             history (the commits touching a path, newest first, with +/- line churn); \
             run `constellation history` first to populate it.\n\
             - constellation_symbol_history: how one symbol (function, method, class, \
             Django model/view/route, or model field) changed over time, the commits \
             that added, modified (signature change), or removed it; run \
             `constellation history --symbols` first.\n\
             - constellation_as_of: the symbols that existed at a past point \
             (at=<commit hash or YYYY-MM-DD>), grouped by file, with their \
             signatures as they were then: \"what did this look like at version \
             X\". Needs `constellation history --symbols`.\n\n\
             Recall caveat: edges come from a static parse, scoped to imports (a cross-file call to \
             a symbol the file does not import is dropped, not guessed). Several dynamic patterns \
             are KNOWN-DARK: a low caller/impact count on these is NOT 'safe to change': (1) a \
             custom QuerySet/Manager method reached only through a CHAINED queryset \
             (`.objects.active().by_year()`; the first hop resolves, later hops do not) or via \
             `self.get_queryset()`; a direct `Model.objects.by_year()` DOES resolve; (2) \
             function-local imports and calls to external module.attr() helpers; (3) a method \
             reached only via a template ({{ obj }}), str()/__str__, the admin, or a \
             string-reference FK. Treat these layers as 'edges may be missing', not 'no edges'.",
        );

        if links > 0 {
            instructions.push_str(
                "\n\nThis index spans multiple repos: imports crossing repository boundaries are \
                 linked, and callers/callees/explore follow those cross-project edges.",
            );
        }

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }
}

/// The constellation database opened and served over stdio until the client
/// disconnects. A background thread catches the graph up with the working tree
/// and then keeps it in sync for the life of the session, so serving starts
/// immediately instead of blocking on an initial re-index.
pub fn serve(database: &Path) -> Result<(), McpError> {
    let store = Store::open(database)?;
    let server = ConstellationServer::new(store);

    start_watcher(database, server.clone());

    serve_stdio(server)
}

/// The server run with no database (see [`ConstellationServer::unavailable`]): it
/// completes the MCP handshake and answers every tool with [`NO_INDEX_MESSAGE`],
/// with no watcher thread. Lets a global `serve` registration stay quiet when
/// launched in a project that has no `.constellation/index.db` (a non-Django
/// repo), instead of exiting and surfacing a connection failure to the client.
pub fn serve_unavailable() -> Result<(), McpError> {
    serve_stdio(ConstellationServer::unavailable())
}

/// The given server run over stdio until the client disconnects: the tokio
/// runtime plus the rmcp serve-and-wait loop shared by [`serve`] and
/// [`serve_unavailable`].
fn serve_stdio(server: ConstellationServer) -> Result<(), McpError> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    runtime.block_on(async move {
        let running = server
            .serve(stdio())
            .await
            .map_err(|error| McpError::Serve(error.to_string()))?;

        running
            .waiting()
            .await
            .map_err(|error| McpError::Serve(error.to_string()))?;

        Ok::<(), McpError>(())
    })
}

/// A detached thread that catches the graph up at startup, then re-indexes
/// the constellation after each debounced burst of file changes and invalidates
/// the explore cache, so a long-running server stays in sync mid-session rather
/// than only at startup. The thread opens its own store connection (SQLite WAL
/// lets the watcher write while queries read) and dies with the process when
/// [`serve`] returns. A panic in the watcher is contained so it cannot abort the
/// serve process; re-indexing simply stops.
fn start_watcher(database: &Path, server: ConstellationServer) {
    let database = database.to_path_buf();

    std::thread::spawn(move || {
        let store = match Store::open(&database) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("constellation: watcher disabled: {error}");

                return;
            }
        };

        let watched = catch_unwind(AssertUnwindSafe(|| {
            constellation_index::watch_constellation(&store, || server.invalidate())
        }));

        match watched {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("constellation: watcher stopped: {error}"),
            Err(_) => eprintln!("constellation: watcher thread panicked; re-index is now disabled"),
        }
    });
}

fn status_text(store: &Store) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let edges = store.count_edges()?;
    let links = store.count_links()?;

    let mut node_total: u32 = 0;
    let mut history_total: u32 = 0;
    let mut symbol_total: u32 = 0;
    let mut lines = String::new();

    for row in &projects {
        let nodes = store.count_nodes(&row.id)?;
        node_total = node_total.saturating_add(nodes);
        history_total = history_total.saturating_add(store.count_history_commits(&row.id)?);
        symbol_total = symbol_total.saturating_add(store.count_symbol_revisions(&row.id)?);

        lines.push_str(&format!(
            "  - {} ({}): {nodes} nodes, indexed {}{}\n",
            row.id,
            row.name,
            indexed_age(row.indexed_at),
            stale_hint(store, &row.id, Path::new(&row.root_path)),
        ));
    }

    Ok(format!(
        "projects: {}\nnodes: {node_total}\nedges: {edges}\ncross-project links: {links}\n\
         history commits: {history_total}\nsymbol revisions: {symbol_total}\n{lines}",
        projects.len(),
    ))
}

/// A timeline for one `history` query: the commits touching `target` (a path
/// substring; `None` lists recent activity across the whole constellation),
/// newest first, each stamped with an absolute date, short hash, churn, and
/// author. Reads the history the `history` command ingests; empty until then.
fn history_text(
    store: &Store,
    target: Option<&str>,
    project: Option<&str>,
    limit: u32,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let pattern = match target {
        Some(target) if !target.is_empty() => format!("%{target}%"),
        _ => "%".to_string(),
    };

    let hits = store.history_for_path(project_id.as_ref(), &pattern, limit)?;

    if hits.is_empty() {
        return Ok(history_empty_message(target));
    }

    assert!(hits.len() as u32 <= limit, "history query respects its limit");

    let label = target.filter(|target| !target.is_empty()).unwrap_or("the constellation");

    let mut out = format!("history of {label}: {} commits, newest first\n", hits.len());

    for hit in &hits {
        let (year, month, day) = ymd_from_epoch_secs(hit.committed_at);
        let short = &hit.commit_hash[..hit.commit_hash.len().min(8)];

        out.push_str(&format!(
            "  {year:04}-{month:02}-{day:02} {short} +{}/-{} ({}f) {}: {}\n",
            hit.insertions, hit.deletions, hit.files_changed, hit.author, hit.summary,
        ));
    }

    Ok(out)
}

/// A timeline for one `symbol_history` query: the commits where a definition
/// matching `symbol` was added, modified, or removed, newest first, each stamped
/// with an absolute date, short hash, change kind, qualified name, and the
/// signature at that revision. Reads the symbol history `history --symbols`
/// ingests; empty until then.
fn symbol_history_text(
    store: &Store,
    symbol: &str,
    project: Option<&str>,
    limit: u32,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let hits = store.symbol_history(project_id.as_ref(), symbol, limit)?;

    if hits.is_empty() {
        if store.has_symbol_revisions(project_id.as_ref())? {
            return Ok(format!(
                "no recorded changes for {symbol:?} \
                 (symbol history is populated, but nothing matches; try the bare name, \
                 or an exact Owner.member like \"PurchaseOrderLineItem.quantity\")"
            ));
        }

        return Ok(format!(
            "no recorded changes for {symbol:?} \
             (run `constellation history --symbols` to populate symbol history)"
        ));
    }

    assert!(hits.len() as u32 <= limit, "symbol history respects its limit");

    let mut out = format!("history of {symbol}: {} changes, newest first\n", hits.len());

    for hit in &hits {
        let (year, month, day) = ymd_from_epoch_secs(hit.committed_at);
        let short = &hit.commit_hash[..hit.commit_hash.len().min(8)];

        let signature = match hit.signature.as_deref() {
            Some(signature) if !signature.is_empty() => format!("  [{signature}]"),
            _ => String::new(),
        };

        out.push_str(&format!(
            "  {year:04}-{month:02}-{day:02} {short} {} {} {}{signature}\n",
            hit.change, hit.kind, hit.qualified_name,
        ));
    }

    Ok(out)
}

/// The symbols alive at one point in time for `constellation_as_of`: those
/// recorded as present (added or modified, not since removed) as of `at` (a
/// commit hash or a "YYYY-MM-DD" date), grouped by file, each with its kind and
/// the signature in effect then. Reads the symbol history `history --symbols`
/// ingests.
fn as_of_text(
    store: &Store,
    at: &str,
    project: Option<&str>,
    path: Option<&str>,
    limit: u32,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let threshold = match resolve_as_of(store, project_id.as_ref(), at)? {
        Some(threshold) => threshold,
        None => {
            return Ok(format!(
                "could not resolve {at:?} to a commit or date (pass a commit hash or YYYY-MM-DD)"
            ));
        }
    };

    let pattern = path.filter(|path| !path.is_empty()).map(|path| format!("%{path}%"));

    let symbols = store.symbols_as_of(project_id.as_ref(), threshold, pattern.as_deref(), limit)?;

    if symbols.is_empty() {
        return Ok(format!(
            "no symbols recorded as of {at} \
             (run `constellation history --symbols`, widen the scope, or pick a later point)"
        ));
    }

    let (year, month, day) = ymd_from_epoch_secs(threshold);

    let mut out = format!("symbols as of {at} ({year:04}-{month:02}-{day:02}): {} alive\n", symbols.len());
    let mut current_file = "";

    for symbol in &symbols {
        if symbol.file_path != current_file {
            out.push_str(&format!("{}:\n", symbol.file_path));
            current_file = symbol.file_path.as_str();
        }

        let signature = match symbol.signature.as_deref() {
            Some(signature) if !signature.is_empty() => format!(" [{signature}]"),
            _ => String::new(),
        };

        out.push_str(&format!("  {} {}{signature}\n", symbol.kind, symbol.qualified_name));
    }

    Ok(out)
}

/// The epoch-second threshold an as-of point resolves to: a "YYYY-MM-DD" date, or
/// else the committer time of the commit whose hash matches `at`. `None` when it
/// is neither a date nor a known commit.
fn resolve_as_of(
    store: &Store,
    project: Option<&ProjectId>,
    at: &str,
) -> Result<Option<i64>, StoreError> {
    if let Some(epoch) = parse_ymd_to_epoch(at) {
        return Ok(Some(epoch));
    }

    store.commit_committed_at(project, at)
}

/// The UTC epoch seconds at the start of a "YYYY-MM-DD" date, or `None` when the
/// string is not exactly such a date.
fn parse_ymd_to_epoch(text: &str) -> Option<i64> {
    let mut parts = text.split('-');

    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    Some(epoch_secs_from_ymd(year, month, day))
}

/// The UTC epoch seconds at midnight of a civil date, by Howard Hinnant's
/// days-from-civil algorithm (the inverse of [`ymd_from_epoch_secs`]).
fn epoch_secs_from_ymd(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_position = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_position + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    (era * 146_097 + day_of_era - 719_468) * 86_400
}

/// The project id whose id or display name equals `name`, or `None` when no
/// project matches.
fn find_project(store: &Store, name: &str) -> Result<Option<ProjectId>, StoreError> {
    let projects = store.all_projects()?;

    let found = projects
        .into_iter()
        .find(|project| project.id.as_str() == name || project.name == name);

    Ok(found.map(|project| project.id))
}

/// The reply when a history query matches nothing, distinguishing "no history
/// ingested yet" from "history exists but nothing touched this path".
fn history_empty_message(target: Option<&str>) -> String {
    match target.filter(|target| !target.is_empty()) {
        Some(target) => format!(
            "no commits touching {target:?} in the indexed history \
             (run `constellation history` to populate it)"
        ),
        None => "no git history indexed (run `constellation history` to populate it)".to_string(),
    }
}

/// The civil date (year, month, day) for `epoch_secs` UTC, by Howard Hinnant's
/// days-to-civil algorithm. Stamps history timelines with absolute dates without
/// a date-library dependency.
fn ymd_from_epoch_secs(epoch_secs: i64) -> (i64, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 { month_position + 3 } else { month_position - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    assert!((1..=12).contains(&month), "month falls in 1..=12");
    assert!((1..=31).contains(&day), "day falls in 1..=31");

    (year, month as u32, day as u32)
}

#[cfg(test)]
mod history_date_tests {
    use super::{epoch_secs_from_ymd, parse_ymd_to_epoch, ymd_from_epoch_secs};

    #[test]
    fn ymd_matches_known_utc_dates() {
        assert_eq!(ymd_from_epoch_secs(0), (1970, 1, 1));
        assert_eq!(ymd_from_epoch_secs(86_400), (1970, 1, 2));
        assert_eq!(ymd_from_epoch_secs(1_700_000_000), (2023, 11, 14));
    }

    #[test]
    fn epoch_from_ymd_is_midnight_and_inverts_ymd() {
        assert_eq!(epoch_secs_from_ymd(1970, 1, 1), 0);
        assert_eq!(epoch_secs_from_ymd(1970, 1, 2), 86_400);

        for &(year, month, day) in &[(1970, 1, 1), (1999, 12, 31), (2023, 11, 14), (2024, 2, 29)] {
            let midnight = epoch_secs_from_ymd(year, month, day);

            assert_eq!(ymd_from_epoch_secs(midnight), (year, month as u32, day as u32));
        }
    }

    #[test]
    fn parse_ymd_accepts_dates_and_rejects_hashes() {
        assert_eq!(parse_ymd_to_epoch("1970-01-01"), Some(0));
        assert_eq!(parse_ymd_to_epoch("2023-06-15"), Some(epoch_secs_from_ymd(2023, 6, 15)));
        assert_eq!(parse_ymd_to_epoch("deadbeef"), None);
        assert_eq!(parse_ymd_to_epoch("2023-13-01"), None);
        assert_eq!(parse_ymd_to_epoch("2023-06-15-7"), None);
    }
}

/// A working-tree staleness suffix for a project's status line (how many
/// files changed or were removed on disk since the last index), or an empty string
/// when the index is current, the root is gone, or the count is unavailable. With
/// the in-session watcher running this is normally empty; a non-empty hint flags
/// the brief window before a re-index, or a watcher that never started.
fn stale_hint(store: &Store, project: &ProjectId, root: &Path) -> String {
    if !root.is_dir() {
        return String::new();
    }

    match constellation_index::count_stale_files(store, project, root) {
        Ok(stale) if stale.changed > 0 || stale.removed > 0 => {
            format!(" ({} changed, {} removed on disk since)", stale.changed, stale.removed)
        }
        _ => String::new(),
    }
}

/// A human-readable "time since last index" for the staleness hint.
fn indexed_age(indexed_at_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(indexed_at_ms, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX));

    let seconds = (now - indexed_at_ms).max(0) / 1000;

    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn search_text(store: &Store, query: &str, limit: u32) -> Result<String, StoreError> {
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

    if nodes.is_empty() {
        return Ok(format!("no symbols matching {query:?}"));
    }

    let needle_lower = needle.to_lowercase();

    nodes.sort_by_key(|node| {
        let exact = u8::from(!node.name.eq_ignore_ascii_case(needle));
        let prefix = u8::from(!node.name.to_lowercase().starts_with(&needle_lower));

        // Source rank first (tests and generated files sink below hand-written
        // code, so a search never leads with a `test_*` method), then kind (a
        // definition outranks a field/variable of the same name: "Inventory"
        // wants `model Inventory`, not a form's `field inventory`), then match
        // quality. So an exact field still beats partial defs once tests are out
        // of the way: "po_number" surfaces the field, not the tests that name it.
        (path_penalty(&node.file_path), kind_rank(node.kind), exact, prefix)
    });

    let matched = nodes.len();
    nodes.truncate(limit as usize);

    let mut out = node_lines(&nodes);

    if matched > nodes.len() {
        out.push_str(&format!("(+{} more; raise limit)\n", matched - nodes.len()));
    }

    Ok(out)
}

/// The nodes a tool's `symbol` argument names, sorted so
/// definitions lead references. A dotted argument (`Class.method`,
/// `Outer.Inner.method`) targets one overload: qualified names are
/// `file_path::Owner.member`, so the member name fetches candidates and the
/// dotted tail filters them to the owner the caller meant, disambiguating a
/// method that exists on many classes. A bare name matches every such node.
fn seed_nodes(store: &Store, symbol: &str) -> Result<Vec<Node>, StoreError> {
    assert!(!symbol.is_empty(), "symbol must not be empty");

    if symbol.contains("::") {
        let mut qualified = store.nodes_qualified(symbol)?;

        if !qualified.is_empty() {
            qualified.sort_by_key(listing_rank);

            return Ok(qualified);
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

    nodes.sort_by_key(listing_rank);

    Ok(nodes)
}

/// The definition nodes drawn from the files whose body content matches `query`, to
/// seed explore's structural ranking. These surface a symbol found only by an
/// identifier in its body (`obj.po_number = …`), which a name or docstring
/// search never matches. Only behavior-defining kinds are seeded.
fn content_seed_nodes(store: &Store, query: &str) -> Result<Vec<Node>, StoreError> {
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

/// Whether a node kind defines behavior worth seeding structural ranking
/// from: function, method, class, model, view, or route, not file/import/field.
fn is_definition_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Class
            | NodeKind::Model
            | NodeKind::View
            | NodeKind::Route
    )
}

/// Whether `qualified` ends with the dotted `path` at a name boundary:
/// the character just before it is `.` (a nested owner) or `::` (a top-level
/// owner), never mid-identifier. `a/b.py::Sync.save` ends with `Sync.save`,
/// not with `ave`.
#[doc(hidden)]
pub fn qualified_name_ends_with(qualified: &str, path: &str) -> bool {
    assert!(!path.is_empty(), "dotted path must not be empty");

    if qualified == path {
        return true;
    }

    match qualified.strip_suffix(path) {
        Some(head) => head.ends_with('.') || head.ends_with("::"),
        None => false,
    }
}

/// The canonical Python import for a definition, from its file path and the top-level
/// owner of its qualified name: `app/x/models.py::Order.save` -> `from app.x.models
/// import Order`. A package `__init__.py` imports as the package. `None` for non-Python
/// nodes and for kinds that are not importable names. A re-export in an `__init__.py`
/// may offer a shorter path; this defining-module import is always valid, and saves an
/// LLM guessing the dotted path (file path is not the import path).
fn python_import_line(node: &Node) -> Option<String> {
    if node.language != Language::Python {
        return None;
    }

    if matches!(
        node.kind,
        NodeKind::File
            | NodeKind::Import
            | NodeKind::Module
            | NodeKind::Route
            | NodeKind::Template
            | NodeKind::Selector
            | NodeKind::Parameter
            | NodeKind::External
    ) {
        return None;
    }

    let path = node.file_path.replace('\\', "/");
    let module = path.strip_suffix(".py")?;
    let module = module.strip_suffix("/__init__").unwrap_or(module);

    if module.is_empty() {
        return None;
    }

    let dotted = module.replace('/', ".");
    let after = node.qualified_name.rsplit("::").next().unwrap_or(&node.qualified_name);
    let owner = after.split('.').next().unwrap_or(after);

    if owner.is_empty() {
        return None;
    }

    Some(format!("from {dotted} import {owner}"))
}

/// Whether a reference covers its target as a test: a `Tests` edge (a TestCase bound by
/// naming), or any non-structural reference (call, instantiation, access) from a file
/// under a test path - a test that exercises the symbol however it reaches it, including
/// the instantiation that is the common Django model-test pattern.
fn is_covering_ref(kind: EdgeKind, caller_path: &str) -> bool {
    kind == EdgeKind::Tests || (kind != EdgeKind::Contains && is_test_path(caller_path))
}

/// Whether a symbol is worth a "no covering tests" flag: a top-level definition a reader
/// would test directly (a model, class, free function, or view), not a method, property,
/// nested `Meta`, or dunder, which inherit coverage from their owner and only add noise.
fn is_coverage_checkable(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Class | NodeKind::Model | NodeKind::Function | NodeKind::View)
        && node.name != "Meta"
        && !(node.name.starts_with("__") && node.name.ends_with("__"))
}

/// The tests covering a symbol: `TestCase` classes the extractor bound to it by the
/// `XTestCase -> X` convention (a `Tests` edge), plus any reference to it from a test
/// module (a call, or the instantiation Django model tests use). `(no covering tests)`
/// when none. The signal an LLM needs before editing: what to run, and whether guarded.
fn tests_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut out = String::new();

    for node in &nodes {
        let mut covering = store.callers(&node.id)?;

        covering.retain(|(kind, caller)| is_covering_ref(*kind, &caller.file_path));

        let covering = dedup_related(covering);

        out.push_str(&format!("{}\n", node_line(node)));

        if covering.is_empty() {
            out.push_str("  (no covering tests)\n");
        }

        for (kind, test, _count) in covering.iter().take(limit as usize) {
            out.push_str(&format!("  [{}] {}\n", kind.as_str(), node_line(test)));
        }
    }

    Ok(out)
}

/// The transitive subclasses of a base class or mixin: every node reached by following
/// incoming `Extends` edges breadth-first, so a deep mixin tree (`HistoryModelMixin`,
/// `BaseDjangoModelService`) comes back whole, across projects. Bounded by hops and
/// `limit`.
fn subclasses_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let seeds = seed_nodes(store, symbol)?;

    if seeds.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut visited: FxHashSet<String> =
        seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut frontier: Vec<NodeId> = seeds.iter().map(|node| node.id.clone()).collect();
    let mut found: Vec<Node> = Vec::new();
    let mut hops: u32 = 0;

    while !frontier.is_empty() && hops < SUBCLASS_HOPS_MAX && (found.len() as u32) < limit {
        hops += 1;

        let mut next: Vec<NodeId> = Vec::new();

        for id in &frontier {
            for (kind, child) in store.callers(id)? {
                if kind != EdgeKind::Extends {
                    continue;
                }

                if visited.insert(child.id.as_str().to_string()) {
                    next.push(child.id.clone());
                    found.push(child);
                }
            }
        }

        frontier = next;
    }

    let mut out = format!("subclasses of {symbol} ({} found):\n", found.len());

    if found.is_empty() {
        out.push_str("  (none)\n");
    }

    for node in found.iter().take(limit as usize) {
        out.push_str(&format!("  {}\n", node_line(node)));
    }

    Ok(out)
}

/// Candidate dead code in one project: definitions with no incoming edge but
/// structural containment, after dropping framework-reached symbols that legitimately
/// lack a static caller. Scoped to one project; over-fetched then path/name-filtered so
/// `limit` rows of real candidates come back.
fn orphans_text(store: &Store, project: Option<&str>, limit: u32) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => id,
            None => return Ok(format!("no project named {name:?}")),
        },
        None => {
            return Ok("pass project=<id or name>: orphans is scoped to one project".to_string());
        }
    };

    let fetched = store.orphan_definitions(&project_id, limit.saturating_mul(6).max(limit))?;

    let mut candidates: Vec<Node> = Vec::new();

    for node in fetched.into_iter().filter(is_orphan_candidate) {
        // A method reached only through a manager/service descriptor (`.objects.by_pk()`,
        // `.services.x()`) has no static caller edge but does have a dark (unresolved)
        // reference by name: it is dispatched dynamically, not dead.
        if store.count_unresolved_named(&node.name)? == 0 {
            candidates.push(node);
        }
    }

    let mut out = format!(
        "orphan candidates in {project_id} ({} shown, verify before deleting):\n",
        candidates.len().min(limit as usize),
    );

    if candidates.is_empty() {
        out.push_str("  (none)\n");
    }

    for node in candidates.iter().take(limit as usize) {
        out.push_str(&format!("  {}\n", node_line(node)));
    }

    Ok(out)
}

/// Whether an edgeless definition is a real dead-code candidate, not a framework hook
/// that simply has no static caller: tests, migrations, package initializers, dunder
/// methods, app configs, and a management command's `handle` are excluded.
fn is_orphan_candidate(node: &Node) -> bool {
    let path = node.file_path.replace('\\', "/");

    if is_test_path(&path) || path.contains("/migrations/") || path.contains("/management/commands/") {
        return false;
    }

    if path.ends_with("__init__.py") || path.ends_with("admin.py") {
        return false;
    }

    let name = node.name.as_str();

    if name.starts_with("__") && name.ends_with("__") {
        return false;
    }

    // Django / django_spire protocol hooks reached by the framework, not a static call.
    if matches!(
        name,
        "Meta" | "handle" | "ready" | "save" | "clean" | "delete" | "get_absolute_url"
            | "breadcrumbs" | "base_breadcrumb"
    ) {
        return false;
    }

    !(name.ends_with("Config") || name.ends_with("Migration") || name.ends_with("Admin"))
}

/// The changed symbols and their blast radius: the definitions overlapping the
/// working-tree diff against `base` (default `HEAD`), per project, each with its direct
/// caller count and a test-coverage flag. Combines `git diff` with the graph, the
/// edit-impact view git alone cannot give.
fn changed_text(store: &Store, base: Option<&str>, limit: u32) -> Result<String, StoreError> {
    let roots = project_roots(store)?;

    let mut out = String::new();
    let mut total: usize = 0;

    for (project_id, root) in &roots {
        let project = ProjectId::new(project_id.clone());

        let mut seen: FxHashSet<String> = FxHashSet::default();
        let mut changed: Vec<Node> = Vec::new();

        for (file, start, end) in git_diff_lines(root, base) {
            for node in store.nodes_in_range(&project, &file, start, end)? {
                if seen.insert(node.id.as_str().to_string()) {
                    changed.push(node);
                }
            }
        }

        if changed.is_empty() {
            continue;
        }

        out.push_str(&format!("[{project_id}] {} changed symbol(s):\n", changed.len()));

        for node in changed.iter().take(limit as usize) {
            let mut callers = store.callers(&node.id)?;
            callers.retain(|(kind, _)| *kind != EdgeKind::Contains);

            let covered = callers.iter().any(|(kind, caller)| is_covering_ref(*kind, &caller.file_path));

            let caller_count = dedup_related(callers).len();
            let coverage = if covered || is_test_path(&node.file_path) {
                ""
            } else {
                "  [no covering tests]"
            };

            out.push_str(&format!("  {}  ({caller_count} direct caller(s)){coverage}\n", node_line(node)));
            total += 1;
        }
    }

    if total == 0 {
        return Ok(
            "no changed symbols (clean working tree vs the diff base, or not a git repo)".to_string(),
        );
    }

    Ok(out)
}

/// The `(file, start_line, end_line)` ranges of the new side of every hunk in
/// `git -C root diff --unified=0 <base>`. Empty when git is unavailable, the path is
/// not a repo, or nothing changed.
fn git_diff_lines(root: &str, base: Option<&str>) -> Vec<(String, u32, u32)> {
    let reference = base.unwrap_or("HEAD");

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("diff")
        .arg("--unified=0")
        .arg("--no-color")
        .arg(reference)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    parse_diff_hunks(&String::from_utf8_lossy(&output.stdout))
}

/// The new-side line ranges parsed from a unified diff: each `+++ b/<path>` sets the
/// current file, each `@@ -a,b +c,d @@` yields `(file, c, c+d-1)` (a zero-length hunk,
/// a pure deletion, maps to its anchor line `c`).
fn parse_diff_hunks(diff: &str) -> Vec<(String, u32, u32)> {
    let mut ranges: Vec<(String, u32, u32)> = Vec::new();
    let mut current_file: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = (path != "/dev/null").then(|| path.to_string());
        } else if line.starts_with("@@")
            && let Some(file) = &current_file
            && let Some((start, len)) = parse_hunk_new_range(line)
        {
            let start = start.max(1);
            let end = if len == 0 { start } else { start + len - 1 };

            ranges.push((file.clone(), start, end));
        }
    }

    ranges
}

/// The `(start, length)` of a hunk header's new side: `@@ -3,2 +5,4 @@` -> `(5, 4)`,
/// `@@ -3 +5 @@` -> `(5, 1)`. `None` when the header is malformed.
fn parse_hunk_new_range(hunk: &str) -> Option<(u32, u32)> {
    let spec = hunk.split('+').nth(1)?.split(' ').next()?;
    let mut parts = spec.split(',');

    let start: u32 = parts.next()?.parse().ok()?;
    let length: u32 = parts.next().and_then(|value| value.parse().ok()).unwrap_or(1);

    Some((start, length))
}

/// A one-line "no covering tests" flag for the definition seeds an explore landed on,
/// the actionable half of codegraph's blast-radius digest: which symbols you are about
/// to read or edit have zero test coverage. Empty when every checked seed is covered or
/// none is a checkable definition; a store error counts a seed as covered (never a false
/// alarm). Bounded to a handful of seeds.
fn explore_coverage_note(store: &Store, seeds: &[Node]) -> String {
    let mut uncovered: Vec<&str> = Vec::new();

    for node in seeds.iter().filter(|node| is_coverage_checkable(node)).take(EXPLORE_COVERAGE_CHECK_MAX) {
        if is_test_path(&node.file_path) {
            continue;
        }

        let covered = store.callers(&node.id).map_or(true, |callers| {
            callers.iter().any(|(kind, caller)| is_covering_ref(*kind, &caller.file_path))
        });

        if !covered {
            uncovered.push(node.name.as_str());
        }
    }

    uncovered.dedup();

    if uncovered.is_empty() {
        return String::new();
    }

    format!("note: no covering tests for: {} (verify before editing)\n\n", uncovered.join(", "))
}

fn node_text(store: &Store, symbol: &str) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let shown = nodes.len().min(NODE_DETAIL_MAX);
    let unambiguous = nodes.len() == 1;
    let mut out = String::new();

    for node in &nodes[..shown] {
        node_detail(&mut out, store, node, unambiguous)?;
    }

    if nodes.len() > shown {
        out.push_str(&format!(
            "(+{} more: {})\n",
            nodes.len() - shown,
            summarize_kinds(&nodes[shown..]),
        ));
    }

    if nodes.len() > 1 {
        let shown_narrow = nodes.len().min(6);
        let narrow: Vec<&str> = nodes.iter().take(shown_narrow).map(targetable_name).collect();
        let suffix = if nodes.len() > shown_narrow { ", …" } else { "" };

        out.push_str(&format!(
            "{} matches: narrow with one of: {}{suffix}\n",
            nodes.len(),
            narrow.join(", "),
        ));

        // The dark-caller count is keyed by name, so it is identical across these
        // same-named overloads: print it once here rather than on every row. It
        // cannot be attributed to a single overload (that is what "unresolved" means).
        let dark = store.count_unresolved_named(&nodes[0].name)?;

        if dark > 0 {
            if is_dispatch_method_name(&nodes[0].name) {
                out.push_str(&format!(
                    "note: {:?} is a common method name; {dark} unbound dynamic-dispatch call(s) \
                     workspace-wide share it, not callers of any one of these overloads\n",
                    nodes[0].name,
                ));
            } else {
                out.push_str(&format!(
                    "dark callers (name-global): {dark} unresolved reference(s) name {:?} \
                     (dynamic dispatch or missing import); not attributable to a single overload\n",
                    nodes[0].name,
                ));
            }
        }
    }

    Ok(out)
}

/// The detail block for one symbol, rendered into `out`: location, signature, docstring,
/// deduped caller/callee counts, the dark-caller trust line, and (for an
/// `unambiguous` symbol) its top callers inline. Extracted from [`node_text`]
/// so each stays one logical unit under the line bound.
fn node_detail(
    out: &mut String,
    store: &Store,
    node: &Node,
    unambiguous: bool,
) -> Result<(), StoreError> {
    assert!(!node.name.is_empty(), "node name must not be empty");

    out.push_str(&format!("{}\n", node_line(node)));

    if let Some(role) = symbol_role(node) {
        out.push_str(&format!("  role: {role}\n"));
    }

    if let Some(signature) = &node.signature {
        out.push_str(&format!("  signature: {signature}\n"));
    }

    if let Some(import) = python_import_line(node) {
        out.push_str(&format!("  import: {import}\n"));
    }

    if let Some(docstring) = &node.docstring {
        out.push_str(&format!("  doc: {}\n", docstring.lines().next().unwrap_or("")));
    }

    let mut callers = store.callers(&node.id)?;
    callers.retain(|(kind, _)| *kind != EdgeKind::Contains);
    let callers = dedup_related(callers);

    let mut callees = store.callees(&node.id)?;
    callees.retain(|(kind, _)| *kind != EdgeKind::Contains);
    let callees = dedup_related(callees);

    out.push_str(&format!(
        "  callers: {}{}  callees: {}{}\n",
        callers.len(),
        edge_kind_breakdown(&callers),
        callees.len(),
        edge_kind_breakdown(&callees),
    ));

    // Dark-caller trust signal: references that named this symbol but never bound
    // to an edge (dynamic dispatch, a missing import). A non-zero count means the
    // resolved caller count above understates real usage. Shown inline only for an
    // unambiguous symbol; for an overloaded name the count is name-global (the
    // same for every overload), so node_text prints it once after the listing
    // rather than repeating an identical line on every row.
    if unambiguous {
        let dark = store.count_unresolved_named(&node.name)?;

        if dark > 0 {
            if is_dispatch_method_name(&node.name) {
                out.push_str(&format!(
                    "  note: {:?} is a common method name; {dark} unbound dynamic-dispatch call(s) \
                     workspace-wide share it, not necessarily callers of this symbol\n",
                    node.name,
                ));
            } else {
                out.push_str(&format!(
                    "  dark callers: {dark} unresolved reference(s) name {:?} (dynamic dispatch or \
                     missing import); resolved caller count understates usage\n",
                    node.name,
                ));
            }
        }
    }

    // For an unambiguous symbol, list its top callers inline (strongest relations
    // first, then non-test source) so the common "who uses this" question needs no
    // follow-up callers call. Skipped for an overloaded name to keep the
    // multi-match summary compact.
    if unambiguous && !callers.is_empty() {
        let home = node.project_id.as_str();
        let mut ranked = callers.clone();
        ranked.sort_by_key(|(kind, other, _)| {
            (edge_rank(*kind), cross_project_rank(other, home), listing_rank(other))
        });

        out.push_str("  called by:\n");

        for (kind, other, count) in ranked.iter().take(NODE_CALLERS_INLINE_MAX) {
            let times = if *count > 1 { format!(" ×{count}") } else { String::new() };

            out.push_str(&format!("    [{}{}] {}\n", kind.as_str(), times, node_line(other)));
        }

        if ranked.len() > NODE_CALLERS_INLINE_MAX {
            out.push_str(&format!(
                "    (+{} more; use constellation_callers for the rest)\n",
                ranked.len() - NODE_CALLERS_INLINE_MAX,
            ));
        }
    }

    Ok(())
}

/// A Django model's effective schema, assembled: own fields, fields inherited up the
/// base-class chain, the bases, and relations to other models. Walks Extends edges
/// upward (bounded), gathering each base's Contains-Field members so an abstract
/// base's or mixin's columns appear on the concrete model. A subclass field shadows
/// a base field of the same name. Relations are deduped across the whole chain.
#[doc(hidden)]
pub fn model_text(store: &Store, symbol: &str) -> Result<String, StoreError> {
    let seeds = seed_nodes(store, symbol)?;

    let models: Vec<Node> =
        seeds.into_iter().filter(|node| matches!(node.kind, NodeKind::Model | NodeKind::Class)).collect();

    if models.is_empty() {
        return Ok(format!("no model or class named {symbol:?}"));
    }

    let mut out = String::new();

    for model in &models {
        out.push_str(&format!("{}\n", node_line(model)));

        let mut visited: FxHashSet<String> = FxHashSet::default();
        visited.insert(model.id.as_str().to_string());

        let mut frontier: Vec<(Node, u32)> = vec![(model.clone(), 0)];
        let mut bases: Vec<Node> = Vec::new();
        let mut own_fields: Vec<Node> = Vec::new();
        let mut inherited_fields: Vec<(Node, String)> = Vec::new();
        let mut relations: Vec<(Node, RelationDir)> = Vec::new();
        let mut relation_ids: FxHashSet<String> = FxHashSet::default();
        let mut walked: usize = 0;

        while let Some((node, depth)) = frontier.pop() {
            walked += 1;

            assert!(walked <= MODEL_NODES_MAX, "model walk exceeded {MODEL_NODES_MAX}");

            let reverse_targets: FxHashSet<String> =
                store.reverse_relation_targets(&node.id)?.into_iter().collect();

            for (kind, other) in store.callees(&node.id)? {
                match kind {
                    EdgeKind::Contains if other.kind == NodeKind::Field => {
                        if depth == 0 {
                            own_fields.push(other);
                        } else {
                            inherited_fields.push((other, node.name.clone()));
                        }
                    }
                    EdgeKind::RelatesTo => {
                        if relation_ids.insert(other.id.as_str().to_string()) {
                            let direction = if reverse_targets.contains(other.id.as_str()) {
                                RelationDir::Reverse
                            } else {
                                RelationDir::Forward
                            };

                            relations.push((other, direction));
                        }
                    }
                    EdgeKind::Extends
                        if depth < MODEL_MRO_DEPTH_MAX
                            && visited.insert(other.id.as_str().to_string()) =>
                    {
                        bases.push(other.clone());
                        frontier.push((other, depth + 1));
                    }
                    _ => {}
                }
            }
        }

        render_model_sections(&mut out, &bases, &own_fields, &inherited_fields, &relations);
    }

    Ok(out)
}

/// The direction of a model relation: outward (a ForeignKey/M2M this model declares)
/// or back (the reverse accessor a model that targets this one creates, the
/// synthesized reverse-relation edge). `model` labels each so a reader tells
/// `inventory.brand` (this model's own FK) from the reverse side a related model
/// exposes, a direction the undifferentiated `relates_to` edge set, where both are
/// outgoing edges, otherwise hides.
#[derive(Clone, Copy)]
enum RelationDir {
    Forward,
    Reverse,
}

impl RelationDir {
    fn arrow(self) -> &'static str {
        match self {
            RelationDir::Forward => "->",
            RelationDir::Reverse => "<-",
        }
    }
}

/// The assembled sections of one model: bases, own then inherited fields (a
/// base field shadowed by an own field of the same name is dropped), and deduped
/// relations, each tagged with its direction.
fn render_model_sections(
    out: &mut String,
    bases: &[Node],
    own_fields: &[Node],
    inherited_fields: &[(Node, String)],
    relations: &[(Node, RelationDir)],
) {
    if bases.is_empty() {
        out.push_str("  bases: (none)\n");
    } else {
        let names: Vec<&str> = bases.iter().map(|base| base.name.as_str()).collect();

        out.push_str(&format!("  bases: {}\n", names.join(", ")));
    }

    let own_names: FxHashSet<&str> = own_fields.iter().map(|field| field.name.as_str()).collect();
    let field_total = own_fields.len()
        + inherited_fields.iter().filter(|(field, _)| !own_names.contains(field.name.as_str())).count();

    out.push_str(&format!("  fields ({field_total}):\n"));

    for field in own_fields {
        out.push_str(&format!("    [own] {}{}\n", field.name, field_signature(field)));
    }

    let mut seen_inherited: FxHashSet<&str> = FxHashSet::default();

    for (field, base) in inherited_fields {
        if own_names.contains(field.name.as_str()) || !seen_inherited.insert(field.name.as_str()) {
            continue;
        }

        out.push_str(&format!("    [{base}] {}{}\n", field.name, field_signature(field)));
    }

    if !relations.is_empty() {
        out.push_str(&format!(
            "  relations ({}): [->] forward FK/M2M this model declares, [<-] reverse (a model that points here):\n",
            relations.len(),
        ));

        for (related, direction) in relations {
            out.push_str(&format!(
                "    [{}] {} ({}:{})\n",
                direction.arrow(),
                related.name,
                related.file_path,
                related.span.start_line,
            ));
        }
    }
}

/// A field's type suffix (e.g. " - CharField(max_length=200)") built from its
/// signature, or an empty string when the extractor captured none.
fn field_signature(field: &Node) -> String {
    match &field.signature {
        Some(signature) if !signature.is_empty() => format!(" - {}", signature.replace('\n', " ")),
        _ => String::new(),
    }
}

/// The `Owner.member` tail of a qualified name (`a/b.py::Cls.save` -> `Cls.save`),
/// the form a caller passes to `node`/`callers`/`callees` to disambiguate an
/// overloaded name.
fn qualified_tail(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// The shortest form a caller can pass to target this exact node: the
/// `Owner.member` tail for a method, or the full `file::name` qualified form for
/// a free function (which has no owner to disambiguate by). Both are accepted by
/// [`seed_nodes`].
fn targetable_name(node: &Node) -> &str {
    let tail = qualified_tail(&node.qualified_name);

    if tail.contains('.') {
        tail
    } else {
        node.qualified_name.as_str()
    }
}

/// The related edges with repeats to the same target collapsed into one row carrying its
/// multiplicity, preserving first-seen order: a view that calls `qs.get()`
/// twelve times lists the target once as `×12`, not twelve identical lines.
fn dedup_related(related: Vec<(EdgeKind, Node)>) -> Vec<(EdgeKind, Node, usize)> {
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
fn edge_kind_breakdown(related: &[(EdgeKind, Node, usize)]) -> String {
    if related.is_empty() {
        return String::new();
    }

    let mut counts: FxHashMap<&str, u32> = FxHashMap::default();

    for (kind, _, _) in related {
        *counts.entry(kind.as_str()).or_insert(0) += 1;
    }

    let mut pairs: Vec<(&str, u32)> = counts.into_iter().collect();
    pairs.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));

    let body = pairs.iter().map(|(kind, count)| format!("{count} {kind}")).collect::<Vec<String>>().join(", ");

    format!(" ({body})")
}

/// A compact "55 import, 2 method" summary of node kinds, most common first.
fn summarize_kinds(nodes: &[Node]) -> String {
    let mut counts: FxHashMap<&str, u32> = FxHashMap::default();

    for node in nodes {
        *counts.entry(node.kind.as_str()).or_insert(0) += 1;
    }

    let mut pairs: Vec<(&str, u32)> = counts.into_iter().collect();
    pairs.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));

    pairs.iter().map(|(kind, count)| format!("{count} {kind}")).collect::<Vec<String>>().join(", ")
}

fn callees_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut out = String::new();

    for node in &nodes {
        let mut related = store.callees(&node.id)?;

        related.retain(|(kind, _)| *kind != EdgeKind::Contains);

        let home = node.project_id.as_str();
        let mut deduped = dedup_related(related);
        deduped.sort_by_key(|(kind, other, _)| {
            (edge_rank(*kind), cross_project_rank(other, home), listing_rank(other))
        });

        out.push_str(&format!("{}\n", node_line(node)));

        if deduped.is_empty() {
            out.push_str("  (none)\n");
        }

        for (kind, other, count) in deduped.iter().take(limit as usize) {
            let times = if *count > 1 { format!(" ×{count}") } else { String::new() };

            out.push_str(&format!("  [{}{}] {}\n", kind.as_str(), times, node_line(other)));
        }

        append_unresolved_callees(store, node, limit, &mut out)?;
    }

    Ok(out)
}

/// The unproven, name-matched callee names appended after a definition's precise
/// callees: the calls in its body the resolver could not bind (a `self.obj.services.x()`
/// descriptor hop, an untyped receiver). Disjoint from the resolved callees, since a
/// bound call leaves the unresolved table. Labeled so the precise list stays trustworthy.
fn append_unresolved_callees(
    store: &Store,
    node: &Node,
    limit: u32,
    out: &mut String,
) -> Result<(), StoreError> {
    let unresolved = store.unresolved_callees_of(&node.id, limit)?;

    if unresolved.is_empty() {
        return Ok(());
    }

    out.push_str("  unresolved callees (name match, receiver type unproven):\n");

    for (name, line) in unresolved.iter().take(limit as usize) {
        out.push_str(&format!("    {name}  ({}:{line})\n", node.file_path));
    }

    Ok(())
}

/// The node ids of every definition matching `symbol`, owned (the
/// store-lock-free handle the path search keeps after releasing the lock).
fn seed_ids(store: &Store, symbol: &str) -> Result<Vec<String>, ErrorData> {
    let nodes =
        seed_nodes(store, symbol).map_err(|error| ErrorData::internal_error(error.to_string(), None))?;

    Ok(nodes.iter().map(|node| node.id.as_str().to_string()).collect())
}

/// The cached graph positions of the given node ids, dropping any id absent from
/// the cache.
fn cache_positions(cache: &ExploreCache, ids: &[String]) -> Vec<usize> {
    ids.iter()
        .filter_map(|id| cache.index.get(id.as_str()).map(|&position| position as usize))
        .collect()
}

/// The innermost symbol(s) at a file:line (the enclosing
/// function/method/class for a traceback frame or grep hit).
fn at_text(store: &Store, file: &str, line: u32) -> Result<String, StoreError> {
    if file.is_empty() {
        return Ok("a file path is required".to_string());
    }

    if line == 0 {
        return Ok("line is 1-based".to_string());
    }

    let nodes = store.nodes_at(file, line)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol spans {file}:{line}"));
    }

    let mut out = format!("{file}:{line} (innermost first):\n");

    for node in nodes.iter().take(AT_RESULTS_MAX) {
        out.push_str(&node_line(node));
        out.push('\n');
    }

    Ok(out)
}

/// The callers of a symbol rendered with the source line of each call site (the "how is
/// this used" view, not just "who uses it"). Listed per call site (no dedup), so a
/// caller that references twice shows both lines; ordered by edge then locality,
/// capped by `limit`.
fn callers_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let roots = project_roots(store)?;
    let mut out = String::new();

    for node in &nodes {
        let home = node.project_id.as_str();
        let mut callers = store.callers_located(&node.id)?;
        callers.retain(|(kind, _, _)| *kind != EdgeKind::Contains);
        callers.sort_by_key(|(kind, caller, _)| {
            (edge_rank(*kind), cross_project_rank(caller, home), listing_rank(caller))
        });

        out.push_str(&format!("{}\n", node_line(node)));

        if callers.is_empty() {
            out.push_str("  (none)\n");
        }

        for (kind, caller, line) in callers.iter().take(limit as usize) {
            out.push_str(&format!("  [{}] {}\n", kind.as_str(), node_line(caller)));

            if *line >= 1
                && let Some(snippet) = call_site_line(&roots, caller, *line)
            {
                out.push_str(&format!("      {}:{line}  {snippet}\n", caller.file_path));
            }
        }
    }

    append_unresolved_callers(store, &nodes, &roots, limit, &mut out)?;

    Ok(out)
}

/// The unproven, name-matched caller sites appended after the precise callers: the
/// `Model.services.x()` / untyped-receiver / overloaded-or-base service calls the
/// resolver dropped rather than bind to a guessed definition. Surfaced under a clear
/// label so the precise edges stay trustworthy while a reader still sees the recall a
/// text search would. Matched by each seed's simple name; deduped across overloads.
fn append_unresolved_callers(
    store: &Store,
    nodes: &[Node],
    roots: &FxHashMap<String, String>,
    limit: u32,
    out: &mut String,
) -> Result<(), StoreError> {
    let mut names: Vec<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut sites: Vec<(Node, u32)> = Vec::new();

    for name in names {
        for (caller, line) in store.unresolved_callers_of(name, limit)? {
            if seen.insert(format!("{}:{line}", caller.id.as_str())) {
                sites.push((caller, line));
            }
        }
    }

    if sites.is_empty() {
        return Ok(());
    }

    out.push_str(
        "  unresolved (name match, receiver type unproven, e.g. a Model.services.x() call):\n",
    );

    for (caller, line) in sites.iter().take(limit as usize) {
        out.push_str(&format!("  [calls?] {}\n", node_line(caller)));

        if let Some(snippet) = call_site_line(roots, caller, *line) {
            out.push_str(&format!("      {}:{line}  {snippet}\n", caller.file_path));
        }
    }

    Ok(())
}

/// The trimmed source of one line in a caller's file, capped, for a call-site
/// snippet. `None` when the file is unreadable or the line is blank.
fn call_site_line(roots: &FxHashMap<String, String>, node: &Node, line: u32) -> Option<String> {
    let source = load_source(roots, node)?;
    let text = source.lines().nth((line - 1) as usize)?.trim();

    if text.is_empty() {
        return None;
    }

    Some(text.chars().take(CALL_SITE_SNIPPET_CHARS_MAX).collect())
}

fn impact_text(store: &Store, symbol: &str, depth: u32) -> Result<String, StoreError> {
    assert!(depth <= IMPACT_DEPTH_MAX, "traversal depth is capped");

    let seeds = seed_nodes(store, symbol)?;

    if seeds.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut visited: FxHashSet<String> = seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut frontier: Vec<NodeId> = seeds.iter().map(|node| node.id.clone()).collect();
    let mut out = String::new();

    // The home projects of the seeds: a caller outside all of them is a
    // cross-project hop, surfaced ahead of same-project callers within each tier.
    let seed_projects: FxHashSet<String> =
        seeds.iter().map(|node| node.project_id.as_str().to_string()).collect();

    if seeds.len() > 1 {
        out.push_str(&format!(
            "{} definitions of {symbol:?} ({}): blast radii merged; narrow with Owner.member.\n",
            seeds.len(),
            summarize_kinds(&seeds),
        ));
    }

    let mut level: u32 = 0;
    let mut printed: usize = 0;
    let mut omitted: usize = 0;
    let mut tests_omitted: usize = 0;

    while level < depth && !frontier.is_empty() {
        level += 1;

        let mut callers: Vec<(EdgeKind, Node)> = Vec::new();

        for id in &frontier {
            callers.extend(store.callers(id)?);
        }

        // Drop containment and the file-level `imports` edges that double every
        // hub's caller list: when a file imports the symbol, the symbol inside
        // that file which actually extends/calls it is already a caller, so the
        // File node is redundant noise, never a meaningful blast-radius entry.
        callers.retain(|(kind, caller)| *kind != EdgeKind::Contains && caller.kind != NodeKind::File);
        callers.sort_by_key(|(kind, caller)| {
            let cross = u8::from(seed_projects.contains(caller.project_id.as_str()));

            (edge_rank(*kind), cross, listing_rank(caller))
        });

        let mut next: Vec<NodeId> = Vec::new();
        let mut printed_this_level: usize = 0;

        for (kind, caller) in callers {
            if is_test_path(&caller.file_path) {
                tests_omitted += 1;
                continue;
            }

            if visited.insert(caller.id.as_str().to_string()) {
                assert!(visited.len() <= IMPACT_NODES_MAX, "impact set exceeded {IMPACT_NODES_MAX}");

                next.push(caller.id.clone());

                if printed < IMPACT_LINES_MAX && printed_this_level < IMPACT_LEVEL_LINES_MAX {
                    out.push_str(&format!("L{level} [{}] {}\n", kind.as_str(), node_line(&caller)));
                    printed += 1;
                    printed_this_level += 1;
                } else {
                    omitted += 1;
                }
            }
        }

        frontier = next;
    }

    if printed == 0 && omitted == 0 {
        out.push_str("no non-test transitive callers\n");
    }

    if omitted > 0 {
        out.push_str(&format!(
            "(+{omitted} more transitive callers; inspect a specific caller, or lower depth)\n",
        ));
    }

    if tests_omitted > 0 {
        out.push_str(&format!("({tests_omitted} test caller(s) omitted)\n"));
    }

    let header = format!("impact of {symbol} (depth {depth}): {} non-test caller(s)\n", printed + omitted);

    Ok(format!("{header}{out}"))
}

/// A map from every project's id to its filesystem root, so explore can read source
/// files after releasing the store lock.
fn project_roots(store: &Store) -> Result<FxHashMap<String, String>, StoreError> {
    let projects = store.all_projects()?;

    let mut roots: FxHashMap<String, String> =
        FxHashMap::with_capacity_and_hasher(projects.len(), Default::default());

    for row in projects {
        roots.insert(row.id.as_str().to_string(), row.root_path);
    }

    Ok(roots)
}

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

    let seed_set: FxHashSet<usize> = seeds.iter().copied().collect();
    let restart = 1.0 / seeds.len() as f64;

    assert!(restart > 0.0, "restart probability is positive");

    let mut preference = vec![0.0_f64; count];

    for &seed in seeds {
        assert!(seed < count, "seed position must index a node");

        preference[seed] = restart;
    }

    let mut rank = preference.clone();

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

        for node in 0..count {
            next[node] += (1.0 - RWR_DAMPING) * preference[node];
        }

        std::mem::swap(&mut rank, &mut next);
    }

    let mut order: Vec<usize> = (0..count).filter(|&node| rank[node] > 0.0).collect();

    assert!(order.len() <= count, "ranking never yields more nodes than exist");

    // One sort instead of two: seeds first, then by descending rank within each
    // group (equivalent to the prior rank-sort followed by a stable seed-sort).
    order.sort_by(|&a, &b| {
        let seed_order = seed_set.contains(&b).cmp(&seed_set.contains(&a));

        seed_order.then_with(|| rank[b].total_cmp(&rank[a]))
    });

    order
}

/// Whether an edge kind carries execution/usage or inheritance flow: a
/// call, a route->view, a view->template render, a url resolve, an event handler,
/// a signal receipt, a decoration, an instantiation, a method override, a class
/// `extends`, or a template `extends`/`includes`. Inheritance (class and template)
/// is included because `path` advertises tracing it: a base class or base layout
/// reachable only through a subclass or a rendered page (a view to its page to
/// `django_spire/page/full_page.html`) still connects. Excludes structural
/// (contains), dependency (imports), and the remaining type relations
/// (relates_to / returns / type_of), which are not "X reaches Y".
#[doc(hidden)]
pub fn is_flow_edge(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls
            | EdgeKind::RoutesTo
            | EdgeKind::Renders
            | EdgeKind::Resolves
            | EdgeKind::Handles
            | EdgeKind::Receives
            | EdgeKind::References
            | EdgeKind::Instantiates
            | EdgeKind::Overrides
            | EdgeKind::Decorates
            | EdgeKind::Extends
            | EdgeKind::ExtendsTemplate
            | EdgeKind::IncludesTemplate
    )
}

/// The shortest call path between two named symbols, rendered as a prepended
/// `# flow` section. When a query names two or more symbols, the path is traced
/// over the directed flow graph (the answer to "how does X reach Y" that a
/// source dump alone does not give, since the path spans files). Returns a
/// prepended `# flow` section, or empty when fewer than two symbols are named or
/// no path connects them. Bounded by [`FLOW_ENDPOINTS_MAX`]/[`FLOW_PATHS_MAX`].
fn flow_section(
    nodes: &[Node],
    out_edges: &[Vec<(u32, EdgeKind)>],
    seed_positions: &[usize],
    query: &str,
) -> String {
    let tokens = query_tokens(query);

    if tokens.len() < 2 {
        return String::new();
    }

    let mut named: Vec<usize> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();

    for &position in seed_positions {
        let name = nodes[position].name.to_lowercase();

        if tokens.iter().any(|token| token == &name) && seen.insert(name) {
            named.push(position);

            if named.len() >= FLOW_ENDPOINTS_MAX {
                break;
            }
        }
    }

    if named.len() < 2 {
        return String::new();
    }

    let mut out = String::new();
    let mut rendered: usize = 0;

    for first in 0..named.len() {
        for second in (first + 1)..named.len() {
            if rendered >= FLOW_PATHS_MAX {
                break;
            }

            let (source, path) = match shortest_flow_path(out_edges, named[first], named[second]) {
                Some(path) => (named[first], path),
                None => match shortest_flow_path(out_edges, named[second], named[first]) {
                    Some(path) => (named[second], path),
                    None => continue,
                },
            };

            render_flow_path(&mut out, nodes, source, &path);
            rendered += 1;
        }
    }

    if out.is_empty() {
        return String::new();
    }

    format!("# flow: call paths among the named symbols:\n{out}\n")
}

/// The shortest directed path from `source` to `target` over flow edges,
/// as the `(node, edge-kind-into-it)` hops after the source. Breadth-first, so
/// the first path found is shortest; bounded by [`FLOW_HOPS_MAX`] hops and
/// [`FLOW_NODES_MAX`] visits.
#[doc(hidden)]
pub fn shortest_flow_path(
    out_edges: &[Vec<(u32, EdgeKind)>],
    source: usize,
    target: usize,
) -> Option<Vec<(usize, EdgeKind)>> {
    if source == target {
        return None;
    }

    let mut visited: FxHashSet<usize> = FxHashSet::default();
    visited.insert(source);

    let mut queue: VecDeque<(usize, u32)> = VecDeque::new();
    queue.push_back((source, 0));

    let mut previous: FxHashMap<usize, (usize, EdgeKind)> = FxHashMap::default();

    while let Some((node, depth)) = queue.pop_front() {
        // Global work bound: a hard fail-fast stop on the whole search.
        if visited.len() > FLOW_NODES_MAX {
            break;
        }

        // Per-path hop bound: this path is too long, stop expanding it but keep
        // searching the rest of the frontier.
        if depth >= FLOW_HOPS_MAX {
            continue;
        }

        for &(neighbor, kind) in &out_edges[node] {
            let neighbor = neighbor as usize;

            if !is_flow_edge(kind) || !visited.insert(neighbor) {
                continue;
            }

            previous.insert(neighbor, (node, kind));

            if neighbor == target {
                let mut path: Vec<(usize, EdgeKind)> = Vec::new();
                let mut current = target;
                let mut guard: u32 = 0;

                while current != source && guard <= FLOW_HOPS_MAX {
                    guard += 1;

                    let Some(&(parent, edge)) = previous.get(&current) else {
                        break;
                    };

                    path.push((current, edge));
                    current = parent;
                }

                path.reverse();

                return Some(path);
            }

            queue.push_back((neighbor, depth + 1));
        }
    }

    None
}

/// The render of one traced path: the source symbol, then one indented `→kind→` line per
/// hop, each with `name (file:line)`.
fn render_flow_path(out: &mut String, nodes: &[Node], source: usize, path: &[(usize, EdgeKind)]) {
    let head = &nodes[source];

    out.push_str(&format!("  {} ({}:{})\n", head.name, head.file_path, head.span.start_line));

    for (node, kind) in path {
        let step = &nodes[*node];

        out.push_str(&format!(
            "    →{}→ {} ({}:{})\n",
            kind.as_str(),
            step.name,
            step.file_path,
            step.span.start_line,
        ));
    }

    out.push('\n');
}

/// The source of ranked nodes emitted grouped by file: each file's relevant
/// symbols in source order, with no line printed twice. Container nodes (a
/// whole file or module) are dropped (their members carry the source) and a
/// symbol fully contained in one already emitted for the file is skipped, so a
/// ranked class and its ranked methods render once, not three times over.
/// Bounded by `max_files`, the byte `budget`, and a hard line cap.
fn render_ranked(
    nodes: &[Node],
    ranked: &[usize],
    roots: &FxHashMap<String, String>,
    max_files: u32,
    budget: usize,
    query: &str,
    outline: bool,
) -> String {
    assert!(budget <= EXPLORE_BYTES_MAX, "byte budget stays within the cap");

    // In outline mode no file renders full source; otherwise the top few do and
    // the rest fall through to signature-only outlines.
    let full_files = if outline { 0 } else { EXPLORE_FULL_FILES_MAX };

    let tokens = query_tokens(query);
    let (file_order, by_file) = group_by_file(nodes, ranked, max_files, &tokens);

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
            out.push_str("# (more relevant files: signatures only; explore or node for full source)\n\n");
        }

        let emitted = if file_index < full_files {
            match load_source(roots, node) {
                Some(source) => {
                    emit_file_source(&mut out, &source, nodes, positions, &mut budget, &mut lines)
                }
                None => emit_file_outline(&mut out, nodes, positions, &mut budget, &mut lines),
            }
        } else {
            emit_file_outline(&mut out, nodes, positions, &mut budget, &mut lines)
        };

        if !emitted {
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

    out
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
) -> (Vec<String>, FxHashMap<String, Vec<usize>>) {
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

        let file_key = format!("{}::{}", node.project_id, node.file_path);

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

    // Rank files: a file whose path covers the query's compound (underscored)
    // tokens first (`purchase_order page_views` lands on the file in the
    // `purchase_order` app named `page_views`, even when its symbols are generic;
    // this key is zero for every ordinary query, so it reshuffles nothing else).
    // Then exact symbol-name matches (the query literally named a symbol defined
    // here: beats any sum of partial hits), then IDF-weighted token relevance (a
    // rare identifier like `subtotal_amount` outweighs a common one like
    // `inventory`/`form_views` that matches dozens of files), then structural rank.
    // Admitting every file before this cut lets an on-the-nose file survive even
    // when the structural walk buried it under common-token mass.
    let file_total = file_order.len().max(1);
    let mut doc_freq: FxHashMap<&str, usize> = FxHashMap::default();

    for key in &file_order {
        let positions = by_file.get(key).expect("ordered file has a group");

        for token in tokens {
            if file_has_token(nodes, positions, token) {
                *doc_freq.entry(token.as_str()).or_insert(0) += 1;
            }
        }
    }

    file_order.sort_by_key(|key| {
        let positions = by_file.get(key).expect("ordered file has a group");
        let coverage = name_token_coverage(nodes, positions, tokens);
        let exact = exact_name_hits(nodes, positions, tokens);
        let weighted = weighted_token_score(nodes, positions, tokens, &doc_freq, file_total);
        let path_coverage = path_token_coverage(nodes, positions, tokens);

        (
            std::cmp::Reverse(path_coverage),
            std::cmp::Reverse(coverage),
            std::cmp::Reverse(exact),
            std::cmp::Reverse(weighted),
            *rwr_rank.get(key).unwrap_or(&usize::MAX),
        )
    });

    file_order.truncate(max_files as usize);

    (file_order, by_file)
}

/// The query's content tokens for ranking: lowercased, three or more characters,
/// snake_case kept whole, with stop words dropped so common prose does not dilute
/// the IDF/coverage signal.
fn query_tokens(query: &str) -> Vec<String> {
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
fn name_token_coverage(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
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
/// multi-word identifier like `purchase_order` or `page_views`, never a common
/// dictionary word) that appear in the file's full path. This is what lets a
/// query naming an app and a file kind land on the right file even when that
/// file's symbols are generically named: for "purchase_order page_views",
/// `procurement/purchase_order/views/page_views.py` covers both path tokens while
/// its only views are `dashboard_view`/`detail_view` (no symbol-name signal can
/// reach it). Restricting to underscored tokens keeps this a no-op for ordinary
/// queries (a PascalCase class, a bare method name, a single word like
/// `inventory`), so it never reshuffles them: it activates only for the
/// path-segment tokens that would otherwise scatter across same-named files.
fn path_token_coverage(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
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
fn exact_name_hits(nodes: &[Node], positions: &[usize], tokens: &[String]) -> usize {
    tokens
        .iter()
        .filter(|token| positions.iter().any(|&position| nodes[position].name.eq_ignore_ascii_case(token)))
        .count()
}

/// Whether a query token appears in the file (in its name or any symbol
/// name as a substring), used to count the token's document frequency for IDF.
fn file_has_token(nodes: &[Node], positions: &[usize], token: &str) -> bool {
    if let Some(&first) = positions.first() {
        let basename = nodes[first].file_path.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();

        if basename.contains(token) {
            return true;
        }
    }

    positions.iter().any(|&position| nodes[position].name.to_lowercase().contains(token))
}

/// The IDF-weighted relevance: each query token the file contains contributes more
/// the rarer it is across the candidate files (`file_total / doc_freq`), so a
/// rare identifier dominates a token that matches dozens of files.
fn weighted_token_score(
    nodes: &[Node],
    positions: &[usize],
    tokens: &[String],
    doc_freq: &FxHashMap<&str, usize>,
    file_total: usize,
) -> u64 {
    tokens
        .iter()
        .filter(|token| file_has_token(nodes, positions, token))
        .map(|token| {
            let frequency = doc_freq.get(token.as_str()).copied().unwrap_or(1).max(1);

            (file_total as u64 * 1000) / frequency as u64
        })
        .sum()
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
            out.push_str(&format!("\n… ({} more lines)", node.span.end_line - end_line));
        }

        out.push_str("\n\n");

        *budget = budget.saturating_sub(header.len() + snippet.len());
        *lines += snippet.lines().count();
        covered_line_end = node.span.end_line;
    }

    true
}

/// An outline of one file: the same top-ranked, non-nested symbols `emit_file_source`
/// would render, but as a header and one-line signature each, no bodies (a cheap
/// pointer to less-relevant code). Returns whether budget remains for more files.
fn emit_file_outline(
    out: &mut String,
    nodes: &[Node],
    positions: &[usize],
    budget: &mut usize,
    lines: &mut usize,
) -> bool {
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

        let mut line = format!(
            "# [{}] {} {} ({}:{})",
            node.project_id,
            node.kind.as_str(),
            node.name,
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
        covered_line_end = node.span.end_line;
    }

    out.push('\n');

    true
}

/// The heuristic for whether a path is a test file, which explore down-ranks out.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);

    lower.contains("/tests/")
        || lower.contains("/test/")
        || base.starts_with("test_")
        || base.starts_with("conftest")
        || base.ends_with("_test.py")
        || base.ends_with(".test.js")
        || base.ends_with(".spec.js")
}

/// The heuristic for whether a path is machine-generated or minified (Django
/// migrations, vendored or collected static assets, minified/bundled JS/CSS,
/// generated protobuf stubs). Such files are real graph nodes but rarely what an
/// agent wants to read first, so the listing tools sink them below source.
fn is_generated_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);

    if base.ends_with(".min.js")
        || base.ends_with(".min.css")
        || base.ends_with(".bundle.js")
        || base.ends_with("_pb2.py")
    {
        return true;
    }

    lower.split('/').any(|segment| matches!(segment, "migrations" | "vendor" | "staticfiles"))
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
fn kind_rank(kind: NodeKind) -> u8 {
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
fn listing_rank(node: &Node) -> (u8, u8) {
    (kind_rank(node.kind), path_penalty(&node.file_path))
}

/// The edge-kind order for caller/callee listings: relationship and call
/// edges (relates_to, calls, routes_to, renders, etc.) rank ahead of structural
/// containment, so "what does X relate to / call" surfaces above X's own
/// methods and fields. Imports and plain references sit in between.
fn edge_rank(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Contains => 3,
        EdgeKind::Imports | EdgeKind::References => 2,
        // Type-annotation edges are real but weak signal: a queryset method whose
        // return type is `PurchaseOrder` is not a "user" of it the way a call or a
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
fn cross_project_rank(node: &Node, home_project: &str) -> u8 {
    u8::from(node.project_id.as_str() == home_project)
}

/// A node's source file read via its project root, or `None` when unavailable.
fn load_source(roots: &FxHashMap<String, String>, node: &Node) -> Option<String> {
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

    truncate_at_boundary(body, budget)
}

/// A copy of `text` truncated to at most `budget` bytes, on a UTF-8 char boundary.
fn truncate_at_boundary(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }

    let mut end = budget;

    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    assert!(end <= budget, "truncation never extends past the budget");
    assert!(text.is_char_boundary(end), "truncation lands on a char boundary");

    format!("{}\u{2026}", &text[..end])
}

/// The files aggregated by their first `depth` path directory segments (root files
/// under `(root)`), returning `(directory, file count, symbol count)` sorted by
/// symbol count descending then name. `depth` 1 is the top-level package; a
/// deeper value breaks a project down by sub-directory.
fn aggregate_by_depth(files: &[FileRow], depth: usize) -> Vec<(String, usize, i64)> {
    assert!(depth >= 1, "aggregation depth is at least one");

    let mut totals: FxHashMap<String, (usize, i64)> = FxHashMap::default();

    for file in files {
        let segments: Vec<&str> = file.path.split('/').collect();

        let key = if segments.len() <= 1 {
            "(root)".to_string()
        } else {
            let take = depth.min(segments.len() - 1);
            segments[..take.max(1)].join("/")
        };

        let entry = totals.entry(key).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += file.node_count;
    }

    let mut directories: Vec<(String, usize, i64)> =
        totals.into_iter().map(|(name, (count, symbols))| (name, count, symbols)).collect();

    directories.sort_by(|left, right| right.2.cmp(&left.2).then(left.0.cmp(&right.0)));

    directories
}

/// The constellation's file layout. With no `filter`, each project is
/// summarized by top-level package (file + symbol counts, a layout map, not a
/// file dump). With a project `filter`, that project's files are listed (capped).
/// Aggregating by default keeps a 2,000-file repo from blowing the response budget.
fn files_text(store: &Store, filter: Option<&str>, pattern: Option<&str>) -> Result<String, StoreError> {
    const DIRECTORIES_MAX: usize = 80;
    const FILES_MATCH_MAX: usize = 100;

    let projects = store.all_projects()?;
    let needle = pattern.map(str::to_lowercase);

    let mut out = String::new();
    let mut shown: u32 = 0;

    for project in projects {
        if let Some(filter) = filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        let files = store.files_for(&project.id)?;
        let symbol_total = store.count_nodes(&project.id)?;

        out.push_str(&format!(
            "[{}] {} ({} files, {symbol_total} symbols)\n",
            project.id,
            project.name,
            files.len(),
        ));

        if let Some(needle) = &needle {
            let mut matched: Vec<&FileRow> =
                files.iter().filter(|file| file.path.to_lowercase().contains(needle)).collect();

            matched.sort_by(|left, right| {
                path_penalty(&left.path).cmp(&path_penalty(&right.path)).then(left.path.cmp(&right.path))
            });

            for file in matched.iter().take(FILES_MATCH_MAX) {
                out.push_str(&format!("  {} ({} symbols)\n", file.path, file.node_count));
            }

            if matched.len() > FILES_MATCH_MAX {
                out.push_str(&format!("  (+{} more matching; narrow the pattern)\n", matched.len() - FILES_MATCH_MAX));
            }

            if matched.is_empty() {
                out.push_str(&format!("  (no files matching {pattern:?})\n", pattern = needle));
            }
        } else {
            // Depth 2 even with no filter: a Django repo nests everything under one
            // top package (`app/`), so depth 1 collapses the whole project to a single
            // useless line. Depth 2 surfaces the per-domain breakdown (`app/inventory`,
            // `app/procurement`), still bounded by DIRECTORIES_MAX.
            let directories = aggregate_by_depth(&files, 2);

            for (name, file_count, symbol_count) in directories.iter().take(DIRECTORIES_MAX) {
                out.push_str(&format!("  {name}/ ({file_count} files, {symbol_count} symbols)\n"));
            }

            if directories.len() > DIRECTORIES_MAX {
                out.push_str(&format!("  (+{} more directories)\n", directories.len() - DIRECTORIES_MAX));
            }

            if filter.is_none() {
                out.push_str("  (pass project=<id> to focus a single project, or pattern=<text> to list files)\n");
            }
        }

        out.push('\n');
        shown += 1;
    }

    if shown == 0 {
        return Ok(match filter {
            Some(filter) => format!("no project matches {filter:?}"),
            None => "no projects indexed".to_string(),
        });
    }

    Ok(out)
}

/// A one-call orientation digest. Per project: file and symbol counts, the
/// Django surface (models/views/routes/templates), the dominant packages, and
/// the constellation-wide cross-project link total. Built from cheap aggregate
/// queries (counts and a GROUP BY), never a full node load.
fn overview_text(store: &Store, filter: Option<&str>) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let links = store.count_links()?;

    let mut out = String::new();
    let mut shown: u32 = 0;

    for project in projects {
        if let Some(filter) = filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        overview_project(&mut out, store, &project)?;
        shown += 1;
    }

    if shown == 0 {
        return Ok(match filter {
            Some(filter) => format!("no project matches {filter:?}"),
            None => "no projects indexed".to_string(),
        });
    }

    out.push_str(&format!(
        "cross-project links: {links}{}\n",
        if links > 0 { " (constellation_links to list)" } else { "" },
    ));

    Ok(out)
}

/// The overview block for one project, rendered into `out`: file/symbol counts, the
/// Django surface, the code surface, and the largest packages. Extracted from
/// [`overview_text`] so the per-project body stays one unit under the line bound.
fn overview_project(out: &mut String, store: &Store, project: &ProjectRow) -> Result<(), StoreError> {
    let files = store.files_for(&project.id)?;
    let counts = store.kind_counts(&project.id)?;

    let lookup: FxHashMap<NodeKind, u32> = counts.iter().copied().collect();
    let symbol_total: u32 = counts.iter().map(|(_, count)| *count).sum();

    out.push_str(&format!(
        "[{}] {} ({} files, {symbol_total} symbols)\n",
        project.id,
        project.name,
        files.len(),
    ));

    let django = kind_summary(
        &lookup,
        &[
            ("models", NodeKind::Model),
            ("views", NodeKind::View),
            ("routes", NodeKind::Route),
            ("templates", NodeKind::Template),
        ],
    );

    if !django.is_empty() {
        out.push_str(&format!("  django: {django}\n"));
    }

    let code = kind_summary(
        &lookup,
        &[
            ("classes", NodeKind::Class),
            ("functions", NodeKind::Function),
            ("methods", NodeKind::Method),
        ],
    );

    if !code.is_empty() {
        out.push_str(&format!("  code: {code}\n"));
    }

    let packages = aggregate_by_depth(&files, 2);

    if !packages.is_empty() {
        let listed: Vec<String> = packages
            .iter()
            .take(OVERVIEW_PACKAGES_MAX)
            .map(|(name, file_count, symbol_count)| format!("{name}/ ({file_count}f {symbol_count}s)"))
            .collect();

        out.push_str(&format!("  packages: {}\n", listed.join(", ")));
    }

    out.push('\n');

    Ok(())
}

/// A " 12 models, 3 views" summary of selected kinds present in `lookup`, in the
/// given order, dropping any with a zero count. Empty when none are present.
fn kind_summary(lookup: &FxHashMap<NodeKind, u32>, kinds: &[(&str, NodeKind)]) -> String {
    kinds
        .iter()
        .filter_map(|(label, kind)| {
            let count = lookup.get(kind).copied().unwrap_or(0);

            (count > 0).then(|| format!("{count} {label}"))
        })
        .collect::<Vec<String>>()
        .join(", ")
}

/// The URL map: every route's pattern to its view to the template the view
/// renders, grouped by project. The app's external surface as one table, the
/// orientation a pile of `urls.py` files cannot give at a glance. `filter`
/// restricts to one project; recommended for a large constellation.
#[doc(hidden)]
pub fn routes_text(
    store: &Store,
    project_filter: Option<&str>,
    pattern_filter: Option<&str>,
) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let needle = pattern_filter.map(str::to_lowercase);

    let mut out = String::new();
    let mut shown_projects: u32 = 0;

    for project in projects {
        if let Some(filter) = project_filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        let mut routes = store.nodes_kind_in(&project.id, NodeKind::Route)?;

        if routes.is_empty() {
            continue;
        }

        routes.sort_by(|left, right| {
            left.file_path.cmp(&right.file_path).then(left.span.start_line.cmp(&right.span.start_line))
        });

        // Resolve each route's view and template up front, then keep only the
        // rows the pattern filter matches (against pattern, view, template, or
        // the full route name), so a single-route question returns that route,
        // not the whole 572-row map.
        let mut rows: Vec<(String, String, String)> = Vec::new();

        for route in &routes {
            let view = store
                .callees(&route.id)?
                .into_iter()
                .find(|(kind, _)| *kind == EdgeKind::RoutesTo)
                .map(|(_, node)| node);

            let template = match &view {
                Some(view) => store
                    .callees(&view.id)?
                    .into_iter()
                    .find(|(kind, _)| *kind == EdgeKind::Renders)
                    .map(|(_, node)| node.name),
                None => None,
            };

            let pattern = route_pattern(&route.qualified_name).to_string();
            let view_name = view.as_ref().map_or("(unresolved)", |node| node.name.as_str()).to_string();
            let template_name = template.as_deref().unwrap_or("(no template)").to_string();

            if let Some(needle) = &needle {
                let haystack =
                    format!("{pattern} {view_name} {template_name} {}", route.qualified_name).to_lowercase();

                if !haystack.contains(needle) {
                    continue;
                }
            }

            rows.push((pattern, view_name, template_name));
        }

        if rows.is_empty() {
            continue;
        }

        shown_projects += 1;

        let matching = match &needle {
            Some(needle) => format!(" matching {needle:?}"),
            None => String::new(),
        };

        out.push_str(&format!("[{}] {} ({} routes{matching})\n", project.id, project.name, rows.len()));

        for (pattern, view_name, template_name) in rows.iter().take(ROUTES_PER_PROJECT_MAX) {
            out.push_str(&format!("  {pattern}  →  {view_name}  →  {template_name}\n"));
        }

        if rows.len() > ROUTES_PER_PROJECT_MAX {
            let hint = if needle.is_some() { "narrow the pattern" } else { "filter by project" };

            out.push_str(&format!("  (+{} more; {hint})\n", rows.len() - ROUTES_PER_PROJECT_MAX));
        }

        out.push('\n');
    }

    if shown_projects == 0 {
        return Ok(match (project_filter, pattern_filter) {
            (_, Some(pattern)) => format!("no routes matching {pattern:?}"),
            (Some(filter), None) => format!("no routes for {filter:?}"),
            (None, None) => "no routes indexed".to_string(),
        });
    }

    Ok(out)
}

/// The URL pattern carried in a route node's qualified name
/// (`…::route::<pattern>` → `<pattern>`), the human-facing part of the route.
fn route_pattern(qualified_name: &str) -> &str {
    qualified_name.split("route::").nth(1).unwrap_or(qualified_name)
}

/// The architectural layer a symbol belongs to, inferred from this
/// codebase's strict file/name conventions, so an agent can tell a page view from
/// a json endpoint, a service from a queryset, at a glance. `None` for ordinary
/// code that matches no convention.
fn symbol_role(node: &Node) -> Option<&'static str> {
    let path = node.file_path.as_str();
    let name = node.name.as_str();

    match node.kind {
        NodeKind::Route => return Some("route"),
        NodeKind::Template => return Some("template"),
        NodeKind::Selector => return Some("css-selector"),
        NodeKind::Model => return Some("model"),
        _ => {}
    }

    if is_test_path(path) {
        return Some("test");
    }

    // Class-name conventions, most specific first.
    if name.ends_with("QuerySet") || name.ends_with("Manager") {
        return Some("queryset");
    }
    if name.ends_with("Service") {
        return Some("service");
    }
    if name.ends_with("Form") {
        return Some("form");
    }
    if name.ends_with("Serializer") {
        return Some("serializer");
    }
    if name.ends_with("Admin") {
        return Some("admin");
    }
    if name.ends_with("Choices") {
        return Some("choices");
    }

    // File-path conventions for the view sub-layers and the service/data layers.
    let role = if path.contains("json_views") {
        "json-view"
    } else if path.contains("form_views") {
        "form-view"
    } else if path.contains("page_views") {
        "page-view"
    } else if path.contains("template_views") {
        "template-view"
    } else if path.ends_with("/views.py") || path.contains("/views/") {
        "view"
    } else if path.contains("queryset") {
        "queryset"
    } else if path.contains("factories") || path.contains("factory") {
        "factory"
    } else if path.ends_with("/services.py") || path.contains("/services/") {
        "service"
    } else if path.ends_with("/forms.py") {
        "form"
    } else if path.ends_with("/admin.py") {
        "admin"
    } else if path.ends_with("/serializers.py") {
        "serializer"
    } else {
        return None;
    };

    Some(role)
}

/// Whether an edge kind extends a feature downstream (followed as
/// callees): the Django request/data path (routing, rendering, template
/// inheritance, model relations, service/queryset instantiation, base mixins,
/// signal handlers). Generic `calls` is excluded so a view's every helper does
/// not dilute the slice.
fn is_feature_downstream(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::RoutesTo
            | EdgeKind::Renders
            | EdgeKind::ExtendsTemplate
            | EdgeKind::IncludesTemplate
            | EdgeKind::RelatesTo
            | EdgeKind::Instantiates
            | EdgeKind::Extends
            | EdgeKind::Receives
            | EdgeKind::Handles
            | EdgeKind::Resolves
    )
}

/// Whether an edge kind is pulled in upstream (callers) from the seed only:
/// the entry points into a feature (the route that hits a view, the view that
/// renders a template, the models that relate to a model).
fn is_feature_upstream(kind: EdgeKind) -> bool {
    matches!(kind, EdgeKind::RoutesTo | EdgeKind::Renders | EdgeKind::RelatesTo)
}

/// Whether a feature edge is followed transitively (the request chain:
/// route->view->template->includes). Other feature edges (relations,
/// instantiation, bases) are collected one hop deep only, so a densely related
/// model does not drag the whole model graph into the slice.
fn is_feature_chain(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::RoutesTo | EdgeKind::Renders | EdgeKind::ExtendsTemplate | EdgeKind::IncludesTemplate
    )
}

/// The display group a node falls into for the feature slice, and its order.
fn feature_category(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Route => 0,
        NodeKind::View => 1,
        NodeKind::Template => 2,
        NodeKind::Model => 3,
        NodeKind::Class => 4,
        NodeKind::Function | NodeKind::Method => 5,
        _ => 6,
    }
}

/// A disambiguation listing for too many same-named definitions to slice as one
/// feature, naming them by the `file::name` a caller passes to target one,
/// instead of interleaving every app's same-named view into a single
/// undifferentiated dump. Seeds arrive pre-ranked (definitions first), so the
/// head of the list is the strongest few.
fn feature_disambiguation(symbol: &str, seeds: &[Node]) -> String {
    const SHOWN_MAX: usize = 12;

    let projects: FxHashSet<&str> = seeds.iter().map(|node| node.project_id.as_str()).collect();

    let mut out = format!(
        "{symbol:?} names {} definitions across {} project(s): too many to slice as one feature. \
         Name one to slice it (pass the file::name shown):\n",
        seeds.len(),
        projects.len(),
    );

    for node in seeds.iter().take(SHOWN_MAX) {
        out.push_str(&format!("  {}\n", node_line(node)));
    }

    if seeds.len() > SHOWN_MAX {
        out.push_str(&format!(
            "  (+{} more; constellation_search {symbol:?} to list all)\n",
            seeds.len() - SHOWN_MAX,
        ));
    }

    out
}

/// The vertical slice of a feature: from a route, view, template, or model,
/// walk the Django structural edges (route->view->template->includes, model
/// relations, service/queryset instantiation, base mixins, signal handlers) into
/// one grouped digest (the whole request/data path an agent must hold for a
/// feature, without chaining callers/callees by hand). Bounded in depth and count.
#[doc(hidden)]
pub fn feature_text(store: &Store, symbol: &str) -> Result<String, StoreError> {
    let seeds: Vec<Node> = seed_nodes(store, symbol)?
        .into_iter()
        .filter(|node| {
            matches!(
                node.kind,
                NodeKind::Model
                    | NodeKind::View
                    | NodeKind::Route
                    | NodeKind::Template
                    | NodeKind::Class
                    | NodeKind::Function
                    | NodeKind::Method
            )
        })
        .collect();

    if seeds.is_empty() {
        return Ok(format!("no model/view/route/template/class named {symbol:?} to slice"));
    }

    if seeds.len() > FEATURE_SEED_DISAMBIG_MAX {
        return Ok(feature_disambiguation(symbol, &seeds));
    }

    let mut visited: FxHashSet<String> = seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut members: Vec<Node> = seeds.clone();
    let mut frontier: Vec<(NodeId, u32)> = seeds.iter().map(|node| (node.id.clone(), 0)).collect();

    while let Some((id, depth)) = frontier.pop() {
        if members.len() >= FEATURE_NODES_MAX {
            break;
        }

        for (kind, node) in store.callees(&id)? {
            if !is_feature_downstream(kind) || !visited.insert(node.id.as_str().to_string()) {
                continue;
            }

            let next = node.id.clone();
            members.push(node);

            if is_feature_chain(kind) && depth + 1 < FEATURE_DEPTH_MAX {
                frontier.push((next, depth + 1));
            }

            if members.len() >= FEATURE_NODES_MAX {
                break;
            }
        }

        // Upstream entry points from the seed only, never recursed.
        if depth == 0 {
            for (kind, node) in store.callers(&id)? {
                if is_feature_upstream(kind) && visited.insert(node.id.as_str().to_string()) {
                    members.push(node);
                }
            }
        }
    }

    members.truncate(FEATURE_NODES_MAX);
    members.sort_by(|left, right| {
        feature_category(left.kind)
            .cmp(&feature_category(right.kind))
            .then(left.file_path.cmp(&right.file_path))
            .then(left.span.start_line.cmp(&right.span.start_line))
    });

    let mut out = format!("feature slice for {symbol:?} ({} symbols):\n", members.len());
    let mut current: Option<u8> = None;

    for node in &members {
        let category = feature_category(node.kind);

        if current != Some(category) {
            out.push_str(&format!("  {}:\n", FEATURE_LABELS[category as usize]));
            current = Some(category);
        }

        match symbol_role(node) {
            Some(role) => out.push_str(&format!("    {} [{role}]\n", node_line(node))),
            None => out.push_str(&format!("    {}\n", node_line(node))),
        }
    }

    Ok(out)
}

/// The cross-project links grouped by repo pair: an import in one repo
/// resolved to a definition in another. Pairs are ordered by link count descending;
/// each edge prints its source and target endpoints. A `filter` restricts to links
/// touching that project on either end. Bounded by `limit`.
fn links_text(store: &Store, filter: Option<&str>, limit: u32) -> Result<String, StoreError> {
    let total = store.count_links()?;

    if total == 0 {
        return Ok("no cross-project links (index a second repo with `constellation link`)".to_string());
    }

    let fetch = limit.saturating_mul(2).clamp(limit, LINKS_FETCH_MAX).max(limit);
    let links = store.link_edges(filter, fetch)?;

    if links.is_empty() {
        return Ok(match filter {
            Some(filter) => format!("no cross-project links touching {filter:?}"),
            None => "no cross-project links".to_string(),
        });
    }

    // Group by directed repo pair, preserving first-seen order, so the output
    // reads pair by pair rather than interleaving every repo combination.
    // Pair keys borrow the project ids out of `links`, which outlives the grouping.
    let mut pair_order: Vec<(&str, &str)> = Vec::new();
    let mut by_pair: FxHashMap<(&str, &str), Vec<&LinkEdge>> = FxHashMap::default();

    for link in &links {
        let pair = (link.source.project_id.as_str(), link.target.project_id.as_str());

        match by_pair.get_mut(&pair) {
            Some(group) => group.push(link),
            None => {
                pair_order.push(pair);
                by_pair.insert(pair, vec![link]);
            }
        }
    }

    pair_order.sort_by_key(|pair| std::cmp::Reverse(by_pair.get(pair).map_or(0, Vec::len)));

    let mut out = format!("cross-project links: {} shown of {total}\n", links.len());
    let mut printed: usize = 0;

    for pair in &pair_order {
        let group = by_pair.get(pair).expect("ordered pair has a group");

        out.push_str(&format!("\n{} -> {}: {}\n", pair.0, pair.1, group.len()));

        for link in group {
            if printed >= limit as usize {
                break;
            }

            out.push_str(&format!(
                "  [{}] {} ({}:{}) -> {} ({}:{})\n",
                link.kind.as_str(),
                link.source.name,
                link.source.file_path,
                link.source.span.start_line,
                link.target.name,
                link.target.file_path,
                link.target.span.start_line,
            ));

            printed += 1;
        }
    }

    if links.len() > printed {
        out.push_str(&format!("(+{} more; raise limit)\n", links.len() - printed));
    }

    Ok(out)
}

/// The single-line render of one node: project, kind, name, qualified name, and
/// file location.
fn node_line(node: &Node) -> String {
    format!(
        "[{}] {} {} @ {} ({}:{})",
        node.project_id,
        node.kind.as_str(),
        node.name,
        node.qualified_name,
        node.file_path,
        node.span.start_line,
    )
}

/// The maximum signature characters appended to a rendered node line before
/// truncation, so a multi-line or very long signature cannot blow up a listing.
const NODE_LINE_SIGNATURE_CHARS_MAX: usize = 120;

/// The render of several nodes, one per line, each followed by a compact,
/// whitespace-collapsed signature when the extractor captured one, so a search
/// shows a symbol's call shape inline (the way codegraph's search does) without a
/// second `node` lookup.
fn node_lines(nodes: &[Node]) -> String {
    let mut out = String::new();

    for node in nodes {
        out.push_str(&node_line(node));

        if let Some(signature) = node.signature.as_deref() {
            let compact = signature.split_whitespace().collect::<Vec<_>>().join(" ");

            if !compact.is_empty() {
                let shown: String = compact.chars().take(NODE_LINE_SIGNATURE_CHARS_MAX).collect();
                let truncated = compact.chars().count() > NODE_LINE_SIGNATURE_CHARS_MAX;
                let ellipsis = if truncated { "…" } else { "" };

                out.push_str(&format!("  {shown}{ellipsis}"));
            }
        }

        out.push('\n');
    }

    out
}

/// The given text wrapped as a successful tool result.
fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text)])
}
