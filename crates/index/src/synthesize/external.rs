//! Stand-in nodes for definitions outside every indexed project.


use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{
    ImportMapping, UnresolvedRef,
};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::limits::SYNTHESIZED_EDGES_MAX;
use crate::paths::project_prefix;

/// The id-fragment marking an external template stub (`{% extends %}` into an
/// installed app), distinguishing it from an external symbol stub so cross-project
/// template redirects key off the right thing.
pub(crate) const EXTERNAL_TEMPLATE_MARKER: &str = "::external::template::";

/// The library-boundary layer, synthesized: turn references an in-project
/// resolution could not satisfy, but whose name is imported from a third-party
/// or stdlib module, into edges to deduplicated External nodes, so `extends`,
/// `decorates`, `calls`, and `imports` into libraries (django, django_spire,
/// decimal, …) become real edges instead of dead-ending at the boundary.
/// Re-derived from scratch each index. Returns the number of external edges.
pub(crate) fn synthesize_external(store: &Store, project: &ProjectId) -> Result<u32, IndexError> {
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
        EdgeKind::Imports
        | EdgeKind::Extends
        | EdgeKind::Decorates
        | EdgeKind::Calls
        | EdgeKind::Instantiates
        | EdgeKind::Returns
        | EdgeKind::TypeOf => {
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
