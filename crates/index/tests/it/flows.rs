//! Execution-flow detection, tracing, and criticality against a Django fixture.

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_index::{FLOW_REACH_NODES_MAX, FlowOptions, compute_flows, retrace_flows};
use constellation_store::{FileIndex, FlowSort, Store};

/// The project every fixture is built under.
const PROJECT: &str = "shop";

/// A node with an explicit kind, name, qualified name, and file.
fn node(kind: NodeKind, name: &str, qualified: &str, file: &str) -> Node {
    Node::new(
        NodeId::from_raw(format!("{PROJECT}::{qualified}")),
        ProjectId::new(PROJECT),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            file_path: file.to_string(),
            language: Language::Python,
        },
        Span::new(1, 10, 0, 0),
        0,
    )
}

/// The file metadata every fixture persists under.
fn file(path: &'static str) -> FileIndex<'static> {
    FileIndex {
        path,
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    }
}

/// A store holding one project, ready for nodes.
fn store() -> (Store, ProjectId) {
    let store = Store::open_in_memory().expect("an in-memory store");
    let project = ProjectId::new(PROJECT);

    store.upsert_project(&project, PROJECT, "/tmp/shop").expect("the project row");

    (store, project)
}

/// The canonical route to view to template to include chain, plus a management
/// command, a migration, and a test, so one fixture exercises detection,
/// tracing, and exclusion together.
fn django_fixture(store: &Store, project: &ProjectId) {
    let route = node(NodeKind::Route, "checkout", "urls.py::route::checkout/", "urls.py");
    let view = node(NodeKind::View, "checkout_view", "views.py::checkout_view", "views.py");
    let service = node(NodeKind::Class, "OrderService", "services.py::OrderService", "services.py");
    let page = node(NodeKind::Template, "shop/checkout.html", "shop/checkout.html", "templates/shop/checkout.html");
    let partial = node(NodeKind::Template, "shop/_totals.html", "shop/_totals.html", "templates/shop/_totals.html");

    let command = node(NodeKind::Class, "Command", "management/commands/sync.py::Command", "management/commands/sync.py");
    let command_step = node(NodeKind::Function, "sync_all", "sync.py::sync_all", "sync.py");

    let migration = node(NodeKind::Class, "Migration", "migrations/0001_initial.py::Migration", "migrations/0001_initial.py");
    let migration_step = node(NodeKind::Function, "forwards", "migrations/0001_initial.py::forwards", "migrations/0001_initial.py");

    let test = node(NodeKind::Function, "test_checkout", "tests/test_views.py::test_checkout", "tests/test_views.py");

    let nodes = vec![
        route.clone(),
        view.clone(),
        service.clone(),
        page.clone(),
        partial.clone(),
        command.clone(),
        command_step.clone(),
        migration.clone(),
        migration_step.clone(),
        test.clone(),
    ];

    let edges = vec![
        Edge::new(route.id.clone(), view.id.clone(), EdgeKind::RoutesTo),
        Edge::new(view.id.clone(), service.id.clone(), EdgeKind::Instantiates),
        Edge::new(view.id.clone(), page.id.clone(), EdgeKind::Renders),
        Edge::new(page.id.clone(), partial.id.clone(), EdgeKind::IncludesTemplate),
        Edge::new(command.id.clone(), command_step.id.clone(), EdgeKind::Calls),
        Edge::new(migration.id.clone(), migration_step.id.clone(), EdgeKind::Calls),
        Edge::new(test.id.clone(), view.id.clone(), EdgeKind::Calls),
    ];

    store
        .persist_file(project, &file("urls.py"), &nodes, &edges, &[], &[], &[])
        .expect("persisting the fixture");
}

#[test]
fn every_route_becomes_a_flow_entry_point_reaching_its_view_and_template() {
    let (store, project) = store();

    django_fixture(&store, &project);

    let stats = compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    assert!(stats.stored > 0, "the fixture produced flows");

    let flows = store.flows(Some(&project), FlowSort::Criticality, 100).expect("reading flows");

    let route_flow = flows
        .iter()
        .find(|flow| flow.entry_kind == "route")
        .expect("the route became a flow entry point");

    assert_eq!(route_flow.name, "checkout/", "the flow is named for its URL pattern");

    let members = store.flow_members(route_flow.id, 100).expect("reading members");
    let names: Vec<&str> = members.iter().map(|(node, _)| node.name.as_str()).collect();

    assert!(names.contains(&"checkout_view"), "the route's flow reaches its view: {names:?}");
    assert!(names.contains(&"shop/checkout.html"), "and the template that view renders: {names:?}");
    assert!(names.contains(&"shop/_totals.html"), "and the include chain below it: {names:?}");
    assert!(names.contains(&"OrderService"), "and what the view instantiates: {names:?}");
}

#[test]
fn a_route_linked_from_a_template_is_recorded_without_being_expanded() {
    let (store, project) = store();

    let route = node(NodeKind::Route, "checkout", "urls.py::route::checkout/", "urls.py");
    let view = node(NodeKind::View, "checkout_view", "views.py::checkout_view", "views.py");

    let page = node(
        NodeKind::Template,
        "shop/checkout.html",
        "shop/checkout.html",
        "templates/shop/checkout.html",
    );

    // The nav a page renders links onward. Expanding those targets would pull in
    // that page's view, its template, and whatever it links to in turn, until one
    // flow holds the whole site and criticality cannot tell two routes apart.
    let linked = node(NodeKind::Route, "account", "urls.py::route::account/", "urls.py");
    let linked_view = node(NodeKind::View, "account_view", "views.py::account_view", "views.py");

    let linked_page = node(
        NodeKind::Template,
        "shop/account.html",
        "shop/account.html",
        "templates/shop/account.html",
    );

    let nodes = vec![
        route.clone(),
        view.clone(),
        page.clone(),
        linked.clone(),
        linked_view.clone(),
        linked_page.clone(),
    ];

    let edges = vec![
        Edge::new(route.id.clone(), view.id.clone(), EdgeKind::RoutesTo),
        Edge::new(view.id.clone(), page.id.clone(), EdgeKind::Renders),
        Edge::new(page.id.clone(), linked.id.clone(), EdgeKind::Resolves),
        Edge::new(linked.id.clone(), linked_view.id.clone(), EdgeKind::RoutesTo),
        Edge::new(linked_view.id.clone(), linked_page.id.clone(), EdgeKind::Renders),
    ];

    store
        .persist_file(&project, &file("urls.py"), &nodes, &edges, &[], &[], &[])
        .expect("persisting the fixture");

    compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    let flows = store.flows(Some(&project), FlowSort::Criticality, 100).expect("reading flows");

    let checkout = flows
        .iter()
        .find(|flow| flow.name == "checkout/")
        .expect("the checkout route became a flow");

    let members = store.flow_members(checkout.id, 100).expect("reading members");
    let names: Vec<&str> = members.iter().map(|(node, _)| node.name.as_str()).collect();

    assert!(names.contains(&"account"), "the link target itself is recorded: {names:?}");
    assert!(!names.contains(&"account_view"), "but the walk stops at it: {names:?}");
    assert!(!names.contains(&"shop/account.html"), "never reaching its template: {names:?}");
}

#[test]
fn a_management_command_is_detected_and_a_migration_is_not() {
    let (store, project) = store();

    django_fixture(&store, &project);
    compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    let flows = store.flows(Some(&project), FlowSort::Criticality, 100).expect("reading flows");
    let kinds: Vec<&str> = flows.iter().map(|flow| flow.entry_kind.as_str()).collect();

    assert!(kinds.contains(&"management_command"), "a management command is an entry point: {kinds:?}");

    let members: Vec<String> = flows
        .iter()
        .flat_map(|flow| store.flow_members(flow.id, 100).unwrap_or_default())
        .map(|(node, _)| node.file_path)
        .collect();

    assert!(
        members.iter().all(|path| !path.contains("migrations/")),
        "a migration never seeds or joins a flow: {members:?}",
    );
}

#[test]
fn tests_are_excluded_by_default_and_included_with_the_flag() {
    let (store, project) = store();

    django_fixture(&store, &project);

    compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    let default_entries: Vec<String> = store
        .flows(Some(&project), FlowSort::Criticality, 100)
        .expect("reading flows")
        .into_iter()
        .map(|flow| flow.entry_node_id)
        .collect();

    assert!(
        default_entries.iter().all(|id| !id.contains("tests/")),
        "a test suite is not a user-facing execution path: {default_entries:?}",
    );

    compute_flows(&store, &project, FlowOptions { depth_max: 15, include_tests: true })
        .expect("computing flows with tests");

    let with_tests: Vec<String> = store
        .flows(Some(&project), FlowSort::Criticality, 100)
        .expect("reading flows")
        .into_iter()
        .map(|flow| flow.entry_node_id)
        .collect();

    assert!(
        with_tests.iter().any(|id| id.contains("tests/")),
        "the flag includes them: {with_tests:?}",
    );
}

#[test]
fn criticality_is_deterministic_across_runs() {
    let (store, project) = store();

    django_fixture(&store, &project);

    compute_flows(&store, &project, FlowOptions::default()).expect("first run");

    let first: Vec<(String, u64)> = store
        .flows(Some(&project), FlowSort::Name, 100)
        .expect("reading flows")
        .into_iter()
        .map(|flow| (flow.name, flow.criticality.to_bits()))
        .collect();

    compute_flows(&store, &project, FlowOptions::default()).expect("second run");

    let second: Vec<(String, u64)> = store
        .flows(Some(&project), FlowSort::Name, 100)
        .expect("reading flows")
        .into_iter()
        .map(|flow| (flow.name, flow.criticality.to_bits()))
        .collect();

    assert_eq!(first, second, "two runs over one graph score bit-identically");
}

#[test]
fn a_route_flow_outranks_an_isolated_true_root() {
    let (store, project) = store();

    django_fixture(&store, &project);
    compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    let flows = store.flows(Some(&project), FlowSort::Criticality, 100).expect("reading flows");

    let route = flows.iter().find(|flow| flow.entry_kind == "route").expect("the route flow");
    let command = flows
        .iter()
        .find(|flow| flow.entry_kind == "management_command")
        .expect("the command flow");

    assert!(
        route.criticality > command.criticality,
        "a URL route outranks a management command: {} vs {}",
        route.criticality,
        command.criticality,
    );
}

#[test]
fn a_single_node_entry_point_is_discarded_rather_than_stored() {
    let (store, project) = store();

    // A route that routes nowhere: an entry point whose reach set is itself.
    let orphan = node(NodeKind::Route, "dangling", "urls.py::route::dangling/", "urls.py");

    store
        .persist_file(&project, &file("urls.py"), std::slice::from_ref(&orphan), &[], &[], &[], &[])
        .expect("persisting the fixture");

    let stats = compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    assert!(stats.entries > 0, "the route was detected as an entry point");
    assert_eq!(stats.stored, 0, "but a flow reaching nothing carries no information, so it is dropped");
}

#[test]
fn an_empty_flows_table_is_an_honest_empty() {
    let (store, project) = store();

    django_fixture(&store, &project);

    assert_eq!(store.count_flows(&project).expect("counting flows"), 0, "nothing before the pass runs");

    assert!(
        store.flows(Some(&project), FlowSort::Criticality, 10).expect("reading flows").is_empty(),
        "and the listing is empty rather than wrong",
    );
}

#[test]
fn recomputing_replaces_rather_than_accumulates() {
    let (store, project) = store();

    django_fixture(&store, &project);

    compute_flows(&store, &project, FlowOptions::default()).expect("first run");
    let first = store.count_flows(&project).expect("counting flows");

    compute_flows(&store, &project, FlowOptions::default()).expect("second run");
    let second = store.count_flows(&project).expect("counting flows");

    assert_eq!(first, second, "a recompute replaces the previous flows wholesale");
    assert!(first > 0, "and the fixture does produce flows");
}

#[test]
fn an_incremental_retrace_produces_the_same_flows_as_a_full_recompute() {
    let full = {
        let (store, project) = store();

        django_fixture(&store, &project);
        compute_flows(&store, &project, FlowOptions::default()).expect("the full recompute");

        flow_shapes(&store, &project)
    };

    let incremental = {
        let (store, project) = store();

        django_fixture(&store, &project);
        compute_flows(&store, &project, FlowOptions::default()).expect("the baseline");

        // The file every entry point in the fixture reaches through, so the
        // retrace has to rebuild all of them rather than a convenient subset.
        retrace_flows(
            &store,
            &project,
            &["views.py".to_string(), "management/commands/sync.py".to_string()],
            FlowOptions::default(),
        )
        .expect("the incremental retrace");

        flow_shapes(&store, &project)
    };

    assert!(!full.is_empty(), "the fixture produces flows to compare");

    assert_eq!(
        incremental, full,
        "retracing the affected flows lands exactly where a full recompute does",
    );
}

#[test]
fn a_retrace_neither_duplicates_nor_drops_an_untouched_flow() {
    let (store, project) = store();

    django_fixture(&store, &project);
    compute_flows(&store, &project, FlowOptions::default()).expect("the baseline");

    let before = flow_shapes(&store, &project);

    // A file no flow's reach set contains: nothing is stale, so nothing is
    // deleted and nothing is inserted.
    retrace_flows(&store, &project, &["unrelated.py".to_string()], FlowOptions::default())
        .expect("retracing an unrelated change");

    assert_eq!(flow_shapes(&store, &project), before, "an unrelated change leaves flows alone");

    // And a real one runs the delete-and-insert without accumulating.
    retrace_flows(&store, &project, &["views.py".to_string()], FlowOptions::default())
        .expect("retracing a real change");

    assert_eq!(
        flow_shapes(&store, &project),
        before,
        "retracing the same graph twice neither duplicates a flow nor loses one",
    );
}

/// The stored flows reduced to the fields that must survive a retrace
/// unchanged, sorted so two stores compare regardless of insertion order. The
/// row id is deliberately excluded: a retrace reinserts, so ids legitimately
/// differ while the flows themselves do not.
fn flow_shapes(store: &Store, project: &ProjectId) -> Vec<(String, String, u32, u32, u64)> {
    let mut shapes: Vec<(String, String, u32, u32, u64)> = store
        .flows(Some(project), FlowSort::Criticality, 10_000)
        .expect("reading flows")
        .into_iter()
        .map(|flow| {
            (
                flow.name,
                flow.entry_kind,
                flow.node_count,
                flow.app_count,
                flow.criticality.to_bits(),
            )
        })
        .collect();

    shapes.sort();

    shapes
}

#[test]
fn a_reach_set_is_bounded_by_the_node_cap() {
    let (store, project) = store();

    // A fan-out wider than a reach set may hold. Width rather than depth,
    // because the depth bound would cut a long chain long before the node cap
    // ever engaged, and it is the node cap under test here.
    let count = FLOW_REACH_NODES_MAX + 200;

    let mut nodes: Vec<Node> = Vec::with_capacity(count + 1);
    let mut edges: Vec<Edge> = Vec::with_capacity(count);

    let entry = node(NodeKind::Route, "wide", "urls.py::route::wide/", "urls.py");
    nodes.push(entry.clone());

    for index in 0..count {
        let step = node(
            NodeKind::Function,
            &format!("step{index}"),
            &format!("fanout.py::step{index}"),
            "fanout.py",
        );

        edges.push(Edge::new(entry.id.clone(), step.id.clone(), EdgeKind::Calls));
        nodes.push(step);
    }

    store
        .persist_file(&project, &file("urls.py"), &nodes, &edges, &[], &[], &[])
        .expect("persisting the fan-out");

    let stats = compute_flows(&store, &project, FlowOptions::default()).expect("computing flows");

    let flows = store.flows(Some(&project), FlowSort::Size, 10).expect("reading flows");
    let widest = flows.first().expect("at least one flow");

    assert!(
        widest.node_count as usize <= FLOW_REACH_NODES_MAX,
        "the reach set respects its cap, got {}",
        widest.node_count,
    );

    assert!(stats.truncated > 0, "and the truncation is reported rather than silent");
    assert!(widest.truncated, "the stored row carries the marker too");
}
