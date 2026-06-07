use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_mcp::{
    EXPLORE_BYTES_BASE, EXPLORE_BYTES_MAX, explore_budget, feature_text, is_flow_edge, model_text,
    path_penalty, qualified_name_ends_with, rank_by_structure, routes_text, shortest_flow_path,
};
use constellation_store::{FileIndex, Store};

#[test]
fn rwr_ranks_connected_above_disconnected() {
    let adjacency: Vec<Vec<u32>> = vec![vec![1], vec![0, 2], vec![1], vec![]];
    let seeds = vec![0usize];

    let ranked = rank_by_structure(&seeds, &adjacency);

    assert_eq!(ranked.first(), Some(&0), "the seed ranks first");
    assert!(ranked.contains(&1), "a direct neighbor is included");
    assert!(ranked.contains(&2), "a two-hop neighbor is included");
    assert!(!ranked.contains(&3), "an unconnected node is excluded");

    let position = |target: usize| ranked.iter().position(|&node| node == target);
    assert!(position(1) < position(2), "the closer neighbor ranks higher");
}

#[test]
fn explore_budget_grows_with_graph_size_then_caps() {
    let small = explore_budget(100);
    let large = explore_budget(50_000);

    assert!(small >= EXPLORE_BYTES_BASE, "a tiny graph still gets the base budget");
    assert!(large > small, "a bigger graph earns a bigger budget");
    assert!(large <= EXPLORE_BYTES_MAX, "the budget is capped");
    assert_eq!(explore_budget(usize::MAX), EXPLORE_BYTES_MAX, "saturates, never overflows");
}

#[test]
fn path_penalty_sinks_tests_and_generated_below_source() {
    assert_eq!(path_penalty("app/views.py"), 0, "hand-written source ranks first");
    assert_eq!(path_penalty("app/tests/test_views.py"), 1, "tests rank below source");
    assert_eq!(path_penalty("app/migrations/0001_initial.py"), 2, "generated ranks last");
    assert_eq!(path_penalty("migrations/0002_auto.py"), 2, "a top-level segment also matches");
    assert_eq!(path_penalty("static/js/app.min.js"), 2, "minified ranks last");
}

#[test]
fn sorting_by_penalty_is_stable_and_orders_source_first() {
    let mut paths = vec![
        "app/migrations/0001_initial.py",
        "app/tests/test_a.py",
        "app/models.py",
        "app/views.py",
    ];

    paths.sort_by_key(|path| path_penalty(path));

    assert_eq!(
        paths,
        vec![
            "app/models.py",
            "app/views.py",
            "app/tests/test_a.py",
            "app/migrations/0001_initial.py",
        ],
        "source keeps input order, then tests, then generated",
    );
}

fn model_node(name: &str, kind: NodeKind) -> Node {
    let mut node = Node::new(
        NodeId::from_raw(format!("blog::models.py::{name}")),
        ProjectId::new("blog"),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("models.py::{name}"),
            file_path: "models.py".to_string(),
            language: Language::Python,
        },
        Span::new(1, 1, 0, 0),
        0,
    );

    node.signature = Some(format!("{name} = CharField()"));

    node
}

fn field_node(owner: &str, name: &str) -> Node {
    Node::new(
        NodeId::from_raw(format!("blog::models.py::{owner}.{name}")),
        ProjectId::new("blog"),
        NodeKind::Field,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("models.py::{owner}.{name}"),
            file_path: "models.py".to_string(),
            language: Language::Python,
        },
        Span::new(2, 2, 0, 0),
        0,
    )
}

#[test]
fn model_text_assembles_inherited_fields_and_shadows_overrides() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let article = model_node("Article", NodeKind::Model);
    let base = model_node("TimeStampedModel", NodeKind::Model);
    let author = model_node("Author", NodeKind::Model);
    let own_title = field_node("Article", "title");
    let base_created = field_node("TimeStampedModel", "created_at");
    let base_title = field_node("TimeStampedModel", "title");

    let nodes = vec![
        article.clone(),
        base.clone(),
        author.clone(),
        own_title.clone(),
        base_created.clone(),
        base_title.clone(),
    ];

    let edges = vec![
        Edge::new(article.id.clone(), own_title.id.clone(), EdgeKind::Contains),
        Edge::new(article.id.clone(), base.id.clone(), EdgeKind::Extends),
        Edge::new(article.id.clone(), author.id.clone(), EdgeKind::RelatesTo),
        Edge::new(base.id.clone(), base_created.id.clone(), EdgeKind::Contains),
        Edge::new(base.id.clone(), base_title.id.clone(), EdgeKind::Contains),
    ];

    let file = FileIndex {
        path: "models.py",
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    };

    store.persist_file(&project, &file, &nodes, &edges, &[], &[], &[]).unwrap();

    let text = model_text(&store, "Article").unwrap();

    assert!(text.contains("bases: TimeStampedModel"), "the base is listed: {text}");
    assert!(text.contains("[own] title"), "the own field is marked own: {text}");
    assert!(text.contains("[TimeStampedModel] created_at"), "the inherited field is attributed: {text}");
    assert!(text.contains("fields (2)"), "the shadowed base 'title' is not double counted: {text}");
    assert!(text.contains("Author"), "the relation target is listed: {text}");

    assert!(
        !text.contains("[TimeStampedModel] title"),
        "the base 'title' is shadowed by the own field, not listed: {text}",
    );
}

/// A node with an arbitrary id, kind, name, qualified name, and file (for the
/// route/view/template fixtures the model helpers above do not cover).
fn make_node(id: &str, kind: NodeKind, name: &str, qualified: &str, file: &str) -> Node {
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
        Span::new(1, 1, 0, 0),
        0,
    )
}

fn blog_file() -> FileIndex<'static> {
    FileIndex {
        path: "models.py",
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    }
}

#[test]
fn model_text_labels_relation_direction() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let article = model_node("Article", NodeKind::Model);
    let author = model_node("Author", NodeKind::Model);
    let comment = model_node("Comment", NodeKind::Model);

    let nodes = vec![article.clone(), author.clone(), comment.clone()];

    // Article declares a ForeignKey to Author (forward); the reverse-relation
    // synthesis pass adds Article -> Comment tagged as a reverse accessor.
    let edges = vec![
        Edge::new(article.id.clone(), author.id.clone(), EdgeKind::RelatesTo),
        Edge::new(article.id.clone(), comment.id.clone(), EdgeKind::RelatesTo)
            .with_provenance("synthesis:reverse-relation"),
    ];

    store.persist_file(&project, &blog_file(), &nodes, &edges, &[], &[], &[]).unwrap();

    let text = model_text(&store, "Article").unwrap();

    assert!(text.contains("[->] Author"), "a forward FK is marked outward: {text}");
    assert!(text.contains("[<-] Comment"), "a reverse accessor is marked back: {text}");
}

#[test]
fn routes_text_filters_to_routes_matching_a_pattern() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let detail_route = make_node(
        "blog::urls.py::route::<int:pk>/detail/",
        NodeKind::Route,
        "detail",
        "urls.py::route::<int:pk>/detail/",
        "urls.py",
    );
    let list_route =
        make_node("blog::urls.py::route::list/", NodeKind::Route, "list", "urls.py::route::list/", "urls.py");
    let detail_view =
        make_node("blog::views.py::detail_view", NodeKind::View, "detail_view", "views.py::detail_view", "views.py");
    let list_view =
        make_node("blog::views.py::list_view", NodeKind::View, "list_view", "views.py::list_view", "views.py");
    let template = make_node(
        "blog::blog/detail.html",
        NodeKind::Template,
        "blog/detail.html",
        "blog/detail.html",
        "templates/blog/detail.html",
    );

    let nodes =
        vec![detail_route.clone(), list_route.clone(), detail_view.clone(), list_view.clone(), template.clone()];

    let edges = vec![
        Edge::new(detail_route.id.clone(), detail_view.id.clone(), EdgeKind::RoutesTo),
        Edge::new(list_route.id.clone(), list_view.id.clone(), EdgeKind::RoutesTo),
        Edge::new(detail_view.id.clone(), template.id.clone(), EdgeKind::Renders),
    ];

    store.persist_file(&project, &blog_file(), &nodes, &edges, &[], &[], &[]).unwrap();

    let filtered = routes_text(&store, Some("blog"), Some("detail")).unwrap();

    assert!(filtered.contains("detail_view"), "the matching route's view is shown: {filtered}");
    assert!(filtered.contains("blog/detail.html"), "and the template it renders: {filtered}");
    assert!(!filtered.contains("list_view"), "the non-matching route is filtered out: {filtered}");
    assert!(filtered.contains("matching \"detail\""), "the header notes the active filter: {filtered}");

    let unmatched = routes_text(&store, Some("blog"), Some("zzz")).unwrap();

    assert!(unmatched.contains("no routes matching"), "an unmatched pattern says so: {unmatched}");
}

#[test]
fn feature_text_disambiguates_an_overloaded_name() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    // Four views all named detail_view across different apps; slicing them as
    // one feature would interleave four unrelated request paths.
    let views: Vec<Node> = (0..4)
        .map(|index| {
            make_node(
                &format!("blog::app{index}/views.py::detail_view"),
                NodeKind::View,
                "detail_view",
                &format!("app{index}/views.py::detail_view"),
                &format!("app{index}/views.py"),
            )
        })
        .collect();

    store.persist_file(&project, &blog_file(), &views, &[], &[], &[], &[]).unwrap();

    let text = feature_text(&store, "detail_view").unwrap();

    assert!(text.contains("4 definitions"), "it reports the overload count: {text}");
    assert!(text.contains("too many to slice"), "it refuses to slice and asks to narrow: {text}");
    assert!(text.contains("app0/views.py::detail_view"), "it lists the file::name to pass: {text}");
}

#[test]
fn qualified_name_ends_with_matches_only_at_name_boundaries() {
    assert!(
        qualified_name_ends_with(
            "app/services.py::PaymentGatewayService.save_model_obj",
            "PaymentGatewayService.save_model_obj",
        ),
        "the owner.member tail matches after the :: file boundary",
    );

    assert!(
        qualified_name_ends_with("a/b.py::Outer.Inner.run", "Inner.run"),
        "a nested tail matches after a . boundary",
    );

    assert!(
        qualified_name_ends_with("save_model_obj", "save_model_obj"),
        "an exactly equal qualified name matches",
    );

    assert!(
        !qualified_name_ends_with("a/b.py::Resync.save", "ync.save"),
        "a suffix landing mid-identifier does not match",
    );

    assert!(
        !qualified_name_ends_with("a/b.py::Other.save_model_obj", "Gateway.save_model_obj"),
        "a different owner does not match",
    );
}

#[test]
fn flow_traces_shortest_call_path_over_flow_edges_only() {
    // 0 -calls-> 1 -calls-> 2 ; 0 -contains-> 3 ; 0 -calls-> 2 directly.
    let out_edges = vec![
        vec![(1u32, EdgeKind::Calls), (3u32, EdgeKind::Contains), (2u32, EdgeKind::Calls)],
        vec![(2u32, EdgeKind::Calls)],
        vec![],
        vec![],
    ];

    // shortest path 0->2 is the direct call, not via 1.
    assert_eq!(shortest_flow_path(&out_edges, 0, 2), Some(vec![(2, EdgeKind::Calls)]));

    // no directed path 2->0.
    assert_eq!(shortest_flow_path(&out_edges, 2, 0), None);

    // a contains edge is not flow, so 0->3 has no path.
    assert_eq!(shortest_flow_path(&out_edges, 0, 3), None);

    assert!(is_flow_edge(EdgeKind::Calls) && is_flow_edge(EdgeKind::RoutesTo));
    assert!(is_flow_edge(EdgeKind::Extends), "path advertises tracing inheritance");

    assert!(
        is_flow_edge(EdgeKind::ExtendsTemplate) && is_flow_edge(EdgeKind::IncludesTemplate),
        "path traces template inheritance (view -> page -> base layout)",
    );

    assert!(!is_flow_edge(EdgeKind::Contains) && !is_flow_edge(EdgeKind::Imports));
}

#[test]
fn flow_traces_multi_hop_when_no_direct_edge() {
    // 0 -calls-> 1 -calls-> 2, no direct 0->2.
    let out_edges = vec![vec![(1u32, EdgeKind::Calls)], vec![(2u32, EdgeKind::Calls)], vec![]];

    assert_eq!(
        shortest_flow_path(&out_edges, 0, 2),
        Some(vec![(1, EdgeKind::Calls), (2, EdgeKind::Calls)]),
    );
}
