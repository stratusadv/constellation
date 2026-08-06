//! Method overrides bound to the base method they replace.


use constellation_graph::{Edge, EdgeKind, NodeId, ProjectId};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::limits::{OVERRIDE_WALK_MAX, SYNTHESIZED_EDGES_MAX};

/// An `Overrides` edge synthesized for each method that redefines a same-named
/// method on an ancestor class. Walks the in-project class hierarchy (resolved
/// `extends` edges) up from each method's owning class to the nearest ancestor
/// that defines the method, and links the override to it: the "what does this
/// override" / "what overrides this base method" navigation a forward call graph
/// hides. Scoped to in-project methods and re-derived each index, like the other
/// synthesis passes; an external base contributes no method to bind under.
pub(crate) fn synthesize_overrides(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
    let extends = store.extends_edges(Some(project))?;
    let methods = store.class_methods(Some(project))?;

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
pub(crate) fn method_owner_id(method_id: &str) -> Option<&str> {
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
    let mut frontier: Vec<&'graph str> = bases.get(owner)?.clone();

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
