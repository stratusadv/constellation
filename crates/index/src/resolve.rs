//! The resolution pass: unresolved references become edges.
//!
//! Runs once the whole project is persisted, because a reference can only be
//! bound against every definition the project has. Two strategies, chosen by
//! size: an in-memory context for a project that fits, a store-backed one for
//! a project that does not.

use std::path::Path;

use constellation_graph::{Edge, EdgeKind, Node, NodeId, NodeKind, ProjectId};
use constellation_resolution::{
    FrameworkResolver, ResolutionContext, UnresolvedRef, edge_from_resolved, resolve_reference,
};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{IndexError, IndexStats};
use crate::context::{ProjectContext, StoreContext};
use crate::limits::{REFERENCE_COUNT_MAX, RESOLVE_BULK_NODES_MIN, RESOLVE_INCREMENTAL_RATIO};
use crate::paths::{
    module_of, namespace_chain, project_root_app_name, resolve_include_module, route_pattern,
    url_prefix_chain,
};
use crate::synthesize::events::synthesize_events;
use crate::synthesize::external::synthesize_external;
use crate::synthesize::overrides::synthesize_overrides;
use crate::synthesize::relations::synthesize_reverse_relations;
use crate::synthesize::templates::synthesize_template_members;

/// The project's references resolved and the derived edge layers
/// (events, reverse relations, external boundary), recording every count into
/// `stats`. Run only after extraction changed the graph.
pub(crate) fn run_resolution_phase(
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
    let routes = store.nodes_kind_in(project, NodeKind::Route)?;

    // The application namespace each urls module declares (`app_name = 'django_spire'`),
    // captured onto the variable node's signature, keyed by the module's file path.
    // Django folds this into the reverse name where an `include()` gives no explicit
    // namespace, and at the root urlconf it is the project-wide prefix.
    let app_name_by_module: FxHashMap<String, String> = store
        .nodes_kind_in(project, NodeKind::Variable)?
        .into_iter()
        .filter(|node| node.name == "app_name")
        .filter_map(|node| node.signature.map(|value| (module_of(&node.file_path), value)))
        .collect();

    let route_by_id: FxHashMap<&str, &Node> =
        routes.iter().map(|route| (route.id.as_str(), route)).collect();

    // The modules that define routes, so an include's dotted module string resolves
    // to the indexed file even when the package root is stripped from the project's
    // paths (`django_spire.ai.urls` indexed as `ai.urls`).
    let url_modules: FxHashSet<String> =
        routes.iter().map(|route| module_of(&route.file_path)).collect();

    // The include map: child url module -> (its namespace, the including module).
    // The namespace is the include's `namespace=` kwarg (on the route signature) or,
    // absent that, the included module's own app_name, Django's fallback.
    let mut includes: FxHashMap<String, (Option<String>, String)> = FxHashMap::default();

    // The same chain keyed by URL fragment rather than namespace: each include's own
    // pattern (`path('schedule/', include('...'))` -> `schedule/`), so a route's
    // declared fragment can be resolved to the path a request actually takes.
    let mut mounts: FxHashMap<String, (String, String)> = FxHashMap::default();

    for (_, reference) in &pending {
        if reference.reference_kind != EdgeKind::Imports {
            continue;
        }

        let Some(route) = route_by_id.get(reference.from_node_id.as_str()) else {
            continue;
        };

        let Some(child) = resolve_include_module(&reference.reference_name, &url_modules) else {
            continue;
        };

        let namespace = route.signature.clone().or_else(|| app_name_by_module.get(&child).cloned());
        let parent = module_of(&reference.file_path);

        mounts.insert(child.clone(), (route_pattern(&route.qualified_name).to_string(), parent.clone()));
        includes.insert(child, (namespace, parent));
    }

    // Each named route's full reverse name, plus a bare-name index for fallback.
    let mut by_reverse_name: FxHashMap<String, NodeId> = FxHashMap::default();
    let mut by_bare_name: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();
    let mut reverse_rows: Vec<(String, String)> = Vec::new();

    // The project's application namespace: the app_name of its root urlconf, the
    // uniquely-shallowest app_name module (django-spire's `urls.py` -> `django_spire`).
    // Django's root urlconf often includes its apps dynamically (a comprehension over
    // installed apps), so the chain cannot walk up to it; prepend it explicitly. A
    // project whose root declares no app_name (so the shallowest is not unique) gets
    // no prefix, the correct result for a top-level app's own routes.
    let root_app_name = project_root_app_name(&app_name_by_module);

    // Every route's mounted path, named or not: an unnamed route is still a URL a
    // request can reach, and the mount rows themselves are what the URL map needs in
    // order to tell an `include()` prefix from an endpoint.
    let mut url_path_rows: Vec<(String, String)> = Vec::with_capacity(routes.len());

    for route in &routes {
        let prefix = url_prefix_chain(&module_of(&route.file_path), &mounts);
        let full = format!("{prefix}{}", route_pattern(&route.qualified_name));

        url_path_rows.push((route.id.as_str().to_string(), full));
    }

    store.replace_route_url_paths(project, &url_path_rows)?;

    for route in &routes {
        // A bare-URL route (`page/`) has no `name=` and cannot be reversed.
        if route.name.contains('/') {
            continue;
        }

        by_bare_name.entry(route.name.as_str()).or_default().push(route);

        if let Some(mut chain) = namespace_chain(&module_of(&route.file_path), &includes, &app_name_by_module) {
            if let Some(root) = &root_app_name
                && chain.first() != Some(root)
            {
                chain.insert(0, root.clone());
            }

            let reverse_name = format!("{}:{}", chain.join(":"), route.name);

            by_reverse_name.insert(reverse_name.clone(), route.id.clone());
            reverse_rows.push((reverse_name, route.id.as_str().to_string()));
        }
    }

    // Persist this project's reverse names even when it has no namespaced reverse of
    // its own to resolve: another project's `{% url 'django_spire:...' %}` resolves
    // against them in the cross-project linker.
    store.replace_route_reverse_names(project, &reverse_rows)?;

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
