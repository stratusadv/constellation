use std::cell::RefCell;
use std::rc::Rc;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId,
};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, UnresolvedRef};
use rustc_hash::FxHashSet;
use tree_sitter::{Node as TsNode, Parser};

use crate::jsexpr::{callee_name, identifier_argument, string_argument, string_literal};
use crate::tsutil::{line_1based, node_text, span_of, to_u32};
use crate::{ExtractionOutput, Extractor};

/// A fail-fast bound on the walk loop.
const WALK_ITERATIONS_MAX: u32 = 5_000_000;

/// A fail-fast bound on the fan-out examined at a single node.
const CHILDREN_MAX: u32 = 1_000_000;

/// The provenance tag on edges this extractor produces.
const PROVENANCE: &str = "extraction:javascript";

/// The JavaScript node types whose value makes a binding a function rather than a
/// plain variable (covers Alpine component factories assigned to consts).
const FUNCTION_VALUES: &[&str] = &[
    "arrow_function",
    "function",
    "function_expression",
    "generator_function",
];

/// The Alpine global on which component/store registrations are made.
const ALPINE_OBJECT: &str = "Alpine";

/// The Alpine registration methods (`Alpine.data`, `Alpine.store`) whose string
/// first argument names a component a template can reference via `x-data`.
const ALPINE_REGISTRARS: &[&str] = &["data", "store"];

/// The call callees that dispatch an event named by their first string argument
/// (`emitter.emit('e')`, `this.$dispatch('e')`). Correlated with listeners.
const DISPATCH_CALLEES: &[&str] = &["emit", "fire", "$dispatch"];

/// The call callees that register a listener for the event named by their first
/// string argument, with a named-function handler as the second argument
/// (`emitter.on('e', handler)`, `el.addEventListener('e', handler)`).
const LISTEN_CALLEES: &[&str] = &["on", "once", "addEventListener"];

/// An extractor of JavaScript (including Alpine.js component code) into graph nodes,
/// containment edges, and the call/import/inheritance references resolution
/// later turns into edges.
pub struct JavaScriptExtractor;

thread_local! {
    /// The per-thread JavaScript parser, reused across files so each file pays
    /// only for its parse, not for parser construction. One parser per rayon
    /// worker thread, no cross-thread sharing.
    static PARSER: RefCell<Parser> = RefCell::new(new_parser());
}

/// A JavaScript parser with the grammar loaded. It panics only on a grammar
/// against tree-sitter ABI mismatch, a build error that cannot arise at runtime
/// in a correctly linked binary.
fn new_parser() -> Parser {
    let language: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();

    assert!(language.node_kind_count() > 0, "javascript grammar must expose node kinds");

    let mut parser = Parser::new();

    parser
        .set_language(&language)
        .expect("the bundled javascript grammar is ABI-compatible with tree-sitter");

    parser
}

impl JavaScriptExtractor {
    /// The extractor; the grammar loads per worker thread on first use.
    pub fn new() -> Self {
        Self
    }
}

impl Default for JavaScriptExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for JavaScriptExtractor {
    fn language(&self) -> Language {
        Language::JavaScript
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

        let scope = Scope {
            prefix: Rc::from(file_path),
            parent_id: Rc::new(file_id.clone()),
            at_file_scope: true,
        };

        let mut stack: Vec<Frame> = Vec::new();
        push_named_children(tree.root_node(), &scope, &mut stack);

        let mut iterations: u32 = 0;

        while let Some(frame) = stack.pop() {
            iterations += 1;

            assert!(iterations <= WALK_ITERATIONS_MAX, "walk exceeded {WALK_ITERATIONS_MAX}");

            process_frame(project, file_path, bytes, &file_id, frame, &mut stack, &mut output);
        }

        let exported = collect_exports(bytes, tree.root_node());
        apply_exports(&mut output, &exported);

        output
    }
}

/// A scope: the parent's qualified name, the containing node, and whether the
/// parent is the file (`::` separator) or a symbol (`.` separator).
#[derive(Clone)]
struct Scope {
    prefix: Rc<str>,
    parent_id: Rc<NodeId>,
    at_file_scope: bool,
}

struct Frame<'tree> {
    node: TsNode<'tree>,
    scope: Scope,
}

fn process_frame<'tree>(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    frame: Frame<'tree>,
    stack: &mut Vec<Frame<'tree>>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    match frame.node.kind() {
        "class_declaration" => {
            if let Some(scope) = handle_class(project, file_path, bytes, &frame, output)
                && let Some(body) = frame.node.child_by_field_name("body")
            {
                push_named_children(body, &scope, stack);
            }
        }
        "function_declaration" | "method_definition" => {
            if let Some(scope) = handle_function(project, file_path, bytes, &frame, output)
                && let Some(body) = frame.node.child_by_field_name("body")
            {
                push_named_children(body, &scope, stack);
            }
        }
        "variable_declarator" => handle_variable(project, file_path, bytes, &frame, stack, output),
        "import_statement" => handle_import(file_path, bytes, file_id, frame.node, output),
        "call_expression" => {
            if let Some(name) = alpine_component(bytes, frame.node) {
                // Walk the component's factory under the component's own scope so
                // its methods attribute to the component (a `Contains` edge, an
                // `x-data`-symmetric id), matching the template extractor's
                // handling of an inline `x-data` object.
                let scope = emit_component(project, file_path, file_id, &name, frame.node, output);

                push_named_children(frame.node, &scope, stack);
            } else {
                if let Some(reference) = call_ref(file_path, bytes, &frame) {
                    output.unresolved_refs.push(reference);
                }

                record_event(bytes, &frame, output);

                push_named_children(frame.node, &frame.scope, stack);
            }
        }
        "new_expression" => {
            if let Some(reference) = new_ref(file_path, bytes, &frame) {
                output.unresolved_refs.push(reference);
            }

            push_named_children(frame.node, &frame.scope, stack);
        }
        _ => push_named_children(frame.node, &frame.scope, stack),
    }
}

fn handle_class(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> Option<Scope> {
    let name_node = frame.node.child_by_field_name("name")?;
    let name = node_text(bytes, name_node);

    assert!(!name.is_empty(), "class name must not be empty");

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(symbol_node(project, NodeKind::Class, name, &qualified_name, file_path, frame.node));

    extends_refs(file_path, bytes, frame.node, &id, output);

    Some(child_scope(qualified_name, id))
}

fn handle_function(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'_>,
    output: &mut ExtractionOutput,
) -> Option<Scope> {
    let name_node = frame.node.child_by_field_name("name")?;
    let name = node_text(bytes, name_node);

    assert!(!name.is_empty(), "function name must not be empty");

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);

    let kind = if frame.node.kind() == "method_definition" {
        NodeKind::Method
    } else {
        NodeKind::Function
    };

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(symbol_node(project, kind, name, &qualified_name, file_path, frame.node));

    Some(child_scope(qualified_name, id))
}

/// A `const`/`let`/`var` binding. A function-valued binding becomes a function
/// node whose body is walked in its own scope; anything else is a variable.
fn handle_variable<'tree>(
    project: &ProjectId,
    file_path: &str,
    bytes: &[u8],
    frame: &Frame<'tree>,
    stack: &mut Vec<Frame<'tree>>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let Some(name_node) = frame.node.child_by_field_name("name") else {
        return;
    };

    if name_node.kind() != "identifier" {
        push_named_children(frame.node, &frame.scope, stack);
        return;
    }

    let name = node_text(bytes, name_node);

    assert!(!name.is_empty(), "variable name must not be empty");

    let value = frame.node.child_by_field_name("value");
    let is_function = value.is_some_and(|node| FUNCTION_VALUES.contains(&node.kind()));

    let qualified_name = join_qualified(&frame.scope, name);
    let id = NodeId::new(project, &qualified_name);
    let kind = if is_function { NodeKind::Function } else { NodeKind::Variable };

    output.edges.push(contains_edge(&frame.scope.parent_id, &id));
    output.nodes.push(symbol_node(project, kind, name, &qualified_name, file_path, frame.node));

    match value {
        Some(node) if is_function => {
            let scope = child_scope(qualified_name, id);
            let body = node.child_by_field_name("body").unwrap_or(node);

            push_named_children(body, &scope, stack);
        }
        Some(node) => push_frame(node, &frame.scope, stack),
        None => {}
    }
}

fn handle_import(
    file_path: &str,
    bytes: &[u8],
    file_id: &NodeId,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let Some(source_node) = node.child_by_field_name("source") else {
        return;
    };

    let Some(module) = string_literal(bytes, source_node) else {
        return;
    };

    if module.is_empty() {
        return;
    }

    assert!(!module.is_empty(), "module specifier is non-empty past the guard");

    let position = node.start_position();
    let line = line_1based(position.row);
    let column = to_u32(position.column);
    let bindings = imported_bindings(bytes, node);

    if bindings.is_empty() {
        output.unresolved_refs.push(UnresolvedRef::new(
            file_id.clone(),
            module,
            EdgeKind::Imports,
            line,
            column,
            file_path,
            Language::JavaScript,
        ));

        return;
    }

    for (local, exported) in bindings {
        let mut reference = UnresolvedRef::new(
            file_id.clone(),
            exported.clone(),
            EdgeKind::Imports,
            line,
            column,
            file_path,
            Language::JavaScript,
        );

        reference.candidates.push(module.to_string());
        output.unresolved_refs.push(reference);

        output.import_mappings.push(ImportMapping {
            local_name: local,
            exported_name: exported,
            source: module.to_string(),
            is_default: false,
            is_namespace: false,
            resolved_path: None,
        });
    }
}

/// The (local name, exported name) bindings an import statement brings in.
/// Empty for a side-effect import (`import './x'`) or a namespace import
/// (`* as ns`), which the caller links at module granularity instead.
fn imported_bindings(bytes: &[u8], node: TsNode<'_>) -> Vec<(String, String)> {
    let mut bindings: Vec<(String, String)> = Vec::new();
    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "import child fan-out exceeded {CHILDREN_MAX}");

        if child.kind() == "import_clause" {
            collect_import_clause(bytes, child, &mut bindings);
        }
    }

    bindings
}

/// The default and named import bindings collected from an `import_clause`. A
/// namespace import is left out so its module links wholesale.
fn collect_import_clause(bytes: &[u8], clause: TsNode<'_>, bindings: &mut Vec<(String, String)>) {
    let mut cursor = clause.walk();
    let mut count: u32 = 0;

    for item in clause.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "import clause fan-out exceeded {CHILDREN_MAX}");

        match item.kind() {
            "identifier" => {
                let name = node_text(bytes, item).to_string();
                bindings.push((name.clone(), name));
            }
            "named_imports" => collect_named_imports(bytes, item, bindings),
            _ => {}
        }
    }
}

/// The (local, exported) pairs collected from a `named_imports` (`{ a, b as c }`):
/// the upstream `name` is the export, the `alias` (when present) the local.
fn collect_named_imports(bytes: &[u8], named: TsNode<'_>, bindings: &mut Vec<(String, String)>) {
    let mut cursor = named.walk();
    let mut count: u32 = 0;

    for specifier in named.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "named-import fan-out exceeded {CHILDREN_MAX}");

        if specifier.kind() == "import_specifier"
            && let Some(name) = specifier.child_by_field_name("name")
        {
            let exported = node_text(bytes, name).to_string();
            let local = specifier
                .child_by_field_name("alias")
                .map_or_else(|| exported.clone(), |alias| node_text(bytes, alias).to_string());

            bindings.push((local, exported));
        }
    }
}

fn extends_refs(
    file_path: &str,
    bytes: &[u8],
    class_node: TsNode<'_>,
    class_id: &NodeId,
    output: &mut ExtractionOutput,
) {
    let mut cursor = class_node.walk();
    let mut count: u32 = 0;

    for child in class_node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "class child fan-out exceeded {CHILDREN_MAX}");

        if child.kind() != "class_heritage" {
            continue;
        }

        if let Some(name) = heritage_name(bytes, child) {
            let position = child.start_position();

            output.unresolved_refs.push(UnresolvedRef::new(
                class_id.clone(),
                name,
                EdgeKind::Extends,
                line_1based(position.row),
                to_u32(position.column),
                file_path,
                Language::JavaScript,
            ));
        }
    }
}

fn call_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    let function = frame.node.child_by_field_name("function")?;
    let name = callee_name(bytes, function)?;

    assert!(!name.is_empty(), "callee name must not be empty");

    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        name,
        EdgeKind::Calls,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::JavaScript,
    ))
}

/// An `Instantiates` reference from the enclosing symbol to a `new X()`
/// constructor. The name is the bare identifier or the trailing property of a
/// member access (`new pkg.Widget()` -> `Widget`).
fn new_ref(file_path: &str, bytes: &[u8], frame: &Frame<'_>) -> Option<UnresolvedRef> {
    let constructor = frame.node.child_by_field_name("constructor")?;
    let name = callee_name(bytes, constructor)?;

    assert!(!name.is_empty(), "constructor name must not be empty");

    let position = frame.node.start_position();

    Some(UnresolvedRef::new(
        frame.scope.parent_id.as_ref().clone(),
        name,
        EdgeKind::Instantiates,
        line_1based(position.row),
        to_u32(position.column),
        file_path,
        Language::JavaScript,
    ))
}

/// The record of an event dispatch or listener registration on a call, for the
/// cross-file synthesis pass to correlate by event name. Dispatch is anchored
/// at the enclosing node; a listener carries its (named) handler.
fn record_event(bytes: &[u8], frame: &Frame<'_>, output: &mut ExtractionOutput) {
    let Some(function) = frame.node.child_by_field_name("function") else {
        return;
    };
    let Some(callee) = callee_name(bytes, function) else {
        return;
    };

    let position = frame.node.start_position();
    let line = line_1based(position.row);
    let column = to_u32(position.column);

    if DISPATCH_CALLEES.contains(&callee) {
        if let Some(event) = string_argument(bytes, frame.node, 0) {
            output.events.push(EventRecord {
                role: EventRole::Dispatch,
                event,
                symbol: frame.scope.parent_id.as_str().to_string(),
                line,
                column,
            });
        }
    } else if LISTEN_CALLEES.contains(&callee)
        && let Some(event) = string_argument(bytes, frame.node, 0)
        && let Some(handler) = identifier_argument(bytes, frame.node, 1)
    {
        output.events.push(EventRecord {
            role: EventRole::Listen,
            event,
            symbol: handler.to_string(),
            line,
            column,
        });
    }
}

/// The component name registered by an `Alpine.data('name', ...)` /
/// `Alpine.store('name', ...)` call, if this call is one.
fn alpine_component(bytes: &[u8], call_node: TsNode<'_>) -> Option<String> {
    let function = call_node.child_by_field_name("function")?;

    if function.kind() != "member_expression" {
        return None;
    }

    let object = function.child_by_field_name("object")?;
    let property = function.child_by_field_name("property")?;

    if node_text(bytes, object) != ALPINE_OBJECT
        || !ALPINE_REGISTRARS.contains(&node_text(bytes, property))
    {
        return None;
    }

    let arguments = call_node.child_by_field_name("arguments")?;
    let first = arguments.named_child(0)?;
    let name = string_literal(bytes, first)?;

    if name.is_empty() {
        return None;
    }

    Some(name.to_string())
}

/// The component's scope, after emitting a function node for an Alpine component
/// registration so a template's `x-data="name()"` resolves to it. The caller
/// walks the factory body under that scope, attributing the component's methods to it.
fn emit_component(
    project: &ProjectId,
    file_path: &str,
    file_id: &NodeId,
    name: &str,
    node: TsNode<'_>,
    output: &mut ExtractionOutput,
) -> Scope {
    assert!(!name.is_empty(), "component name must not be empty");
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let qualified_name = format!("{file_path}::alpine::{name}");
    let id = NodeId::new(project, &qualified_name);

    output.edges.push(contains_edge(file_id, &id));
    output.nodes.push(symbol_node(project, NodeKind::Function, name, &qualified_name, file_path, node));

    child_scope(qualified_name, id)
}

/// The base class name from a `class_heritage` (`extends Base`).
fn heritage_name<'bytes>(bytes: &'bytes [u8], heritage: TsNode<'_>) -> Option<&'bytes str> {
    let mut cursor = heritage.walk();

    for child in heritage.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => return Some(node_text(bytes, child)),
            "member_expression" => {
                return child.child_by_field_name("property").map(|property| node_text(bytes, property));
            }
            _ => {}
        }
    }

    None
}

fn push_named_children<'tree>(node: TsNode<'tree>, scope: &Scope, stack: &mut Vec<Frame<'tree>>) {
    let mut cursor = node.walk();
    let mut count: u32 = 0;

    for child in node.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "child fan-out exceeded {CHILDREN_MAX}");

        push_frame(child, scope, stack);
    }
}

fn push_frame<'tree>(node: TsNode<'tree>, scope: &Scope, stack: &mut Vec<Frame<'tree>>) {
    stack.push(Frame { node, scope: scope.clone() });
}

fn child_scope(qualified_name: String, id: NodeId) -> Scope {
    assert!(!qualified_name.is_empty(), "qualified_name must not be empty");

    Scope {
        prefix: Rc::from(qualified_name.as_str()),
        parent_id: Rc::new(id),
        at_file_scope: false,
    }
}

fn join_qualified(scope: &Scope, name: &str) -> String {
    assert!(!name.is_empty(), "name must not be empty");
    assert!(!scope.prefix.is_empty(), "scope prefix must not be empty");

    if scope.at_file_scope {
        format!("{}::{name}", scope.prefix)
    } else {
        format!("{}.{name}", scope.prefix)
    }
}

fn contains_edge(parent: &NodeId, child: &NodeId) -> Edge {
    Edge::new(parent.clone(), child.clone(), EdgeKind::Contains).with_provenance(PROVENANCE)
}

fn file_node(project: &ProjectId, file_path: &str, file_id: &NodeId, root: TsNode<'_>) -> Node {
    assert!(!file_path.is_empty(), "file_path must not be empty");

    let name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);

    assert!(!name.is_empty(), "file node name must not be empty");

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: file_path.to_string(),
        file_path: file_path.to_string(),
        language: Language::JavaScript,
    };

    Node::new(file_id.clone(), project.clone(), NodeKind::File, identity, span_of(root), 0)
}

fn symbol_node(
    project: &ProjectId,
    kind: NodeKind,
    name: &str,
    qualified_name: &str,
    file_path: &str,
    node: TsNode<'_>,
) -> Node {
    assert!(!name.is_empty(), "node name must not be empty");
    assert!(!qualified_name.is_empty(), "qualified_name must not be empty");

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: qualified_name.to_string(),
        file_path: file_path.to_string(),
        language: Language::JavaScript,
    };

    Node::new(NodeId::new(project, qualified_name), project.clone(), kind, identity, span_of(node), 0)
}

/// The names a module marks as exported, for [`apply_exports`] to flag.
///
/// A separate bounded walk, kept out of the main extraction pass so export
/// handling stays isolated: ESM `export <decl>` / `export { a, b as c }` /
/// `export default <named>`, and CommonJS `module.exports` / `exports.x`.
fn collect_exports(bytes: &[u8], root: TsNode<'_>) -> FxHashSet<String> {
    let mut exported: FxHashSet<String> = FxHashSet::default();
    let mut stack: Vec<TsNode> = vec![root];
    let mut iterations: u32 = 0;

    while let Some(node) = stack.pop() {
        iterations += 1;

        assert!(iterations <= WALK_ITERATIONS_MAX, "export walk exceeded {WALK_ITERATIONS_MAX}");

        match node.kind() {
            "export_statement" => collect_export_statement(bytes, node, &mut exported),
            "assignment_expression" => collect_commonjs_export(bytes, node, &mut exported),
            _ => {}
        }

        let mut cursor = node.walk();
        let mut count: u32 = 0;

        for child in node.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "export child fan-out exceeded {CHILDREN_MAX}");

            stack.push(child);
        }
    }

    exported
}

/// The flagging of the file-scope nodes whose name a module exports. Membership is
/// by name among this file's top-level symbols (one file per extraction output), so a
/// nested method sharing an exported name is left untouched.
fn apply_exports(output: &mut ExtractionOutput, exported: &FxHashSet<String>) {
    if exported.is_empty() {
        return;
    }

    for node in &mut output.nodes {
        let file_scoped = node.qualified_name.rsplit("::").next() == Some(node.name.as_str());

        if file_scoped && exported.contains(&node.name) {
            node.is_exported = true;
        }
    }
}

/// The names introduced by one `export_statement`: an inline declaration
/// (`export class X` / `export const a, b`), a named export clause
/// (`export { a, b as c }` -> the locals `a`, `b`), or a named default
/// (`export default function f`). An anonymous default exports no name.
fn collect_export_statement(bytes: &[u8], node: TsNode<'_>, exported: &mut FxHashSet<String>) {
    if let Some(declaration) = node.child_by_field_name("declaration") {
        collect_declared_names(bytes, declaration, exported);

        return;
    }

    if let Some(value) = node.child_by_field_name("value") {
        if value.kind() == "identifier" {
            exported.insert(node_text(bytes, value).to_string());
        }

        return;
    }

    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        if child.kind() == "export_clause" {
            collect_export_clause(bytes, child, exported);
        }
    }
}

/// The names a declaration binds: a class/function name, or each name
/// in a `const`/`let`/`var` declaration.
fn collect_declared_names(bytes: &[u8], declaration: TsNode<'_>, exported: &mut FxHashSet<String>) {
    match declaration.kind() {
        "class_declaration" | "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = declaration.child_by_field_name("name") {
                exported.insert(node_text(bytes, name).to_string());
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = declaration.walk();
            let mut count: u32 = 0;

            for child in declaration.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "declarator fan-out exceeded {CHILDREN_MAX}");

                if child.kind() == "variable_declarator"
                    && let Some(name) = child.child_by_field_name("name")
                    && name.kind() == "identifier"
                {
                    exported.insert(node_text(bytes, name).to_string());
                }
            }
        }
        _ => {}
    }
}

/// The local names of an `export { a, b as c }` clause (`a` and `b`,
/// the symbols this module defines, not their exported aliases).
fn collect_export_clause(bytes: &[u8], clause: TsNode<'_>, exported: &mut FxHashSet<String>) {
    let mut cursor = clause.walk();
    let mut count: u32 = 0;

    for specifier in clause.named_children(&mut cursor) {
        count += 1;

        assert!(count <= CHILDREN_MAX, "export-clause fan-out exceeded {CHILDREN_MAX}");

        if specifier.kind() == "export_specifier"
            && let Some(name) = specifier.child_by_field_name("name")
        {
            exported.insert(node_text(bytes, name).to_string());
        }
    }
}

/// The names a CommonJS assignment exports: `module.exports = X` or
/// `module.exports = { a, b }`, and `exports.x = Y` / `module.exports.x = Y`
/// (the local `Y`). Other assignments contribute nothing.
fn collect_commonjs_export(bytes: &[u8], node: TsNode<'_>, exported: &mut FxHashSet<String>) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };

    if left.kind() != "member_expression" {
        return;
    }

    let Some(right) = node.child_by_field_name("right") else {
        return;
    };

    if is_module_exports(bytes, left) {
        collect_export_value(bytes, right, exported);
    } else if is_exports_member(bytes, left) && right.kind() == "identifier" {
        exported.insert(node_text(bytes, right).to_string());
    }
}

/// The value a `module.exports = ...` assigns: a bare identifier, or the
/// identifier members of an object literal (`{ a, b, c: d }` -> `a`, `b`, `d`).
fn collect_export_value(bytes: &[u8], value: TsNode<'_>, exported: &mut FxHashSet<String>) {
    match value.kind() {
        "identifier" => {
            exported.insert(node_text(bytes, value).to_string());
        }
        "object" => {
            let mut cursor = value.walk();
            let mut count: u32 = 0;

            for member in value.named_children(&mut cursor) {
                count += 1;

                assert!(count <= CHILDREN_MAX, "exports-object fan-out exceeded {CHILDREN_MAX}");

                match member.kind() {
                    "shorthand_property_identifier" => {
                        exported.insert(node_text(bytes, member).to_string());
                    }
                    "pair" => {
                        if let Some(member_value) = member.child_by_field_name("value")
                            && member_value.kind() == "identifier"
                        {
                            exported.insert(node_text(bytes, member_value).to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Whether a member expression is exactly `module.exports`.
fn is_module_exports(bytes: &[u8], member: TsNode<'_>) -> bool {
    let object = member.child_by_field_name("object");
    let property = member.child_by_field_name("property");

    object.is_some_and(|node| node_text(bytes, node) == "module")
        && property.is_some_and(|node| node_text(bytes, node) == "exports")
}

/// Whether a member expression targets a named CommonJS export: `exports.x` or
/// `module.exports.x`.
fn is_exports_member(bytes: &[u8], member: TsNode<'_>) -> bool {
    let Some(object) = member.child_by_field_name("object") else {
        return false;
    };

    match object.kind() {
        "identifier" => node_text(bytes, object) == "exports",
        "member_expression" => is_module_exports(bytes, object),
        _ => false,
    }
}
