//! Benchmark for `resolve_reference` against a context populated with many
//! same-named symbols. Each lookup goes through `ResolutionContext::nodes_by_name`,
//! which returns an owned `Vec<Node>` (a full clone of every matching node) so
//! this bench measures the per-reference clone cost that the deferred
//! `Arc<Node>` / borrowed-handle trait change targets. Run it before and after
//! that change to confirm the win and guard against regression.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use constellation_graph::{
    EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{ImportMapping, ResolutionContext, UnresolvedRef, resolve_reference};

fn main() {
    divan::main();
}

/// How many same-named `save` methods the context holds, a realistic count for
/// a common method name across a large Django project.
const SAME_NAME_NODES: usize = 200;

/// A context whose `nodes_by_name` returns a large bucket of same-named method
/// nodes, each carrying realistic string fields so the clone copies real data:
/// the cost under test.
struct BenchContext {
    root: PathBuf,
    by_name: Vec<Arc<Node>>,
}

impl BenchContext {
    fn new(count: usize) -> Self {
        let project = ProjectId::new("bench");

        let by_name = (0..count)
            .map(|index| {
                let qualified = format!("app/module_{index}/services.py::Service{index}.save");

                let identity = NodeIdentity {
                    name: "save".to_string(),
                    qualified_name: qualified.clone(),
                    file_path: format!("app/module_{index}/services.py"),
                    language: Language::Python,
                };

                let mut node = Node::new(
                    NodeId::new(&project, &qualified),
                    project.clone(),
                    NodeKind::Method,
                    identity,
                    Span::new(10, 24, 4, 8),
                    0,
                );

                node.signature = Some("def save(self, *, commit: bool = True) -> None".to_string());
                node.docstring = Some("Persist the model instance to the database.".to_string());

                Arc::new(node)
            })
            .collect();

        Self { root: PathBuf::from("/tmp/bench"), by_name }
    }
}

impl ResolutionContext for BenchContext {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        if name == "save" { self.by_name.clone() } else { Vec::new() }
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

    fn file_exists(&self, _file_path: &str) -> bool {
        false
    }

    fn read_file(&self, _file_path: &str) -> Option<String> {
        None
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

#[divan::bench]
fn resolve_call_by_name(bencher: divan::Bencher) {
    let context = BenchContext::new(SAME_NAME_NODES);

    let reference = UnresolvedRef::new(
        NodeId::from_raw("bench::app/views.py::handler".to_string()),
        "save",
        EdgeKind::Calls,
        12,
        8,
        "app/views.py",
        Language::Python,
    );

    bencher
        .bench_local(|| resolve_reference(divan::black_box(&reference), divan::black_box(&context)));
}

/// The success path: the reference lives in the same file as one candidate, so
/// scoping keeps it and the call resolves, exercising `ResolvedRef` construction
/// (the per-resolved-edge cost). Measures the reference-clone removal.
#[divan::bench]
fn resolve_call_resolved(bencher: divan::Bencher) {
    let context = BenchContext::new(SAME_NAME_NODES);

    let reference = UnresolvedRef::new(
        NodeId::from_raw("bench::app/module_0/services.py::caller".to_string()),
        "save",
        EdgeKind::Calls,
        12,
        8,
        "app/module_0/services.py",
        Language::Python,
    );

    bencher
        .bench_local(|| resolve_reference(divan::black_box(&reference), divan::black_box(&context)));
}
