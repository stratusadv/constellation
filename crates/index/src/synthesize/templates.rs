//! Template variable access bound to the model member it names.


use constellation_graph::{Edge, EdgeKind, NodeId, NodeKind, ProjectId};
use constellation_resolution::{
    COLLECTION_CONTEXT, UnresolvedRef,
};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::limits::{MEMBER_CHAIN_WALK_MAX, SYNTHESIZED_EDGES_MAX, TEMPLATE_VIEW_WALK_MAX};

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
pub(crate) fn synthesize_template_members(
    store: &Store,
    project: &ProjectId,
) -> Result<u32, IndexError> {
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

        // `self` is not a local with a recorded type, it is the class the method
        // is defined on, so `self.entries.all()` inside a model method resolves
        // through the owner rather than through `instance_types`. Without this
        // every reverse-relation collection reached from a model's own methods
        // stayed dark: on one real portal that was 796 references, the single
        // largest unresolved name in the whole graph.
        let base_model = match instance_types.get(&(view.clone(), base_local.clone())) {
            Some(model) => model.clone(),
            None if base_local == "self" => {
                match enclosing_model(store, project, &reference.from_node_id)? {
                    Some(model) => model,
                    None => continue,
                }
            }
            None => continue,
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

/// The model a method belongs to, for resolving `self`.
///
/// The owner is read straight off the node id, whose tail is the extractor's
/// `Owner.member` qualified name, so no second query is needed. A plain function
/// has no owner in its tail and yields `None`, which is correct: `self` outside a
/// method resolves to nothing.
fn enclosing_model(
    store: &Store,
    project: &ProjectId,
    method: &NodeId,
) -> Result<Option<String>, IndexError> {
    let tail = method.as_str().rsplit("::").next().unwrap_or_default();

    let Some((owner, _member)) = tail.rsplit_once('.') else {
        return Ok(None);
    };

    model_node_in_project(store, project, owner)
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
