//! The composed multi-axis filter, end to end against a store.

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_mcp::cursor::Page;
use constellation_mcp::winnow::{RawCriterion, WINNOW_CRITERIA_MAX};
use constellation_mcp::winnow_text;
use constellation_store::{FileIndex, Store};

/// A node spanning `lines` source lines, so the `lines` axis has something to
/// compare.
fn node(kind: NodeKind, name: &str, file: &str, lines: u32) -> Node {
    Node::new(
        NodeId::from_raw(format!("shop::{file}::{name}")),
        ProjectId::new("shop"),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("{file}::{name}"),
            file_path: file.to_string(),
            language: Language::Python,
        },
        Span::new(1, lines.max(1), 0, 0),
        0,
    )
}

/// A store holding a small Django-shaped fixture: two models, a view that calls
/// one of them, and a test covering only the other.
fn fixture() -> (Store, ProjectId) {
    let store = Store::open_in_memory().expect("an in-memory store");
    let project = ProjectId::new("shop");

    store.upsert_project(&project, "shop", "/tmp/shop").expect("the project row");

    let order = node(NodeKind::Model, "Order", "orders/models.py", 40);
    let invoice = node(NodeKind::Model, "Invoice", "billing/models.py", 12);
    let view = node(NodeKind::View, "checkout_view", "orders/views.py", 20);
    let covering = node(NodeKind::Function, "test_order", "orders/tests/test_models.py", 6);

    let nodes = vec![order.clone(), invoice.clone(), view.clone(), covering.clone()];

    let edges = vec![
        Edge::new(view.id.clone(), order.id.clone(), EdgeKind::Instantiates),
        Edge::new(invoice.id.clone(), order.id.clone(), EdgeKind::RelatesTo),
        Edge::new(covering.id.clone(), order.id.clone(), EdgeKind::Tests),
    ];

    let file = FileIndex {
        path: "orders/models.py",
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    };

    store.persist_file(&project, &file, &nodes, &edges, &[], &[], &[]).expect("persisting");

    (store, project)
}

/// A criterion, with no churn window.
fn criterion<'a>(axis: &'a str, op: &'a str, value: &'a str) -> RawCriterion<'a> {
    RawCriterion { axis, op, value, window_days: None }
}

/// A winnow query run against the fixture with default paging.
fn winnow(store: &Store, criteria: &[RawCriterion<'_>]) -> String {
    winnow_text(store, criteria, None, 25, &Page::default(), 0).expect("winnowing")
}

#[test]
fn each_axis_filters_in_isolation() {
    let (store, _project) = fixture();

    let by_kind = winnow(&store, &[criterion("kind", "eq", "model")]);

    assert!(by_kind.contains("Order"), "the kind axis keeps models: {by_kind}");
    assert!(by_kind.contains("Invoice"), "both of them: {by_kind}");
    assert!(!by_kind.contains("checkout_view"), "and drops the view: {by_kind}");

    let by_name = winnow(&store, &[criterion("name", "contains", "order")]);

    assert!(by_name.contains("Order"), "the name axis matches case-insensitively: {by_name}");

    let by_glob = winnow(&store, &[criterion("name", "matches", "*_view")]);

    assert!(by_glob.contains("checkout_view"), "the matches axis is a glob: {by_glob}");
    assert!(!by_glob.contains("Invoice"), "anchored at both ends: {by_glob}");

    let by_file = winnow(&store, &[criterion("file", "contains", "billing/")]);

    assert!(by_file.contains("Invoice"), "the file axis narrows by path: {by_file}");
    assert!(!by_file.contains("checkout_view"), "{by_file}");

    let by_lines = winnow(&store, &[criterion("lines", ">", "30")]);

    assert!(by_lines.contains("Order"), "the lines axis compares span length: {by_lines}");
    assert!(!by_lines.contains("Invoice"), "the short model is excluded: {by_lines}");
}

#[test]
fn the_edge_axes_read_real_edges() {
    let (store, _project) = fixture();

    let relates = winnow(&store, &[criterion("relates_to", "contains", "Order")]);

    assert!(relates.contains("Invoice"), "the model with the foreign key: {relates}");
    assert!(!relates.contains("checkout_view"), "{relates}");

    let called_by = winnow(&store, &[criterion("called_by", "contains", "checkout_view")]);

    assert!(called_by.contains("Order"), "what the view instantiates: {called_by}");
}

#[test]
fn coverage_is_read_through_the_shared_predicate() {
    let (store, _project) = fixture();

    let tested = winnow(&store, &[criterion("kind", "eq", "model"), criterion("tested", "eq", "true")]);

    assert!(tested.contains("Order"), "the model a TestCase binds to is covered: {tested}");
    assert!(!tested.contains("Invoice"), "the uncovered one is filtered out: {tested}");

    let untested =
        winnow(&store, &[criterion("kind", "eq", "model"), criterion("tested", "eq", "false")]);

    assert!(untested.contains("Invoice"), "and appears in the complement: {untested}");
    assert!(!untested.contains("| Order"), "{untested}");
}

#[test]
fn criteria_are_anded_across_three_axes() {
    let (store, _project) = fixture();

    let text = winnow(
        &store,
        &[
            criterion("kind", "eq", "model"),
            criterion("relates_to", "contains", "Order"),
            criterion("tested", "eq", "false"),
        ],
    );

    assert!(text.contains("Invoice"), "the one symbol satisfying all three: {text}");
    assert!(!text.contains("checkout_view"), "{text}");

    assert!(
        text.contains("AND"),
        "the response restates the composed query so the agent sees what was applied: {text}",
    );
}

#[test]
fn an_unknown_axis_is_rejected_with_the_valid_values() {
    let (store, _project) = fixture();

    let text = winnow(&store, &[criterion("complexity", "gt", "10")]);

    assert!(text.contains("unknown axis"), "the criterion is refused, not ignored: {text}");
    assert!(text.contains("Valid axes"), "and the alternatives are listed: {text}");
    assert!(text.contains("lines"), "including the honest proxy for it: {text}");
}

#[test]
fn an_unknown_op_and_an_unsupported_op_both_name_the_alternatives() {
    let (store, _project) = fixture();

    let unknown = winnow(&store, &[criterion("name", "startswith", "Order")]);

    assert!(unknown.contains("unknown op"), "{unknown}");

    let unsupported = winnow(&store, &[criterion("kind", "contains", "model")]);

    assert!(unsupported.contains("does not accept"), "{unsupported}");
    assert!(unsupported.contains("It accepts"), "{unsupported}");
}

#[test]
fn too_many_criteria_is_refused_rather_than_truncated() {
    let (store, _project) = fixture();

    let criteria: Vec<RawCriterion<'_>> =
        (0..=WINNOW_CRITERIA_MAX).map(|_| criterion("kind", "eq", "model")).collect();

    let text = winnow(&store, &criteria);

    assert!(text.contains("at most"), "the cap is stated: {text}");
    assert!(text.contains("Split the query"), "and a way forward given: {text}");
}

#[test]
fn an_empty_result_reports_the_surviving_count_after_each_criterion() {
    let (store, _project) = fixture();

    let text = winnow(
        &store,
        &[criterion("kind", "eq", "model"), criterion("name", "eq", "NoSuchModel")],
    );

    assert!(text.contains("no symbols match"), "{text}");

    // Every criterion is accounted for, not just whichever one the cost reorder
    // happened to apply last: a criterion that matches plenty on its own reads as
    // the culprit unless the counts above it show the field was already empty.
    assert!(
        text.contains("kind eq model -> "),
        "the first criterion's survivors are counted: {text}",
    );

    assert!(
        text.contains("name eq nosuchmodel -> 0"),
        "and the row that reaches zero is visible: {text}",
    );
}

#[test]
fn an_unknown_rank_lists_the_valid_ranks() {
    let (store, _project) = fixture();

    let text = winnow_text(
        &store,
        &[criterion("kind", "eq", "model")],
        Some("popularity"),
        25,
        &Page::default(),
        0,
    )
    .expect("winnowing");

    assert!(text.contains("unknown rank"), "{text}");
    assert!(text.contains("criticality"), "the valid ranks are listed: {text}");
}

#[test]
fn a_page_offers_a_cursor_and_the_next_page_continues() {
    let (store, _project) = fixture();

    let first = winnow_text(
        &store,
        &[criterion("kind", "in", "model,view,function")],
        Some("name"),
        2,
        &Page::default(),
        7,
    )
    .expect("winnowing");

    assert!(first.contains("next: cursor=2.7"), "a truncated page offers its tail: {first}");

    let page = constellation_mcp::cursor::resolve(Some("2.7"), 7);
    let second = winnow_text(
        &store,
        &[criterion("kind", "in", "model,view,function")],
        Some("name"),
        2,
        &page,
        7,
    )
    .expect("winnowing");

    assert!(!second.contains("next: cursor"), "the last page offers none: {second}");

    let stale = constellation_mcp::cursor::resolve(Some("2.7"), 8);
    let restarted = winnow_text(
        &store,
        &[criterion("kind", "in", "model,view,function")],
        Some("name"),
        2,
        &stale,
        8,
    )
    .expect("winnowing");

    assert!(
        restarted.contains("cursor expired"),
        "a cursor issued against an older index is reported, not silently honoured: {restarted}",
    );
}

#[test]
fn the_same_query_renders_identically_twice() {
    let (store, _project) = fixture();

    let criteria = [criterion("kind", "eq", "model")];

    assert_eq!(
        winnow(&store, &criteria),
        winnow(&store, &criteria),
        "two runs over one index emit byte-identical output",
    );
}
