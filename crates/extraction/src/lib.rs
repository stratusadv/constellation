#![forbid(unsafe_code)]

//! Source extraction: parse a target-stack file into graph nodes, the
//! structural edges known at parse time, and the unresolved references that
//! later become resolved edges. tree-sitter backs the language extractors;
//! Python is implemented here, with the front-end languages to follow.

use constellation_graph::{Edge, Language, Node, ProjectId};
use constellation_resolution::{EventRecord, ImportMapping, UnresolvedRef};

mod css;
mod django;
mod javascript;
mod jsexpr;
mod python;
mod template;
mod tsutil;

pub use css::CssExtractor;
pub use javascript::JavaScriptExtractor;
pub use python::PythonExtractor;
pub use template::TemplateExtractor;

/// A hard cap on the size of a single source file we will parse. Files larger
/// than this are skipped rather than parsed, bounding per-file work.
pub const SOURCE_BYTES_MAX: usize = 8 * 1024 * 1024;

/// A hard cap on the number of nodes a single file may contribute.
pub const NODES_PER_FILE_MAX: u32 = 100_000;

/// The product of extracting one file: nodes found, structural edges between
/// them, and the references awaiting resolution.
#[derive(Clone, Debug, Default)]
pub struct ExtractionOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub unresolved_refs: Vec<UnresolvedRef>,
    pub import_mappings: Vec<ImportMapping>,
    pub events: Vec<EventRecord>,
}

impl ExtractionOutput {
    /// An empty output, with the invariants asserted to hold.
    pub fn empty() -> Self {
        let output = Self::default();

        assert!(output.nodes.is_empty(), "empty output carries no nodes");
        assert!(output.unresolved_refs.is_empty(), "empty output carries no references");

        output
    }
}

/// A language-specific extractor that parses one file into graph nodes, edges,
/// and unresolved references.
///
/// One implementation exists per parsed language. `Sync` so the indexer can
/// run extraction across files in parallel over a shared extractor reference.
pub trait Extractor: Sync {
    /// The language this extractor handles.
    fn language(&self) -> Language;

    /// The nodes, edges, and references extracted from `source` at `file_path`.
    fn extract(&self, project: &ProjectId, file_path: &str, source: &str) -> ExtractionOutput;
}
