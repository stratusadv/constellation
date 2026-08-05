use constellation_extraction::{ExtractionOutput, Extractor, JavaScriptExtractor};
use constellation_graph::{EdgeKind, Node, NodeKind, ProjectId};
use constellation_resolution::EventRole;

fn run(file_path: &str, source: &str) -> ExtractionOutput {
    let extractor = JavaScriptExtractor::new();
    let project = ProjectId::new("shop");

    extractor.extract(&project, file_path, source)
}

fn node<'output>(output: &'output ExtractionOutput, name: &str) -> &'output Node {
    output
        .nodes
        .iter()
        .find(|node| node.name == name)
        .unwrap_or_else(|| panic!("missing node {name}"))
}

#[test]
fn alpine_component_methods_attribute_to_the_component() {
    let source = "Alpine.data('cart', () => ({
    items: [],
    addItem(product) {
        this.items.push(product);
    },
}));
";

    let output = run("shop/static/cart.js", source);

    let component = node(&output, "cart");
    assert_eq!(component.qualified_name, "shop/static/cart.js::alpine::cart");

    let method = node(&output, "addItem");
    assert_eq!(method.qualified_name, "shop/static/cart.js::alpine::cart.addItem");

    let contained = output.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Contains && edge.source == component.id && edge.target == method.id
    });

    assert!(contained, "the component contains its method");
}

#[test]
fn new_expression_emits_instantiates() {
    let source = "function build() {
    return new Widget(1);
}
";

    let output = run("shop/build.js", source);

    let instantiates = output.unresolved_refs.iter().any(|reference| {
        reference.reference_kind == EdgeKind::Instantiates && reference.reference_name == "Widget"
    });

    assert!(instantiates, "new Widget() yields an Instantiates reference");
}

#[test]
fn esm_export_marks_is_exported() {
    let source = "export function publicFn() {}
function privateFn() {}
export const widget = 1;
";

    let output = run("shop/api.js", source);

    assert!(node(&output, "publicFn").is_exported, "exported function flagged");
    assert!(node(&output, "widget").is_exported, "exported const flagged");
    assert!(!node(&output, "privateFn").is_exported, "non-exported function not flagged");
}

#[test]
fn export_clause_and_commonjs_mark_is_exported() {
    let named = "function a() {}
function b() {}
export { a };
";

    let output = run("shop/named.js", named);

    assert!(node(&output, "a").is_exported, "named export flags the local");
    assert!(!node(&output, "b").is_exported, "unexported local stays false");

    let commonjs = "function handler() {}
module.exports = handler;
";

    let output = run("shop/cjs.js", commonjs);

    assert!(node(&output, "handler").is_exported, "module.exports = X flags X");

    assert_eq!(
        names_of(&output, NodeKind::Function),
        vec!["handler".to_string()],
        "the handler is still a single function node",
    );
}

fn names_of(output: &ExtractionOutput, kind: NodeKind) -> Vec<String> {
    output
        .nodes
        .iter()
        .filter(|node| node.kind == kind)
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn dispatch_and_listen_calls_record_events() {
    let source = "function wire(el) {
    this.$dispatch('refresh');
    el.addEventListener('save', handler);
}
";

    let output = run("shop/wire.js", source);

    assert!(
        output.events.iter().any(|event| event.role == EventRole::Dispatch && event.event == "refresh"),
        "$dispatch records a dispatch event, got {:?}",
        output.events,
    );

    assert!(
        output.events.iter().any(|event| event.role == EventRole::Listen && event.event == "save"),
        "addEventListener records a listen event, got {:?}",
        output.events,
    );
}

#[test]
fn class_methods_are_contained_by_their_class() {
    let source = "class Cart {
    add(item) {
        this.items.push(item);
    }
}
";

    let output = run("shop/cart_class.js", source);

    let cart = node(&output, "Cart");
    let add = node(&output, "add");

    assert_eq!(cart.kind, NodeKind::Class, "Cart is a class node");

    assert!(
        output.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Contains && edge.source == cart.id && edge.target == add.id
        }),
        "the class contains its method",
    );
}
