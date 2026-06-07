#![forbid(unsafe_code)]

//! Project indexing: walk a repository, parse each supported file with the
//! matching extractor, and persist the resulting graph into the store. This
//! is the orchestration layer that turns a directory on disk into one
//! project's slice of the constellation.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use constellation_extraction::{
    CssExtractor, ExtractionOutput, Extractor, JavaScriptExtractor, PythonExtractor,
    SOURCE_BYTES_MAX, TemplateExtractor,
};
use constellation_graph::{Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span};
use constellation_linking::{ImportLinker, LinkContext, PendingImport, is_linkable, module_matches};
use constellation_resolution::{
    COLLECTION_CONTEXT, DjangoResolver, EventRole, FrameworkResolver, ImportMapping,
    ResolutionContext, UnresolvedRef, edge_from_resolved, resolve_reference,
};
use constellation_store::{FileIndex, Store, StoreError};
use ignore::{WalkBuilder, WalkState};
use notify::{Event, RecursiveMode, Watcher, recommended_watcher};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use thiserror::Error;

mod companions;

pub use companions::{CompanionTarget, discover_companions, discover_versions};

/// The fail-fast bound on the number of filesystem entries one walk may visit.
pub const FILE_COUNT_MAX: u32 = 5_000_000;

/// The files extracted per parallel batch. Bounds peak memory (a batch's graphs are
/// held until persisted) while keeping every CPU busy between store writes.
const EXTRACT_CHUNK_MAX: usize = 256;

/// The fail-fast bound on references processed in one resolution pass.
const REFERENCE_COUNT_MAX: u32 = 50_000_000;

/// The project node count below which a bulk in-memory load for resolution is cheap
/// enough that per-query store lookups are not worth their overhead.
const RESOLVE_BULK_NODES_MIN: u32 = 50_000;

/// The per-query path is chosen only when nodes outnumber pending references
/// by at least this factor: the incremental case on a large project, where a
/// full node load would dominate. Otherwise the bulk path amortizes better.
const RESOLVE_INCREMENTAL_RATIO: u64 = 8;

/// An event with more dispatchers or listeners than this is skipped when synthesizing
/// edges; a generic name (`change`, `click`) over-links without type info.
const EVENT_FANOUT_MAX: usize = 6;

/// The fail-fast bound on synthesized event edges produced for one project.
const SYNTHESIZED_EDGES_MAX: u32 = 1_000_000;

/// The id-fragment marking an external template stub (`{% extends %}` into an
/// installed app), distinguishing it from an external symbol stub so cross-project
/// template redirects key off the right thing.
const EXTERNAL_TEMPLATE_MARKER: &str = "::external::template::";

/// The directory names skipped wholesale during the walk, alongside their subtrees.
const SKIP_DIRECTORIES: &[&str] = &[
    ".constellation",
    ".git",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
    "venv",
];

/// The tallies produced by one indexing run, for progress reporting and diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub files_indexed: u32,
    pub files_unchanged: u32,
    pub files_removed: u32,
    pub files_skipped: u32,
    pub nodes: u32,
    pub edges: u32,
    pub unresolved_refs: u32,
    pub resolved_edges: u32,
    pub unresolved_remaining: u32,
    pub synthesized_edges: u32,
    pub external_edges: u32,
}

/// The errors that can occur during indexing, walking, or watching.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("walk error: {0}")]
    Walk(#[from] ignore::Error),

    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),
}

/// The roll-back guard for the bulk write transaction: disarmed on success, rolls
/// back on drop when the indexing walk returns early with an error.
struct BulkGuard<'store> {
    store: &'store Store,
    armed: bool,
}

impl Drop for BulkGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.store.bulk_rollback();
        }
    }
}

/// The quiet period after the last filesystem event before re-indexing.
const DEBOUNCE: Duration = Duration::from_millis(400);

/// The fail-fast bound on events drained during one debounce window.
const DEBOUNCE_EVENTS_MAX: u32 = 5_000_000;

/// The fail-fast bound on how far the include tree is walked building one route's
/// namespace chain, far past any real URL nesting depth.
const NAMESPACE_DEPTH_MAX: u32 = 32;

/// The fail-fast bound on the ancestor classes one override search walks, far past
/// any real inheritance hierarchy, so the search is provably finite even on a
/// malformed or cyclic `extends` graph.
const OVERRIDE_WALK_MAX: u32 = 1_000_000;

/// The fail-fast bound on the reverse render/include walk from one accessed template
/// up to the views that render it, far past any real template nesting depth.
const TEMPLATE_VIEW_WALK_MAX: u32 = 1_000_000;

/// The fail-fast bound on the inheritance-chain walk one member lookup makes.
const MEMBER_CHAIN_WALK_MAX: u32 = 1_000_000;

/// The index of every supported file under `root` into `store` as project `project`.
pub fn index_project(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
) -> Result<IndexStats, IndexError> {
    index_project_reporting(store, project, name, root, |_phase| {})
}

/// A progress event emitted while indexing, for a caller rendering a UI.
#[derive(Clone, Copy, Debug)]
pub enum IndexPhase {
    /// The file walk and extraction: `files_done` of `files_total` visited.
    Extracting { files_done: u32, files_total: u32 },
    /// The reference-resolution and edge-synthesis phase, after extraction: emitted
    /// once when the phase begins, with no per-item granularity.
    Resolving,
}

/// A fingerprint of the running binary (its size and modification time)
/// to detect that the extractor changed since a project was last indexed. A
/// rebuilt binary has a new fingerprint, so the next index re-extracts every
/// file instead of keeping nodes the old extractor produced. Returns `None` when
/// the executable cannot be stat'd, which leaves the incremental skip in force.
fn index_fingerprint() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let metadata = std::fs::metadata(&path).ok()?;

    let size = metadata.len();
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0);

    Some(format!("{size}:{modified_ms}"))
}

/// The index of every supported file under `root`, reporting progress through
/// `on_phase` as files are extracted and as resolution begins. [`index_project`]
/// is this with a no-op reporter.
pub fn index_project_reporting(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    mut on_phase: impl FnMut(IndexPhase),
) -> Result<IndexStats, IndexError> {
    assert!(!name.is_empty(), "project name must not be empty");
    assert!(root.is_dir(), "project root must be a directory: {root:?}");

    let root_absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_absolute.as_path();

    store.upsert_project(project, name, &root.to_string_lossy())?;

    // A change to the extractor (a rebuilt binary) leaves every source file's
    // content hash unchanged, so the per-file skip would keep the old extractor's
    // nodes. Compare the binary's fingerprint to the project's stamp and, on a
    // mismatch, re-extract every file by passing an empty hash baseline.
    let fingerprint = index_fingerprint();
    let force_full = match fingerprint.as_deref() {
        Some(current) => store.index_version(project)? != current,
        None => false,
    };

    let extractors: Vec<Box<dyn Extractor>> = vec![
        Box::new(PythonExtractor::new()),
        Box::new(TemplateExtractor::new()),
        Box::new(JavaScriptExtractor::new()),
        Box::new(CssExtractor::new()),
    ];
    let frameworks = detect_frameworks(root);
    let existing = if force_full { FxHashMap::default() } else { store.file_hashes(project)? };
    let mut stats = IndexStats::default();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut files_done: u32 = 0;

    let paths = collect_file_paths(root)?;
    let files_total = to_u32(paths.len());

    store.bulk_begin()?;
    let mut bulk = BulkGuard { store, armed: true };

    for chunk in paths.chunks(EXTRACT_CHUNK_MAX) {
        let outcomes: Vec<ExtractOutcome> = chunk
            .par_iter()
            .map(|path| extract_one(project, &extractors, &frameworks, &existing, root, path))
            .collect();

        for outcome in outcomes {
            persist_outcome(store, project, outcome, &mut stats, &mut seen)?;

            files_done += 1;

            on_phase(IndexPhase::Extracting { files_done, files_total: files_total.max(files_done) });
        }
    }

    stats.files_removed = remove_missing(store, project, &existing, &seen)?;

    bulk.armed = false;
    store.bulk_commit()?;

    if stats.files_indexed > 0 || stats.files_removed > 0 {
        on_phase(IndexPhase::Resolving);

        run_resolution_phase(store, project, root, &frameworks, &mut stats)?;
    }

    // Stamp the project with the binary that indexed it, so the next run with the
    // same binary trusts the content-hash skip again. Only after a successful
    // index; a failed run rolls back its writes and must re-extract next time.
    if let Some(current) = fingerprint.as_deref() {
        store.set_index_version(project, current)?;
    }

    Ok(stats)
}

/// Every regular file's path under `root`, collected and bounded by
/// [`FILE_COUNT_MAX`] so a pathological tree fails fast rather than walking
/// unbounded.
fn collect_file_paths(root: &Path) -> Result<Vec<PathBuf>, IndexError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut visited: u32 = 0;

    for entry in walk(root) {
        let entry = entry?;
        visited += 1;

        assert!(visited <= FILE_COUNT_MAX, "walk exceeded {FILE_COUNT_MAX} entries");

        if entry.file_type().is_some_and(|file_type| file_type.is_file()) {
            paths.push(entry.path().to_path_buf());
        }
    }

    Ok(paths)
}

/// The project's references resolved and the derived edge layers
/// (events, reverse relations, external boundary), recording every count into
/// `stats`. Run only after extraction changed the graph.
fn run_resolution_phase(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    frameworks: &[Box<dyn FrameworkResolver>],
    stats: &mut IndexStats,
) -> Result<(), IndexError> {
    let (resolved, remaining) = resolve_project(store, project, root, frameworks)?;

    // Bind namespaced `reverse('app:page:detail')` references that generic
    // resolution leaves pending, using the include-namespace chain.
    let reverse_linked = link_namespaced_reverses(store, project)?;

    // Gate styles: a class reference that matched no indexed selector can never
    // resolve (the project's CSS is fully known by now), so drop it rather than
    // persist dead weight or let it false-link across projects later.
    let styles_dropped = store.delete_unresolved_kind(project, EdgeKind::Styles)?;

    stats.resolved_edges = resolved + reverse_linked;

    stats.unresolved_remaining =
        remaining.saturating_sub(styles_dropped).saturating_sub(reverse_linked);

    stats.synthesized_edges = synthesize_events(store, project)?;
    stats.synthesized_edges += synthesize_reverse_relations(store, project)?;
    stats.synthesized_edges += synthesize_overrides(store, project)?;
    stats.synthesized_edges += synthesize_template_members(store, project)?;
    stats.external_edges = synthesize_external(store, project)?;

    Ok(())
}

/// The binding of `reverse('app:page:detail')` references to the exact route under that
/// include-namespace chain. Generic resolution leaves a namespaced (`a:b:c`)
/// reverse pending (no route node is named with colons) because the correct
/// target depends on the `include(..., namespace=...)` chain that reaches it,
/// which spans files. This pass reconstructs that chain from the include routes
/// (whose `namespace=` was captured onto the route node's signature) and the
/// pending include `Imports` references (whose name is the included module),
/// computes each named route's full reverse name, and resolves the pending
/// namespaced `Resolves` references against it. A reverse whose chain cannot be
/// rebuilt falls back to a unique same-name route, and otherwise stays pending:
/// never a guessed, wrong edge.
fn link_namespaced_reverses(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let pending = store.load_unresolved(Some(project))?;

    let has_namespaced_reverse = pending.iter().any(|(_, reference)| {
        reference.reference_kind == EdgeKind::Resolves && reference.reference_name.contains(':')
    });

    if !has_namespaced_reverse {
        return Ok(0);
    }

    let routes = store.nodes_kind_in(project, NodeKind::Route)?;

    // The include map: included module -> (instance namespace, including module).
    // The namespace rides on the include route node's signature; the included
    // module is the pending `Imports` reference's name; the including module is
    // that reference's own file.
    let route_by_id: FxHashMap<&str, &Node> =
        routes.iter().map(|route| (route.id.as_str(), route)).collect();

    let mut includes: FxHashMap<String, (String, String)> = FxHashMap::default();

    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::Imports {
            continue;
        }

        let Some(route) = route_by_id.get(reference.from_node_id.as_str()) else {
            continue;
        };

        let Some(namespace) = route.signature.clone() else {
            continue;
        };

        includes.insert(reference.reference_name.clone(), (namespace, module_of(&reference.file_path)));
    }

    // Each named route's full reverse name, plus a bare-name index for fallback.
    let mut by_reverse_name: FxHashMap<String, NodeId> = FxHashMap::default();
    let mut by_bare_name: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();

    for route in &routes {
        // A bare-URL route (`page/`) has no `name=` and cannot be reversed.
        if route.name.contains('/') {
            continue;
        }

        by_bare_name.entry(route.name.as_str()).or_default().push(route);

        if let Some(chain) = namespace_chain(&module_of(&route.file_path), &includes) {
            by_reverse_name.insert(format!("{}:{}", chain.join(":"), route.name), route.id.clone());
        }
    }

    let mut resolved: Vec<(i64, Edge)> = Vec::new();

    for (reference_id, reference) in &pending {
        if reference.reference_kind != EdgeKind::Resolves || !reference.reference_name.contains(':') {
            continue;
        }

        let target = by_reverse_name.get(&reference.reference_name).cloned().or_else(|| {
            // Fallback to a unique same-name route; never bind when ambiguous.
            let bare = reference.reference_name.rsplit(':').next().unwrap_or(&reference.reference_name);

            match by_bare_name.get(bare) {
                Some(matches) if matches.len() == 1 => Some(matches[0].id.clone()),
                _ => None,
            }
        });

        if let Some(target) = target {
            let edge = Edge::new(reference.from_node_id.clone(), target, EdgeKind::Resolves)
                .with_provenance("resolution:reverse-namespace");

            resolved.push((*reference_id, edge));
        }
    }

    Ok(store.commit_resolved(&resolved)?)
}

/// The dotted module path of a Python file: `app/partner/urls/page_urls.py` ->
/// `app.partner.urls.page_urls`, and a package `__init__.py` to the package
/// itself (`app/partner/urls/__init__.py` -> `app.partner.urls`). Matches the
/// module strings Django `include('app.partner.urls')` calls carry.
#[doc(hidden)]
pub fn module_of(file_path: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let without_extension = normalized.strip_suffix(".py").unwrap_or(&normalized);
    let module = without_extension.strip_suffix("/__init__").unwrap_or(without_extension);

    module.replace('/', ".")
}

/// The instance-namespace chain from the root urlconf down to `module`, walking
/// the include map child -> parent. Returned root-first (`["partner", "page"]`
/// for `app.partner.urls.page_urls`), or `None` when no captured include reaches
/// the module (so it carries no reverse namespace). Bounded by
/// [`NAMESPACE_DEPTH_MAX`], and a visited set breaks any cyclic include.
#[doc(hidden)]
pub fn namespace_chain(module: &str, includes: &FxHashMap<String, (String, String)>) -> Option<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();
    let mut visited: FxHashSet<String> = FxHashSet::default();
    let mut current = module.to_string();
    let mut depth: u32 = 0;

    while let Some((namespace, parent)) = includes.get(&current) {
        depth += 1;

        assert!(depth <= NAMESPACE_DEPTH_MAX, "namespace walk exceeded {NAMESPACE_DEPTH_MAX} levels");

        if !visited.insert(current.clone()) {
            break;
        }

        chain.push(namespace.clone());
        current = parent.clone();
    }

    if chain.is_empty() {
        return None;
    }

    chain.reverse();

    Some(chain)
}

/// The deletion of files recorded in the store that the walk no longer found on disk,
/// returning how many were removed.
fn remove_missing(
    store: &Store,
    project: &ProjectId,
    existing: &FxHashMap<String, String>,
    seen: &FxHashSet<String>,
) -> Result<u32, IndexError> {
    let mut removed: u32 = 0;
    let mut count: u32 = 0;

    for path in existing.keys() {
        count += 1;

        assert!(count <= FILE_COUNT_MAX, "removal scan exceeded {FILE_COUNT_MAX} files");

        if !seen.contains(path) {
            store.remove_file(project, path)?;
            removed += 1;
        }
    }

    assert!(removed <= count, "removed no more files than were scanned");

    Ok(removed)
}

/// The framework resolvers that apply to the project at `root`.
fn detect_frameworks(root: &Path) -> Vec<Box<dyn FrameworkResolver>> {
    let context = FsContext::new(root);
    let mut active: Vec<Box<dyn FrameworkResolver>> = Vec::new();

    let django = DjangoResolver;

    if django.detect(&context) {
        active.push(Box::new(django));
    }

    assert!(active.len() <= 1, "only the django resolver is registered");

    active
}

/// The project's pending references resolved into edges. Each reference is
/// matched against the project's own graph; matches become edges and the
/// reference is cleared, the rest stay pending for cross-project linking.
fn resolve_project(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    frameworks: &[Box<dyn FrameworkResolver>],
) -> Result<(u32, u32), IndexError> {
    let pending = store.load_unresolved(Some(project))?;

    if pending.is_empty() {
        return Ok((0, 0));
    }

    assert!(!pending.is_empty(), "pending references are present past the empty guard");

    let node_count = store.count_nodes(project)?;

    let resolved = if use_store_backed(pending.len(), node_count) {
        let context = StoreContext {
            store,
            project: project.clone(),
            root: root.to_path_buf(),
        };

        resolve_pending(&pending, &context, frameworks)
    } else {
        let context = ProjectContext::load(store, project, root)?;

        resolve_pending(&pending, &context, frameworks)
    };

    let written = store.commit_resolved(&resolved)?;
    let total = u32::try_from(pending.len()).unwrap_or(u32::MAX);

    assert!(written <= total, "resolved edges cannot exceed pending references");

    Ok((written, total.saturating_sub(written)))
}

/// Whether to resolve via per-query store lookups instead of a bulk in-memory
/// load: only when the project is large and its pending references are few
/// relative to its nodes, so materializing every node would dominate the cost.
#[doc(hidden)]
pub fn use_store_backed(pending: usize, node_count: u32) -> bool {
    node_count >= RESOLVE_BULK_NODES_MIN
        && (pending as u64).saturating_mul(RESOLVE_INCREMENTAL_RATIO) < node_count as u64
}

/// The resolution of each pending reference against `context` (the core resolver first,
/// then any framework resolver whose languages match) into the (reference id,
/// edge) pairs to commit. Shared by the bulk and per-query resolution paths, so
/// both produce identical edges from the same graph.
fn resolve_pending(
    pending: &[(i64, UnresolvedRef)],
    context: &dyn ResolutionContext,
    frameworks: &[Box<dyn FrameworkResolver>],
) -> Vec<(i64, Edge)> {
    let mut resolved: Vec<(i64, Edge)> = Vec::with_capacity(pending.len());
    let mut seen: u32 = 0;

    for (reference_id, reference) in pending {
        seen += 1;

        assert!(seen <= REFERENCE_COUNT_MAX, "resolution exceeded {REFERENCE_COUNT_MAX} refs");

        // The template member-access pipeline (`accesses_member`, `context_type`)
        // is resolved by the type-scoped synthesis pass, not generic or framework
        // name resolution, which would bind the model/member name to any
        // same-named node. Leave these pending for that pass to consume.
        if matches!(
            reference.reference_kind,
            EdgeKind::AccessesMember
                | EdgeKind::ContextType
                | EdgeKind::LoopBinding
                | EdgeKind::ReverseAccessor
                | EdgeKind::DerivedCollection
        ) {
            continue;
        }

        let resolved_ref = resolve_reference(reference, context).or_else(|| {
            frameworks
                .iter()
                .filter(|framework| framework.languages().contains(&reference.language))
                .find_map(|framework| framework.resolve(reference, context))
        });

        if let Some(resolved_ref) = resolved_ref {
            resolved.push((*reference_id, edge_from_resolved(&resolved_ref)));
        }
    }

    assert!(resolved.len() <= pending.len(), "no more edges than references are produced");

    resolved
}

/// The dispatcher -> handler edges synthesized from a project's event records:
/// correlate dispatch sites and listener registrations by event name, resolve
/// each listener's handler to its JS function, and link every dispatcher of
/// that event to it. Replaces the project's prior synthesized edges (always
/// re-derived from scratch). Returns the number written.
fn synthesize_events(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let events = store.events_for(project)?;

    let mut listeners: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut dispatchers: FxHashMap<String, Vec<(String, u32)>> = FxHashMap::default();

    for event in events {
        match event.role {
            EventRole::Listen => listeners.entry(event.event).or_default().push(event.symbol),
            EventRole::Dispatch => {
                dispatchers.entry(event.event).or_default().push((event.symbol, event.line));
            }
        }
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: FxHashSet<(String, String)> = FxHashSet::default();
    let mut count: u32 = 0;

    for (event, sites) in &dispatchers {
        let Some(handler_names) = listeners.get(event) else {
            continue;
        };

        if sites.len() > EVENT_FANOUT_MAX || handler_names.len() > EVENT_FANOUT_MAX {
            continue;
        }

        let mut handlers: Vec<Node> = Vec::new();

        for name in handler_names {
            if let Some(node) = resolve_handler(store, project, name)? {
                handlers.push(node);
            }
        }

        for (dispatcher_id, line) in sites {
            for handler in &handlers {
                if handler.id.as_str() == dispatcher_id.as_str() {
                    continue;
                }

                let key = (dispatcher_id.clone(), handler.id.as_str().to_string());

                if !seen.insert(key) {
                    continue;
                }

                count += 1;

                assert!(count <= SYNTHESIZED_EDGES_MAX, "synthesis exceeded {SYNTHESIZED_EDGES_MAX} edges");

                // A synthesized event edge connects two nodes of this project: the
                // dispatcher (from this project's events) to a handler resolved
                // within it. Both ids are namespaced to `project`.
                assert!(
                    project_prefix(dispatcher_id) == project.as_str(),
                    "synthesized event dispatcher is in-project",
                );

                assert!(
                    handler.id.project_prefix() == project.as_str(),
                    "synthesized event handler is in-project",
                );

                edges.push(
                    Edge::new(NodeId::from_raw(dispatcher_id.clone()), handler.id.clone(), EdgeKind::Calls)
                        .at(*line, 0)
                        .with_provenance(format!("synthesis:event:{event}")),
                );
            }
        }
    }

    assert!(edges.len() <= SYNTHESIZED_EDGES_MAX as usize, "synthesized edges stay within the cap");

    Ok(store.replace_synthesized_edges(project, "synthesis:event", &edges)?)
}

/// The reverse direction of each model relation, synthesized. A `relates_to` from a
/// model with a foreign key / M2M / O2O to its target always implies a reverse
/// accessor on the target (`author.article_set`, or a `related_name`), so the
/// target model relates back to the source. Emitting the reverse edge lets
/// `callees`/`constellation_model` on the target surface the models that point at
/// it: the "what relates to this model" navigation Django's reverse accessors
/// give but a forward-only graph hides.
/// Scoped to relations whose both endpoints are in `project` so each re-index can
/// re-derive them idempotently; the forward set already excludes prior reverses.
fn synthesize_reverse_relations(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let relations = store.relation_edges(project)?;

    // Borrow the relation strings for the dedup sets: they live in `relations`
    // for the whole pass, so no tuple needs cloning to look one up.
    let forward: FxHashSet<(&str, &str)> =
        relations.iter().map(|(source, target)| (source.as_str(), target.as_str())).collect();

    let mut edges: Vec<Edge> = Vec::with_capacity(relations.len());
    let mut seen: FxHashSet<(&str, &str)> = FxHashSet::default();
    let mut count: u32 = 0;

    for (source, target) in &relations {
        let same_project = project_prefix(source) == project.as_str()
            && project_prefix(target) == project.as_str();

        if !same_project || source == target {
            continue;
        }

        let reverse = (target.as_str(), source.as_str());

        // Skip when a real forward relation already runs target->source (a
        // genuine FK both ways), or when this reverse was already queued.
        if forward.contains(&reverse) || !seen.insert(reverse) {
            continue;
        }

        count += 1;

        assert!(count <= SYNTHESIZED_EDGES_MAX, "reverse synthesis exceeded {SYNTHESIZED_EDGES_MAX} edges");

        edges.push(
            Edge::new(NodeId::from_raw(target.clone()), NodeId::from_raw(source.clone()), EdgeKind::RelatesTo)
                .with_provenance("synthesis:reverse-relation"),
        );
    }

    Ok(store.replace_synthesized_edges(project, "synthesis:reverse", &edges)?)
}

/// An `Overrides` edge synthesized for each method that redefines a same-named
/// method on an ancestor class. Walks the in-project class hierarchy (resolved
/// `extends` edges) up from each method's owning class to the nearest ancestor
/// that defines the method, and links the override to it: the "what does this
/// override" / "what overrides this base method" navigation a forward call graph
/// hides. Scoped to in-project methods and re-derived each index, like the other
/// synthesis passes; an external base contributes no method to bind under.
fn synthesize_overrides(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let extends = store.extends_edges(project)?;
    let methods = store.class_methods(project)?;

    // Subclass id -> its base class ids.
    let mut bases: FxHashMap<&str, Vec<&str>> = FxHashMap::default();

    for (subclass, base) in &extends {
        bases.entry(subclass.as_str()).or_default().push(base.as_str());
    }

    // (owning class id, method name) -> method id.
    let mut by_owner: FxHashMap<(&str, &str), &str> = FxHashMap::default();

    for (id, name) in &methods {
        if let Some(owner) = method_owner_id(id) {
            by_owner.insert((owner, name.as_str()), id.as_str());
        }
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut count: u32 = 0;

    for (id, name) in &methods {
        let Some(owner) = method_owner_id(id) else {
            continue;
        };

        let Some(base_method) = nearest_base_method(owner, name.as_str(), &bases, &by_owner) else {
            continue;
        };

        if base_method == id.as_str() {
            continue;
        }

        count += 1;

        assert!(count <= SYNTHESIZED_EDGES_MAX, "override synthesis exceeded {SYNTHESIZED_EDGES_MAX} edges");

        edges.push(
            Edge::new(
                NodeId::from_raw(id.clone()),
                NodeId::from_raw(base_method.to_string()),
                EdgeKind::Overrides,
            )
            .with_provenance("synthesis:override"),
        );
    }

    Ok(store.replace_synthesized_edges(project, "synthesis:override", &edges)?)
}

/// The owning class id of a method node id: everything before the final `.`
/// (`blog::models.py::Article.save` -> `blog::models.py::Article`). Returns
/// `None` for an id with no `.` member separator (not a class method).
fn method_owner_id(method_id: &str) -> Option<&str> {
    method_id.rsplit_once('.').map(|(owner, _method)| owner)
}

/// The id of the nearest ancestor class's method named `name`, walking up from
/// `owner` through `bases`. A visited set and a hard hop bound make a diamond or
/// cyclic hierarchy terminate.
fn nearest_base_method<'graph>(
    owner: &'graph str,
    name: &'graph str,
    bases: &FxHashMap<&'graph str, Vec<&'graph str>>,
    by_owner: &FxHashMap<(&'graph str, &'graph str), &'graph str>,
) -> Option<&'graph str> {
    let mut frontier: Vec<&'graph str> = match bases.get(owner) {
        Some(list) => list.clone(),
        None => return None,
    };

    let mut visited: FxHashSet<&'graph str> = FxHashSet::default();
    let mut hops: u32 = 0;

    while let Some(class) = frontier.pop() {
        hops += 1;

        assert!(hops <= OVERRIDE_WALK_MAX, "override walk exceeded {OVERRIDE_WALK_MAX} hops");

        if !visited.insert(class) {
            continue;
        }

        if let Some(method) = by_owner.get(&(class, name)) {
            return Some(method);
        }

        if let Some(next) = bases.get(class) {
            for base in next {
                frontier.push(base);
            }
        }
    }

    None
}

/// The project segment of a node id (`blog::app.py::X` -> `blog`): everything
/// before the first `::` separator, or the whole string if absent.
fn project_prefix(node_id: &str) -> &str {
    node_id.split("::").next().unwrap_or(node_id)
}

/// The JS function or method a listener's handler name resolves to,
/// the target a synthesized event edge points at.
fn resolve_handler(
    store: &Store,
    project: &ProjectId,
    handler: &str,
) -> Result<Option<Node>, IndexError> {
    assert!(!handler.is_empty(), "handler name must not be empty");

    let node = store.nodes_named_in(project, handler)?.into_iter().find(|node| {
        node.language == Language::JavaScript
            && matches!(node.kind, NodeKind::Function | NodeKind::Method)
    });

    if let Some(found) = &node {
        assert!(found.language == Language::JavaScript, "a resolved handler is javascript");
    }

    Ok(node)
}

/// The `AccessesMember` edges synthesized from a template's variable-attribute
/// accesses to the model member each names, TYPE-SCOPED so a `{{ var.attr }}`
/// binds only to the member of the model the rendering view gives `var`. Joins
/// the facts the extractor left pending: the `AccessesMember` reference
/// (template, var, attr), the `ContextType` reference (view: var -> model, an
/// instance or (for a queryset / `get_list_or_404`) a collection, the
/// `LoopBinding` reference (template: loop_var <- source), and the
/// `Renders`/`include`/`extends` chain up from the template to its views. A
/// variable types either as a direct instance context var, or as a `{% for %}`
/// loop var over a collection context var (its element model). Emits an edge only
/// when the var resolves to exactly one model across every rendering view AND
/// that model has exactly one member of that name up its inheritance chain (own
/// shadowing inherited): any ambiguity (unknown type, two types across views, a
/// same-named member on two models, a member ambiguous across two bases) drops,
/// never a guessed edge. Re-derived each index.
fn synthesize_template_members(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let pending = store.load_unresolved(Some(project))?;

    let accesses: Vec<&UnresolvedRef> = pending
        .iter()
        .map(|(_, reference)| reference)
        .filter(|reference| reference.reference_kind == EdgeKind::AccessesMember)
        .collect();

    if accesses.is_empty() {
        return Ok(store.replace_synthesized_edges(project, "synthesis:template-member", &[])?);
    }

    // (view id, variable) -> model node id, split by whether the variable holds a
    // single instance (`{{ var.attr }}` types directly) or a collection (only its
    // `{% for x in var %}` loop elements type as the model).
    let mut instance_types: FxHashMap<(String, String), String> = FxHashMap::default();
    let mut collection_types: FxHashMap<(String, String), String> = FxHashMap::default();

    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::ContextType {
            continue;
        }

        let Some(variable) = reference.candidates.first() else {
            continue;
        };

        let Some(model_id) = model_node_in_project(store, project, &reference.reference_name)? else {
            continue;
        };

        let key = (reference.from_node_id.as_str().to_string(), variable.clone());

        if reference.candidates.iter().any(|candidate| candidate == COLLECTION_CONTEXT) {
            collection_types.insert(key, model_id);
        } else {
            instance_types.insert(key, model_id);
        }
    }

    if instance_types.is_empty() && collection_types.is_empty() {
        return Ok(store.replace_synthesized_edges(project, "synthesis:template-member", &[])?);
    }

    // template id -> its `{% for loop_var in source[.accessor] %}` bindings.
    let mut loops: FxHashMap<String, Vec<(String, String, Option<String>)>> = FxHashMap::default();

    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::LoopBinding {
            continue;
        }

        let Some(loop_variable) = reference.candidates.first() else {
            continue;
        };

        let accessor = reference.candidates.get(1).cloned();

        loops
            .entry(reference.from_node_id.as_str().to_string())
            .or_default()
            .push((loop_variable.clone(), reference.reference_name.clone(), accessor));
    }

    // (target model id, accessor) -> the related model id the accessor yields a
    // collection of, from each FK's `related_name`, so `article.comments` types
    // back to the Comment that declares the FK.
    let mut reverse_accessors: FxHashMap<(String, String), String> = FxHashMap::default();

    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::ReverseAccessor {
            continue;
        }

        let Some(accessor) = reference.candidates.first() else {
            continue;
        };

        let Some(target_id) = model_node_in_project(store, project, &reference.reference_name)? else {
            continue;
        };

        reverse_accessors
            .insert((target_id, accessor.clone()), reference.from_node_id.as_str().to_string());
    }

    // Derived collections: a view local `events = record.events.all()` is a
    // collection of the model that `record`'s `events` reverse accessor yields.
    // Resolved now that instance types and reverse accessors are known, then
    // folded into the collection types a `{% for x in events %}` loop draws on.
    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::DerivedCollection {
            continue;
        }

        let (Some(new_variable), Some(accessor)) =
            (reference.candidates.first(), reference.candidates.get(1))
        else {
            continue;
        };

        let view = reference.from_node_id.as_str().to_string();
        let base_local = reference.reference_name.clone();

        let Some(base_model) = instance_types.get(&(view.clone(), base_local)).cloned() else {
            continue;
        };

        let Some(model_id) = reverse_accessors.get(&(base_model, accessor.clone())).cloned() else {
            continue;
        };

        collection_types.insert((view, new_variable.clone()), model_id);
    }

    let mut ancestry_cache: FxHashMap<String, TemplateAncestry> = FxHashMap::default();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: FxHashSet<(String, String)> = FxHashSet::default();
    let mut count: u32 = 0;

    for reference in &accesses {
        let Some(variable) = reference.candidates.first() else {
            continue;
        };

        let template_id = reference.from_node_id.as_str();

        if !ancestry_cache.contains_key(template_id) {
            let ancestry = template_ancestry(store, template_id)?;

            ancestry_cache.insert(template_id.to_string(), ancestry);
        }

        let ancestry = &ancestry_cache[template_id];
        let views = &ancestry.views;

        // The distinct models the accessed variable can hold: a direct instance
        // context var, or a loop var whose source is a collection context var.
        let mut models: FxHashSet<&str> = FxHashSet::default();

        for view in views {
            if let Some(model_id) = instance_types.get(&(view.clone(), variable.clone())) {
                models.insert(model_id.as_str());
            }
        }

        // Loop bindings from this template and every template that includes it:
        // a loop variable bound in a parent table is in scope in its row partials.
        for template in &ancestry.templates {
            let Some(bindings) = loops.get(template) else {
                continue;
            };

            for (loop_variable, source, accessor) in bindings {
                if loop_variable != variable {
                    continue;
                }

                match accessor {
                    // `{% for x in source %}`: source is a collection context var.
                    None => {
                        for view in views {
                            if let Some(model_id) = collection_types.get(&(view.clone(), source.clone())) {
                                models.insert(model_id.as_str());
                            }
                        }
                    }
                    // `{% for x in obj.accessor %}`: obj is an instance context var
                    // typed to T; T's `accessor` reverse relation yields the model.
                    Some(accessor) => {
                        for view in views {
                            if let Some(object_model) = instance_types.get(&(view.clone(), source.clone()))
                                && let Some(model_id) =
                                    reverse_accessors.get(&(object_model.clone(), accessor.clone()))
                            {
                                models.insert(model_id.as_str());
                            }
                        }
                    }
                }
            }
        }

        if models.len() != 1 {
            continue;
        }

        let model_id = models.iter().next().copied().expect("exactly one model present");

        let Some(member_id) = unique_member(store, model_id, &reference.reference_name)? else {
            continue;
        };

        if !seen.insert((template_id.to_string(), member_id.clone())) {
            continue;
        }

        count += 1;

        assert!(
            count <= SYNTHESIZED_EDGES_MAX,
            "template-member synthesis exceeded {SYNTHESIZED_EDGES_MAX} edges",
        );

        edges.push(
            Edge::new(reference.from_node_id.clone(), NodeId::from_raw(member_id), EdgeKind::AccessesMember)
                .at(reference.line, reference.column)
                .with_provenance("synthesis:template-member"),
        );
    }

    Ok(store.replace_synthesized_edges(project, "synthesis:template-member", &edges)?)
}

/// The unique Model node named `name` in `project`, or `None` when there is no
/// such model or more than one (ambiguous, never guessed). The model a
/// `get_object_or_404(Model, ...)` names lives in the view's own project.
fn model_node_in_project(
    store: &Store,
    project: &ProjectId,
    name: &str,
) -> Result<Option<String>, IndexError> {
    let mut found: Option<String> = None;

    for node in store.nodes_named(name)? {
        if node.project_id.as_str() != project.as_str() || node.kind != NodeKind::Model {
            continue;
        }

        if found.is_some() {
            return Ok(None);
        }

        found = Some(node.id.as_str().to_string());
    }

    Ok(found)
}

/// The id of the model's member named `member`, resolved up the inheritance
/// chain: its own `Contains` members first, then those of its bases (abstract
/// bases, mixins, cross-project bases the `Extends` edges reach). The shallowest
/// definition wins, so an own field shadows a base field of the same name, and
/// an inherited field (e.g. `is_active` on a base mixin) resolves when the model
/// itself does not declare it. `None` when no class in the chain declares it, or
/// when the shallowest level that does declares it more than once (a genuine
/// ambiguity across two bases): never a guessed member.
fn unique_member(store: &Store, model_id: &str, member: &str) -> Result<Option<String>, IndexError> {
    const DEPTH_MAX: u32 = 16;

    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(model_id.to_string());

    let mut frontier: Vec<(NodeId, u32)> = vec![(NodeId::from_raw(model_id.to_string()), 0)];
    let mut found: Vec<(u32, String)> = Vec::new();
    let mut walked: u32 = 0;

    while let Some((id, depth)) = frontier.pop() {
        walked += 1;

        assert!(walked <= MEMBER_CHAIN_WALK_MAX, "member-chain walk exceeded {MEMBER_CHAIN_WALK_MAX}");

        for (kind, node) in store.callees(&id)? {
            match kind {
                EdgeKind::Contains if node.name == member => {
                    found.push((depth, node.id.as_str().to_string()));
                }
                EdgeKind::Extends if depth < DEPTH_MAX && visited.insert(node.id.as_str().to_string()) => {
                    frontier.push((node.id.clone(), depth + 1));
                }
                _ => {}
            }
        }
    }

    let Some(depth_min) = found.iter().map(|(depth, _)| *depth).min() else {
        return Ok(None);
    };

    let mut shallowest = found.iter().filter(|(depth, _)| *depth == depth_min).map(|(_, id)| id);

    let first = shallowest.next().cloned();

    match shallowest.next() {
        Some(_) => Ok(None),
        None => Ok(first),
    }
}

/// The views and ancestor templates reachable up a template's reverse
/// render/include/extends chain. `views` holds every view that renders the
/// template (directly or through an include/extends chain), used to type a
/// context variable. `templates` holds the template itself plus every template
/// that transitively includes or extends it, because a `{% for %}` loop variable
/// bound in a parent is in scope in the partials it includes. Bounded in depth
/// and total visits.
struct TemplateAncestry {
    views: Vec<String>,
    templates: Vec<String>,
}

fn template_ancestry(store: &Store, template_id: &str) -> Result<TemplateAncestry, IndexError> {
    const DEPTH_MAX: u32 = 8;

    let mut views: Vec<String> = Vec::new();
    let mut templates: Vec<String> = vec![template_id.to_string()];
    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(template_id.to_string());

    let mut frontier: Vec<(NodeId, u32)> = vec![(NodeId::from_raw(template_id.to_string()), 0)];
    let mut walked: u32 = 0;

    while let Some((id, depth)) = frontier.pop() {
        walked += 1;

        assert!(walked <= TEMPLATE_VIEW_WALK_MAX, "template-view walk exceeded {TEMPLATE_VIEW_WALK_MAX}");

        for (kind, node) in store.callers(&id)? {
            match kind {
                EdgeKind::Renders => {
                    if visited.insert(node.id.as_str().to_string()) {
                        views.push(node.id.as_str().to_string());
                    }
                }
                EdgeKind::IncludesTemplate | EdgeKind::ExtendsTemplate
                    if depth < DEPTH_MAX && visited.insert(node.id.as_str().to_string()) =>
                {
                    templates.push(node.id.as_str().to_string());
                    frontier.push((node.id.clone(), depth + 1));
                }
                _ => {}
            }
        }
    }

    Ok(TemplateAncestry { views, templates })
}

/// The library-boundary layer, synthesized: turn references an in-project
/// resolution could not satisfy, but whose name is imported from a third-party
/// or stdlib module, into edges to deduplicated External nodes, so `extends`,
/// `decorates`, `calls`, and `imports` into libraries (django, django_spire,
/// decimal, …) become real edges instead of dead-ending at the boundary.
/// Re-derived from scratch each index. Returns the number of external edges.
fn synthesize_external(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let roots = first_party_roots(&store.project_file_paths(project)?);
    let template_names = local_template_names(store, project)?;

    let mut mappings_by_file: FxHashMap<String, FxHashMap<String, ImportMapping>> = FxHashMap::default();

    for (file_path, mapping) in store.all_import_mappings(project)? {
        mappings_by_file
            .entry(file_path)
            .or_default()
            .insert(mapping.local_name.clone(), mapping);
    }

    let pending = store.load_unresolved(Some(project))?;

    let mut nodes: FxHashMap<String, Node> = FxHashMap::default();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: FxHashSet<(String, String, &'static str)> = FxHashSet::default();
    let mut count: u32 = 0;

    for (_reference_id, reference) in &pending {
        let Some(target) = external_target(project, reference, &mappings_by_file, &roots, &template_names)
        else {
            continue;
        };

        nodes.entry(target.id.clone()).or_insert_with(|| make_external_node(project, &target));

        let key = (
            reference.from_node_id.as_str().to_string(),
            target.id.clone(),
            reference.reference_kind.as_str(),
        );

        if !seen.insert(key) {
            continue;
        }

        count += 1;

        assert!(count <= SYNTHESIZED_EDGES_MAX, "external synthesis exceeded {SYNTHESIZED_EDGES_MAX} edges");

        // The edge runs from an in-project reference to an external node this
        // project owns: both endpoints are namespaced to `project`.
        assert!(
            reference.from_node_id.project_prefix() == project.as_str(),
            "external edge originates in-project",
        );

        assert!(
            project_prefix(&target.id) == project.as_str(),
            "external target id is namespaced to the project",
        );

        edges.push(
            Edge::new(reference.from_node_id.clone(), NodeId::from_raw(target.id), reference.reference_kind)
                .at(reference.line, reference.column)
                .with_provenance("external"),
        );
    }

    let node_list: Vec<Node> = nodes.into_values().collect();

    Ok(store.replace_external(project, &node_list, &edges)?)
}

/// The fields needed to build the External node a boundary-crossing reference points at.
struct ExternalTarget {
    id: String,
    name: String,
    qualified_name: String,
    file_path: String,
    language: Language,
}

/// A reference classified as targeting an external library/stdlib symbol (a Python
/// import) or an external template (`{% include/extends %}` into an installed
/// app's templates), returning the External node to create, or `None` when it
/// is first-party (should resolve locally) or not externalizable.
fn external_target(
    project: &ProjectId,
    reference: &UnresolvedRef,
    mappings_by_file: &FxHashMap<String, FxHashMap<String, ImportMapping>>,
    roots: &FxHashSet<String>,
    template_names: &FxHashSet<String>,
) -> Option<ExternalTarget> {
    match reference.reference_kind {
        EdgeKind::Imports | EdgeKind::Extends | EdgeKind::Decorates | EdgeKind::Calls => {
            let mapping = mappings_by_file
                .get(&reference.file_path)?
                .get(&reference.reference_name)?;

            if mapping.exported_name.is_empty() || !is_external_module(&mapping.source, roots) {
                return None;
            }

            let qualified_name = format!("{}.{}", mapping.source, mapping.exported_name);

            Some(ExternalTarget {
                id: format!("{}::external::{qualified_name}", project.as_str()),
                name: mapping.exported_name.clone(),
                qualified_name,
                file_path: format!("<external>/{}", mapping.source),
                language: reference.language,
            })
        }
        EdgeKind::IncludesTemplate | EdgeKind::ExtendsTemplate => {
            let path = reference.reference_name.as_str();

            if path.is_empty() || template_names.contains(path) {
                return None;
            }

            Some(ExternalTarget {
                id: format!("{}{EXTERNAL_TEMPLATE_MARKER}{path}", project.as_str()),
                name: path.to_string(),
                qualified_name: path.to_string(),
                file_path: format!("<external>/{path}"),
                language: reference.language,
            })
        }
        _ => None,
    }
}

/// The top-level module roots that belong to the project, from its file paths,
/// used to tell a first-party import from an external one.
fn first_party_roots(file_paths: &[String]) -> FxHashSet<String> {
    let mut roots: FxHashSet<String> = FxHashSet::default();

    for path in file_paths {
        let head = path.split('/').next().unwrap_or(path);
        let root = head.strip_suffix(".py").unwrap_or(head);

        if !root.is_empty() {
            roots.insert(root.to_string());
        }
    }

    roots
}

/// Whether an import's source module resolves outside the project: not a
/// relative import, and its top segment is not a first-party root.
fn is_external_module(module: &str, roots: &FxHashSet<String>) -> bool {
    if module.is_empty() || module.starts_with('.') {
        return false;
    }

    let head = module.split('.').next().unwrap_or(module);

    !roots.contains(head)
}

/// The logical names of the project's own templates (the path Django uses to
/// reference them (what `template_name` produces). An include/extends of a name
/// not in this set is external: it lives in an installed app, not the repo.
fn local_template_names(store: &Store, project: &ProjectId) -> Result<FxHashSet<String>, IndexError> {
    let mut names: FxHashSet<String> = FxHashSet::default();

    for node in store.nodes_kind_in(project, NodeKind::Template)? {
        names.insert(node.name);
    }

    Ok(names)
}

/// An External node built from a classified [`ExternalTarget`].
fn make_external_node(project: &ProjectId, target: &ExternalTarget) -> Node {
    Node::new(
        NodeId::from_raw(target.id.clone()),
        project.clone(),
        NodeKind::External,
        NodeIdentity {
            name: target.name.clone(),
            qualified_name: target.qualified_name.clone(),
            file_path: target.file_path.clone(),
            language: target.language,
        },
        Span::new(1, 1, 0, 0),
        0,
    )
}

/// The whole constellation, linked: match every project's still-pending imports
/// against symbols exported by other projects, persist the matches as
/// cross-project edges, and clear the references they resolved. Returns the
/// number of cross-project edges written.
pub fn link_constellation(store: &Store) -> Result<u32, IndexError> {
    let nodes = store.all_nodes(None)?;

    let reference_only: FxHashSet<String> =
        store.reference_only_project_ids()?.into_iter().collect();

    let redirects = external_redirects(&nodes, &reference_only);
    let template_overrides = template_override_edges(&nodes, &reference_only);

    let context = ConstellationContext::new(nodes, &reference_only);
    let pending = store.load_unresolved(None)?;
    let linker = ImportLinker;

    let mut links: Vec<(i64, Edge)> = Vec::with_capacity(pending.len());
    let mut seen: u32 = 0;

    for (reference_id, reference) in &pending {
        seen += 1;

        assert!(seen <= REFERENCE_COUNT_MAX, "linking exceeded {REFERENCE_COUNT_MAX} refs");

        let edge = match reference.reference_kind {
            EdgeKind::Imports => {
                let pending_import = PendingImport {
                    project_id: ProjectId::new(reference.from_node_id.project_prefix()),
                    from_node_id: reference.from_node_id.clone(),
                    reference_name: reference.reference_name.clone(),
                    module: reference.candidates.first().cloned().unwrap_or_default(),
                    line: reference.line,
                    column: reference.column,
                };

                linker.link(&pending_import, &context).map(|link| link.edge)
            }
            EdgeKind::RelatesTo | EdgeKind::Receives | EdgeKind::AdminOf => {
                cross_project_relation(reference, &context)
            }
            _ => None,
        };

        if let Some(edge) = edge {
            links.push((*reference_id, edge));
        }
    }

    assert!(links.len() <= pending.len(), "no more links than pending references");

    let linked = store.commit_resolved(&links)?;

    // Collapse external import-stubs into the real cross-project definitions they
    // shadow, so a model "extends an external mixin" extends the real indexed
    // class across the boundary and `node` shows one definition. Computed from the
    // pre-link node snapshot; safe to apply after, since it only retargets edges
    // onto definitions that already exist.
    if !redirects.is_empty() {
        store.unify_externals(&redirects)?;
    }

    persist_template_overrides(store, template_overrides)?;

    Ok(linked)
}

/// The cross-project template overrides: a portal's vendored copy of a namespaced
/// template (`templates/django_spire/page/full_page.html`) shadows the original it
/// copies. For each template whose name is owned by one project's namespace
/// (`django_spire/...` -> django-spire) and is also defined elsewhere, emit an
/// `OverridesTemplate` edge from each non-owner copy to the canonical original, so
/// `callers` on the original shows which projects override it. A name with no
/// canonical owner is left alone: no false edge.
fn template_override_edges(nodes: &[Node], reference_only: &FxHashSet<String>) -> Vec<Edge> {
    let mut by_name: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();

    for node in nodes {
        // A reference-only version copy is for direct comparison, not a target of
        // cross-project override synthesis, so it joins neither side.
        if node.kind == NodeKind::Template && !reference_only.contains(node.project_id.as_str()) {
            by_name.entry(node.name.as_str()).or_default().push(node);
        }
    }

    let mut edges: Vec<Edge> = Vec::new();

    for (name, copies) in &by_name {
        if copies.len() < 2 {
            continue;
        }

        let owner = template_owner(name);

        let Some(original) = copies.iter().find(|node| node.project_id.as_str() == owner.as_str())
        else {
            continue;
        };

        for copy in copies {
            if copy.project_id != original.project_id {
                edges.push(
                    Edge::new(copy.id.clone(), original.id.clone(), EdgeKind::OverridesTemplate)
                        .with_provenance("synthesis:template-override"),
                );
            }
        }
    }

    edges
}

/// The cross-project template-override edges persisted, grouped by the overriding
/// project. Every project gets a replace (empty clears) so a removed vendored copy
/// drops its stale override on the next link.
fn persist_template_overrides(store: &Store, edges: Vec<Edge>) -> Result<(), IndexError> {
    let mut by_project: FxHashMap<String, Vec<Edge>> = FxHashMap::default();

    for edge in edges {
        by_project.entry(edge.source.project_prefix().to_string()).or_default().push(edge);
    }

    for project in store.all_projects()? {
        let edges = by_project.remove(project.id.as_str()).unwrap_or_default();

        store.replace_synthesized_edges(&project.id, "synthesis:template-override", &edges)?;
    }

    Ok(())
}

/// A leftover model reference linked to the sole model or class of that name in
/// another project. Covers a cross-project ORM relation (`relates_to`: a foreign
/// key to a model the project does not define locally) and a cross-project signal
/// (`receives`: a `@receiver(sender=Model)` whose model lives in another repo,
/// e.g. a portal handler on django-spire's `AuthUser`). An ambiguous name (defined
/// in more than one other project) stays unlinked, the same no-false-edge
/// discipline the import linker keeps. The edge carries the reference's own kind.
fn cross_project_relation(reference: &UnresolvedRef, context: &dyn LinkContext) -> Option<Edge> {
    let project = reference.from_node_id.project_prefix();

    let mut matched = context
        .exports_by_name(&reference.reference_name)
        .into_iter()
        .filter(|node| {
            node.project_id.as_str() != project
                && matches!(node.kind, NodeKind::Model | NodeKind::Class)
        });

    let (Some(target), None) = (matched.next(), matched.next()) else {
        return None;
    };

    let provenance = format!("link:{}->{}", project, target.project_id);

    Some(
        Edge::new(reference.from_node_id.clone(), target.id.clone(), reference.reference_kind)
            .at(reference.line, reference.column)
            .with_provenance(provenance),
    )
}

/// The map from each external stub to the single real cross-project definition it shadows.
/// A stub `django_spire.history.mixins.HistoryModelMixin` matches a non-external,
/// linkable definition of the same simple name in another project whose file path
/// agrees with the stub's module, the same module-path evidence the import linker
/// requires. An ambiguous stub (two projects define the name) is left alone.
fn external_redirects(nodes: &[Node], reference_only: &FxHashSet<String>) -> Vec<(NodeId, NodeId)> {
    let mut definitions: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();
    let mut templates: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();

    for node in nodes {
        if node.kind == NodeKind::External {
            continue;
        }

        // A reference-only version is never the canonical definition a stub
        // resolves to; excluding it here keeps unification from retargeting an
        // external stub onto an arbitrary version copy. Reference-only stubs
        // themselves still redirect outward: the stub loop below is unfiltered.
        if reference_only.contains(node.project_id.as_str()) {
            continue;
        }

        if is_linkable(node.kind) {
            definitions.entry(node.name.as_str()).or_default().push(node);
        }

        if node.kind == NodeKind::Template {
            templates.entry(node.name.as_str()).or_default().push(node);
        }
    }

    let mut redirects: Vec<(NodeId, NodeId)> = Vec::new();

    for stub in nodes.iter().filter(|node| node.kind == NodeKind::External) {
        // A `{% extends/include 'spire/base.html' %}` stub redirects to the real
        // template of that name in another project. Template names are globally
        // namespaced by app directory, so an exact-name match needs no module
        // evidence; an ambiguous name (two projects own it) is left alone.
        if stub.id.as_str().contains(EXTERNAL_TEMPLATE_MARKER) {
            if let Some(definition) = canonical_template(templates.get(stub.name.as_str()), stub) {
                redirects.push((stub.id.clone(), definition.id.clone()));
            }

            continue;
        }

        let Some((module, _name)) = stub.qualified_name.rsplit_once('.') else {
            continue;
        };

        let Some(candidates) = definitions.get(stub.name.as_str()) else {
            continue;
        };

        let mut matched = candidates
            .iter()
            .filter(|node| node.project_id != stub.project_id && module_matches(module, &node.file_path));

        if let (Some(definition), None) = (matched.next(), matched.next()) {
            redirects.push((stub.id.clone(), definition.id.clone()));
        }
    }

    redirects
}

/// The real template `stub` should redirect to among `candidates`: templates of
/// the same name in any project. The sole other-project template wins outright;
/// when several projects own the name (a portal that vendored a copy of a
/// django-spire base under its own `templates/django_spire/...`) the canonical
/// owner wins: the project whose id matches the name's leading namespace
/// (`django_spire/page/full_page.html` -> `django-spire`), so a vendored
/// duplicate never shadows the origin. Still ambiguous returns `None`, no false edge.
fn canonical_template<'nodes>(
    candidates: Option<&'nodes Vec<&'nodes Node>>,
    stub: &Node,
) -> Option<&'nodes Node> {
    let others: Vec<&'nodes Node> = candidates?
        .iter()
        .copied()
        .filter(|node| node.project_id != stub.project_id)
        .collect();

    if others.len() == 1 {
        return Some(others[0]);
    }

    let owner = template_owner(&stub.name);
    let mut owned = others.iter().copied().filter(|node| node.project_id.as_str() == owner.as_str());

    match (owned.next(), owned.next()) {
        (Some(definition), None) => Some(definition),
        _ => None,
    }
}

/// The project id that canonically owns a template name: its leading namespace
/// segment with underscores normalized to hyphens (`django_spire/page/full_page
/// .html` -> `django-spire`). A bare name (`base.html`) maps to itself, matching
/// no project, so it stays ambiguous rather than binding to an arbitrary copy.
#[doc(hidden)]
pub fn template_owner(name: &str) -> String {
    name.split('/').next().unwrap_or_default().replace('_', "-")
}

/// The initial index of `root`, then a watch that re-indexes (incrementally) after each
/// debounced burst of filesystem changes. `on_index` is called with the stats
/// of every index, initial and subsequent. Blocks until the watcher stops.
pub fn watch_project(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    mut on_index: impl FnMut(&IndexStats),
) -> Result<(), IndexError> {
    assert!(!name.is_empty(), "project name must not be empty");
    assert!(root.is_dir(), "project root must be a directory: {root:?}");

    let root_absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_absolute.as_path();

    on_index(&index_project(store, project, name, root)?);

    let (sender, receiver) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;

    watcher.watch(root, RecursiveMode::Recursive)?;

    loop {
        let Ok(event) = receiver.recv() else {
            return Ok(());
        };

        if !is_relevant(&event) {
            continue;
        }

        drain_debounce(&receiver);

        on_index(&index_project(store, project, name, root)?);
    }
}

/// Every project re-indexed from its stored root (incrementally, skipping unchanged
/// files) and re-link a multi-project constellation when anything changed.
/// Returns whether the graph changed, so a caller can skip cache invalidation
/// and relinking on a no-op refresh. The shared refresh run at startup and after
/// each watched change.
pub fn refresh_constellation(store: &Store) -> Result<bool, IndexError> {
    let projects = store.all_projects()?;

    let mut changed = false;

    for row in &projects {
        let root = Path::new(&row.root_path);

        if root.is_dir() {
            let stats = index_project(store, &row.id, &row.name, root)?;

            if stats.files_indexed > 0 || stats.files_removed > 0 {
                changed = true;
            }
        }
    }

    if changed && projects.len() > 1 {
        link_constellation(store)?;
    }

    Ok(changed)
}

/// The catch-up with the working tree, then a watch of every indexed project's
/// root and, after each debounced burst of changes, refresh the constellation
/// and invoke `on_change` (only when the graph actually changed) so a
/// long-running server can drop its caches. Blocks until the channel closes;
/// meant to run on a dedicated thread. A panic in one re-index is contained so
/// the watcher survives it. Progress goes to stderr, never stdout, which the
/// MCP server reserves for its protocol.
pub fn watch_constellation(store: &Store, mut on_change: impl FnMut()) -> Result<(), IndexError> {
    match catch_unwind(AssertUnwindSafe(|| refresh_constellation(store))) {
        Ok(Ok(true)) => {
            eprintln!("constellation: caught up with on-disk changes before serving");
            on_change();
        }
        Ok(Ok(false)) => {}
        Ok(Err(error)) => eprintln!("constellation: initial catch-up failed: {error}"),
        Err(_) => eprintln!("constellation: initial catch-up panicked; serving the existing graph"),
    }

    let projects = store.all_projects()?;

    let (sender, receiver) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;

    let mut watched: u32 = 0;

    for row in &projects {
        let root = Path::new(&row.root_path);

        if root.is_dir() {
            watcher.watch(root, RecursiveMode::Recursive)?;
            watched += 1;
        }
    }

    assert!(watched as usize <= projects.len(), "watched no more roots than projects");

    if watched == 0 {
        return Ok(());
    }

    loop {
        let Ok(event) = receiver.recv() else {
            return Ok(());
        };

        if !is_relevant(&event) {
            continue;
        }

        drain_debounce(&receiver);

        match catch_unwind(AssertUnwindSafe(|| refresh_constellation(store))) {
            Ok(Ok(true)) => {
                eprintln!("constellation: re-indexed after a change");
                on_change();
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => eprintln!("constellation: re-index failed: {error}"),
            Err(_) => eprintln!("constellation: re-index panicked; skipped this change"),
        }
    }
}

/// The working-tree staleness for one project relative to the last index: how many
/// source files now have a newer modification time (or are new), and how many
/// indexed files have since been removed. Stat-only (never reads file contents),
/// so it is cheap enough for a status check.
#[derive(Clone, Copy, Debug, Default)]
pub struct StaleFiles {
    pub changed: u32,
    pub removed: u32,
}

/// The [`StaleFiles`] for a project, computed by walking its root and comparing each
/// source file's modification time to the stored baseline.
pub fn count_stale_files(
    store: &Store,
    project: &ProjectId,
    root: &Path,
) -> Result<StaleFiles, IndexError> {
    let stored = Arc::new(store.file_mtimes(project)?);
    let hashes = Arc::new(store.file_hashes(project)?);

    // Walk and stat in parallel: both the gitignore traversal and the per-file
    // mtime syscall (slow on Windows) dominate the stale check, and there is no
    // shared mutable index state to serialize on. Workers tally into shared
    // counters; the visit count is bounded, quitting the walk gracefully on
    // overflow rather than panicking inside a worker thread.
    let changed = Arc::new(AtomicU32::new(0));
    let visited = Arc::new(AtomicU32::new(0));
    let seen: Arc<Mutex<FxHashSet<String>>> = Arc::new(Mutex::new(FxHashSet::default()));

    // Clones the walk consumes; the originals stay live to read after run().
    let stored_walk = Arc::clone(&stored);
    let hashes_walk = Arc::clone(&hashes);
    let changed_walk = Arc::clone(&changed);
    let seen_walk = Arc::clone(&seen);
    let root_owned = root.to_path_buf();

    walk_parallel(root).run(move || {
        let stored = Arc::clone(&stored_walk);
        let hashes = Arc::clone(&hashes_walk);
        let changed = Arc::clone(&changed_walk);
        let visited = Arc::clone(&visited);
        let seen = Arc::clone(&seen_walk);
        let root = root_owned.clone();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };

            if visited.fetch_add(1, Ordering::Relaxed) >= FILE_COUNT_MAX {
                return WalkState::Quit;
            }

            if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path();

            let supported = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(Language::from_extension)
                .is_some();

            if !supported {
                return WalkState::Continue;
            }

            // The index skips minified/bundled vendor assets (`is_minified`), so
            // they are never stored; the stale check must skip them too, or each,
            // forever absent from the index, counts as a phantom "changed" on
            // every status call.
            if is_minified(path) {
                return WalkState::Continue;
            }

            let Some(relative) = relative_path(&root, path) else {
                return WalkState::Continue;
            };

            // mtime is the cheap pre-filter; a bumped mtime (a checkout, a
            // formatter, a sync) is confirmed against the content hash (what
            // indexing actually keys re-extraction on), so a touched-but-unchanged
            // file is not reported stale.
            let stale = match stored.get(&relative) {
                Some(&stored_ms) if modified_ms(path) <= stored_ms => false,
                Some(_) => match std::fs::read(path) {
                    Ok(bytes) => hashes.get(&relative).map(String::as_str) != Some(hash_hex(&bytes).as_str()),
                    Err(_) => true,
                },
                None => true,
            };

            if stale {
                changed.fetch_add(1, Ordering::Relaxed);
            }

            seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(relative);

            WalkState::Continue
        })
    });

    let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let changed = changed.load(Ordering::Relaxed);

    assert!(changed <= to_u32(seen.len()), "changed files cannot exceed source files seen");

    let removed = stored.keys().filter(|path| !seen.contains(path.as_str())).count();

    Ok(StaleFiles { changed, removed: to_u32(removed) })
}

/// Whether an event touches an indexable path, i.e. at least one of its paths
/// is not inside an ignored directory. Events confined to `.constellation`
/// (the store's own writes) are dropped so re-indexing cannot feed itself.
fn is_relevant(event: &notify::Result<Event>) -> bool {
    let Ok(event) = event else {
        return false;
    };

    event.paths.iter().any(|path| !is_ignored_path(path))
}

/// Whether a path lies inside any skipped directory.
#[doc(hidden)]
pub fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(name)
            if name.to_str().is_some_and(|name| SKIP_DIRECTORIES.contains(&name)))
    })
}

/// The drain of queued events until the channel is quiet for [`DEBOUNCE`], collapsing
/// a burst of edits into one re-index.
fn drain_debounce(receiver: &Receiver<notify::Result<Event>>) {
    let mut count: u32 = 0;

    while receiver.recv_timeout(DEBOUNCE).is_ok() {
        count += 1;

        assert!(count <= DEBOUNCE_EVENTS_MAX, "debounce drained over {DEBOUNCE_EVENTS_MAX} events");
    }
}

/// The result of extracting one file off the store thread, the parallel half of
/// indexing. The store-touching half ([`persist_outcome`]) runs serially.
enum ExtractOutcome {
    /// A graph awaiting persistence, with the file metadata to
    /// record alongside it.
    Indexed {
        relative: String,
        content_hash: String,
        language: Language,
        size_bytes: u64,
        modified_at_ms: i64,
        source: String,
        output: ExtractionOutput,
    },
    /// The content hash matched the stored hash; left untouched.
    Unchanged { relative: String },
    /// A file of an unsupported language, unreadable, or over the parse cap.
    Ignored,
}

/// Whether a path is a minified or bundled asset (`*.min.js`, `*.min.css`,
/// `*.bundle.js`): vendor code with no readable structure, excluded from the
/// index so it cannot pollute the graph.
fn is_minified(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    // Case-insensitive suffix tests without allocating a lowercased copy per file.
    ends_with_ignore_ascii_case(name, ".min.js")
        || ends_with_ignore_ascii_case(name, ".min.css")
        || ends_with_ignore_ascii_case(name, ".bundle.js")
}

/// Whether `text` ends with `suffix`, comparing ASCII case-insensitively without
/// allocating: `text.len()`/`suffix.len()` are byte lengths, and the suffixes
/// here are ASCII, so a byte-tail compare is correct.
fn ends_with_ignore_ascii_case(text: &str, suffix: &str) -> bool {
    let text = text.as_bytes();
    let suffix = suffix.as_bytes();

    text.len() >= suffix.len() && text[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// The extraction of one file into its graph without touching the store, so it can run in
/// parallel. Skips re-extraction when the content hash is unchanged; ignores
/// unsupported, unreadable, or oversized files.
fn extract_one(
    project: &ProjectId,
    extractors: &[Box<dyn Extractor>],
    frameworks: &[Box<dyn FrameworkResolver>],
    existing: &FxHashMap<String, String>,
    root: &Path,
    path: &Path,
) -> ExtractOutcome {
    assert!(!extractors.is_empty(), "at least one extractor must be available");

    let Some(language) = path.extension().and_then(|ext| ext.to_str()).and_then(Language::from_extension)
    else {
        return ExtractOutcome::Ignored;
    };

    // Minified/bundled vendor assets parse into thousands of mangled one-letter
    // "symbols" that pollute search, files, and counts (a `Chart.min.js` alone
    // yields hundreds) and never help an agent; skip them entirely.
    if is_minified(path) {
        return ExtractOutcome::Ignored;
    }

    let Some(extractor) = extractors.iter().find(|extractor| extractor.language() == language) else {
        return ExtractOutcome::Ignored;
    };

    let Ok(source) = std::fs::read_to_string(path) else {
        return ExtractOutcome::Ignored;
    };

    if source.len() > SOURCE_BYTES_MAX {
        return ExtractOutcome::Ignored;
    }

    let Some(relative) = relative_path(root, path) else {
        return ExtractOutcome::Ignored;
    };

    let content_hash = hash_hex(source.as_bytes());

    if existing.get(&relative).is_some_and(|stored| stored == &content_hash) {
        return ExtractOutcome::Unchanged { relative };
    }

    // Isolate each file: a parser assert or tree-sitter edge case must skip that
    // one file, not abort the whole parallel index. The release profile is
    // panic=unwind, so this catches; the extractors create fresh parser state per
    // call (they are Sync), so a caught panic cannot corrupt a sibling file.
    let extracted = catch_unwind(AssertUnwindSafe(|| {
        let mut output = extractor.extract(project, &relative, &source);
        merge_frameworks(project, frameworks, language, &relative, &source, &mut output);

        output
    }));

    let output = match extracted {
        Ok(output) => output,
        Err(_) => {
            eprintln!("constellation: skipped {relative}: extraction panicked");

            return ExtractOutcome::Ignored;
        }
    };

    ExtractOutcome::Indexed {
        relative,
        content_hash,
        language,
        size_bytes: source.len() as u64,
        modified_at_ms: modified_ms(path),
        source,
        output,
    }
}

/// The persistence of one extraction outcome, folded into the running stats. Runs on
/// the indexing thread, serializing every store write.
fn persist_outcome(
    store: &Store,
    project: &ProjectId,
    outcome: ExtractOutcome,
    stats: &mut IndexStats,
    seen: &mut FxHashSet<String>,
) -> Result<(), IndexError> {
    match outcome {
        ExtractOutcome::Indexed {
            relative,
            content_hash,
            language,
            size_bytes,
            modified_at_ms,
            source,
            output,
        } => {
            let file = FileIndex {
                path: &relative,
                content_hash: &content_hash,
                language,
                size_bytes,
                modified_at_ms,
                source: &source,
            };

            store.persist_file(
                project,
                &file,
                &output.nodes,
                &output.edges,
                &output.unresolved_refs,
                &output.import_mappings,
                &output.events,
            )?;

            stats.files_indexed += 1;
            stats.nodes = stats.nodes.saturating_add(to_u32(output.nodes.len()));
            stats.edges = stats.edges.saturating_add(to_u32(output.edges.len()));
            stats.unresolved_refs =
                stats.unresolved_refs.saturating_add(to_u32(output.unresolved_refs.len()));

            seen.insert(relative);
        }
        ExtractOutcome::Unchanged { relative } => {
            stats.files_unchanged += 1;
            seen.insert(relative);
        }
        ExtractOutcome::Ignored => stats.files_skipped += 1,
    }

    Ok(())
}

/// The run of each applicable framework's extractor over the file, merging its
/// route nodes and references into `output`, linking each new node to the file.
fn merge_frameworks(
    project: &ProjectId,
    frameworks: &[Box<dyn FrameworkResolver>],
    language: Language,
    relative: &str,
    source: &str,
    output: &mut ExtractionOutput,
) {
    assert!(!relative.is_empty(), "relative path must not be empty");

    let file_id = NodeId::new(project, relative);

    for framework in frameworks {
        if !framework.languages().contains(&language) {
            continue;
        }

        let extra = framework.extract(project, relative, source);

        for node in &extra.nodes {
            let edge = Edge::new(file_id.clone(), node.id.clone(), EdgeKind::Contains)
                .with_provenance("framework");

            output.edges.push(edge);
        }

        output.nodes.extend(extra.nodes);
        output.unresolved_refs.extend(extra.references);
    }
}

/// Whether a directory's entire subtree should be skipped.
fn is_skipped_directory(entry: &ignore::DirEntry) -> bool {
    entry.file_type().is_some_and(|file_type| file_type.is_dir())
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| SKIP_DIRECTORIES.contains(&name))
}

/// The file walk: a recursive walk of `root` that honors `.gitignore`
/// (root and nested, with negations, even outside a git repo) on top of the
/// always-skipped [`SKIP_DIRECTORIES`]. Hidden files are kept: the skip list and
/// `.gitignore` decide what to drop, not a blanket dotfile rule.
fn walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);

    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|entry| !is_skipped_directory(entry));

    builder
}

fn walk(root: &Path) -> ignore::Walk {
    walk_builder(root).build()
}

/// The same walk, parallelized across worker threads for the read-only stale
/// check, where the gitignore traversal and per-file stat dominate and there is
/// no shared mutable index state to serialize on.
fn walk_parallel(root: &Path) -> ignore::WalkParallel {
    walk_builder(root).build_parallel()
}

/// The path of `file` relative to `root`, with separators normalized to `/`
/// so node ids stay stable across platforms.
fn relative_path(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let normalized = relative.to_string_lossy().replace('\\', "/");

    if normalized.is_empty() {
        return None;
    }

    Some(normalized)
}

/// A short content fingerprint for change detection. Not cryptographic, only
/// stable within one build of the tool, which is all re-index detection needs.
fn hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);

    let hex = format!("{:016x}", hasher.finish());

    assert!(hex.len() == 16, "a content hash is sixteen hex digits");

    hex
}

/// The file modification time in epoch milliseconds, or 0 when unavailable.
fn modified_ms(path: &Path) -> i64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    let Ok(elapsed) = modified.duration_since(UNIX_EPOCH) else {
        return 0;
    };

    i64::try_from(elapsed.as_millis()).unwrap_or(0)
}

/// A saturating `usize` -> `u32` conversion; per-file counts are bounded well under the cap.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A project's graph held in memory for bulk resolution: nodes plus name,
/// qualified-name, file, and kind indexes over them, so every lookup is a hash
/// map read with no store round-trip per reference.
struct ProjectContext {
    root: PathBuf,
    nodes: Vec<Arc<Node>>,
    by_name: FxHashMap<String, Vec<u32>>,
    by_lower_name: FxHashMap<String, Vec<u32>>,
    by_qualified_name: FxHashMap<String, Vec<u32>>,
    by_file: FxHashMap<String, Vec<u32>>,
    by_kind: FxHashMap<NodeKind, Vec<u32>>,
    mappings_by_file: FxHashMap<String, Vec<ImportMapping>>,
}

impl ProjectContext {
    fn load(store: &Store, project: &ProjectId, root: &Path) -> Result<Self, IndexError> {
        // Wrap each node in an `Arc` once at load. Every `nodes_by_*` lookup then
        // hands back reference-counted handles, so a name matching many nodes
        // clones counts instead of deep-copying each ~200-byte node.
        let nodes: Vec<Arc<Node>> =
            store.all_nodes(Some(project))?.into_iter().map(Arc::new).collect();

        assert!(nodes.len() <= u32::MAX as usize, "a project must hold fewer than u32::MAX nodes");

        let count = nodes.len();

        let mut by_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_lower_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_qualified_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_file: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_kind: FxHashMap<NodeKind, Vec<u32>> = FxHashMap::default();

        for (index, node) in nodes.iter().enumerate() {
            let position = to_u32(index);

            by_name.entry(node.name.clone()).or_default().push(position);
            by_lower_name.entry(node.name.to_lowercase()).or_default().push(position);
            by_qualified_name.entry(node.qualified_name.clone()).or_default().push(position);
            by_file.entry(node.file_path.clone()).or_default().push(position);
            by_kind.entry(node.kind).or_default().push(position);
        }

        assert!(by_name.len() <= count, "names index at most one entry per node");

        let mut mappings_by_file: FxHashMap<String, Vec<ImportMapping>> = FxHashMap::default();

        for (file_path, mapping) in store.all_import_mappings(project)? {
            mappings_by_file.entry(file_path).or_default().push(mapping);
        }

        Ok(Self {
            root: root.to_path_buf(),
            nodes,
            by_name,
            by_lower_name,
            by_qualified_name,
            by_file,
            by_kind,
            mappings_by_file,
        })
    }

    fn collect(&self, indices: Option<&Vec<u32>>) -> Vec<Arc<Node>> {
        let Some(indices) = indices else {
            return Vec::new();
        };

        indices
            .iter()
            .map(|&index| {
                assert!((index as usize) < self.nodes.len(), "index points at a node");

                Arc::clone(&self.nodes[index as usize])
            })
            .collect()
    }
}

impl ResolutionContext for ProjectContext {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_name.get(name))
    }

    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_lower_name.get(lower_name))
    }

    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_qualified_name.get(qualified_name))
    }

    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>> {
        self.collect(self.by_kind.get(&kind))
    }

    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_file.get(file_path))
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        self.by_file.keys().cloned().collect()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, file_path: &str, _language: Language) -> Vec<ImportMapping> {
        self.mappings_by_file.get(file_path).cloned().unwrap_or_default()
    }
}

/// A resolution context that answers each lookup with an indexed store query
/// instead of loading the whole project graph into memory. For incremental
/// re-resolution on a large project, where only a few references change and
/// materializing every node would dominate the cost. A failed query degrades to
/// an empty result (the reference simply stays pending), never a panic.
struct StoreContext<'store> {
    store: &'store Store,
    project: ProjectId,
    root: PathBuf,
}

impl ResolutionContext for StoreContext<'_> {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_named_in(&self.project, name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_lower_named_in(&self.project, lower_name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_qualified_in(&self.project, qualified_name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>> {
        self.store.nodes_kind_in(&self.project, kind).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>> {
        self.store.nodes_file_in(&self.project, file_path).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        self.store.project_file_paths(&self.project).unwrap_or_default()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, file_path: &str, _language: Language) -> Vec<ImportMapping> {
        self.store.import_mappings_in(&self.project, file_path).unwrap_or_default()
    }
}

/// A filesystem-only resolution context for framework detection, run before
/// any nodes exist: graph lookups are empty, file access reads the repo root.
struct FsContext {
    root: PathBuf,
}

impl FsContext {
    fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }
}

impl ResolutionContext for FsContext {
    fn nodes_by_name(&self, _name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_lower_name(&self, _lower_name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_qualified_name(&self, _qualified_name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_kind(&self, _kind: NodeKind) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_in_file(&self, _file_path: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        Vec::new()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, _file_path: &str, _language: Language) -> Vec<ImportMapping> {
        Vec::new()
    }
}

/// Every project's nodes indexed by simple name, for the cross-project export
/// lookups [`ImportLinker`] makes.
struct ConstellationContext {
    by_name: FxHashMap<String, Vec<Arc<Node>>>,
}

impl ConstellationContext {
    /// The cross-project export index over `nodes`, excluding any node whose
    /// project is in `reference_only`: a reference-only version is queryable and
    /// links out, but its symbols are never cross-project link targets, so two
    /// indexed versions of one library cannot compete to win an ambiguous import.
    fn new(nodes: Vec<Node>, reference_only: &FxHashSet<String>) -> Self {
        let mut by_name: FxHashMap<String, Vec<Arc<Node>>> =
            FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());

        for node in nodes {
            if reference_only.contains(node.project_id.as_str()) {
                continue;
            }

            by_name.entry(node.name.clone()).or_default().push(Arc::new(node));
        }

        Self { by_name }
    }
}

impl LinkContext for ConstellationContext {
    fn exports_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.by_name.get(name).cloned().unwrap_or_default()
    }
}
