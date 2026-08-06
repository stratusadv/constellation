//! `constellation_feature`: every layer of one Django feature, from
//! route to template.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, Node, NodeId, NodeKind,
};
use constellation_store::{Store, StoreError};
use rustc_hash::FxHashSet;

use crate::limits::{FEATURE_DEPTH_MAX, FEATURE_NODES_MAX, FEATURE_SEED_DISAMBIGUATION_MAX};
use crate::render::node_line;
use crate::symbols::symbol_role;
use crate::tools::search::seed_nodes;

/// The labels for the feature-slice groups, indexed by [`feature_category`].
const FEATURE_LABELS: [&str; 7] = [
    "routes",
    "views",
    "templates",
    "models",
    "classes",
    "functions",
    "other",
];

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

/// Whether a plain `calls` target belongs in the slice: a view reaches its data
/// layer by calling a service, queryset, form, or model method, and dropping
/// every `calls` edge left the slice as routes and templates with no data layer
/// at all. Only role-carrying targets and models qualify, so a view's private
/// formatting helper still stays out. Followed one hop from the seed, never
/// recursed.
fn is_feature_call_target(node: &Node) -> bool {
    if node.kind == NodeKind::Model {
        return true;
    }

    matches!(
        symbol_role(node),
        Some("service" | "queryset" | "form" | "serializer" | "model"),
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
        let _ = writeln!(out, "  {}", node_line(node));
    }

    if seeds.len() > SHOWN_MAX {
        let _ = writeln!(out,
            "  (+{} more; `search` {symbol:?} to list all)",
            seeds.len() - SHOWN_MAX,
        );
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

    if seeds.len() > FEATURE_SEED_DISAMBIGUATION_MAX {
        return Ok(feature_disambiguation(symbol, &seeds));
    }

    let mut visited: FxHashSet<String> = seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut members: Vec<Node> = seeds.clone();
    // The third element says whether a plain `calls` edge from this node reaches
    // the feature's data layer. True for the seeds, and for the view a route
    // names: a route is pure indirection, so slicing from the route rather than
    // from its view must not lose the services that view calls.
    let mut frontier: Vec<(NodeId, u32, bool)> =
        seeds.iter().map(|node| (node.id.clone(), 0, true)).collect();

    while let Some((id, depth, data_layer_source)) = frontier.pop() {
        if members.len() >= FEATURE_NODES_MAX {
            break;
        }

        for (kind, node) in store.callees(&id)? {
            let structural = is_feature_downstream(kind);
            let data_layer =
                data_layer_source && kind == EdgeKind::Calls && is_feature_call_target(&node);

            if !(structural || data_layer) || !visited.insert(node.id.as_str().to_string()) {
                continue;
            }

            let next = node.id.clone();
            let routed_view = kind == EdgeKind::RoutesTo;

            members.push(node);

            if is_feature_chain(kind) && depth + 1 < FEATURE_DEPTH_MAX {
                frontier.push((next, depth + 1, routed_view));
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
            let _ = writeln!(out, "  {}:", FEATURE_LABELS[category as usize]);
            current = Some(category);
        }

        match symbol_role(node) {
            Some(role) => out.push_str(&format!("    {} [{role}]\n", node_line(node))),
            None => out.push_str(&format!("    {}\n", node_line(node))),
        }
    }

    Ok(out)
}
