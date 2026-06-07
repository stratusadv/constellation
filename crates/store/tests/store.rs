use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, UnresolvedRef};
use constellation_store::{FileIndex, Store};

fn sample_node(id: &str, name: &str) -> Node {
    Node::new(
        NodeId::from_raw(id.to_string()),
        ProjectId::new("blog"),
        NodeKind::Function,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("blog.{name}"),
            file_path: "app.py".to_string(),
            language: Language::Python,
        },
        Span::new(1, 1, 0, 0),
        0,
    )
}

fn node_in(project: &str, file: &str, name: &str, kind: NodeKind) -> Node {
    Node::new(
        NodeId::from_raw(format!("{project}::{file}::{name}")),
        ProjectId::new(project),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("{file}::{name}"),
            file_path: file.to_string(),
            language: Language::Python,
        },
        Span::new(1, 1, 0, 0),
        0,
    )
}

fn file_named(path: &'static str) -> FileIndex<'static> {
    FileIndex {
        path,
        content_hash: "hash",
        language: Language::Python,
        size_bytes: 10,
        modified_at_ms: 0,
        source: "",
    }
}

fn sample_file() -> FileIndex<'static> {
    FileIndex {
        path: "app.py",
        content_hash: "hash",
        language: Language::Python,
        size_bytes: 10,
        modified_at_ms: 0,
        source: "",
    }
}

#[test]
fn fts_stays_consistent_when_a_node_id_recurs_in_one_batch() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![
        sample_node("blog::app.py::handler", "handler_old"),
        sample_node("blog::app.py::handler", "handler_new"),
    ];

    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    assert_eq!(store.count_nodes(&project).unwrap(), 1, "the shared id collapses to one row");

    let found = store.search_nodes("handler", 10).unwrap();

    assert_eq!(found.len(), 1, "search returns the one surviving node, got {found:?}");
    assert_eq!(found[0].name, "handler_new", "the later write wins");
}

#[test]
fn search_content_matches_body_identifiers_with_stemming() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let file = FileIndex {
        path: "services.py",
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "def save_model_obj(self):\n    obj.po_number = compute_next()\n",
    };

    store.persist_file(&project, &file, &[], &[], &[], &[], &[]).unwrap();

    let body = store.search_content("po_number", 10).unwrap();

    assert!(
        body.iter().any(|(_, path)| path == "services.py"),
        "a body identifier the name index never sees is found by content search",
    );

    let stemmed = store.search_content("numbers", 10).unwrap();

    assert!(
        stemmed.iter().any(|(_, path)| path == "services.py"),
        "porter stemming matches 'numbers' to the 'number' in po_number",
    );

    assert!(store.search_content("xyzzy", 10).unwrap().is_empty(), "an absent term matches nothing");
}

#[test]
fn fts_reflects_a_reindex_that_renames_a_node() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let first = vec![sample_node("blog::app.py::handler", "alphaname")];
    store.persist_file(&project, &sample_file(), &first, &[], &[], &[], &[]).unwrap();

    let second = vec![sample_node("blog::app.py::handler", "betaname")];
    store.persist_file(&project, &sample_file(), &second, &[], &[], &[], &[]).unwrap();

    assert!(store.search_nodes("alphaname", 10).unwrap().is_empty(), "the old name leaves the index");

    let found = store.search_nodes("betaname", 10).unwrap();

    assert_eq!(found.len(), 1, "the new name is searchable");
}

#[test]
fn kind_counts_groups_nodes_by_kind() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![
        node_in("blog", "models.py", "Article", NodeKind::Model),
        node_in("blog", "models.py", "Comment", NodeKind::Model),
        node_in("blog", "views.py", "index", NodeKind::Function),
    ];

    store.persist_file(&project, &file_named("models.py"), &nodes, &[], &[], &[], &[]).unwrap();

    let counts = store.kind_counts(&project).unwrap();

    let model_count = counts.iter().find(|(kind, _)| *kind == NodeKind::Model).map(|(_, n)| *n);
    let function_count = counts.iter().find(|(kind, _)| *kind == NodeKind::Function).map(|(_, n)| *n);

    assert_eq!(model_count, Some(2), "two models grouped under one kind, got {counts:?}");
    assert_eq!(function_count, Some(1), "one function counted, got {counts:?}");
}

#[test]
fn link_edges_returns_cross_project_edges_with_both_endpoints() {
    let store = Store::open_in_memory().unwrap();
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");

    store.upsert_project(&blog, "blog", "/tmp/blog").unwrap();
    store.upsert_project(&shop, "shop", "/tmp/shop").unwrap();

    let source = node_in("blog", "app.py", "use_widget", NodeKind::Function);
    let target = node_in("shop", "lib.py", "Widget", NodeKind::Class);

    store
        .persist_file(&blog, &file_named("app.py"), std::slice::from_ref(&source), &[], &[], &[], &[])
        .unwrap();

    store
        .persist_file(&shop, &file_named("lib.py"), std::slice::from_ref(&target), &[], &[], &[], &[])
        .unwrap();

    let edge = Edge::new(source.id.clone(), target.id.clone(), EdgeKind::Imports)
        .with_provenance("link:blog->shop");

    // A reference id with no matching unresolved row makes the paired delete a
    // no-op, so this isolates the edge insert the linker performs.
    store.commit_resolved(&[(0, edge)]).unwrap();

    assert_eq!(store.count_links().unwrap(), 1, "the link is counted");

    let links = store.link_edges(None, 10).unwrap();

    assert_eq!(links.len(), 1, "one link edge with both endpoints hydrated");
    assert_eq!(links[0].source.name, "use_widget", "source endpoint resolved");
    assert_eq!(links[0].target.name, "Widget", "target endpoint resolved");
    assert_eq!(links[0].target.project_id.as_str(), "shop", "target carries its own project");
    assert_eq!(links[0].provenance, "link:blog->shop", "provenance preserved");

    // The project filter is pushed into SQL, so it matches by either endpoint
    // regardless of how few links the window would otherwise hold.
    assert_eq!(store.link_edges(Some("blog"), 10).unwrap().len(), 1, "filter matches the source side");
    assert_eq!(store.link_edges(Some("shop"), 10).unwrap().len(), 1, "filter matches the target side");
    assert!(store.link_edges(Some("absent"), 10).unwrap().is_empty(), "unknown project matches nothing");
}

#[test]
fn count_unresolved_named_counts_dark_references() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let caller = node_in("blog", "views.py", "index", NodeKind::Function);

    let reference = UnresolvedRef::new(
        caller.id.clone(),
        "by_year",
        EdgeKind::Calls,
        2,
        0,
        "views.py",
        Language::Python,
    );

    store
        .persist_file(&project, &file_named("views.py"), &[caller], &[], &[reference], &[], &[])
        .unwrap();

    assert_eq!(store.count_unresolved_named("by_year").unwrap(), 1, "the dark reference is counted");
    assert_eq!(store.count_unresolved_named("absent").unwrap(), 0, "an unnamed symbol has none");
}

#[test]
fn search_nodes_any_matches_when_not_all_terms_do() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![sample_node("blog::app.py::alpha_helper", "alpha_helper")];
    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    assert!(store.search_nodes("alpha missing", 10).unwrap().is_empty());

    let any = store.search_nodes_any("alpha missing", 10).unwrap();

    assert_eq!(any.len(), 1, "any-term match finds the node, got {any:?}");
}

#[test]
fn open_in_memory_applies_the_schema() {
    let store = Store::open_in_memory().unwrap();

    assert!(store.schema_version().unwrap() != 0, "init stamps the schema fingerprint");
}

#[test]
fn index_version_defaults_empty_and_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    assert_eq!(store.index_version(&project).unwrap(), "", "a fresh project has no version stamp");

    store.set_index_version(&project, "binary-fingerprint-1").unwrap();

    assert_eq!(
        store.index_version(&project).unwrap(),
        "binary-fingerprint-1",
        "the stamp is read back after being set",
    );
}

#[test]
fn persisting_nodes_and_an_edge_populates_caller_and_callee_lookups() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let caller = node_in("blog", "app.py", "caller", NodeKind::Function);
    let callee = node_in("blog", "app.py", "callee", NodeKind::Function);
    let edge = Edge::new(caller.id.clone(), callee.id.clone(), EdgeKind::Calls).at(3, 0);

    let nodes = vec![caller, callee];

    store
        .persist_file(&project, &file_named("app.py"), &nodes, std::slice::from_ref(&edge), &[], &[], &[])
        .unwrap();

    assert_eq!(store.count_nodes(&project).unwrap(), 2, "both nodes are persisted");
    assert_eq!(store.count_edges().unwrap(), 1, "the edge is persisted");

    let callees = store.callees(&nodes[0].id).unwrap();

    assert_eq!(callees.len(), 1, "the caller has one callee, got {callees:?}");
    assert_eq!(callees[0].0, EdgeKind::Calls, "the edge kind is preserved");
    assert_eq!(callees[0].1.name, "callee", "the callee endpoint is hydrated");

    let callers = store.callers(&nodes[1].id).unwrap();

    assert_eq!(callers.len(), 1, "the callee has one caller");
    assert_eq!(callers[0].1.name, "caller", "the caller endpoint is hydrated");
}

#[test]
fn nodes_kind_in_filters_by_kind() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![
        node_in("blog", "models.py", "Article", NodeKind::Model),
        node_in("blog", "models.py", "save", NodeKind::Function),
    ];

    store.persist_file(&project, &file_named("models.py"), &nodes, &[], &[], &[], &[]).unwrap();

    let models = store.nodes_kind_in(&project, NodeKind::Model).unwrap();

    assert_eq!(models.len(), 1, "only the model matches the kind filter");
    assert_eq!(models[0].name, "Article", "the model is returned");
}

#[test]
fn remove_file_drops_its_nodes() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![sample_node("blog::app.py::handler", "handler")];
    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    assert_eq!(store.count_nodes(&project).unwrap(), 1, "the file's node is present");

    store.remove_file(&project, "app.py").unwrap();

    assert_eq!(store.count_nodes(&project).unwrap(), 0, "removing the file drops its graph");
}

#[test]
fn import_mappings_round_trip_through_a_file() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let mapping = ImportMapping {
        local_name: "do_help".to_string(),
        exported_name: "helper".to_string(),
        source: ".utils".to_string(),
        is_default: false,
        is_namespace: false,
        resolved_path: None,
    };

    store
        .persist_file(&project, &file_named("views.py"), &[], &[], &[], std::slice::from_ref(&mapping), &[])
        .unwrap();

    let mappings = store.import_mappings_in(&project, "views.py").unwrap();

    assert_eq!(mappings.len(), 1, "the mapping is stored against its file");
    assert_eq!(mappings[0].local_name, "do_help", "the local alias survives");
    assert_eq!(mappings[0].exported_name, "helper", "the exported name survives");
    assert_eq!(mappings[0].source, ".utils", "the import source survives");
}

#[test]
fn events_round_trip_through_a_file() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let event = EventRecord {
        role: EventRole::Dispatch,
        event: "cart-updated".to_string(),
        symbol: "blog::cart.js::add".to_string(),
        line: 4,
        column: 2,
    };

    store
        .persist_file(&project, &file_named("cart.js"), &[], &[], &[], &[], std::slice::from_ref(&event))
        .unwrap();

    let events = store.events_for(&project).unwrap();

    assert_eq!(events.len(), 1, "the event is stored");
    assert_eq!(events[0].role, EventRole::Dispatch, "the dispatch role round-trips");
    assert_eq!(events[0].event, "cart-updated", "the event name round-trips");
    assert_eq!(events[0].line, 4, "the source line round-trips");
}

#[test]
fn an_on_disk_store_keeps_its_schema_and_data_across_a_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let project = ProjectId::new("blog");

    let version = {
        let store = Store::open(&path).unwrap();

        store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

        let nodes = vec![sample_node("blog::app.py::handler", "handler")];
        store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

        store.schema_version().unwrap()
    };

    let store = Store::open(&path).unwrap();

    assert_eq!(store.schema_version().unwrap(), version, "reopening keeps the same schema fingerprint");
    assert_eq!(store.count_nodes(&project).unwrap(), 1, "the persisted node survives a reopen");
}

#[test]
fn reference_only_round_trips_and_lists() {
    let store = Store::open_in_memory().unwrap();
    let canonical = ProjectId::new("django-spire");
    let version = ProjectId::new("django-spire@next");

    store.upsert_project(&canonical, "django-spire", "/tmp/spire").unwrap();
    store.upsert_project(&version, "django-spire@next", "/tmp/spire-next").unwrap();

    assert!(
        store.reference_only_project_ids().unwrap().is_empty(),
        "a fresh project defaults to not reference-only",
    );

    store.set_reference_only(&version, true).unwrap();

    let ids = store.reference_only_project_ids().unwrap();

    assert_eq!(ids, vec!["django-spire@next".to_string()], "only the flagged project is listed");

    let flagged = store
        .all_projects()
        .unwrap()
        .into_iter()
        .find(|row| row.id.as_str() == "django-spire@next")
        .expect("the version project row");

    assert!(flagged.reference_only, "all_projects reports the reference-only flag");

    store.set_reference_only(&version, false).unwrap();

    assert!(
        store.reference_only_project_ids().unwrap().is_empty(),
        "clearing the flag removes it from the list",
    );
}
