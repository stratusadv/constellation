use constellation_graph::{Language, Node, ProjectId};

use crate::context::ResolutionContext;
use crate::refs::{ResolvedRef, UnresolvedRef};

/// The nodes and references a framework resolver synthesizes from a file beyond
/// what generic extraction sees (for Django: route nodes and the references
/// that link them to their view handlers).
#[derive(Clone, Debug, Default)]
pub struct FrameworkExtractionResult {
    pub nodes: Vec<Node>,
    pub references: Vec<UnresolvedRef>,
}

impl FrameworkExtractionResult {
    /// An empty result with no nodes or references.
    pub fn empty() -> Self {
        let result = Self::default();

        assert!(result.nodes.is_empty(), "empty result carries no nodes");
        assert!(result.references.is_empty(), "empty result carries no references");

        result
    }
}

/// A framework-specific resolver. `detect` runs once per project; the
/// remaining hooks participate in extraction and resolution. Every hook past
/// the first three has a no-op default so a resolver implements only what it
/// needs.
pub trait FrameworkResolver: Sync {
    /// The resolver's stable identifier, recorded in edge provenance.
    fn name(&self) -> &str;

    /// The languages this resolver participates in; a reference in any other
    /// language skips it.
    fn languages(&self) -> &[Language];

    /// Whether this resolver applies to the project, decided once per project
    /// from marker files and layout.
    fn detect(&self, context: &dyn ResolutionContext) -> bool;

    /// The target node one reference resolves to, or `None` when this
    /// resolver cannot confidently bind it.
    fn resolve(
        &self,
        reference: &UnresolvedRef,
        context: &dyn ResolutionContext,
    ) -> Option<ResolvedRef>;

    /// Whether to opt a reference name past the name-exists pre-filter even when no symbol
    /// declares it, needed for dynamic dispatch where the target is an
    /// attribute rather than a declared node (e.g. Django's `_iterable_class`).
    fn claims_reference(&self, _name: &str) -> bool {
        false
    }

    /// The framework-specific nodes and references synthesized from one file.
    fn extract(
        &self,
        _project: &ProjectId,
        _file_path: &str,
        _content: &str,
    ) -> FrameworkExtractionResult {
        FrameworkExtractionResult::empty()
    }

    /// The cross-file finalization run once after all per-file extraction
    /// completes, for symbols whose final form depends on a sibling file.
    fn post_extract(&self, _context: &dyn ResolutionContext) -> Vec<Node> {
        Vec::new()
    }
}
