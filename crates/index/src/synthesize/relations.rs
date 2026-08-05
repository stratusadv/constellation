//! The reverse side of each model relation.


use constellation_graph::{Edge, EdgeKind, NodeId, ProjectId};
use constellation_store::Store;
use rustc_hash::FxHashSet;

use crate::IndexError;
use crate::limits::SYNTHESIZED_EDGES_MAX;
use crate::paths::project_prefix;

/// The reverse direction of each model relation, synthesized. A `relates_to` from a
/// model with a foreign key / M2M / O2O to its target always implies a reverse
/// accessor on the target (`author.article_set`, or a `related_name`), so the
/// target model relates back to the source. Emitting the reverse edge lets
/// `callees`/`constellation_model` on the target surface the models that point at
/// it: the "what relates to this model" navigation Django's reverse accessors
/// give but a forward-only graph hides.
/// Scoped to relations whose both endpoints are in `project` so each re-index can
/// re-derive them idempotently; the forward set already excludes prior reverses.
pub(crate) fn synthesize_reverse_relations(
    store: &Store,
    project: &ProjectId,
) -> Result<u32, IndexError> {
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
