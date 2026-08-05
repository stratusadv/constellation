//! `constellation_flows`, `constellation_affected_flows`, and
//! `constellation_path`: Django execution flows.

use std::fmt::Write;

use std::collections::VecDeque;

use constellation_graph::{
    EdgeKind, Node, ProjectId,
};
use constellation_store::{FlowRow, FlowSort, Store, StoreError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::limits::{
    AFFECTED_FLOW_NODES_MAX, FLOWS_FETCH_MAX, FLOW_CANDIDATES_MAX, FLOW_ENDPOINTS_MAX,
    FLOW_HOPS_MAX, FLOW_NODES_MAX, FLOW_PATHS_MAX,
};
use crate::rank::query_tokens;
use crate::tools::changed::changed_candidates;
use crate::tools::history::find_project;
use crate::tools::impact::project_roots;

/// Whether any project in the constellation has computed flows, so an empty
/// result can tell "the pass never ran" apart from "it ran, nothing matched".
pub(crate) fn any_flows_computed(store: &Store) -> Result<bool, StoreError> {
    for project in store.all_projects()? {
        if store.count_flows(&project.id)? > 0 {
            return Ok(true);
        }
    }

    Ok(false)
}

/// The precomputed Django execution flows, ranked, grouped by project. Each
/// names its entry point and the shape of its reach set, so the listing answers
/// "what are the user-facing paths here" with no symbol named first.
#[doc(hidden)]
pub fn flows_text(
    store: &Store,
    project_filter: Option<&str>,
    pattern: Option<&str>,
    sort: Option<&str>,
    limit: u32,
) -> Result<String, StoreError> {
    let project_id = match project_filter {
        Some(name) => match find_project(store, name)? {
            Some(id) => Some(id),
            None => return Ok(format!("no project named {name:?}")),
        },
        None => None,
    };

    let sort = match sort.map(FlowSort::from_str_label) {
        Some(Some(sort)) => sort,
        Some(None) => {
            return Ok(format!(
                "unknown sort {:?}; valid values are criticality, size, name",
                sort.unwrap_or_default(),
            ));
        }
        None => FlowSort::Criticality,
    };

    let needle = pattern.filter(|pattern| !pattern.is_empty()).map(str::to_lowercase);
    let fetch = if needle.is_some() { FLOWS_FETCH_MAX } else { limit };

    let mut flows = store.flows(project_id.as_ref(), sort, fetch.max(limit))?;

    if let Some(needle) = &needle {
        flows.retain(|flow| {
            flow.name.to_lowercase().contains(needle) || flow.entry_kind.contains(needle)
        });
    }

    if flows.is_empty() {
        return flows_empty_message(store, project_filter, pattern);
    }

    let matched = flows.len();
    flows.truncate(limit as usize);

    let mut out = format!("execution flows ({} of {matched} shown, most critical first):\n", flows.len());

    render_flow_rows(&mut out, &flows);

    if matched > flows.len() {
        let _ = writeln!(out, "(+{} more; raise limit or narrow with pattern=)", matched - flows.len());
    }

    Ok(out)
}

/// The flow rows rendered, grouped by project in first-seen order, each on one
/// line naming its entry kind, reach shape, and criticality.
fn render_flow_rows(out: &mut String, flows: &[FlowRow]) {
    let mut current_project = "";

    for flow in flows {
        if flow.project_id != current_project {
            let _ = writeln!(out, "[{}]", flow.project_id);
            current_project = flow.project_id.as_str();
        }

        let crossing = if flow.project_count > 0 {
            format!(" crossing {} other project(s)", flow.project_count)
        } else {
            String::new()
        };

        let truncated = if flow.truncated { "  [reach truncated]" } else { "" };

        let _ = writeln!(out,
            "  {} ({}) -> {} symbols across {} files in {} app(s){crossing}, depth {}, criticality {:.2}{truncated}",
            flow.name,
            flow.entry_kind,
            flow.node_count,
            flow.file_count,
            flow.app_count,
            flow.depth_max,
            flow.criticality,
        );
    }
}

/// The reply when a flow query matches nothing, distinguishing "no flows
/// computed yet" from "flows exist but none matches this filter".
fn flows_empty_message(
    store: &Store,
    project_filter: Option<&str>,
    pattern: Option<&str>,
) -> Result<String, StoreError> {
    if !any_flows_computed(store)? {
        return Ok(
            "no execution flows computed (run `constellation flows` to trace them)".to_string()
        );
    }

    Ok(match (project_filter, pattern) {
        (_, Some(pattern)) if !pattern.is_empty() => format!("no flows matching {pattern:?}"),
        (Some(project), _) => format!("no flows for {project:?}"),
        (None, _) => "no flows match".to_string(),
    })
}

/// The flows a change participates in: the working-tree diff (or an explicit
/// file list) mapped to changed symbols, then to the flows whose reach set
/// contains them, ranked by criticality. The review question "what can this
/// break for a user", answered from the graph.
#[doc(hidden)]
pub fn affected_flows_text(
    store: &Store,
    base: Option<&str>,
    files: Option<&[String]>,
    limit: u32,
) -> Result<String, StoreError> {
    let (node_ids, source) = match files.filter(|files| !files.is_empty()) {
        Some(files) => (nodes_in_files(store, files)?, "the given files".to_string()),
        None => (changed_node_ids(store, base)?, format!("the diff against {}", base.unwrap_or("HEAD"))),
    };

    if node_ids.is_empty() {
        return Ok(format!("no indexed symbols in {source}"));
    }

    let flows = store.flows_for_nodes(&node_ids, limit)?;

    if flows.is_empty() {
        if !any_flows_computed(store)? {
            return Ok(
                "no execution flows computed (run `constellation flows` to trace them)".to_string()
            );
        }

        return Ok(format!(
            "no flows contain the {} symbol(s) in {source} \
             (the change sits outside every traced execution path)",
            node_ids.len(),
        ));
    }

    let mut out = format!(
        "flows affected by {source}: {} flow(s) over {} changed symbol(s), most critical first:\n",
        flows.len(),
        node_ids.len(),
    );

    render_flow_rows(&mut out, &flows);

    Ok(out)
}

/// The ids of every indexed symbol defined in the given files, across projects.
/// Paths are matched as constellation stores them, with separators normalized.
fn nodes_in_files(store: &Store, files: &[String]) -> Result<Vec<String>, StoreError> {
    let projects = store.all_projects()?;
    let mut ids: Vec<String> = Vec::new();

    for project in &projects {
        for file in files {
            let normalized = file.replace('\\', "/");

            for node in store.nodes_file_in(&project.id, &normalized)? {
                if ids.len() >= AFFECTED_FLOW_NODES_MAX {
                    return Ok(ids);
                }

                ids.push(node.id.as_str().to_string());
            }
        }
    }

    Ok(ids)
}

/// The ids of every symbol the working-tree diff against `base` touched, across
/// every indexed project, bounded so an enormous branch diff stays one query.
fn changed_node_ids(store: &Store, base: Option<&str>) -> Result<Vec<String>, StoreError> {
    let roots = project_roots(store)?;

    let mut project_ids: Vec<String> = roots.keys().cloned().collect();
    project_ids.sort_unstable();

    let mut ids: Vec<String> = Vec::new();

    for project_id in &project_ids {
        let root = roots.get(project_id).expect("every listed project has a root");
        let project = ProjectId::new(project_id.clone());

        for candidate in changed_candidates(store, &project, root, base)? {
            if ids.len() >= AFFECTED_FLOW_NODES_MAX {
                return Ok(ids);
            }

            ids.push(candidate.node.id.as_str().to_string());
        }
    }

    Ok(ids)
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
#[doc(hidden)]
pub fn flow_section(
    nodes: &[Node],
    out_edges: &[Vec<(u32, EdgeKind)>],
    seed_positions: &[usize],
    query: &str,
) -> String {
    let tokens = query_tokens(query);

    if tokens.len() < 2 {
        return String::new();
    }

    // Grouped by name, not deduped to one node per name: a query word like
    // `_modal_view` names a dozen definitions across apps, and the highest-ranked
    // one is rarely the one the sibling endpoint connects to. Keeping every
    // candidate turns "the first-ranked pair is unconnected" back into the question
    // actually asked, "is any definition of X connected to any definition of Y".
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();

    for &position in seed_positions {
        let name = nodes[position].name.to_lowercase();

        if !tokens.iter().any(|token| token == &name) {
            continue;
        }

        match groups.iter_mut().find(|(known, _)| known == &name) {
            Some((_, candidates)) => {
                if candidates.len() < FLOW_CANDIDATES_MAX {
                    candidates.push(position);
                }
            }
            None => {
                if groups.len() < FLOW_ENDPOINTS_MAX {
                    groups.push((name, vec![position]));
                }
            }
        }
    }

    if groups.len() < 2 {
        return String::new();
    }

    let mut out = String::new();
    let mut rendered: usize = 0;
    let mut asked: Vec<(usize, usize)> = Vec::new();

    for first in 0..groups.len() {
        for second in (first + 1)..groups.len() {
            if rendered >= FLOW_PATHS_MAX {
                break;
            }

            match connect_groups(nodes, out_edges, &groups[first].1, &groups[second].1) {
                PairOutcome::Connected(source, path) => {
                    render_flow_path(&mut out, nodes, source, &path);
                    rendered += 1;
                }
                PairOutcome::Unconnected => asked.push((first, second)),
                PairOutcome::Containment => {}
            }
        }
    }

    // Naming two symbols asks a question, and silence is not an answer to it: a
    // reader cannot tell an unconnected pair from a feature that failed to run.
    // Only reported when nothing connected; a partial answer is an answer, and
    // appending "no path" beside a rendered path reads as a contradiction.
    if out.is_empty() {
        if asked.is_empty() {
            return String::new();
        }

        let mut names: Vec<&str> = Vec::new();

        for (first, second) in &asked {
            for &group in &[*first, *second] {
                let name = nodes[groups[group].1[0]].name.as_str();

                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }

        let searched: usize = groups.iter().map(|(_, candidates)| candidates.len()).sum();
        let breadth = if searched > groups.len() {
            format!(" ({searched} definitions of those names searched)")
        } else {
            String::new()
        };

        return format!(
            "# flow: no call path among {} within {FLOW_HOPS_MAX} hops{breadth} (edges may be \
             missing for dynamic dispatch)\n\n",
            names.join(", "),
        );
    }

    format!("# flow: call paths among the named symbols:\n{out}\n")
}

/// The outcome of tracing one pair of named endpoints.
enum PairOutcome {
    /// A path, as the `(source, hops)` a renderer needs.
    Connected(usize, Vec<(usize, EdgeKind)>),
    /// A real question with no answer: some candidate pairing was two distinct
    /// symbols, and none of them connected. Worth reporting.
    Unconnected,
    /// The non-question outcome: every candidate pairing was a class and its own member.
    /// Containment carries no call path, so reporting one absent reads as a
    /// missing edge when nothing is missing.
    Containment,
}

/// The first connected pairing between two groups of same-named definitions. Every
/// candidate of the left name is tried against every candidate of the right, in both
/// directions, so one unconnected first-ranked definition never stands in for the
/// whole name. A member and the class that declares it are one symbol named twice
/// (`ProductionScheduleTargetQuerySet for_date`), not two endpoints with a question
/// between them, so containment pairs are skipped and reported as such.
fn connect_groups(
    nodes: &[Node],
    out_edges: &[Vec<(u32, EdgeKind)>],
    left: &[usize],
    right: &[usize],
) -> PairOutcome {
    let mut distinct = false;

    for &source in left {
        for &target in right {
            if is_owner_member(&nodes[source], &nodes[target]) {
                continue;
            }

            distinct = true;

            if let Some(path) = shortest_flow_path(out_edges, source, target) {
                return PairOutcome::Connected(source, path);
            }

            if let Some(path) = shortest_flow_path(out_edges, target, source) {
                return PairOutcome::Connected(target, path);
            }
        }
    }

    if distinct {
        PairOutcome::Unconnected
    } else {
        PairOutcome::Containment
    }
}

/// Whether one of two named symbols declares the other: a class and its method, a
/// module and the symbol inside it. Their qualified names nest
/// (`…::Owner` and `…::Owner.member`), which is containment, and containment
/// carries no call path to report on.
fn is_owner_member(left: &Node, right: &Node) -> bool {
    nests_within(&left.qualified_name, &right.qualified_name)
        || nests_within(&right.qualified_name, &left.qualified_name)
}

/// Whether `inner` is `outer` plus one more qualified segment.
fn nests_within(outer: &str, inner: &str) -> bool {
    !outer.is_empty()
        && inner.len() > outer.len()
        && inner.starts_with(outer)
        && inner[outer.len()..].starts_with('.')
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
pub(crate) fn render_flow_path(out: &mut String, nodes: &[Node], source: usize, path: &[(usize, EdgeKind)]) {
    let head = &nodes[source];

    let _ = writeln!(out, "  {} ({}:{})", head.name, head.file_path, head.span.start_line);

    for (node, kind) in path {
        let step = &nodes[*node];

        let _ = writeln!(out,
            "    →{}→ {} ({}:{})",
            kind.as_str(),
            step.name,
            step.file_path,
            step.span.start_line,
        );
    }

    out.push('\n');
}
