//! The server's state and lifecycle: what one session holds, how it opens and
//! closes, and the helpers its tools are built from.
//!
//! The tool declarations themselves are in [`router`], because a list of forty
//! tools reads as a list only when nothing else shares the file.
//!
//! Two things decide how this server behaves under concurrent tool calls. The
//! graph is read through a [`StorePool`], so requests contend only when they
//! outnumber its connections rather than on one global handle. And the explore
//! cache is handed out as an `Arc`, so ranking and rendering (which read source
//! files off disk) run with no lock held at all.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use constellation_graph::{EdgeKind, Node, NodeKind, Profile};
use constellation_store::{READERS_MAX, Store, StoreError, StorePool};
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, ServiceExt, transport::stdio};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::error::{McpError, NO_INDEX_MESSAGE, lock_recover, panic_error, run_blocking};
use crate::git::now_unix_secs;
use crate::limits::{EXPLORE_SESSION_RECORD_MAX, EXPLORE_SYMBOLS_MAX, FLOW_HOPS_MAX};
use crate::rank::{commit_times_by_file, explore_budget, query_names, rank_by_structure};
use crate::render::{RenderRequest, file_key, render_ranked, text_result, with_hint};
use crate::tools::flows::{flow_section, render_flow_path, shortest_flow_path};
use crate::tools::impact::project_roots;
use crate::tools::search::{
    content_seed_nodes, explore_coverage_note, explore_uncovered_seeds, seed_nodes,
};
use crate::{hints, recency};

mod router;

/// The working-tree snapshot the watcher keeps refreshed.
///
/// The one deliberate global in this crate. `node_line` is called from roughly
/// thirty places across every tool, and threading a snapshot handle through all
/// of them would put a parameter in every signature to serve one four-character
/// suffix. It is written exactly once, by [`ConstellationServer::start_watcher`],
/// and read-only afterwards; when it was never set (a server built for a test,
/// or one whose watcher failed to start) every symbol simply renders unmarked.
static WORKING_TREE: std::sync::OnceLock<constellation_index::GitStatusHandle> =
    std::sync::OnceLock::new();

/// The working-tree marker for one symbol's file, or an empty string when the
/// file is clean, the snapshot is absent, or the project is not a git checkout.
pub(crate) fn working_tree_marker(node: &Node) -> &'static str {
    match WORKING_TREE.get() {
        Some(handle) => {
            handle.snapshot().state(node.project_id.as_str(), &node.file_path).marker()
        }
        None => "",
    }
}

/// The read connections one server opens: one per core, bounded by the pool's
/// own cap. An MCP client issues a handful of concurrent tool calls at most.
fn reader_count() -> usize {
    std::thread::available_parallelism().map_or(1, |count| count.get()).min(READERS_MAX)
}

/// The in-process cache for `explore`: the node list, the undirected adjacency
/// that random-walk ranking traverses, and the directed edges flow tracing
/// follows.
///
/// Immutable once built, and handed to callers as an `Arc`, so a query ranks and
/// renders against it without holding any lock. It is replaced wholesale when
/// the graph changes underneath the server (see
/// [`ConstellationServer::invalidate`]); a query already holding one finishes
/// against the graph it started from rather than seeing it change mid-render.
struct ExploreCache {
    generation: u64,
    nodes: Vec<Node>,
    /// The node positions ordered by node id, so looking a symbol up by id is a
    /// binary search over `nodes`. A hash map here held a second copy of every
    /// id for the life of the cache; this holds four bytes a node.
    by_id: Vec<u32>,
    adjacency: Vec<Vec<u32>>,
    out_edges: Vec<Vec<(u32, EdgeKind)>>,
}

impl ExploreCache {
    fn build(store: &Store, generation: u64) -> Result<Self, StoreError> {
        let nodes = store.all_nodes(None)?;
        let edges = store.all_edges_kinded()?;

        let count = nodes.len();

        assert!(count <= u32::MAX as usize, "graph must hold fewer than u32::MAX nodes");

        let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); count];
        let mut out_edges: Vec<Vec<(u32, EdgeKind)>> = vec![Vec::new(); count];

        {
            // A transient map for the edge join: there are far more endpoint
            // lookups than nodes, so this wants hashing rather than a search.
            // It borrows its keys from `nodes` and is dropped before the cache
            // is returned, so it costs no long-lived allocation.
            let mut positions: FxHashMap<&str, u32> =
                FxHashMap::with_capacity_and_hasher(count, Default::default());

            for (position, node) in nodes.iter().enumerate() {
                positions.insert(node.id.as_str(), position as u32);
            }

            for (source, target, kind) in &edges {
                let (Some(&from), Some(&to)) =
                    (positions.get(source.as_str()), positions.get(target.as_str()))
                else {
                    continue;
                };

                adjacency[from as usize].push(to);
                adjacency[to as usize].push(from);
                out_edges[from as usize].push((to, *kind));
            }
        }

        let mut by_id: Vec<u32> = (0..count as u32).collect();

        by_id.sort_unstable_by(|&left, &right| {
            nodes[left as usize].id.as_str().cmp(nodes[right as usize].id.as_str())
        });

        assert!(adjacency.len() == nodes.len(), "adjacency has one entry per node");
        assert!(out_edges.len() == nodes.len(), "out-edges have one entry per node");
        assert!(by_id.len() == nodes.len(), "the id index covers every node exactly once");

        Ok(Self { generation, nodes, by_id, adjacency, out_edges })
    }

    /// The position of the node with this id, or `None` when the cache does not
    /// hold it (an id from a query that raced a re-index).
    fn position(&self, id: &str) -> Option<usize> {
        let slot = self
            .by_id
            .binary_search_by(|&position| self.nodes[position as usize].id.as_str().cmp(id))
            .ok()?;

        Some(self.by_id[slot] as usize)
    }

    /// The cached positions of the given node ids, dropping any the cache does
    /// not hold.
    fn positions<'ids>(&self, ids: impl IntoIterator<Item = &'ids str>) -> Vec<usize> {
        ids.into_iter().filter_map(|id| self.position(id)).collect()
    }
}

/// The whole of what one `explore` query takes from the store, gathered while a pool
/// connection is held and complete before ranking begins.
///
/// The point of the struct is the lifetime: once it exists, the rest of the
/// query (ranking, flow tracing, reading source off disk) touches no lock, so a
/// large explore does not block a concurrent `search` or `callers`.
struct ExploreSeeds {
    cache: Arc<ExploreCache>,
    seed_positions: Vec<usize>,
    uncovered_seeds: Vec<(String, String)>,
    roots: FxHashMap<String, String>,
    last_commits: FxHashMap<String, i64>,
}

/// The outcome of an `explore` query before any ranking happened.
enum ExploreStart {
    /// The server has no database.
    NoIndex,
    /// The query matched no symbol, by name, content, or fallback.
    NoMatch,
    Ready(Box<ExploreSeeds>),
}

/// The MCP server itself: the graph every tool reads, plus the per-session state
/// that shapes what it answers with (the explore cache, the index generation a
/// cursor is validated against, the recent-tool intent behind a hint, and the
/// files this session has surfaced).
///
/// Cloneable and cheap to clone: every field is behind an `Arc`, so the clones
/// the transport hands each request share one graph and one session rather than
/// each opening their own.
#[derive(Clone)]
pub struct ConstellationServer {
    /// The graph database, or `None` when serving outside any indexed project
    /// (an unavailable server): every tool then returns [`NO_INDEX_MESSAGE`].
    ///
    /// Read through a pool rather than one handle, so concurrent tool calls are
    /// concurrent. There is no outer lock because the pool is fixed at
    /// construction and every connection inside it is `query_only`.
    store: Arc<Option<StorePool>>,
    /// The workspace's company conventions, read once from its
    /// `.constellation/config.toml` at startup. Query-time judgment reads it:
    /// which names the framework reaches, and so which edgeless definition is
    /// really dead code.
    profile: Arc<Profile>,
    explore_cache: Arc<Mutex<Option<Arc<ExploreCache>>>>,
    generation: Arc<AtomicU64>,
    /// The last few tool names, so a hint can suggest review follow-ups during
    /// a review and exploration follow-ups during exploration.
    intent: Arc<Mutex<hints::SessionIntent>>,
    /// The files this session has surfaced, which break ties inside a relevance
    /// band. In memory only: a session's attention is not a fact about the code.
    session: Arc<Mutex<recency::SessionFiles>>,
    /// The running watcher, held so server shutdown stops it deterministically
    /// rather than leaving a re-index running against a closing database.
    watcher: Arc<Mutex<Option<constellation_index::WatchHandle>>>,
}

impl ConstellationServer {
    /// The server for the database at `path`, reading through a pool sized to
    /// the host, under the profile that database's workspace configures. The
    /// form [`serve`] uses.
    pub fn open(path: &Path) -> Result<Self, McpError> {
        let pool = StorePool::open(path, reader_count())?;
        let profile = constellation_index::load_profile_for_database(path);

        Ok(Self::with_pool(Some(pool), profile))
    }

    /// A new server wrapping the given store, under the default profile.
    ///
    /// Reads through a pool of one, since the caller already owns the
    /// connection. [`ConstellationServer::open`] is the concurrent form, and the
    /// only one that can locate a workspace config to read a profile from.
    pub fn new(store: Store) -> Self {
        Self::with_pool(Some(StorePool::single(store)), Profile::default())
    }

    /// A server with no database: it completes the MCP handshake and answers
    /// every tool with [`NO_INDEX_MESSAGE`], rather than failing to start. Built by
    /// [`serve_unavailable`] when `serve` is launched outside any indexed project,
    /// so a global registration stays quiet in non-Django repos instead of erroring.
    pub fn unavailable() -> Self {
        Self::with_pool(None, Profile::default())
    }

    fn with_pool(store: Option<StorePool>, profile: Profile) -> Self {
        Self {
            store: Arc::new(store),
            profile: Arc::new(profile),
            explore_cache: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            intent: Arc::new(Mutex::new(hints::SessionIntent::new())),
            session: Arc::new(Mutex::new(recency::SessionFiles::new())),
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    /// The background watcher started and retained. A watcher that fails to
    /// start is reported and the server continues serving the graph as indexed,
    /// since a stale graph beats no server at all.
    pub fn start_watcher(&self, database: &Path) {
        let server = self.clone();

        match constellation_index::watch_constellation(database, move || server.invalidate()) {
            Ok(handle) => {
                // Published once; every later read is lock-free apart from the
                // snapshot's own mutex. A second server in the same process
                // keeps the first one's handle, which is correct: they watch
                // the same roots.
                let _ = WORKING_TREE.set(handle.git_status());

                *lock_recover(&self.watcher) = Some(handle);
            }
            Err(error) => eprintln!("constellation: watcher disabled: {error}"),
        }
    }

    /// The watcher stopped and joined, so no re-index outlives the server. Safe
    /// to call more than once.
    pub fn shutdown(&self) {
        if let Some(mut handle) = lock_recover(&self.watcher).take() {
            handle.stop();
        }
    }

    /// The files a response surfaced recorded against this session, so a later
    /// query can break a relevance tie toward what the agent has been working
    /// on. Bounded and cooldown-guarded by [`recency::SessionFiles`].
    pub(crate) fn record_session_files<'nodes>(&self, paths: impl Iterator<Item = &'nodes str>) {
        let now = now_unix_secs();
        let mut session = lock_recover(&self.session);

        for path in paths.take(recency::SESSION_FILES_MAX) {
            session.touch(path, now);
        }
    }

    /// The recency score of one file for the current session, blending its
    /// working-tree state, this session's attention, and commit history.
    pub(crate) fn file_recency(&self, project_id: &str, file_path: &str, commit: f64) -> f64 {
        let state = match WORKING_TREE.get() {
            Some(handle) => handle.snapshot().state(project_id, file_path),
            None => constellation_index::WorkingTreeState::Clean,
        };

        let session = lock_recover(&self.session).score(file_path, now_unix_secs());

        recency::file_recency(state, session, commit)
    }

    /// A tool call recorded and the hint line for its response, or an empty
    /// string when nothing applies.
    pub(crate) fn hint_for(&self, tool: &'static str, facts: &hints::HintFacts) -> String {
        let mut intent = lock_recover(&self.intent);
        let line = hints::hint(tool, facts, &intent).unwrap_or_default();

        intent.record(tool);

        line
    }

    /// The generation bump that makes the next `explore` rebuild its cached
    /// adjacency. Call after the graph changes underneath the server (a
    /// mid-session re-index).
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The result of `action` against the graph, contained so a panic becomes an
    /// error response and store work never starves the runtime.
    ///
    /// `unavailable` supplies the answer when the server has no database, and is
    /// only called in that case. Generic in the result rather than fixed to
    /// `String`, because a tool that wants a count or a flag should say so:
    /// returning one through this used to mean formatting it and parsing it back.
    pub(crate) fn with_store<T>(
        &self,
        unavailable: impl FnOnce() -> T,
        action: impl FnOnce(&Store) -> Result<T, StoreError>,
    ) -> Result<T, ErrorData> {
        run_blocking(|| {
            let result = catch_unwind(AssertUnwindSafe(|| match self.store.as_ref() {
                Some(pool) => pool.with_read(action),
                None => Ok(unavailable()),
            }));

            match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(ErrorData::internal_error(error.to_string(), None)),
                Err(_) => Err(panic_error()),
            }
        })
    }

    /// The text `action` renders from the graph, degrading to
    /// [`NO_INDEX_MESSAGE`] when the server has no database. What every text
    /// tool is built on.
    pub(crate) fn with_store_text(
        &self,
        action: impl FnOnce(&Store) -> Result<String, StoreError>,
    ) -> Result<String, ErrorData> {
        self.with_store(|| NO_INDEX_MESSAGE.to_string(), action)
    }

    /// The current explore cache, rebuilt first when the graph has changed under
    /// it. The lock is held only long enough to hand out an `Arc`.
    fn explore_cache(&self, store: &Store) -> Result<Arc<ExploreCache>, StoreError> {
        let generation = self.generation.load(Ordering::Relaxed);
        let mut cached = lock_recover(&self.explore_cache);

        if let Some(fresh) = cached.as_ref().filter(|cache| cache.generation == generation) {
            return Ok(Arc::clone(fresh));
        }

        let built = Arc::new(ExploreCache::build(store, generation)?);

        *cached = Some(Arc::clone(&built));

        Ok(built)
    }

    /// The handler for one `explore` query: ranks the graph by structure from the query's
    /// seeds and returns the relevant source. Run under `block_in_place` so its
    /// store work never starves the runtime's event loop, and contained so a
    /// panic becomes an error response rather than an unanswered request.
    pub(crate) fn explore(
        &self,
        query: &str,
        max_files: u32,
        outline: bool,
    ) -> Result<CallToolResult, ErrorData> {
        self.contained(|| self.explore_locked(query, max_files, outline))
    }

    /// The text one `explore` query returns, without the MCP result wrapper.
    /// The entry point the eval harness measures retrieval quality through, so
    /// it exercises exactly the ranking the agent sees rather than a
    /// reimplementation of it.
    pub fn explore_text(&self, query: &str, max_files: u32) -> Result<String, McpError> {
        self.explore_locked(query, max_files, false)
            .map_err(|error| McpError::Serve(error.message.to_string()))
    }

    /// An explore query, start to finish: gather from the store, then rank and
    /// render with nothing locked.
    fn explore_locked(
        &self,
        query: &str,
        max_files: u32,
        outline: bool,
    ) -> Result<String, ErrorData> {
        match self.explore_start(query)? {
            ExploreStart::NoIndex => Ok(NO_INDEX_MESSAGE.to_string()),
            ExploreStart::NoMatch => Ok(format!("no symbols matching {query:?}")),
            ExploreStart::Ready(seeds) => {
                Ok(self.explore_render(&seeds, query, max_files, outline))
            }
        }
    }

    /// The whole of what one explore query needs from the graph, read while a pool
    /// connection is held. Nothing after this touches the store.
    fn explore_start(&self, query: &str) -> Result<ExploreStart, ErrorData> {
        let Some(pool) = self.store.as_ref() else {
            return Ok(ExploreStart::NoIndex);
        };

        let started = pool.with_read(|store| {
            let Some(seeds) = explore_seed_nodes(store, query)? else {
                return Ok(ExploreStart::NoMatch);
            };

            let uncovered_seeds = explore_uncovered_seeds(store, &seeds);
            let cache = self.explore_cache(store)?;

            let seed_positions = cache.positions(seeds.iter().map(|node| node.id.as_str()));

            let gathered = ExploreSeeds {
                seed_positions,
                uncovered_seeds,
                roots: project_roots(store)?,
                last_commits: commit_times_by_file(store)?,
                cache,
            };

            Ok(ExploreStart::Ready(Box::new(gathered)))
        });

        started.map_err(internal_error)
    }

    /// The response for one explore query, ranked and rendered from an already
    /// gathered [`ExploreSeeds`]. Holds no store connection and no cache lock,
    /// so the file reads inside `render_ranked` block nothing else.
    fn explore_render(
        &self,
        seeds: &ExploreSeeds,
        query: &str,
        max_files: u32,
        outline: bool,
    ) -> String {
        let cache = seeds.cache.as_ref();
        let node_count = cache.nodes.len();

        assert!(seeds.seed_positions.len() <= node_count, "seed positions index the cached graph");

        let ranked = rank_by_structure(&seeds.seed_positions, &cache.adjacency);
        let now = now_unix_secs();

        let recency = |project_id: &str, file_path: &str| {
            let commit = recency::commit_recency(
                seeds.last_commits.get(&file_key(project_id, file_path)).copied(),
                now,
            );

            self.file_recency(project_id, file_path, commit)
        };

        let flow = flow_section(&cache.nodes, &cache.out_edges, &seeds.seed_positions, query);

        let render =
            RenderRequest { budget: explore_budget(node_count), max_files, outline, query };

        let (body, emitted) =
            render_ranked(&cache.nodes, &ranked, &seeds.roots, &render, recency);

        let emitted_ids: Vec<&str> =
            emitted.iter().map(|&position| cache.nodes[position].id.as_str()).collect();

        let coverage_note = explore_coverage_note(&seeds.uncovered_seeds, &emitted_ids);

        // Record what this call surfaced, so a later query in the same session
        // breaks a relevance tie toward the area the agent is working in.
        self.record_session_files(
            ranked
                .iter()
                .take(EXPLORE_SESSION_RECORD_MAX)
                .map(|&position| cache.nodes[position].file_path.as_str()),
        );

        let facts = hints::HintFacts {
            at_byte_budget: body.contains("output budget reached"),
            flows_available: false,
            has_uncovered_symbol: !coverage_note.is_empty(),
            named_model: cache
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::Model && query_names(query, &node.name)),
            named_symbol: seeds
                .seed_positions
                .first()
                .map(|&position| cache.nodes[position].name.clone()),
        };

        let hint = self.hint_for("constellation_explore", &facts);

        with_hint(format!("{flow}{coverage_note}{body}"), &hint)
    }

    /// The text one `path` query returns, without the MCP result wrapper, the
    /// sibling of [`ConstellationServer::explore_text`]. Exported for the
    /// integration suite, which pins the rendered chain.
    #[doc(hidden)]
    pub fn path_text(&self, from: &str, to: &str) -> Result<String, McpError> {
        self.path_locked(from, to).map_err(|error| McpError::Serve(error.message.to_string()))
    }

    /// The handler for one `path` query, contained like [`ConstellationServer::explore`] so
    /// a panic becomes an error response and store work never starves the runtime.
    pub(crate) fn path(&self, from: &str, to: &str) -> Result<CallToolResult, ErrorData> {
        self.contained(|| self.path_locked(from, to))
    }

    /// The shortest flow path between two symbols over the cached directed
    /// graph. Reuses explore's adjacency cache, then searches both directions so
    /// "how does X reach Y" finds the link regardless of which endpoint the
    /// caller named first.
    fn path_locked(&self, from: &str, to: &str) -> Result<String, ErrorData> {
        let Some(pool) = self.store.as_ref() else {
            return Ok(NO_INDEX_MESSAGE.to_string());
        };

        let endpoints = pool
            .with_read(|store| {
                let from_ids = seed_ids(store, from)?;
                let to_ids = seed_ids(store, to)?;
                let cache = self.explore_cache(store)?;

                Ok((from_ids, to_ids, cache))
            })
            .map_err(internal_error)?;

        let (from_ids, to_ids, cache) = endpoints;

        if from_ids.is_empty() {
            return Ok(format!("no symbol named {from:?}"));
        }

        if to_ids.is_empty() {
            return Ok(format!("no symbol named {to:?}"));
        }

        let from_positions = cache.positions(from_ids.iter().map(String::as_str));
        let to_positions = cache.positions(to_ids.iter().map(String::as_str));

        match trace_path(&cache, &from_positions, &to_positions) {
            Some(rendered) => Ok(rendered.render(from, to)),
            None => Ok(format!(
                "no flow path between {from:?} and {to:?} within {FLOW_HOPS_MAX} hops \
                 (call / route / render / instantiate / inherit / template-inherit edges)"
            )),
        }
    }

    /// A handler run off the async runtime and contained, so a panic becomes
    /// an error response rather than an unanswered request or a dead process.
    fn contained(
        &self,
        handler: impl FnOnce() -> Result<String, ErrorData>,
    ) -> Result<CallToolResult, ErrorData> {
        run_blocking(|| match catch_unwind(AssertUnwindSafe(handler)) {
            Ok(Ok(text)) => Ok(text_result(text)),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(panic_error()),
        })
    }
}

/// A store failure rendered as the MCP error the client receives.
fn internal_error(error: StoreError) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

/// The seed nodes one explore query starts from: name and docstring matches,
/// widened to an any-token match when the strict one found nothing, plus the
/// definitions of files whose *body* matched. `None` when nothing matched at all.
fn explore_seed_nodes(store: &Store, query: &str) -> Result<Option<Vec<Node>>, StoreError> {
    let mut seeds = store.search_nodes(query, EXPLORE_SYMBOLS_MAX)?;

    if seeds.is_empty() {
        seeds = store.search_nodes_any(query, EXPLORE_SYMBOLS_MAX)?;
    }

    let mut seen: FxHashSet<String> =
        seeds.iter().map(|node| node.id.as_str().to_string()).collect();

    for node in content_seed_nodes(store, query)? {
        if seen.insert(node.id.as_str().to_string()) {
            seeds.push(node);
        }
    }

    Ok((!seeds.is_empty()).then_some(seeds))
}

/// A traced flow path, and which direction it runs in.
struct TracedPath {
    /// Whether the path runs from the caller's `to` back to their `from`, which
    /// is worth saying: the answer is real but not the one they asked for.
    reversed: bool,
    source: usize,
    hops: Vec<(usize, EdgeKind)>,
    nodes: Arc<ExploreCache>,
}

impl TracedPath {
    fn render(&self, from: &str, to: &str) -> String {
        let mut out = if self.reversed {
            format!("# path {to} -> {from} (only this direction connects):\n")
        } else {
            format!("# path {from} -> {to}:\n")
        };

        render_flow_path(&mut out, &self.nodes.nodes, self.source, &self.hops);

        out
    }
}

/// The first flow path found between any pair of endpoints, forwards for
/// preference and backwards otherwise.
fn trace_path(
    cache: &Arc<ExploreCache>,
    from_positions: &[usize],
    to_positions: &[usize],
) -> Option<TracedPath> {
    for &source in from_positions {
        for &target in to_positions {
            if let Some(hops) = shortest_flow_path(&cache.out_edges, source, target) {
                return Some(TracedPath {
                    reversed: false,
                    source,
                    hops,
                    nodes: Arc::clone(cache),
                });
            }

            if let Some(hops) = shortest_flow_path(&cache.out_edges, target, source) {
                return Some(TracedPath {
                    reversed: true,
                    source: target,
                    hops,
                    nodes: Arc::clone(cache),
                });
            }
        }
    }

    None
}

/// The constellation database opened and served over stdio until the client
/// disconnects. A background thread catches the graph up with the working tree
/// and then keeps it in sync for the life of the session, so serving starts
/// immediately instead of blocking on an initial re-index.
pub fn serve(database: &Path) -> Result<(), McpError> {
    let server = ConstellationServer::open(database)?;

    server.start_watcher(database);

    let outcome = serve_stdio(server.clone());

    // Stop and join before returning, so the process never exits with a
    // re-index still writing to the database it is about to close.
    server.shutdown();

    outcome
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
        let running =
            server.serve(stdio()).await.map_err(|error| McpError::Serve(error.to_string()))?;

        running.waiting().await.map_err(|error| McpError::Serve(error.to_string()))?;

        Ok::<(), McpError>(())
    })
}

/// The node ids of every definition matching `symbol`, owned (the
/// connection-free handle the path search keeps after the read returns).
fn seed_ids(store: &Store, symbol: &str) -> Result<Vec<String>, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    Ok(nodes.iter().map(|node| node.id.as_str().to_string()).collect())
}
