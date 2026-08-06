//! The [`constellation_resolution::ResolutionContext`] implementations.
//!
//! Each answers the same questions (what is named here, what does this import
//! bind to) from a different source: an in-memory project, the store, the
//! filesystem, or the whole constellation. The resolver is written once
//! against the trait and works for all four.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use constellation_graph::{Language, Node, NodeKind, ProjectId};
use constellation_linking::LinkContext;
use constellation_resolution::{
    ImportMapping, ResolutionContext,
};
use constellation_store::Store;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::IndexError;
use crate::walk::to_u32;

/// A project's graph held in memory for bulk resolution: nodes plus name,
/// qualified-name, file, and kind indexes over them, so every lookup is a hash
/// map read with no store round-trip per reference.
pub(crate) struct ProjectContext {
    root: PathBuf,
    nodes: Vec<Arc<Node>>,
    by_name: FxHashMap<String, Vec<u32>>,
    by_lower_name: FxHashMap<String, Vec<u32>>,
    by_qualified_name: FxHashMap<String, Vec<u32>>,
    by_file: FxHashMap<String, Vec<u32>>,
    by_kind: FxHashMap<NodeKind, Vec<u32>>,
    mappings_by_file: FxHashMap<String, Vec<ImportMapping>>,
}

impl ProjectContext {
    pub(crate) fn load(
        store: &Store,
        project: &ProjectId,
        root: &Path,
    ) -> Result<Self, IndexError> {
        // Wrap each node in an `Arc` once at load. Every `nodes_by_*` lookup then
        // hands back reference-counted handles, so a name matching many nodes
        // clones counts instead of deep-copying each ~200-byte node.
        let nodes: Vec<Arc<Node>> =
            store.all_nodes(Some(project))?.into_iter().map(Arc::new).collect();

        assert!(nodes.len() <= u32::MAX as usize, "a project must hold fewer than u32::MAX nodes");

        let count = nodes.len();

        let mut by_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_lower_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_qualified_name: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_file: FxHashMap<String, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(count, Default::default());
        let mut by_kind: FxHashMap<NodeKind, Vec<u32>> = FxHashMap::default();

        for (index, node) in nodes.iter().enumerate() {
            let position = to_u32(index);

            by_name.entry(node.name.clone()).or_default().push(position);
            by_lower_name.entry(node.name.to_lowercase()).or_default().push(position);
            by_qualified_name.entry(node.qualified_name.clone()).or_default().push(position);
            by_file.entry(node.file_path.clone()).or_default().push(position);
            by_kind.entry(node.kind).or_default().push(position);
        }

        assert!(by_name.len() <= count, "names index at most one entry per node");

        let mut mappings_by_file: FxHashMap<String, Vec<ImportMapping>> = FxHashMap::default();

        for (file_path, mapping) in store.all_import_mappings(project)? {
            mappings_by_file.entry(file_path).or_default().push(mapping);
        }

        Ok(Self {
            root: root.to_path_buf(),
            nodes,
            by_name,
            by_lower_name,
            by_qualified_name,
            by_file,
            by_kind,
            mappings_by_file,
        })
    }

    pub(crate) fn collect(&self, indices: Option<&Vec<u32>>) -> Vec<Arc<Node>> {
        let Some(indices) = indices else {
            return Vec::new();
        };

        indices
            .iter()
            .map(|&index| {
                assert!((index as usize) < self.nodes.len(), "index points at a node");

                Arc::clone(&self.nodes[index as usize])
            })
            .collect()
    }
}

impl ResolutionContext for ProjectContext {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_name.get(name))
    }

    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_lower_name.get(lower_name))
    }

    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_qualified_name.get(qualified_name))
    }

    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>> {
        self.collect(self.by_kind.get(&kind))
    }

    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>> {
        self.collect(self.by_file.get(file_path))
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        self.by_file.keys().cloned().collect()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, file_path: &str, _language: Language) -> Vec<ImportMapping> {
        self.mappings_by_file.get(file_path).cloned().unwrap_or_default()
    }
}

/// A resolution context that answers each lookup with an indexed store query
/// instead of loading the whole project graph into memory. For incremental
/// re-resolution on a large project, where only a few references change and
/// materializing every node would dominate the cost. A failed query degrades to
/// an empty result (the reference simply stays pending), never a panic.
pub(crate) struct StoreContext<'store> {
    pub(crate) store: &'store Store,
    pub(crate) project: ProjectId,
    pub(crate) root: PathBuf,
}

impl ResolutionContext for StoreContext<'_> {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_named_in(&self.project, name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_lower_named_in(&self.project, lower_name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>> {
        self.store.nodes_qualified_in(&self.project, qualified_name).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>> {
        self.store.nodes_kind_in(&self.project, kind).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>> {
        self.store.nodes_file_in(&self.project, file_path).unwrap_or_default().into_iter().map(Arc::new).collect()
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        self.store.project_file_paths(&self.project).unwrap_or_default()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, file_path: &str, _language: Language) -> Vec<ImportMapping> {
        self.store.import_mappings_in(&self.project, file_path).unwrap_or_default()
    }
}

/// A filesystem-only resolution context for framework detection, run before
/// any nodes exist: graph lookups are empty, file access reads the repo root.
pub(crate) struct FsContext {
    root: PathBuf,
}

impl FsContext {
    pub(crate) fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }
}

impl ResolutionContext for FsContext {
    fn nodes_by_name(&self, _name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_lower_name(&self, _lower_name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_qualified_name(&self, _qualified_name: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_by_kind(&self, _kind: NodeKind) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn nodes_in_file(&self, _file_path: &str) -> Vec<Arc<Node>> {
        Vec::new()
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.root.join(file_path).is_file()
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(file_path)).ok()
    }

    fn all_files(&self) -> Vec<String> {
        Vec::new()
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn import_mappings(&self, _file_path: &str, _language: Language) -> Vec<ImportMapping> {
        Vec::new()
    }
}

/// The nodes of every project indexed by simple name, for the cross-project export
/// lookups [`ImportLinker`] makes.
pub(crate) struct ConstellationContext {
    by_name: FxHashMap<String, Vec<Arc<Node>>>,
    package_to_project: FxHashMap<String, String>,
}

impl ConstellationContext {
    /// The cross-project export index over `nodes`, excluding any node whose
    /// project is in `reference_only`: a reference-only version is queryable and
    /// links out, but its symbols are never cross-project link targets, so two
    /// indexed versions of one library cannot compete to win an ambiguous import.
    /// `package_to_project` maps an installed package name to the project indexed
    /// from it, backing the package-evidence link fallback.
    pub(crate) fn new(
        nodes: Vec<Node>,
        reference_only: &FxHashSet<String>,
        package_to_project: FxHashMap<String, String>,
    ) -> Self {
        let mut by_name: FxHashMap<String, Vec<Arc<Node>>> =
            FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());

        for node in nodes {
            if reference_only.contains(node.project_id.as_str()) {
                continue;
            }

            by_name.entry(node.name.clone()).or_default().push(Arc::new(node));
        }

        Self { by_name, package_to_project }
    }
}

impl LinkContext for ConstellationContext {
    fn exports_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.by_name.get(name).cloned().unwrap_or_default()
    }

    fn project_for_package(&self, package: &str) -> Option<&str> {
        self.package_to_project.get(package).map(String::as_str)
    }
}
