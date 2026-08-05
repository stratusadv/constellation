//! Precomputed Django execution flows.
//!
//! `feature` and `path` are both on-demand and anchored: the agent must already
//! know a symbol to ask. A flow inverts that. Every framework entry point Django
//! can dispatch is detected up front, the bounded set of symbols reachable from
//! it is traced, and the result is scored for criticality and persisted, so
//! "list every execution path, ranked" and "which user-facing flows does my diff
//! touch" become single lookups.
//!
//! A flow is a **reach set, not a single path**: `(entry point, the bounded set
//! of symbols reachable from it through [`FLOW_TRAVERSAL_KINDS`], the maximum
//! breadth-first depth reached)`. Nothing here orders those symbols into a
//! chain, and the persisted column is named `reach_json` to say so.
//!
//! Detection is precise rather than heuristic, because constellation already
//! indexes `NodeKind::Route` as a first-class node and carries `RoutesTo`,
//! `Renders`, `Receives`, `Handles`, and `AdminOf` edges. There is no regex over
//! decorator source anywhere in this module beyond reading the decorator list
//! the extractor already captured.

use constellation_graph::{
    EdgeKind, Node, NodeKind, ProjectId, app_segment, is_covering_ref, is_generated_path,
    is_migration_path, is_security_sensitive, is_test_path,
};
use constellation_store::{FlowMember, FlowRecord, Store};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

use crate::IndexError;

/// The edge kinds a flow trace follows.
///
/// Deliberately narrower than the `is_flow_edge` set `constellation_path` uses.
/// `path` answers "how does X reach Y" for a caller who named both ends, so it
/// may follow `Extends`, `Decorates`, `References`, and `Overrides` to find any
/// connection at all. A flow starts from one end only and must stay bounded:
/// following `Extends` would pull every base class, and through a shared mixin
/// every subclass in the constellation, into a single reach set. The remaining
/// kinds are the Django request and dispatch path itself.
///
/// `Resolves` is followed, but a route reached that way is a leaf: see
/// [`trace_reach`], where a template's `{% url %}` targets are recorded without
/// being expanded. Walking onward from them is the other way a reach set swallows
/// the constellation, through page chrome rather than through a mixin.
pub const FLOW_TRAVERSAL_KINDS: &[EdgeKind] = &[
    EdgeKind::Calls,
    EdgeKind::ExtendsTemplate,
    EdgeKind::Handles,
    EdgeKind::IncludesTemplate,
    EdgeKind::Instantiates,
    EdgeKind::Receives,
    EdgeKind::Renders,
    EdgeKind::Resolves,
    EdgeKind::RoutesTo,
];

/// The breadth-first depth bound on one flow trace. Route to view to template to
/// include is four hops; fifteen leaves generous room for service layers without
/// letting a call chain wander the whole graph.
pub const FLOW_DEPTH_MAX: u32 = 15;

/// The node bound on one flow's reach set. On overflow the set is cut short and
/// the flow is marked truncated, never silently capped.
pub const FLOW_REACH_NODES_MAX: usize = 2_000;

/// The bound on flows stored per project. On overflow the highest-criticality
/// flows are kept and the dropped count is reported.
pub const FLOWS_TOTAL_MAX: usize = 20_000;

/// The distinct app packages in a reach set at which app spread saturates.
const APP_SPREAD_SATURATION: u32 = 5;

/// The distinct foreign projects in a reach set at which cross-project reach
/// saturates. A flow that crosses a repository boundary at all is notable.
const CROSS_PROJECT_SATURATION: u32 = 2;

/// The external and unresolved targets in a reach set at which the
/// "leaves the graph" factor saturates.
const EXTERNAL_SATURATION: u32 = 5;

/// The criticality weights, which sum to one.
const ENTRY_KIND_WEIGHT: f64 = 0.20;
const APP_SPREAD_WEIGHT: f64 = 0.20;
const SECURITY_WEIGHT: f64 = 0.20;
const TEST_GAP_WEIGHT: f64 = 0.15;
const CROSS_PROJECT_WEIGHT: f64 = 0.10;
const EXTERNAL_WEIGHT: f64 = 0.10;
const DEPTH_WEIGHT: f64 = 0.05;

/// The decorator fragments that mark a Django REST Framework view.
const DRF_DECORATORS: &[&str] = &["api_view", "renderer_classes", "permission_classes"];

/// The base-class name fragments that mark a Django REST Framework view.
const DRF_BASES: &[&str] = &["APIView", "GenericAPIView", "ViewSet"];

/// The decorator fragments that mark a Celery task.
const TASK_DECORATORS: &[&str] = &["periodic_task", "shared_task", "task"];

/// The decorator fragments that mark a signal receiver.
const RECEIVER_DECORATORS: &[&str] = &["receiver"];

/// The class of framework entry point a flow starts from, ordered by how
/// confidently Django dispatches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    AdminAction,
    AppConfigHook,
    CeleryTask,
    DrfView,
    ManagementCommand,
    ModelLifecycle,
    Route,
    SignalReceiver,
    TrueRoot,
}

impl EntryKind {
    /// The snake_case label stored for this entry kind.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::AdminAction => "admin_action",
            EntryKind::AppConfigHook => "app_config_hook",
            EntryKind::CeleryTask => "celery_task",
            EntryKind::DrfView => "drf_view",
            EntryKind::ManagementCommand => "management_command",
            EntryKind::ModelLifecycle => "model_lifecycle",
            EntryKind::Route => "route",
            EntryKind::SignalReceiver => "signal_receiver",
            EntryKind::TrueRoot => "true_root",
        }
    }

    /// The `0.0..=1.0` confidence that this entry point is genuinely reachable
    /// from outside the codebase, which is the entry-kind criticality factor.
    pub fn weight(self) -> f64 {
        let weight = match self {
            EntryKind::Route | EntryKind::DrfView => 1.0,
            EntryKind::CeleryTask | EntryKind::ManagementCommand | EntryKind::SignalReceiver => 0.6,
            EntryKind::AdminAction => 0.4,
            EntryKind::AppConfigHook | EntryKind::ModelLifecycle | EntryKind::TrueRoot => 0.3,
        };

        assert!((0.0..=1.0).contains(&weight), "an entry weight lands in 0..=1");

        weight
    }
}

/// The knobs a flow computation accepts.
#[derive(Clone, Copy, Debug)]
pub struct FlowOptions {
    /// The breadth-first depth bound, clamped to [`FLOW_DEPTH_MAX`].
    pub depth_max: u32,
    /// Whether test files may seed a flow. Off by default: a test suite is not a
    /// user-facing execution path.
    pub include_tests: bool,
}

impl Default for FlowOptions {
    fn default() -> Self {
        Self { depth_max: FLOW_DEPTH_MAX, include_tests: false }
    }
}

/// The outcome of one flow computation, for the CLI summary.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlowStats {
    /// The flows discarded past [`FLOWS_TOTAL_MAX`], lowest criticality first.
    pub dropped: u32,
    /// The entry points detected before single-node flows were discarded.
    pub entries: u32,
    /// The flows stored.
    pub stored: u32,
    /// The flows whose reach set hit [`FLOW_REACH_NODES_MAX`].
    pub truncated: u32,
}

/// The whole-graph adjacency a flow trace walks, built once per computation.
///
/// Nodes come from every project, not just the one being traced, so a flow that
/// crosses a repository boundary is followed rather than cut. Positions index
/// the `nodes` vector; the traversal never touches the store.
struct FlowGraph {
    admin_targets: FxHashSet<u32>,
    covered: Vec<bool>,
    has_incoming_flow: Vec<bool>,
    index: FxHashMap<String, u32>,
    nodes: Vec<Node>,
    out_edges: Vec<Vec<u32>>,
    receives_targets: FxHashSet<u32>,
    unresolved: FxHashMap<String, u32>,
}

impl FlowGraph {
    /// The graph loaded from the store and reduced to what flow tracing needs:
    /// the flow-kind out-edges, which nodes already have an incoming flow edge
    /// (so a true root can be told from a reachable symbol), which are admin or
    /// signal targets, which carry test coverage, and how much dynamic dispatch
    /// each emits.
    fn build(store: &Store) -> Result<Self, IndexError> {
        let nodes = store.all_nodes(None)?;
        let edges = store.all_edges_kinded()?;
        let unresolved = store.unresolved_counts_by_source(None)?;

        let count = nodes.len();

        assert!(count <= u32::MAX as usize, "graph must hold fewer than u32::MAX nodes");

        let mut index: FxHashMap<String, u32> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());

        for (position, node) in nodes.iter().enumerate() {
            index.insert(node.id.as_str().to_string(), position as u32);
        }

        let mut graph = Self {
            admin_targets: FxHashSet::default(),
            covered: vec![false; count],
            has_incoming_flow: vec![false; count],
            index,
            nodes,
            out_edges: vec![Vec::new(); count],
            receives_targets: FxHashSet::default(),
            unresolved,
        };

        graph.absorb_edges(&edges);

        assert!(graph.out_edges.len() == graph.nodes.len(), "one edge list per node");
        assert!(graph.covered.len() == graph.nodes.len(), "one coverage flag per node");

        Ok(graph)
    }

    /// The edge list folded into the adjacency and the per-node flags. One pass,
    /// because each edge feeds several of them at once.
    fn absorb_edges(&mut self, edges: &[(String, String, EdgeKind)]) {
        for (source, target, kind) in edges {
            let (Some(&from), Some(&to)) =
                (self.index.get(source.as_str()), self.index.get(target.as_str()))
            else {
                continue;
            };

            if is_covering_ref(*kind, &self.nodes[from as usize].file_path) {
                self.covered[to as usize] = true;
            }

            match kind {
                EdgeKind::AdminOf => {
                    self.admin_targets.insert(to);
                }
                EdgeKind::Receives => {
                    self.receives_targets.insert(to);
                }
                _ => {}
            }

            if FLOW_TRAVERSAL_KINDS.contains(kind) {
                self.out_edges[from as usize].push(to);
                self.has_incoming_flow[to as usize] = true;
            }
        }
    }
}

/// The flows of one project recomputed from scratch and stored, replacing
/// whatever was there. The `constellation flows` command.
pub fn compute_flows(
    store: &Store,
    project: &ProjectId,
    options: FlowOptions,
) -> Result<FlowStats, IndexError> {
    let graph = FlowGraph::build(store)?;
    let entries = detect_entries(&graph, project, options);

    let (records, stats) = trace_and_score(&graph, &entries, options);

    store.replace_flows(project, &records)?;

    Ok(stats)
}

/// The flows of one project retraced incrementally: every flow whose reach set
/// touched a changed file is deleted, its entry point re-detected along with any
/// new entry point in those files, and only those are traced again. Cheaper than
/// a full recompute; equivalent whenever a change cannot make an untouched entry
/// point newly reach a changed file, which is the common editing case.
pub fn retrace_flows(
    store: &Store,
    project: &ProjectId,
    changed_files: &[String],
    options: FlowOptions,
) -> Result<FlowStats, IndexError> {
    if changed_files.is_empty() {
        return Ok(FlowStats::default());
    }

    let affected = store.flows_touching_files(project, changed_files)?;

    let stale_ids: Vec<i64> = affected.iter().map(|flow| flow.id).collect();
    let stale_entries: FxHashSet<String> =
        affected.iter().map(|flow| flow.entry_node_id.clone()).collect();

    // The retrace runs against the graph as it stands, with the stale flows
    // still in the table: nothing here reads them, and deleting first would open
    // a window where a crash leaves the project short of the flows it was about
    // to rewrite. The delete and the insert commit together at the end instead.
    let graph = FlowGraph::build(store)?;
    let changed: FxHashSet<&str> = changed_files.iter().map(String::as_str).collect();

    let entries: Vec<(u32, EntryKind)> = detect_entries(&graph, project, options)
        .into_iter()
        .filter(|(position, _)| {
            let node = &graph.nodes[*position as usize];

            stale_entries.contains(node.id.as_str()) || changed.contains(node.file_path.as_str())
        })
        .collect();

    let (records, stats) = trace_and_score(&graph, &entries, options);

    store.replace_flow_subset(project, &stale_ids, &records)?;

    Ok(stats)
}

/// The detected entry points traced, scored, ranked by criticality, and cut to
/// [`FLOWS_TOTAL_MAX`]. Shared by the full and incremental paths so both produce
/// identical records for the same entry point.
fn trace_and_score(
    graph: &FlowGraph,
    entries: &[(u32, EntryKind)],
    options: FlowOptions,
) -> (Vec<FlowRecord>, FlowStats) {
    let depth_max = options.depth_max.clamp(1, FLOW_DEPTH_MAX);

    let mut records: Vec<FlowRecord> = Vec::with_capacity(entries.len());
    let mut stats = FlowStats {
        entries: u32::try_from(entries.len()).unwrap_or(u32::MAX),
        ..FlowStats::default()
    };

    for &(position, entry_kind) in entries {
        let reach = trace_reach(graph, position, depth_max);

        // A single-node flow is an entry point that reaches nothing: it carries
        // no path information, so it is discarded rather than stored.
        if reach.members.len() < 2 {
            continue;
        }

        if reach.truncated {
            stats.truncated = stats.truncated.saturating_add(1);
        }

        records.push(build_record(graph, position, entry_kind, &reach));
    }

    records.sort_by(|left, right| {
        right.criticality.total_cmp(&left.criticality).then(left.name.cmp(&right.name))
    });

    if records.len() > FLOWS_TOTAL_MAX {
        stats.dropped = u32::try_from(records.len() - FLOWS_TOTAL_MAX).unwrap_or(u32::MAX);
        records.truncate(FLOWS_TOTAL_MAX);
    }

    stats.stored = u32::try_from(records.len()).unwrap_or(u32::MAX);

    assert!(records.len() <= FLOWS_TOTAL_MAX, "stored flows respect their total cap");

    (records, stats)
}

/// A traced reach set: the members with the depth each was first reached at,
/// the deepest level reached, and whether the trace hit its node cap.
struct Reach {
    depth_max: u32,
    members: Vec<(u32, u32)>,
    truncated: bool,
}

/// The bounded set of nodes reachable from `entry` through
/// [`FLOW_TRAVERSAL_KINDS`], breadth-first with an explicit queue and visited
/// set (never recursion), so the traversal is provably finite in both depth and
/// node count.
fn trace_reach(graph: &FlowGraph, entry: u32, depth_max: u32) -> Reach {
    assert!(depth_max >= 1, "a flow trace walks at least one level");
    assert!((entry as usize) < graph.nodes.len(), "an entry position indexes a node");

    let mut visited: FxHashSet<u32> = FxHashSet::default();
    visited.insert(entry);

    let mut queue: VecDeque<(u32, u32)> = VecDeque::new();
    queue.push_back((entry, 0));

    let mut reach = Reach { depth_max: 0, members: vec![(entry, 0)], truncated: false };

    while let Some((node, depth)) = queue.pop_front() {
        assert!(depth <= depth_max, "the queue never holds a node past the depth bound");

        if depth >= depth_max {
            continue;
        }

        let from_template = graph.nodes[node as usize].kind == NodeKind::Template;

        for &neighbor in &graph.out_edges[node as usize] {
            if !visited.insert(neighbor) {
                continue;
            }

            if reach.members.len() >= FLOW_REACH_NODES_MAX {
                reach.truncated = true;

                return reach;
            }

            reach.members.push((neighbor, depth + 1));
            reach.depth_max = reach.depth_max.max(depth + 1);

            // A route reached from a template is a `{% url %}` link target, not
            // the continuation of this request. Expanding it would follow
            // `RoutesTo` into that page's view and template, whose own chrome
            // links onward again, so every flow that renders a page carrying a
            // nav bar absorbs the whole site: reach sets saturate at the node cap
            // and criticality stops telling two routes apart. The link itself is
            // worth recording, so the route stays a member; only the walk stops.
            if from_template && graph.nodes[neighbor as usize].kind == NodeKind::Route {
                continue;
            }

            queue.push_back((neighbor, depth + 1));
        }
    }

    assert!(reach.members.len() <= FLOW_REACH_NODES_MAX, "a reach set respects its node cap");

    reach
}

/// A traced reach set turned into the record the store persists: the aggregate
/// shape (files, apps, projects, depth), the criticality, and the members.
fn build_record(
    graph: &FlowGraph,
    entry: u32,
    entry_kind: EntryKind,
    reach: &Reach,
) -> FlowRecord {
    let entry_node = &graph.nodes[entry as usize];
    let shape = ReachShape::measure(graph, entry_node, reach);

    let criticality = score_criticality(entry_kind, &shape, reach.depth_max);

    let members: Vec<FlowMember> = reach
        .members
        .iter()
        .map(|&(position, depth)| FlowMember {
            depth,
            node_id: graph.nodes[position as usize].id.as_str().to_string(),
        })
        .collect();

    FlowRecord {
        app_count: shape.app_count,
        criticality,
        depth_max: reach.depth_max,
        entry_kind: entry_kind.as_str().to_string(),
        entry_node_id: entry_node.id.as_str().to_string(),
        file_count: shape.file_count,
        members,
        name: flow_name(entry_node),
        project_count: shape.project_count,
        truncated: reach.truncated,
    }
}

/// The measured shape of one reach set, everything criticality scores from.
struct ReachShape {
    app_count: u32,
    external_count: u32,
    file_count: u32,
    project_count: u32,
    security_fraction: f64,
    test_gap: f64,
}

impl ReachShape {
    /// The reach set measured in one pass: distinct files, apps and foreign
    /// projects, the fraction of members whose name is security-sensitive, the
    /// fraction with no covering test, and how often the set leaves the graph
    /// (an external stub or an unresolved dynamic reference).
    fn measure(graph: &FlowGraph, entry_node: &Node, reach: &Reach) -> Self {
        let home_project = entry_node.project_id.as_str();

        let mut apps: FxHashSet<&str> = FxHashSet::default();
        let mut files: FxHashSet<&str> = FxHashSet::default();
        let mut projects: FxHashSet<&str> = FxHashSet::default();

        let mut external: u32 = 0;
        let mut sensitive: u32 = 0;
        let mut uncovered: u32 = 0;

        for &(position, _) in &reach.members {
            let node = &graph.nodes[position as usize];

            apps.insert(app_segment(&node.file_path));
            files.insert(node.file_path.as_str());

            if node.project_id.as_str() != home_project {
                projects.insert(node.project_id.as_str());
            }

            if node.kind == NodeKind::External {
                external = external.saturating_add(1);
            }

            external =
                external.saturating_add(graph.unresolved.get(node.id.as_str()).copied().unwrap_or(0));

            if is_security_sensitive(&node.name, &node.qualified_name) {
                sensitive = sensitive.saturating_add(1);
            }

            if !graph.covered[position as usize] {
                uncovered = uncovered.saturating_add(1);
            }
        }

        let total = reach.members.len().max(1) as f64;

        Self {
            app_count: u32::try_from(apps.len()).unwrap_or(u32::MAX),
            external_count: external,
            file_count: u32::try_from(files.len()).unwrap_or(u32::MAX),
            project_count: u32::try_from(projects.len()).unwrap_or(u32::MAX),
            security_fraction: f64::from(sensitive) / total,
            test_gap: f64::from(uncovered) / total,
        }
    }
}

/// The blended criticality of one flow, in `0.0..=1.0`. App spread beats raw
/// file spread for Django: crossing an app boundary is the coupling signal,
/// crossing a file inside one app is not.
fn score_criticality(entry_kind: EntryKind, shape: &ReachShape, depth_max: u32) -> f64 {
    assert!((0.0..=1.0).contains(&shape.security_fraction), "a fraction lands in 0..=1");
    assert!((0.0..=1.0).contains(&shape.test_gap), "a fraction lands in 0..=1");

    let criticality = entry_kind.weight() * ENTRY_KIND_WEIGHT
        + ratio(shape.app_count, APP_SPREAD_SATURATION) * APP_SPREAD_WEIGHT
        + shape.security_fraction * SECURITY_WEIGHT
        + shape.test_gap * TEST_GAP_WEIGHT
        + ratio(shape.project_count, CROSS_PROJECT_SATURATION) * CROSS_PROJECT_WEIGHT
        + ratio(shape.external_count, EXTERNAL_SATURATION) * EXTERNAL_WEIGHT
        + ratio(depth_max, FLOW_DEPTH_MAX) * DEPTH_WEIGHT;

    let criticality = criticality.clamp(0.0, 1.0);

    assert!(criticality >= 0.0, "a criticality is never negative");
    assert!(criticality <= 1.0, "a criticality never exceeds one");

    criticality
}

/// The `0.0..=1.0` ratio of a count against a saturation bound.
fn ratio(value: u32, saturation: u32) -> f64 {
    assert!(saturation > 0, "a saturation bound is positive");

    f64::from(value.min(saturation)) / f64::from(saturation)
}

/// The display name of a flow: a route's URL pattern qualified by the app that
/// declares it, or otherwise the entry symbol's `Owner.member` tail, which reads
/// as a feature name where a bare method name does not.
///
/// The app prefix is not decoration. Django URL patterns repeat across apps by
/// design, so a bare pattern collides constantly: one real portal produced 895
/// flows under 664 distinct names, with 33 of them called `<int:pk>/delete/`.
/// A ranked list where a third of the rows share a name cannot be acted on.
fn flow_name(entry: &Node) -> String {
    if entry.kind == NodeKind::Route {
        let pattern = entry.qualified_name.split("route::").nth(1).unwrap_or(&entry.name);

        if !pattern.is_empty() {
            return match url_scope(&entry.file_path) {
                Some(scope) => format!("{scope} {pattern}"),
                None => pattern.to_string(),
            };
        }
    }

    let tail = entry.qualified_name.rsplit("::").next().unwrap_or(&entry.qualified_name);

    if tail.is_empty() { entry.name.clone() } else { tail.to_string() }
}

/// The app path a `urls.py` belongs to, as the dotted-free directory chain
/// between an optional leading `app/` and the `urls` module itself:
/// `app/company/contact/urls/__init__.py` becomes `company/contact`. `None` when
/// the path carries no such chain, so a project-level urlconf is left unadorned
/// rather than prefixed with something meaningless.
fn url_scope(file_path: &str) -> Option<String> {
    let path = file_path.replace('\\', "/");

    let mut segments: Vec<&str> = path.split('/').collect();

    // Drop the file itself, then the `urls` package or module that holds it.
    segments.pop()?;

    if segments.last() == Some(&"urls") {
        segments.pop();
    }

    if segments.first() == Some(&"app") || segments.first() == Some(&"apps") {
        segments.remove(0);
    }

    let scope = segments.join("/");

    if scope.is_empty() { None } else { Some(scope) }
}

/// The entry points detected in one project, as `(node position, entry kind)`.
/// Migrations are always excluded and tests are excluded unless asked for; both
/// are code paths a reviewer never calls a user-facing flow.
fn detect_entries(
    graph: &FlowGraph,
    project: &ProjectId,
    options: FlowOptions,
) -> Vec<(u32, EntryKind)> {
    let mut entries: Vec<(u32, EntryKind)> = Vec::new();

    for (position, node) in graph.nodes.iter().enumerate() {
        if node.project_id.as_str() != project.as_str() {
            continue;
        }

        if is_migration_path(&node.file_path) || is_generated_path(&node.file_path) {
            continue;
        }

        if !options.include_tests && is_test_path(&node.file_path) {
            continue;
        }

        let position = position as u32;

        if let Some(kind) = classify_entry(graph, node, position) {
            entries.push((position, kind));
        }
    }

    assert!(entries.len() <= graph.nodes.len(), "no node is an entry point twice");

    entries
}

/// The entry kind one node qualifies as, most confident first, or `None` when it
/// is an ordinary reachable symbol rather than something Django dispatches.
fn classify_entry(graph: &FlowGraph, node: &Node, position: u32) -> Option<EntryKind> {
    if node.kind == NodeKind::Route {
        return Some(EntryKind::Route);
    }

    if is_drf_view(node) {
        return Some(EntryKind::DrfView);
    }

    if is_management_command(node) {
        return Some(EntryKind::ManagementCommand);
    }

    if has_decorator(node, TASK_DECORATORS) {
        return Some(EntryKind::CeleryTask);
    }

    if graph.receives_targets.contains(&position) || has_decorator(node, RECEIVER_DECORATORS) {
        return Some(EntryKind::SignalReceiver);
    }

    if graph.admin_targets.contains(&position) {
        return Some(EntryKind::AdminAction);
    }

    if is_app_config_hook(node) {
        return Some(EntryKind::AppConfigHook);
    }

    if is_model_lifecycle(node) {
        return Some(EntryKind::ModelLifecycle);
    }

    // A true root: a definition nothing in the graph flows into. Framework
    // dispatch is already covered by the branches above, so what remains here is
    // genuinely un-called code that still deserves a flow of its own when it
    // reaches something.
    let is_definition = matches!(
        node.kind,
        NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Model | NodeKind::View
    );

    if is_definition && !graph.has_incoming_flow[position as usize] {
        return Some(EntryKind::TrueRoot);
    }

    None
}

/// Whether any of the node's decorators contains one of `fragments`, matched
/// case-insensitively against the decorator text the extractor captured.
fn has_decorator(node: &Node, fragments: &[&str]) -> bool {
    node.decorators.iter().any(|decorator| {
        let lower = decorator.to_ascii_lowercase();

        fragments.iter().any(|fragment| lower.contains(fragment))
    })
}

/// Whether a node is a Django REST Framework view: a `View` or `Class` carrying
/// a DRF decorator, or one whose name or signature names a DRF base.
fn is_drf_view(node: &Node) -> bool {
    if !matches!(node.kind, NodeKind::View | NodeKind::Class | NodeKind::Function) {
        return false;
    }

    if has_decorator(node, DRF_DECORATORS) {
        return true;
    }

    let signature = node.signature.as_deref().unwrap_or_default();

    DRF_BASES.iter().any(|base| signature.contains(base))
}

/// Whether a node is a Django management command: the `Command` class under
/// `management/commands/`, or that class's `handle` method.
fn is_management_command(node: &Node) -> bool {
    if !constellation_graph::is_management_command_path(&node.file_path) {
        return false;
    }

    node.name == "Command" || node.name == "handle"
}

/// Whether a node is an `AppConfig.ready` hook, which Django calls once at
/// startup with no static caller.
fn is_app_config_hook(node: &Node) -> bool {
    let path = node.file_path.replace('\\', "/");

    node.name == "ready" && path.ends_with("apps.py")
}

/// Whether a node is an overridden model lifecycle hook (`save`, `delete`,
/// `clean`), which Django calls on the ORM's behalf.
fn is_model_lifecycle(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Method) && matches!(node.name.as_str(), "save" | "delete" | "clean")
}

#[cfg(test)]
mod tests {
    use super::{
        APP_SPREAD_WEIGHT, CROSS_PROJECT_WEIGHT, DEPTH_WEIGHT, ENTRY_KIND_WEIGHT, EXTERNAL_WEIGHT,
        EntryKind, FLOW_DEPTH_MAX, FLOW_TRAVERSAL_KINDS, ReachShape, SECURITY_WEIGHT, url_scope,
        TEST_GAP_WEIGHT, score_criticality,
    };

    use constellation_graph::EdgeKind;

    fn flat_shape() -> ReachShape {
        ReachShape {
            app_count: 1,
            external_count: 0,
            file_count: 1,
            project_count: 0,
            security_fraction: 0.0,
            test_gap: 0.0,
        }
    }

    #[test]
    fn the_criticality_weights_sum_to_one() {
        let total = ENTRY_KIND_WEIGHT
            + APP_SPREAD_WEIGHT
            + SECURITY_WEIGHT
            + TEST_GAP_WEIGHT
            + CROSS_PROJECT_WEIGHT
            + EXTERNAL_WEIGHT
            + DEPTH_WEIGHT;

        assert!((total - 1.0).abs() < 1e-9, "criticality weights sum to one, got {total}");
    }

    #[test]
    fn criticality_is_monotone_in_each_factor() {
        let base = score_criticality(EntryKind::TrueRoot, &flat_shape(), 1);

        let spread = ReachShape { app_count: 5, ..flat_shape() };
        let sensitive = ReachShape { security_fraction: 1.0, ..flat_shape() };
        let untested = ReachShape { test_gap: 1.0, ..flat_shape() };
        let crossing = ReachShape { project_count: 2, ..flat_shape() };
        let leaky = ReachShape { external_count: 5, ..flat_shape() };

        assert!(score_criticality(EntryKind::Route, &flat_shape(), 1) > base, "entry kind");
        assert!(score_criticality(EntryKind::TrueRoot, &spread, 1) > base, "app spread");
        assert!(score_criticality(EntryKind::TrueRoot, &sensitive, 1) > base, "security");
        assert!(score_criticality(EntryKind::TrueRoot, &untested, 1) > base, "test gap");
        assert!(score_criticality(EntryKind::TrueRoot, &crossing, 1) > base, "cross project");
        assert!(score_criticality(EntryKind::TrueRoot, &leaky, 1) > base, "external");

        assert!(
            score_criticality(EntryKind::TrueRoot, &flat_shape(), FLOW_DEPTH_MAX) > base,
            "depth",
        );
    }

    #[test]
    fn a_maximal_flow_scores_exactly_one() {
        let maximal = ReachShape {
            app_count: u32::MAX,
            external_count: u32::MAX,
            file_count: u32::MAX,
            project_count: u32::MAX,
            security_fraction: 1.0,
            test_gap: 1.0,
        };

        let criticality = score_criticality(EntryKind::Route, &maximal, FLOW_DEPTH_MAX);

        assert!((criticality - 1.0).abs() < 1e-9, "a maximal flow lands on one, got {criticality}");
    }

    #[test]
    fn criticality_is_deterministic() {
        let first = score_criticality(EntryKind::Route, &flat_shape(), 3);
        let second = score_criticality(EntryKind::Route, &flat_shape(), 3);

        assert_eq!(first.to_bits(), second.to_bits(), "the same inputs score bit-identically");
    }

    #[test]
    fn the_traversal_set_excludes_inheritance_and_structure() {
        for kind in [EdgeKind::Calls, EdgeKind::RoutesTo, EdgeKind::Renders, EdgeKind::IncludesTemplate] {
            assert!(FLOW_TRAVERSAL_KINDS.contains(&kind), "{kind:?} is part of the request path");
        }

        for kind in [
            EdgeKind::Contains,
            EdgeKind::Decorates,
            EdgeKind::Extends,
            EdgeKind::Imports,
            EdgeKind::Overrides,
            EdgeKind::References,
            EdgeKind::RelatesTo,
            EdgeKind::Returns,
            EdgeKind::TypeOf,
        ] {
            assert!(
                !FLOW_TRAVERSAL_KINDS.contains(&kind),
                "{kind:?} would explode the reach set or is not execution",
            );
        }
    }

    #[test]
    fn a_url_scope_names_the_app_that_declares_the_route() {
        assert_eq!(url_scope("app/asset/urls/__init__.py").as_deref(), Some("asset"));
        assert_eq!(url_scope("app/company/contact/urls/__init__.py").as_deref(), Some("company/contact"));
        assert_eq!(url_scope("app/harvest/urls.py").as_deref(), Some("harvest"));
        assert_eq!(url_scope("apps/orders/urls.py").as_deref(), Some("orders"));
        assert_eq!(url_scope("orders/urls.py").as_deref(), Some("orders"));

        assert_eq!(url_scope("urls.py"), None, "a project-level urlconf gets no prefix");
        assert_eq!(url_scope("app/urls.py"), None, "nor does one directly under app/");
    }

    #[test]
    fn entry_weights_rank_by_dispatch_confidence() {
        assert!(EntryKind::Route.weight() > EntryKind::CeleryTask.weight());
        assert!(EntryKind::CeleryTask.weight() > EntryKind::AdminAction.weight());
        assert!(EntryKind::AdminAction.weight() > EntryKind::TrueRoot.weight());
    }
}
