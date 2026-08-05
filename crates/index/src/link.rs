//! Linking one project's graph to another's.
//!
//! Runs after every project is indexed and resolved, and adds the edges that
//! cross a repository boundary: an import into a companion library, a route
//! reversed in another project, a method inherited from a third-party base.


use constellation_graph::{Edge, EdgeKind, Node, NodeId, NodeKind, ProjectId};
use constellation_linking::{ImportLinker, LinkContext, PendingImport, is_linkable, module_matches};
use constellation_resolution::{
    ImportMapping,
    MANAGER_SUFFIXES, QUERYSET_BUILTINS, QUERYSET_DISPATCH, RECEIVER_ROOT,
    SENTINEL_PREFIX, SUPER_DISPATCH, UnresolvedRef,
};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::context::ConstellationContext;
use crate::limits::{OVERRIDE_WALK_MAX, REFERENCE_COUNT_MAX};
use crate::paths::{class_name_of, file_stem_is, package_root_name, template_owner};
use crate::synthesize::external::EXTERNAL_TEMPLATE_MARKER;
use crate::synthesize::overrides::method_owner_id;

/// The whole constellation, linked: match every project's still-pending imports
/// against symbols exported by other projects, persist the matches as
/// cross-project edges, and clear the references they resolved. Returns the
/// number of edges written, cross-project links plus the inherited-method calls
/// that only become bindable once those links exist.
pub fn link_constellation(store: &Store) -> Result<u32, IndexError> {
    let nodes = store.all_nodes(None)?;

    let reference_only: FxHashSet<String> =
        store.reference_only_project_ids()?.into_iter().collect();

    // The package an import spells (`django_spire`) maps to the project indexed
    // from it (`django-spire`), keyed by the installed package directory name. Only
    // real projects are targets; a reference-only version shares the package name,
    // so including it would make the key ambiguous. Backs the package-evidence
    // fallback in both stub unification and import linking.
    let package_to_project: FxHashMap<String, String> = store
        .all_projects()?
        .into_iter()
        .filter(|project| !project.reference_only)
        .filter_map(|project| {
            package_root_name(&project.root_path)
                .map(|package| (package.to_string(), project.id.as_str().to_string()))
        })
        .collect();

    let redirects = external_redirects(&nodes, &reference_only, &package_to_project);
    let template_overrides = template_override_edges(&nodes, &reference_only);

    let context = ConstellationContext::new(nodes, &reference_only, package_to_project);
    let pending = store.load_unresolved(None)?;
    let linker = ImportLinker;

    // Every project's route reverse names, for resolving a `{% url 'django_spire:...' %}`
    // / reverse() into the route another project defines. Grouped by reverse name so
    // an ambiguous name (two projects own it) stays unlinked.
    let mut reverse_index: FxHashMap<String, Vec<(String, String)>> = FxHashMap::default();

    for (reverse_project, reverse_name, route_id) in store.route_reverse_names()? {
        reverse_index.entry(reverse_name).or_default().push((reverse_project, route_id));
    }

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
            EdgeKind::Handles | EdgeKind::UsesTag => cross_project_handler(reference, &context),
            EdgeKind::Resolves => cross_project_reverse(reference, &reverse_index),
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

    // Only now does every class point at its real base, including the ones a
    // companion package defines, so an inherited call can finally be bound.
    let inherited = link_inherited_methods(store)?;

    // Every resolution, linking, and synthesis pass has now emitted its edges. The
    // external-synthesis and synthesized-edge passes write in bulk and never delete
    // the reference rows they satisfy, so clear those now-resolved rows here, at the
    // one point where all edges exist: the pending table is left holding only
    // references that bind to nothing.
    store.delete_satisfied_unresolved()?;

    Ok(linked.saturating_add(inherited))
}

/// The calls bound to a method one of the caller's *ancestor* classes defines,
/// the resolution Python's own lookup performs and a per-class name match cannot.
/// Three reference shapes reach an inherited definition and no other pass can
/// bind them:
///
/// - `super().method()`, whose whole meaning is "skip this class". The extractor
///   marks it with [`SUPER_DISPATCH`] and the name resolver refuses it outright,
///   so an ancestor is the only target it can ever have.
/// - `self.method()` / `cls.method()` where the enclosing class inherits the
///   method rather than defining it, which instance-method resolution misses
///   because it looks up one exact `Class.method`.
/// - `Model.objects.method()` whose custom queryset inherits the method from a
///   shared base (`HistoryQuerySet.active`), which manager dispatch misses for
///   the same reason.
///
/// Runs last, from [`link_constellation`], because a base class usually lives in
/// a companion package and the `extends` edge only reaches it once
/// `Store::unify_externals` has collapsed the import stub onto the real
/// definition.
///
/// Ambiguity resolves to nothing, never to a guess: the walk takes the shallowest
/// ancestor depth that defines the name and binds only when that depth holds
/// exactly one definition, so a diamond whose two branches both define a method
/// stays pending.
fn link_inherited_methods(store: &Store) -> Result<u32, IndexError> {
    let pending = store.load_unresolved(None)?;

    if pending.is_empty() {
        return Ok(0);
    }

    let extends = store.extends_edges(None)?;
    let methods = store.class_methods(None)?;
    let classes = store.class_identities()?;
    let callables = store.callable_identities()?;
    let fields = store.field_relations()?;

    let mut imports: FxHashMap<(String, String), Vec<ImportMapping>> = FxHashMap::default();

    for project in store.all_projects()? {
        for (file_path, mapping) in store.all_import_mappings(&project.id)? {
            imports
                .entry((project.id.as_str().to_string(), file_path))
                .or_default()
                .push(mapping);
        }
    }

    let graph = ClassIndex::build(&extends, &methods, &classes, &callables, &fields);
    let reverse = reverse_accessor_map(&pending);

    let mut bound: Vec<(i64, Edge)> = Vec::new();
    let mut seen: u32 = 0;

    for (reference_id, reference) in &pending {
        seen += 1;

        assert!(seen <= REFERENCE_COUNT_MAX, "inheritance walk exceeded {REFERENCE_COUNT_MAX} refs");

        let target = inherited_call_target(reference, &graph)
            .or_else(|| receiver_typed_target(reference, &graph, &imports, &reverse));

        let Some(target) = target else {
            continue;
        };

        if target == reference.from_node_id.as_str() {
            continue;
        }

        let edge = Edge::new(
            reference.from_node_id.clone(),
            NodeId::from_raw(target.to_string()),
            EdgeKind::Calls,
        )
        .at(reference.line, reference.column)
        .with_provenance("resolution:inherited-method");

        bound.push((*reference_id, edge));
    }

    assert!(bound.len() <= pending.len(), "no more edges than pending references");

    Ok(store.commit_resolved(&bound)?)
}

/// The class hierarchy and callable pool an inheritance or receiver walk reads,
/// built once per link and borrowed by every lookup so no pass rebuilds it.
struct ClassIndex<'graph> {
    /// The base class ids of each subclass id.
    bases: FxHashMap<&'graph str, Vec<&'graph str>>,
    /// The method id for each (owning class id, method name) pair.
    by_owner: FxHashMap<(&'graph str, &'graph str), &'graph str>,
    /// The class id for each class qualified name.
    by_qualified: FxHashMap<&'graph str, &'graph str>,
    /// The class ids for each class simple name.
    by_name: FxHashMap<&'graph str, Vec<&'graph str>>,
    /// The (id, defining file path) for each callable simple name.
    callables: FxHashMap<&'graph str, Vec<(&'graph str, &'graph str)>>,
    /// The model each (owning model id, field name) pair relates to.
    field_types: FxHashMap<(&'graph str, &'graph str), &'graph str>,
}

/// The target a call whose receiver is a bare imported name resolves to, or
/// `None` when the receiver cannot be typed or more than one target answers.
///
/// Two receiver kinds carry hard evidence, both of them import bindings rather
/// than inference:
///
/// - a **module** (`portal_views.template_view()`): the file imported
///   `portal_views`, and exactly one callable of the called name is defined in a
///   file that both is named `portal_views` and matches the import's source
///   module. Nothing is guessed; the definition is in the module the receiver
///   names.
/// - a **class** (`AssetTypeChoices.to_glue_choices()`): the receiver names an
///   indexed class, and the method is that class's own or a single ancestor's.
///
/// Generic name resolution has already had its chance at these references and
/// declined, which is why they are still pending: it binds a cross-file name only
/// when the file imports *that name*, and here the file imports the receiver
/// instead.
fn receiver_typed_target<'graph>(
    reference: &UnresolvedRef,
    graph: &ClassIndex<'graph>,
    imports: &FxHashMap<(String, String), Vec<ImportMapping>>,
    reverse: &FxHashMap<(&str, &str), &'graph str>,
) -> Option<&'graph str> {
    if reference.reference_kind != EdgeKind::Calls {
        return None;
    }

    let mut candidates = reference.candidates.iter();

    if candidates.next().map(String::as_str) != Some(RECEIVER_ROOT) {
        return None;
    }

    let receiver = candidates.next()?;
    let name = reference.reference_name.as_str();

    assert!(!receiver.is_empty(), "a receiver-typed reference carries its receiver");

    if let Some((root, attribute)) = receiver.split_once('.') {
        return relation_call_target(reference, root, attribute, graph, reverse);
    }

    let project = reference.from_node_id.project_prefix().to_string();
    let bindings = imports.get(&(project, reference.file_path.clone()))?;
    let binding = bindings.iter().find(|mapping| mapping.local_name == *receiver)?;

    assert!(!binding.source.is_empty(), "an import binding names its source module");

    // The receiver names a class the constellation indexes: the method is its own
    // or a single ancestor's.
    if let Some(class) = sole_class_named(receiver, graph) {
        return graph
            .by_owner
            .get(&(class, name))
            .copied()
            .or_else(|| sole_inherited_method(class, name, &graph.bases, &graph.by_owner));
    }

    // The receiver names a module: take the callable of that name defined in it.
    // The import names the *package* the module sits in, so the module the
    // receiver stands for is that source plus the receiver itself
    // (`django_spire.contrib.generic_views` + `portal_views`); comparing the bare
    // source against the file would never match, since the file path carries the
    // module name too.
    let mut module = String::with_capacity(binding.source.len() + receiver.len() + 1);
    module.push_str(&binding.source);
    module.push('.');
    module.push_str(receiver);

    let mut found: Option<&'graph str> = None;

    for (id, file_path) in graph.callables.get(name).into_iter().flatten() {
        if !file_stem_is(file_path, receiver) || !module_matches(&module, file_path) {
            continue;
        }

        match found {
            None => found = Some(id),
            Some(previous) if previous != *id => return None,
            _ => {}
        }
    }

    found
}

/// The target of a call made through a two-part receiver, `<root>.<attribute>`.
///
/// The root is typed one of two ways, and only these two: `self`/`cls`, whose
/// model the enclosing method already names, or a local the source annotated,
/// whose type the extractor carried along. The attribute is then a relation on
/// that model, either a field it declares (`self.locations` on `HarvestLoad` ->
/// `Location`) or a `related_name` another model points back with
/// (`company.contacts` -> `CompanyContact`). The called name is looked up on the
/// related model and its queryset.
///
/// An untyped local root stays unresolved: nothing in the graph says what it
/// holds, and a name lookup would be a guess.
fn relation_call_target<'graph>(
    reference: &UnresolvedRef,
    root: &str,
    attribute: &str,
    graph: &ClassIndex<'graph>,
    reverse: &FxHashMap<(&str, &str), &'graph str>,
) -> Option<&'graph str> {
    assert!(!attribute.is_empty(), "a two-part receiver names its attribute");

    let (owner_class, model) = if matches!(root, "self" | "cls") {
        let owner = method_owner_id(reference.from_node_id.as_str())?;

        (owner, class_name_of(owner))
    } else {
        // The annotated type of the root, carried past the receiver text.
        let model = reference.candidates.get(2).map(String::as_str)?;

        (sole_class_named(model, graph)?, model)
    };

    let related = graph
        .field_types
        .get(&(owner_class, attribute))
        .copied()
        .or_else(|| reverse.get(&(model, attribute)).copied())?;

    graph.model_method(related, &reference.reference_name)
}

/// The `(model name, accessor) -> related model name` map Django's
/// `related_name=` sets up, read from the reverse-accessor references the
/// extractor recorded and the template synthesis also consumes. An accessor two
/// models claim on the same name maps to neither.
fn reverse_accessor_map(pending: &[(i64, UnresolvedRef)]) -> FxHashMap<(&str, &str), &str> {
    let mut claims: FxHashMap<(&str, &str), Option<&str>> = FxHashMap::default();

    for (_id, reference) in pending {
        if reference.reference_kind != EdgeKind::ReverseAccessor {
            continue;
        }

        let Some(accessor) = reference.candidates.first() else {
            continue;
        };

        // The reference runs to the target model it is accessed from; the model
        // the accessor yields is the one that declared the relation.
        let source = reference.from_node_id.as_str();
        let owner = source.rsplit("::").next().unwrap_or(source);

        claims
            .entry((reference.reference_name.as_str(), accessor.as_str()))
            .and_modify(|held| {
                if *held != Some(owner) {
                    *held = None;
                }
            })
            .or_insert(Some(owner));
    }

    claims.into_iter().filter_map(|(key, owner)| owner.map(|owner| (key, owner))).collect()
}

/// The id of the sole class named `name`, or `None` when the constellation holds
/// none or several: an ambiguous class name types no receiver.
fn sole_class_named<'graph>(name: &str, graph: &ClassIndex<'graph>) -> Option<&'graph str> {
    let classes = graph.by_name.get(name)?;

    match classes.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// The single ancestor-defined method a pending call binds to, or `None` when the
/// reference is not one of the inherited shapes or no unambiguous ancestor
/// defines the name.
fn inherited_call_target<'graph>(
    reference: &UnresolvedRef,
    graph: &ClassIndex<'graph>,
) -> Option<&'graph str> {
    let (bases, by_owner) = (&graph.bases, &graph.by_owner);
    let (by_qualified, by_name) = (&graph.by_qualified, &graph.by_name);

    if reference.reference_kind != EdgeKind::Calls {
        return None;
    }

    let name = reference.reference_name.as_str();

    assert!(!name.is_empty(), "a call reference names a method");

    let sentinel = reference.candidates.first().map(String::as_str);
    let trailing = reference.candidates.iter().find(|candidate| {
        !candidate.is_empty() && !candidate.starts_with(SENTINEL_PREFIX)
    })?;

    match sentinel {
        Some(SUPER_DISPATCH) => {
            let class = by_qualified.get(trailing.as_str())?;

            sole_inherited_method(class, name, bases, by_owner)
        }
        Some(QUERYSET_DISPATCH) => {
            if QUERYSET_BUILTINS.contains(&name) {
                return None;
            }

            sole_manager_method(trailing, name, bases, by_owner, by_name)
        }
        Some(candidate) if !candidate.starts_with(SENTINEL_PREFIX) => {
            let class = by_qualified.get(candidate)?;

            // A method the class defines itself was instance-method resolution's
            // to bind; only an inherited one is left for this pass.
            if by_owner.contains_key(&(*class, name)) {
                return None;
            }

            sole_inherited_method(class, name, bases, by_owner)
        }
        _ => None,
    }
}

/// The sole method named `name` reachable from the queryset and manager classes
/// Django's convention gives `model`, taking a class's own definition over an
/// inherited one. `None` when the model names no such class, or when two of them
/// reach different definitions.
fn sole_manager_method<'graph>(
    model: &str,
    name: &str,
    bases: &FxHashMap<&'graph str, Vec<&'graph str>>,
    by_owner: &FxHashMap<(&'graph str, &'graph str), &'graph str>,
    by_name: &FxHashMap<&str, Vec<&'graph str>>,
) -> Option<&'graph str> {
    assert!(!model.is_empty(), "a dispatch reference names a model");

    let mut owner_name = String::new();
    let mut found: Option<&'graph str> = None;

    for suffix in MANAGER_SUFFIXES {
        owner_name.clear();
        owner_name.push_str(model);
        owner_name.push_str(suffix);

        for owner in by_name.get(owner_name.as_str()).into_iter().flatten() {
            let target = by_owner
                .get(&(*owner, name))
                .copied()
                .or_else(|| sole_inherited_method(owner, name, bases, by_owner));

            match (target, found) {
                (Some(target), None) => found = Some(target),
                (Some(target), Some(previous)) if target != previous => return None,
                _ => {}
            }
        }
    }

    found
}

/// The id of the method named `name` on the shallowest ancestor depth of `class`
/// that defines it. `None` when no ancestor defines it, or when that depth holds
/// more than one definition: an ambiguous diamond binds to neither branch rather
/// than to whichever the walk reached first. A visited set and a hard hop bound
/// make a cyclic hierarchy terminate.
fn sole_inherited_method<'graph>(
    class: &str,
    name: &str,
    bases: &FxHashMap<&'graph str, Vec<&'graph str>>,
    by_owner: &FxHashMap<(&'graph str, &'graph str), &'graph str>,
) -> Option<&'graph str> {
    let mut level: Vec<&'graph str> = bases.get(class).cloned()?;
    let mut visited: FxHashSet<&str> = FxHashSet::default();
    let mut next: Vec<&'graph str> = Vec::new();
    let mut hops: u32 = 0;

    visited.insert(class);

    while !level.is_empty() {
        hops += 1;

        assert!(hops <= OVERRIDE_WALK_MAX, "inheritance walk exceeded {OVERRIDE_WALK_MAX} hops");

        let mut found: Option<&'graph str> = None;

        for ancestor in &level {
            if !visited.insert(ancestor) {
                continue;
            }

            match (by_owner.get(&(*ancestor, name)).copied(), found) {
                (Some(method), None) => found = Some(method),
                (Some(method), Some(previous)) if method != previous => return None,
                _ => {}
            }

            if let Some(above) = bases.get(ancestor) {
                next.extend_from_slice(above);
            }
        }

        if found.is_some() {
            return found;
        }

        std::mem::swap(&mut level, &mut next);
        next.clear();
    }

    None
}

/// The cross-project template overrides: a workspace's vendored copy of a namespaced
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
/// e.g. a workspace handler on django-spire's `AuthUser`). An ambiguous name (defined
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

/// A leftover Alpine `Handles` reference linked to the sole function/method of that
/// name in another project: a template's `@click="close_modal()"` whose handler is
/// an `x-data` method or `Alpine.data` function defined in an installed app
/// (django-spire's modal component), which per-project resolution cannot see across
/// the boundary. An ambiguous name (defined in more than one other project) stays
/// unlinked, the same no-false-edge discipline the import and relation links keep.
fn cross_project_handler(reference: &UnresolvedRef, context: &dyn LinkContext) -> Option<Edge> {
    let project = reference.from_node_id.project_prefix();

    let mut matched = context
        .exports_by_name(&reference.reference_name)
        .into_iter()
        .filter(|node| {
            node.project_id.as_str() != project
                && matches!(node.kind, NodeKind::Function | NodeKind::Method)
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

/// A leftover namespaced reverse (`{% url 'django_spire:auth:user:page:detail' %}`,
/// `reverse('django_spire:...')`) linked to the route another project defines, found
/// by exact reverse name in `reverse_index`. Within-project reverses resolved during
/// that project's own pass, so a still-pending namespaced reverse names a route across
/// the boundary. An ambiguous name (owned by more than one other project) stays
/// unlinked, the same no-false-edge discipline the other cross-project links keep.
fn cross_project_reverse(
    reference: &UnresolvedRef,
    reverse_index: &FxHashMap<String, Vec<(String, String)>>,
) -> Option<Edge> {
    let project = reference.from_node_id.project_prefix();

    let targets = reverse_index.get(&reference.reference_name)?;

    let mut cross = targets.iter().filter(|(owner, _)| owner.as_str() != project);

    let (Some((owner, route_id)), None) = (cross.next(), cross.next()) else {
        return None;
    };

    let provenance = format!("link:{}->{}", project, owner);

    Some(
        Edge::new(reference.from_node_id.clone(), NodeId::from_raw(route_id.clone()), EdgeKind::Resolves)
            .at(reference.line, reference.column)
            .with_provenance(provenance),
    )
}

/// The map from each external stub to the single real cross-project definition it shadows.
/// A stub `django_spire.history.mixins.HistoryModelMixin` matches a non-external,
/// linkable definition of the same simple name in another project whose file path
/// agrees with the stub's module, the same module-path evidence the import linker
/// requires. Failing that, the stub module's top-level package names a companion
/// project directly (`package_to_project`), scoping to a sole definition there, so a
/// re-exported symbol's non-import edges (instantiations, annotations) collapse onto
/// the real definition just as its import links. An ambiguous stub (two definitions
/// after scoping) is left alone.
fn external_redirects(
    nodes: &[Node],
    reference_only: &FxHashSet<String>,
    package_to_project: &FxHashMap<String, String>,
) -> Vec<(NodeId, NodeId)> {
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

            continue;
        }

        let package = module.split('.').next().unwrap_or("");

        if let Some(project) = package_to_project.get(package) {
            let mut scoped = candidates
                .iter()
                .filter(|node| node.project_id != stub.project_id && node.project_id.as_str() == project);

            if let (Some(definition), None) = (scoped.next(), scoped.next()) {
                redirects.push((stub.id.clone(), definition.id.clone()));
            }
        }
    }

    redirects
}

/// The real template `stub` should redirect to among `candidates`: templates of
/// the same name in any project. The sole other-project template wins outright;
/// when several projects own the name (a workspace that vendored a copy of a
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

impl<'graph> ClassIndex<'graph> {
    pub(crate) fn build(
        extends: &'graph [(String, String)],
        methods: &'graph [(String, String)],
        classes: &'graph [(String, String, String)],
        callables: &'graph [(String, String, String)],
        fields: &'graph [(String, String)],
    ) -> Self {
        let mut bases: FxHashMap<&str, Vec<&str>> = FxHashMap::default();

        for (subclass, base) in extends {
            bases.entry(subclass.as_str()).or_default().push(base.as_str());
        }

        let mut by_owner: FxHashMap<(&str, &str), &str> = FxHashMap::default();

        for (id, name) in methods {
            if let Some(owner) = method_owner_id(id) {
                by_owner.insert((owner, name.as_str()), id.as_str());
            }
        }

        let mut by_qualified: FxHashMap<&str, &str> = FxHashMap::default();
        let mut by_name: FxHashMap<&str, Vec<&str>> = FxHashMap::default();

        for (id, qualified_name, name) in classes {
            by_qualified.insert(qualified_name.as_str(), id.as_str());
            by_name.entry(name.as_str()).or_default().push(id.as_str());
        }

        let mut by_callable: FxHashMap<&str, Vec<(&str, &str)>> = FxHashMap::default();

        for (id, name, file_path) in callables {
            by_callable.entry(name.as_str()).or_default().push((id.as_str(), file_path.as_str()));
        }

        let mut field_types: FxHashMap<(&str, &str), &str> = FxHashMap::default();

        for (id, related) in fields {
            if let Some((owner, field)) = id.rsplit_once('.') {
                field_types.insert((owner, field), related.as_str());
            }
        }

        Self { bases, by_owner, by_qualified, by_name, callables: by_callable, field_types }
    }

    /// The sole method named `name` callable on `model`: the model's own method,
    /// or one on the single queryset or manager class Django's naming convention
    /// gives it (its own or inherited). `None` when nothing or more than one
    /// definition answers.
    pub(crate) fn model_method(&self, model: &str, name: &str) -> Option<&'graph str> {
        let class = sole_class_named(model, self)?;

        if let Some(method) = self.by_owner.get(&(class, name)) {
            return Some(method);
        }

        if let Some(method) = sole_inherited_method(class, name, &self.bases, &self.by_owner) {
            return Some(method);
        }

        sole_manager_method(model, name, &self.bases, &self.by_owner, &self.by_name)
    }
}
