//! The false answers this server used to give, each pinned by the shape of the
//! output that made it wrong.
//!
//! Every case here was a tool answering confidently and incorrectly rather than
//! failing: "no call path" over an edge that exists, "no covering tests" on tested
//! code, "(none)" for a caller sitting in the same file. Those cost a reader more
//! than an error would, because there is nothing in them to distrust.

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_mcp::cursor::Page;
use constellation_mcp::{flow_section, impact_text, subclasses_text, tests_text};
use constellation_store::{FileIndex, Store};


/// A node in project `blog`, addressable as `<file>::<qualified>`.
fn node(id: &str, kind: NodeKind, name: &str, qualified: &str, file: &str, line: u32) -> Node {
    Node::new(
        NodeId::from_raw(id.to_string()),
        ProjectId::new("blog"),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            file_path: file.to_string(),
            language: Language::Python,
        },
        Span::new(line, line, 0, 0),
        0,
    )
}

fn file_index(path: &'static str) -> FileIndex<'static> {
    FileIndex {
        path,
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    }
}

/// A store holding one project with the given nodes and edges, all attributed to
/// one file so a single `persist_file` writes them.
fn store_with(path: &'static str, nodes: &[Node], edges: &[Edge]) -> Store {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();
    store.persist_file(&project, &file_index(path), nodes, edges, &[], &[], &[]).unwrap();

    store
}

#[test]
fn a_flow_trace_tries_every_definition_of_a_name() {
    // Two apps define `_modal_view`. Only the second one calls the helper. The
    // trace used to keep one node per name, so whichever definition ranked first
    // stood in for the whole name, and the pair reported "no call path" over an
    // edge one hop long. That line is the first thing explore prints.
    let unconnected = node(
        "blog::a::_modal_view",
        NodeKind::View,
        "_modal_view",
        "a/views.py::_modal_view",
        "a/views.py",
        10,
    );

    let connected = node(
        "blog::b::_modal_view",
        NodeKind::View,
        "_modal_view",
        "b/views.py::_modal_view",
        "b/views.py",
        20,
    );

    let helper = node(
        "blog::b::_choices",
        NodeKind::Function,
        "_choices",
        "b/views.py::_choices",
        "b/views.py",
        30,
    );

    let nodes = vec![unconnected.clone(), connected.clone(), helper.clone()];
    let out_edges: Vec<Vec<(u32, EdgeKind)>> = vec![
        Vec::new(),
        vec![(2, EdgeKind::Calls)],
        Vec::new(),
    ];

    let text = flow_section(&nodes, &out_edges, &[0, 1, 2], "_modal_view _choices");

    assert!(
        text.contains("call paths among the named symbols"),
        "the connected definition is found even though it is not the first candidate: {text}",
    );

    assert!(!text.contains("no call path"), "no false negative is reported: {text}");
}

#[test]
fn an_unconnected_pair_still_says_so() {
    let left = node("blog::x::alpha", NodeKind::Function, "alpha", "x.py::alpha", "x.py", 1);
    let right = node("blog::x::beta", NodeKind::Function, "beta", "x.py::beta", "x.py", 2);

    let nodes = vec![left, right];
    let out_edges: Vec<Vec<(u32, EdgeKind)>> = vec![Vec::new(), Vec::new()];

    let text = flow_section(&nodes, &out_edges, &[0, 1], "alpha beta");

    assert!(
        text.contains("no call path"),
        "a genuinely unconnected pair is reported, not silently dropped: {text}",
    );
}

#[test]
fn a_class_and_its_own_member_are_not_reported_unconnected() {
    let owner = node("blog::x::Order", NodeKind::Class, "Order", "x.py::Order", "x.py", 1);
    let member = node("blog::x::Order.total", NodeKind::Method, "total", "x.py::Order.total", "x.py", 2);

    let nodes = vec![owner, member];
    let out_edges: Vec<Vec<(u32, EdgeKind)>> = vec![Vec::new(), Vec::new()];

    let text = flow_section(&nodes, &out_edges, &[0, 1], "Order total");

    assert!(
        text.is_empty(),
        "containment carries no call path, so nothing is claimed either way: {text}",
    );
}

#[test]
fn an_untested_member_points_at_its_owners_coverage() {
    // A property is only ever reached by an attribute read, which leaves no edge,
    // so its caller set is empty whether or not tests exercise it. Saying
    // "(no covering tests)" flatly there tells a reader to go ahead and rewrite
    // guarded code.
    let model = node("blog::m::Line", NodeKind::Model, "Line", "m.py::Line", "m.py", 1);

    let property = node(
        "blog::m::Line.is_fresh",
        NodeKind::Property,
        "is_fresh",
        "m.py::Line.is_fresh",
        "m.py",
        5,
    );

    let test = node(
        "blog::t::test_line",
        NodeKind::Function,
        "test_line",
        "tests/test_m.py::test_line",
        "tests/test_m.py",
        3,
    );

    let nodes = vec![model.clone(), property.clone(), test.clone()];
    let edges = vec![Edge::new(test.id.clone(), model.id.clone(), EdgeKind::Calls)];

    let store = store_with("m.py", &nodes, &edges);
    let text = tests_text(&store, "Line.is_fresh", 10).unwrap();

    assert!(
        text.contains("under-detected"),
        "the member says what it cannot know rather than asserting an absence: {text}",
    );

    assert!(
        text.contains("its owner Line has 1 covering test reference"),
        "and points at the coverage that is measurable: {text}",
    );
}

#[test]
fn an_untested_top_level_definition_still_says_so_plainly() {
    let orphan = node("blog::m::widget", NodeKind::Function, "widget", "m.py::widget", "m.py", 1);
    let store = store_with("m.py", &[orphan], &[]);

    let text = tests_text(&store, "widget", 10).unwrap();

    assert!(
        text.contains("(no covering tests)"),
        "a free function's empty caller set is real evidence, and reads plainly: {text}",
    );

    assert!(!text.contains("under-detected"), "the hedge is not added where it does not apply: {text}");
}

#[test]
fn impact_counts_callers_apart_from_schema_relations() {
    // `services = LineService()` makes the model an `instantiates` caller of the
    // service, and every foreign key pointing at that model used to ride in behind
    // it as part of the service's "blast radius".
    let service = node("blog::s::LineService", NodeKind::Class, "LineService", "s.py::LineService", "s.py", 1);
    let model = node("blog::s::Line", NodeKind::Model, "Line", "s.py::Line", "s.py", 10);
    let other = node("blog::s::Trash", NodeKind::Model, "Trash", "s.py::Trash", "s.py", 20);
    let caller = node("blog::s::run", NodeKind::Function, "run", "s.py::run", "s.py", 30);

    let nodes = vec![service.clone(), model.clone(), other.clone(), caller.clone()];

    let edges = vec![
        Edge::new(model.id.clone(), service.id.clone(), EdgeKind::Instantiates),
        Edge::new(other.id.clone(), model.id.clone(), EdgeKind::RelatesTo),
        Edge::new(caller.id.clone(), model.id.clone(), EdgeKind::Calls),
    ];

    let store = store_with("s.py", &nodes, &edges);
    let text = impact_text(&store, "LineService", 3, &Page::default(), 1).unwrap();

    assert!(
        text.contains("2 non-test caller(s)"),
        "the model that instantiates it and the function that calls that model count: {text}",
    );

    assert!(
        text.contains("type/schema association"),
        "the foreign key is reported as an association, not as a caller: {text}",
    );
}

#[test]
fn subclasses_offers_a_cursor_into_its_tail() {
    let base = node("blog::b::Mixin", NodeKind::Class, "Mixin", "b.py::Mixin", "b.py", 1);

    let mut nodes = vec![base.clone()];
    let mut edges: Vec<Edge> = Vec::new();

    for index in 0..5u32 {
        let child = node(
            &format!("blog::b::Child{index}"),
            NodeKind::Class,
            &format!("Child{index}"),
            &format!("b.py::Child{index}"),
            "b.py",
            10 + index,
        );

        edges.push(Edge::new(child.id.clone(), base.id.clone(), EdgeKind::Extends));
        nodes.push(child);
    }

    let store = store_with("b.py", &nodes, &edges);
    let text = subclasses_text(&store, "Mixin", 2, &Page::default(), 1).unwrap();

    assert!(text.contains("2 shown of"), "the listing says how much of the set it is showing: {text}");
    assert!(text.contains("cursor="), "and offers a way to read the rest: {text}");
}
