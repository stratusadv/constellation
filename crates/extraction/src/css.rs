use std::cell::RefCell;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId,
};
use rustc_hash::FxHashSet;
use tree_sitter::{Node as TsNode, Parser};

use crate::tsutil::{node_text, span_of};
use crate::{ExtractionOutput, Extractor};

/// A fail-fast bound on the walk loop.
const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single node.
const CHILDREN_MAX: u32 = 1_000_000;

/// The provenance tag on edges this extractor produces.
const PROVENANCE: &str = "extraction:css";

/// An extractor of CSS class and id selectors as [`Selector`](NodeKind::Selector)
/// nodes, so a template's `class="card"` can resolve to the `.card` rule.
pub struct CssExtractor;

thread_local! {
    /// The per-thread CSS parser, reused across files so each file pays only for
    /// its parse, not for parser construction. One parser per rayon worker thread,
    /// no cross-thread sharing.
    static PARSER: RefCell<Parser> = RefCell::new(new_parser());
}

/// A CSS parser with the grammar loaded. It panics only on a grammar against
/// tree-sitter ABI mismatch, a build error that cannot arise at runtime in a
/// correctly linked binary.
fn new_parser() -> Parser {
    let language: tree_sitter::Language = tree_sitter_css::LANGUAGE.into();

    assert!(language.node_kind_count() > 0, "css grammar must expose node kinds");

    let mut parser = Parser::new();

    parser
        .set_language(&language)
        .expect("the bundled css grammar is ABI-compatible with tree-sitter");

    parser
}

impl CssExtractor {
    /// The extractor; the grammar loads per worker thread on first use.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CssExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for CssExtractor {
    fn language(&self) -> Language {
        Language::Css
    }

    fn extract(&self, project: &ProjectId, file_path: &str, source: &str) -> ExtractionOutput {
        assert!(!file_path.is_empty(), "file_path must not be empty");

        let mut output = ExtractionOutput::empty();

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(source, None)) else {
            return output;
        };

        let bytes = source.as_bytes();
        let file_id = NodeId::new(project, file_path);

        output.nodes.push(file_node(project, file_path, &file_id, tree.root_node()));

        let mut seen: FxHashSet<String> = FxHashSet::default();
        let mut stack: Vec<TsNode> = vec![tree.root_node()];
        let mut iterations: u32 = 0;

        while let Some(node) = stack.pop() {
            iterations += 1;

            assert!(iterations <= WALK_ITERATIONS_MAX, "walk exceeded {WALK_ITERATIONS_MAX}");

            if let Some(selector) = selector_of(bytes, node)
                && let Some(built) = build_selector(project, file_path, node, selector, &mut seen)
            {
                let edge = Edge::new(file_id.clone(), built.id.clone(), EdgeKind::Contains)
                    .with_provenance(PROVENANCE);

                output.edges.push(edge);
                output.nodes.push(built);
            }

            push_named_children(node, &mut stack);
        }

        output
    }
}

/// The file node for a parsed stylesheet.
fn file_node(project: &ProjectId, file_path: &str, file_id: &NodeId, root: TsNode<'_>) -> Node {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);

    assert!(!name.is_empty(), "file node name must not be empty");

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: file_path.to_string(),
        file_path: file_path.to_string(),
        language: Language::Css,
    };

    Node::new(file_id.clone(), project.clone(), NodeKind::File, identity, span_of(root), 0)
}

/// A CSS selector's sigil (`.` for class, `#` for id) and bare name.
struct Selector {
    sigil: char,
    name: String,
}

/// The selector a node defines, if it is a class or id selector.
fn selector_of(bytes: &[u8], node: TsNode<'_>) -> Option<Selector> {
    let (child_kind, sigil) = match node.kind() {
        "class_selector" => ("class_name", '.'),
        "id_selector" => ("id_name", '#'),
        _ => return None,
    };

    let name = named_child_text(bytes, node, child_kind)?;
    let name = name.trim_start_matches(['.', '#']);

    if name.is_empty() {
        return None;
    }

    Some(Selector {
        sigil,
        name: name.to_string(),
    })
}

/// A [`Selector`](NodeKind::Selector) node, deduplicated per file by its
/// sigil-qualified name. The node's `name` is the bare identifier so a
/// template's `class="card"` matches by name.
fn build_selector(
    project: &ProjectId,
    file_path: &str,
    node: TsNode<'_>,
    selector: Selector,
    seen: &mut FxHashSet<String>,
) -> Option<Node> {
    assert!(!file_path.is_empty(), "file_path must not be empty");
    assert!(!selector.name.is_empty(), "selector name must not be empty");

    let qualified_name = format!("{file_path}::{}{}", selector.sigil, selector.name);

    if !seen.insert(qualified_name.clone()) {
        return None;
    }

    let identity = NodeIdentity {
        name: selector.name,
        qualified_name: qualified_name.clone(),
        file_path: file_path.to_string(),
        language: Language::Css,
    };

    let span = span_of(node);

    Some(Node::new(
        NodeId::new(project, &qualified_name),
        project.clone(),
        NodeKind::Selector,
        identity,
        span,
        0,
    ))
}

/// The text of the first named child of `node` with the given kind.
fn named_child_text<'bytes>(
    bytes: &'bytes [u8],
    node: TsNode<'_>,
    child_kind: &str,
) -> Option<&'bytes str> {
    let mut cursor = node.walk();

    node.named_children(&mut cursor)
        .find(|child| child.kind() == child_kind)
        .map(|child| node_text(bytes, child))
}

fn push_named_children<'tree>(node: TsNode<'tree>, stack: &mut Vec<TsNode<'tree>>) {
    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "child fan-out exceeded {CHILDREN_MAX}");

        stack.push(child);
    }
}
