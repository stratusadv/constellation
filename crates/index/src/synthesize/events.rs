//! Signal senders bound to their receivers.


use constellation_graph::{Edge, EdgeKind, Language, Node, NodeId, NodeKind, ProjectId};
use constellation_resolution::EventRole;
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::limits::{EVENT_PAIRS_MAX, SYNTHESIZED_EDGES_MAX};
use crate::paths::project_prefix;

/// The dispatcher -> handler edges synthesized from a project's event records:
/// correlate dispatch sites and listener registrations by event name, resolve
/// each listener's handler to its JS function, and link every dispatcher of
/// that event to it. Replaces the project's prior synthesized edges (always
/// re-derived from scratch). Returns the number written.
pub(crate) fn synthesize_events(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
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

        if sites.len().saturating_mul(handler_names.len()) > EVENT_PAIRS_MAX {
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
