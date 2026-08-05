#![forbid(unsafe_code)]

//! Cross-project linking, constellation's reason to exist. After each project
//! resolves its own references, the imports that pointed outside it remain
//! pending. The linker matches those pending imports against symbols exported
//! by *other* projects and emits the edges that span them. Those cross-project
//! edges are the constellation's connective tissue.

use std::sync::Arc;

use constellation_graph::{
    Edge, EdgeKind, Node, NodeId, NodeKind, ProjectId, is_generated_path,
};

/// An import left unresolved within its own project, a candidate for linking
/// to a symbol in another project.
#[derive(Clone, Debug)]
pub struct PendingImport {
    pub project_id: ProjectId,
    pub from_node_id: NodeId,
    pub reference_name: String,
    pub module: String,
    pub line: u32,
    pub column: u32,
}

/// An inferred edge that crosses a project boundary, with the confidence of the
/// inference. The wrapped edge's endpoints are guaranteed to live in different
/// projects.
#[derive(Clone, Debug)]
pub struct ProjectLink {
    pub edge: Edge,
    pub confidence: f32,
}

impl ProjectLink {
    /// A project link, asserting the edge crosses a project boundary and
    /// the confidence is in `[0.0, 1.0]`.
    pub fn new(edge: Edge, confidence: f32) -> Self {
        assert!(edge.is_cross_project(), "a project link must cross a project boundary");
        assert!(confidence >= 0.0, "confidence must be non-negative");
        assert!(confidence <= 1.0, "confidence must not exceed one");

        Self { edge, confidence }
    }
}

/// The cross-project symbol view a linker queries: exported symbols by simple
/// name across every indexed project at once.
pub trait LinkContext {
    /// The nodes with the given simple name, across all indexed projects.
    fn exports_by_name(&self, name: &str) -> Vec<Arc<Node>>;

    /// The project a top-level import package resolves to, mapping the installed
    /// package name an import spells (`django_spire`) to the companion project
    /// indexed from it (`django-spire`), or `None` when no indexed project owns
    /// that package. Lets a re-exported or package-rooted import link to the right
    /// project when the defining file path alone gives no module-path evidence.
    fn project_for_package(&self, package: &str) -> Option<&str>;
}

/// The linker that ties pending imports to symbols exported by other projects.
pub struct ImportLinker;

impl ImportLinker {
    /// The cross-project link for one pending import, or `None` when none is
    /// confident. A module-path match wins outright; failing that, a single
    /// candidate across all other projects links at lower confidence; anything
    /// more is too ambiguous and stays unlinked.
    pub fn link(&self, pending: &PendingImport, context: &dyn LinkContext) -> Option<ProjectLink> {
        assert!(!pending.reference_name.is_empty(), "pending import name must not be empty");

        let mut candidates: Vec<Arc<Node>> = context
            .exports_by_name(&pending.reference_name)
            .into_iter()
            .filter(|node| {
                // A `path` symbol parsed out of a bundled `Chart.min.js` must
                // never become the link target for `from django.urls import path`.
                node.project_id != pending.project_id
                    && is_linkable(node.kind)
                    && !is_generated_path(&node.file_path)
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Require module-path evidence: the import's dotted module must agree with
        // the defining file's path. A bare same-name match with no path evidence is
        // almost always a collision between a third-party import both projects share
        // (`from django.urls import path`, `from django_spire… import HistoryQuerySet`)
        // and an unrelated first-party symbol; linking those manufactures false
        // cross-project dependencies, the worst failure for this tool. No evidence,
        // no link.
        if pending.module.is_empty() {
            return None;
        }

        if let Some(index) =
            candidates.iter().position(|node| module_matches(&pending.module, &node.file_path))
        {
            assert!(index < candidates.len(), "matched index stays within candidates");

            let node = candidates.swap_remove(index);

            return Some(make_link(pending, &node, 0.85));
        }

        // Without a defining-file path match, fall back to package-to-project
        // evidence: the import's top-level package names a companion project
        // directly (`django_spire.contrib.seeding` -> the django-spire project). A
        // re-export (the symbol re-exported from a module deeper than the imported
        // package) or a stripped package root leaves no path-suffix overlap for
        // `module_matches`, yet the package identity still pins the project. Scope
        // candidates to that project and link a sole export; more than one stays
        // unlinked, the same no-false-edge discipline the path match keeps.
        let package = pending.module.split('.').next().unwrap_or("");

        if !package.is_empty()
            && let Some(project) = context.project_for_package(package)
        {
            candidates.retain(|node| node.project_id.as_str() == project);

            if candidates.len() == 1 {
                let node = candidates.swap_remove(0);

                return Some(make_link(pending, &node, 0.8));
            }
        }

        None
    }
}

/// The cross-project import edge for a matched pending import.
fn make_link(pending: &PendingImport, target: &Node, confidence: f32) -> ProjectLink {
    assert!(confidence >= 0.0, "confidence must be non-negative");
    assert!(confidence <= 1.0, "confidence must not exceed one");

    let provenance = format!("link:{}->{}", pending.project_id, target.project_id);

    let edge = Edge::new(pending.from_node_id.clone(), target.id.clone(), EdgeKind::Imports)
        .at(pending.line, pending.column)
        .with_provenance(provenance);

    ProjectLink::new(edge, confidence)
}

/// Whether a node kind is something another project would import.
pub fn is_linkable(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Constant
            | NodeKind::File
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Model
            | NodeKind::Variable
            | NodeKind::View
    )
}

/// Whether an import's module path agrees with a defining file's path. Both are
/// reduced to dotted module form and compared as suffixes, so `src/` and other
/// roots that differ between projects do not defeat the match.
pub fn module_matches(module: &str, file_path: &str) -> bool {
    assert!(!module.is_empty(), "module path must not be empty");

    let file_module = dotted_module(file_path);

    !file_module.is_empty() && (file_module.ends_with(module) || module.ends_with(&file_module))
}

/// A file path reduced to a dotted module path: `src/app/models.py` ->
/// `src.app.models`, with a trailing `__init__` dropped.
fn dotted_module(file_path: &str) -> String {
    let without_extension = file_path.strip_suffix(".py").unwrap_or(file_path);
    let without_init = without_extension.strip_suffix("/__init__").unwrap_or(without_extension);

    let dotted = without_init.replace('/', ".");

    assert!(dotted.len() <= file_path.len(), "dotted module is no longer than the path");

    dotted
}
