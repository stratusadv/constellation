use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, UnresolvedRef};
use constellation_store::{
    CommitFile, CommitRecord, FileIndex, Store, SymbolChange, SymbolRevision,
};

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
        source: "def save_model_obj(self):\n    obj.order_number = compute_next()\n",
    };

    store.persist_file(&project, &file, &[], &[], &[], &[], &[]).unwrap();

    let body = store.search_content("order_number", 10).unwrap();

    assert!(
        body.iter().any(|(_, path)| path == "services.py"),
        "a body identifier the name index never sees is found by content search",
    );

    let stemmed = store.search_content("numbers", 10).unwrap();

    assert!(
        stemmed.iter().any(|(_, path)| path == "services.py"),
        "porter stemming matches 'numbers' to the 'number' in order_number",
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

/// The search for a symbol's own name returns that symbol, even when many others
/// contain the word and the result set is truncated below their number.
///
/// The regression test for a search that had no `ORDER BY` at all: matches came
/// back in FTS rowid order, so on a real index the `Inventory` model did not
/// appear in the top forty hits for "Inventory", crowded out by the views,
/// forms, and serializers that merely contain the string. Measured against a
/// goldset, that alone cost two thirds of the achievable mean reciprocal rank.
#[test]
fn an_exact_name_outranks_the_symbols_that_merely_contain_it() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    // The decoys are persisted first, so rowid order alone would bury the exact
    // match: this fails without the ordering and passes with it.
    let mut nodes: Vec<Node> = (0..40)
        .map(|index| {
            let name = format!("Inventory{index}Serializer");

            sample_node(&format!("blog::app.py::{name}"), &name)
        })
        .collect();

    nodes.push(sample_node("blog::app.py::Inventory", "Inventory"));

    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    let hits = store.search_nodes("Inventory", 5).unwrap();

    assert_eq!(
        hits.first().map(|node| node.name.as_str()),
        Some("Inventory"),
        "the symbol actually called Inventory ranks first, got {:?}",
        hits.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(),
    );
}

#[test]
fn a_name_prefix_outranks_a_name_that_merely_contains_the_query() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes = vec![
        sample_node("blog::app.py::CustomerOrderLine", "CustomerOrderLine"),
        sample_node("blog::app.py::OrderService", "OrderService"),
    ];

    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    let hits = store.search_nodes("Order", 5).unwrap();

    assert_eq!(
        hits.first().map(|node| node.name.as_str()),
        Some("OrderService"),
        "a name starting with the query beats one containing it, got {:?}",
        hits.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(),
    );
}

#[test]
fn search_ordering_is_stable_across_identical_runs() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let nodes: Vec<Node> = (0..12)
        .map(|index| {
            let name = format!("ReportBuilder{index}");

            sample_node(&format!("blog::app.py::{name}"), &name)
        })
        .collect();

    store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();

    let names = |hits: Vec<Node>| -> Vec<String> { hits.into_iter().map(|n| n.name).collect() };

    assert_eq!(
        names(store.search_nodes("ReportBuilder", 12).unwrap()),
        names(store.search_nodes("ReportBuilder", 12).unwrap()),
        "equal candidates order identically run to run",
    );
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

    assert_eq!(store.schema_version().unwrap(), version, "reopening keeps the same schema version");
    assert_eq!(store.count_nodes(&project).unwrap(), 1, "the persisted node survives a reopen");
}

/// A database written before a table was added is upgraded in place, not deleted.
///
/// This is the regression test for the schema-hash design that preceded explicit
/// versioning: it stamped a hash of the whole schema file into `user_version` and
/// deleted the database whenever the hash moved, so *adding* a table destroyed
/// every existing index on the next open, on a read path, with no warning.
///
/// The older database is simulated by dropping the newest tables from a current
/// one rather than by checking in a binary fixture, which would rot. The property
/// under test is the one that matters either way: re-applying the schema creates
/// what is missing and keeps what is there.
#[test]
fn a_database_missing_a_newly_added_table_is_upgraded_rather_than_discarded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let project = ProjectId::new("blog");

    {
        let store = Store::open(&path).unwrap();

        store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

        let nodes = vec![sample_node("blog::app.py::handler", "handler")];
        store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();
    }

    // Every table a later release appended, removed: exactly the shape of a
    // database built by a previous one. `resolved_refs` is listed because an
    // additive table must reach an existing database through the schema
    // re-apply, not through a version bump that would make an older build
    // discard the whole index over a table it never queries.
    let connection = rusqlite::Connection::open(&path).unwrap();

    for table in ["flow_membership", "flow", "resolved_refs"] {
        connection.execute_batch(&format!("DROP TABLE IF EXISTS {table}")).unwrap();
    }

    drop(connection);

    let store = Store::open(&path).unwrap();

    assert_eq!(
        store.count_nodes(&project).unwrap(),
        1,
        "the indexed graph survives an additive schema change",
    );

    assert_eq!(
        store.count_flows(&project).unwrap(),
        0,
        "and the table added by the upgrade exists, empty, rather than erroring",
    );

    assert_eq!(
        store.requeue_refs_into_file(&project, "app.py").unwrap(),
        0,
        "the reference archive is queryable too, so no version bump was needed to add it",
    );
}

#[test]
fn a_database_stamped_by_an_unrecognized_schema_is_discarded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let project = ProjectId::new("blog");

    {
        let store = Store::open(&path).unwrap();

        store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

        let nodes = vec![sample_node("blog::app.py::handler", "handler")];
        store.persist_file(&project, &sample_file(), &nodes, &[], &[], &[], &[]).unwrap();
    }

    // What every database written before versioning carries: a hash of the schema
    // file in the pragma that now holds a version. It names no schema this build
    // knows, so rebuilding is the only honest reading of it.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 1_234_567_i32).unwrap();
    drop(connection);

    let store = Store::open(&path).unwrap();

    assert_eq!(
        store.count_nodes(&project).unwrap(),
        0,
        "an unrecognized schema is rebuilt rather than read as though it matched",
    );
}

#[test]
fn a_database_from_a_newer_build_is_discarded_rather_than_misread() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.db");
    let project = ProjectId::new("blog");

    {
        let store = Store::open(&path).unwrap();

        store.upsert_project(&project, "blog", "/tmp/blog").unwrap();
    }

    let current = Store::open(&path).unwrap().schema_version().unwrap();

    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "user_version", i32::try_from(current).unwrap() + 1)
        .unwrap();
    drop(connection);

    let store = Store::open(&path).unwrap();

    assert_eq!(
        store.schema_version().unwrap(),
        current,
        "a database from a newer constellation is rebuilt at this build's version",
    );
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

#[test]
fn history_round_trips_and_aggregates_churn_by_path() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("demo");
    store.upsert_project(&project, "demo", "/tmp/demo").unwrap();

    let commits = vec![
        CommitRecord {
            commit_hash: "a".repeat(40),
            author: "Ada".to_string(),
            committed_at: 1_700_000_000,
            summary: "add orders".to_string(),
            files: vec![
                CommitFile { file_path: "orders/models.py".to_string(), insertions: 10, deletions: 0 },
                CommitFile { file_path: "core/util.py".to_string(), insertions: 2, deletions: 1 },
            ],
        },
        CommitRecord {
            commit_hash: "b".repeat(40),
            author: "Bob".to_string(),
            committed_at: 1_700_100_000,
            summary: "tweak orders".to_string(),
            files: vec![CommitFile {
                file_path: "orders/views.py".to_string(),
                insertions: 4,
                deletions: 4,
            }],
        },
    ];

    let stored = store.replace_history(&project, &commits).unwrap();
    assert_eq!(stored, 2);
    assert_eq!(store.count_history_commits(&project).unwrap(), 2);

    let hits = store.history_for_path(Some(&project), "%orders/%", 10).unwrap();
    assert_eq!(hits.len(), 2, "both commits touched orders/");
    assert_eq!(hits[0].commit_hash, "b".repeat(40), "newest commit first");
    assert_eq!(hits[0].insertions, 4);
    assert_eq!(hits[1].insertions, 10, "core/util.py excluded from the orders/ churn");
    assert_eq!(hits[1].files_changed, 1);

    let stored_again = store.replace_history(&project, &[]).unwrap();
    assert_eq!(stored_again, 0, "replace clears the prior history");
    assert_eq!(store.count_history_commits(&project).unwrap(), 0);
}

#[test]
fn symbol_revisions_round_trip_query_and_cascade() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("demo");
    store.upsert_project(&project, "demo", "/tmp/demo").unwrap();

    // symbol_revisions foreign-key to commits, so the commits must exist first.
    let commits = vec![
        CommitRecord {
            commit_hash: "a".repeat(40),
            author: "Ada".to_string(),
            committed_at: 1_700_000_000,
            summary: "add Order".to_string(),
            files: vec![CommitFile { file_path: "orders/models.py".to_string(), insertions: 5, deletions: 0 }],
        },
        CommitRecord {
            commit_hash: "b".repeat(40),
            author: "Bob".to_string(),
            committed_at: 1_700_100_000,
            summary: "add total field".to_string(),
            files: vec![CommitFile { file_path: "orders/models.py".to_string(), insertions: 1, deletions: 0 }],
        },
    ];
    store.replace_history(&project, &commits).unwrap();

    let revisions = vec![
        SymbolRevision {
            commit_hash: "a".repeat(40),
            file_path: "orders/models.py".to_string(),
            qualified_name: "orders.Order".to_string(),
            name: "Order".to_string(),
            kind: "model".to_string(),
            change: SymbolChange::Added,
            signature: None,
        },
        SymbolRevision {
            commit_hash: "b".repeat(40),
            file_path: "orders/models.py".to_string(),
            qualified_name: "orders.Order.total".to_string(),
            name: "total".to_string(),
            kind: "field".to_string(),
            change: SymbolChange::Added,
            signature: Some("DecimalField".to_string()),
        },
    ];

    let stored = store.replace_symbol_revisions(&project, &revisions).unwrap();
    assert_eq!(stored, 2);
    assert_eq!(store.count_symbol_revisions(&project).unwrap(), 2);

    let order = store.symbol_history(Some(&project), "Order", 10).unwrap();
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].qualified_name, "orders.Order");
    assert_eq!(order[0].change, "added");
    assert_eq!(order[0].summary, "add Order", "the commit subject is joined in");

    let total = store.symbol_history(Some(&project), "total", 10).unwrap();
    assert_eq!(total.len(), 1, "matched by exact name; the suffix match excludes the parent class");
    assert_eq!(total[0].kind, "field");
    assert_eq!(total[0].signature.as_deref(), Some("DecimalField"));

    let touches = store.history_file_touches(&project, 100).unwrap();
    assert_eq!(touches.len(), 2);
    assert_eq!(touches[0].commit_hash, "a".repeat(40), "oldest commit first");
    assert_eq!(touches[1].commit_hash, "b".repeat(40));

    store.replace_history(&project, &[]).unwrap();
    assert_eq!(
        store.count_symbol_revisions(&project).unwrap(),
        0,
        "re-ingesting commits cascades symbol revisions away",
    );
}

#[test]
fn symbol_history_matches_an_owner_member_past_the_file_prefix() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("demo");
    store.upsert_project(&project, "demo", "/tmp/demo").unwrap();

    assert!(
        !store.has_symbol_revisions(Some(&project)).unwrap(),
        "a fresh project has no symbol history",
    );
    assert!(!store.has_symbol_revisions(None).unwrap(), "no project has any history either");

    let commits = vec![CommitRecord {
        commit_hash: "a".repeat(40),
        author: "Ada".to_string(),
        committed_at: 1_700_000_000,
        summary: "add quantity".to_string(),
        files: vec![CommitFile {
            file_path: "app/orders/models.py".to_string(),
            insertions: 3,
            deletions: 0,
        }],
    }];
    store.replace_history(&project, &commits).unwrap();

    // The live extractor qualifies a member as `file_path::Owner.member`, so the
    // character just before the owner is the `::` of the file prefix, not a `.`. The
    // `%.{symbol}` suffix alone could never cross it, so an `Owner.member` query missed.
    let revisions = vec![SymbolRevision {
        commit_hash: "a".repeat(40),
        file_path: "app/orders/models.py".to_string(),
        qualified_name: "app/orders/models.py::Order.quantity".to_string(),
        name: "quantity".to_string(),
        kind: "field".to_string(),
        change: SymbolChange::Added,
        signature: Some("IntegerField".to_string()),
    }];
    store.replace_symbol_revisions(&project, &revisions).unwrap();

    assert!(store.has_symbol_revisions(Some(&project)).unwrap(), "the symbol pass populated history");

    let by_member = store.symbol_history(Some(&project), "Order.quantity", 10).unwrap();
    assert_eq!(by_member.len(), 1, "an Owner.member query matches past the file prefix");
    assert_eq!(by_member[0].qualified_name, "app/orders/models.py::Order.quantity");
    assert_eq!(by_member[0].signature.as_deref(), Some("IntegerField"));

    let by_bare = store.symbol_history(Some(&project), "quantity", 10).unwrap();
    assert_eq!(by_bare.len(), 1, "the bare field name still matches by name");
}

#[test]
fn unresolved_calls_surface_by_name_and_by_source() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");
    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    // A caller whose `obj.services.save_model_obj(...)` the resolver dropped: an
    // overloaded/builtin service method on a descriptor receiver, recorded unbound.
    let caller = node_in("blog", "views.py", "update_view", NodeKind::Function);

    let save_ref = UnresolvedRef::new(
        caller.id.clone(),
        "save_model_obj",
        EdgeKind::Calls,
        96,
        8,
        "views.py",
        Language::Python,
    );

    // A Django queryset builtin on an untyped receiver: incidental dynamic dispatch,
    // must be excluded from the callee view.
    let builtin_ref = UnresolvedRef::new(
        caller.id.clone(),
        "all",
        EdgeKind::Calls,
        50,
        4,
        "views.py",
        Language::Python,
    );

    store
        .persist_file(
            &project,
            &file_named("views.py"),
            std::slice::from_ref(&caller),
            &[],
            &[save_ref, builtin_ref],
            &[],
            &[],
        )
        .unwrap();

    let by_name = store.unresolved_callers_of("save_model_obj", 20).unwrap();
    assert_eq!(by_name.len(), 1, "the dropped call surfaces as an unproven caller");
    assert_eq!(by_name[0].0.name, "update_view", "the enclosing node is the caller");
    assert_eq!(by_name[0].1, 96, "the call-site line is carried");

    assert!(
        store.unresolved_callers_of("absent_method", 20).unwrap().is_empty(),
        "an unreferenced name surfaces nothing",
    );

    let from_caller = store.unresolved_callees_of(&caller.id, 20).unwrap();
    assert_eq!(from_caller.len(), 1, "the queryset builtin `all` is filtered, save_model_obj kept");
    assert_eq!(from_caller[0], ("save_model_obj".to_string(), 96));
}

#[test]
fn orphan_definitions_excludes_referenced_symbols() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");
    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let referenced = node_in("blog", "app.py", "referenced", NodeKind::Function);
    let orphan = node_in("blog", "app.py", "orphan", NodeKind::Function);
    let edge = Edge::new(orphan.id.clone(), referenced.id.clone(), EdgeKind::Calls).at(2, 0);

    store
        .persist_file(
            &project,
            &file_named("app.py"),
            &[referenced, orphan],
            std::slice::from_ref(&edge),
            &[],
            &[],
            &[],
        )
        .unwrap();

    let orphans = store.orphan_definitions(&project, 50).unwrap();

    assert_eq!(orphans.len(), 1, "only the unreferenced definition is an orphan");
    assert_eq!(orphans[0].name, "orphan");
}

#[test]
fn nodes_in_range_returns_overlapping_definitions_innermost_first() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");
    store.upsert_project(&project, "blog", "/tmp/blog").unwrap();

    let class = Node::new(
        NodeId::from_raw("blog::x.py::Outer".to_string()),
        ProjectId::new("blog"),
        NodeKind::Class,
        NodeIdentity {
            name: "Outer".to_string(),
            qualified_name: "x.py::Outer".to_string(),
            file_path: "x.py".to_string(),
            language: Language::Python,
        },
        Span::new(10, 30, 0, 0),
        0,
    );
    let method = Node::new(
        NodeId::from_raw("blog::x.py::Outer.run".to_string()),
        ProjectId::new("blog"),
        NodeKind::Method,
        NodeIdentity {
            name: "run".to_string(),
            qualified_name: "x.py::Outer.run".to_string(),
            file_path: "x.py".to_string(),
            language: Language::Python,
        },
        Span::new(15, 20, 0, 0),
        0,
    );

    store
        .persist_file(&project, &file_named("x.py"), &[class, method], &[], &[], &[], &[])
        .unwrap();

    let hit = store.nodes_in_range(&project, "x.py", 16, 18).unwrap();
    assert_eq!(hit.len(), 2, "both the method and its enclosing class overlap");
    assert_eq!(hit[0].name, "run", "innermost (smallest span) first");
    assert_eq!(hit[1].name, "Outer");

    let miss = store.nodes_in_range(&project, "x.py", 40, 41).unwrap();
    assert!(miss.is_empty(), "a range past every span returns nothing");
}

#[test]
fn symbols_as_of_reconstructs_the_live_set() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("demo");
    store.upsert_project(&project, "demo", "/tmp/demo").unwrap();

    let file = "orders/models.py";
    let touched = || vec![CommitFile { file_path: file.to_string(), insertions: 1, deletions: 0 }];
    let commits = vec![
        CommitRecord { commit_hash: "a".repeat(40), author: "Ada".to_string(), committed_at: 100, summary: "add Order".to_string(), files: touched() },
        CommitRecord { commit_hash: "b".repeat(40), author: "Ada".to_string(), committed_at: 200, summary: "add note".to_string(), files: touched() },
        CommitRecord { commit_hash: "c".repeat(40), author: "Ada".to_string(), committed_at: 300, summary: "rework".to_string(), files: touched() },
    ];
    store.replace_history(&project, &commits).unwrap();

    let revision = |hash: &str, qualified_name: &str, name: &str, change: SymbolChange, signature: Option<&str>| SymbolRevision {
        commit_hash: hash.to_string(),
        file_path: file.to_string(),
        qualified_name: qualified_name.to_string(),
        name: name.to_string(),
        kind: "method".to_string(),
        change,
        signature: signature.map(str::to_string),
    };
    let revisions = vec![
        revision(&"a".repeat(40), "orders.Order", "Order", SymbolChange::Added, None),
        revision(&"a".repeat(40), "orders.Order.total", "total", SymbolChange::Added, Some("(self)")),
        revision(&"b".repeat(40), "orders.Order.note", "note", SymbolChange::Added, Some("(self)")),
        revision(&"c".repeat(40), "orders.Order.note", "note", SymbolChange::Removed, Some("(self)")),
        revision(&"c".repeat(40), "orders.Order.total", "total", SymbolChange::Modified, Some("(self, tax)")),
    ];
    store.replace_symbol_revisions(&project, &revisions).unwrap();

    let names = |at: i64| {
        let mut live: Vec<String> = store
            .symbols_as_of(Some(&project), at, None, 100)
            .unwrap()
            .into_iter()
            .map(|symbol| symbol.qualified_name)
            .collect();
        live.sort();

        live
    };

    assert_eq!(names(150), ["orders.Order", "orders.Order.total"], "note not added yet at t=150");
    assert_eq!(
        names(250),
        ["orders.Order", "orders.Order.note", "orders.Order.total"],
        "all three alive at t=250",
    );
    assert_eq!(names(350), ["orders.Order", "orders.Order.total"], "note removed by t=350");

    let total_signature = |at: i64| {
        store
            .symbols_as_of(Some(&project), at, None, 100)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.qualified_name == "orders.Order.total")
            .unwrap()
            .signature
    };
    assert_eq!(total_signature(150).as_deref(), Some("(self)"));
    assert_eq!(total_signature(350).as_deref(), Some("(self, tax)"), "the modified signature is in effect after t=300");

    assert_eq!(store.commit_committed_at(Some(&project), "bbbb").unwrap(), Some(200), "hash prefix resolves to its time");
    assert_eq!(store.commit_committed_at(Some(&project), "nope").unwrap(), None);
}

#[test]
fn git_ingest_stamp_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("demo");
    store.upsert_project(&project, "demo", "/tmp/demo").unwrap();

    assert_eq!(store.git_ingest_stamp(&project).unwrap(), None, "no stamp until ingested");

    store.set_git_ingest_stamp(&project, "abc123|fp|true").unwrap();
    assert_eq!(store.git_ingest_stamp(&project).unwrap().as_deref(), Some("abc123|fp|true"));

    store.set_git_ingest_stamp(&project, "def456|fp|true").unwrap();
    assert_eq!(
        store.git_ingest_stamp(&project).unwrap().as_deref(),
        Some("def456|fp|true"),
        "a new stamp overwrites the prior one",
    );
}
