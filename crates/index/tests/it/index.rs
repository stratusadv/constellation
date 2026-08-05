use std::path::Path;

use constellation_graph::{EdgeKind, Language, NodeId, NodeKind, ProjectId};
use constellation_index::{
    count_stale_files, index_project, is_ignored_path, link_constellation, module_of,
    namespace_chain, template_owner, use_store_backed,
};
use constellation_store::Store;
use rustc_hash::FxHashMap;

#[test]
fn indexes_a_python_file_into_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let source = "class Article:\n    def publish(self):\n        return 1\n";
    std::fs::write(directory.path().join("models.py"), source).unwrap();
    std::fs::create_dir(directory.path().join("__pycache__")).unwrap();
    std::fs::write(directory.path().join("__pycache__").join("skip.py"), "x = 1\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    let stats = index_project(&store, &project, "blog", directory.path()).unwrap();

    assert_eq!(stats.files_indexed, 1, "the __pycache__ file must be skipped");
    assert!(stats.nodes >= 3, "file + class + method, got {}", stats.nodes);
    assert_eq!(store.count_nodes(&project).unwrap(), stats.nodes);
    assert!(store.count_edges().unwrap() >= 2, "contains edges expected");
}

#[test]
fn reindexing_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.py"), "def f():\n    return 1\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    let first = index_project(&store, &project, "proj", directory.path()).unwrap();
    let second = index_project(&store, &project, "proj", directory.path()).unwrap();

    assert!(first.files_indexed >= 1, "first index parses the file");
    assert_eq!(second.files_indexed, 0, "unchanged file must not be re-parsed");
    assert_eq!(second.files_unchanged, 1, "unchanged file is counted as unchanged");

    assert_eq!(
        store.count_nodes(&project).unwrap(),
        first.nodes,
        "the graph is stable across a no-op re-index",
    );
}

#[test]
fn watch_ignores_store_and_skip_dirs() {
    assert!(is_ignored_path(Path::new("repo/.constellation/index.db")));
    assert!(is_ignored_path(Path::new("repo/.git/index")));
    assert!(is_ignored_path(Path::new("repo/node_modules/x/y.js")));
    assert!(!is_ignored_path(Path::new("repo/app/views.py")));
    assert!(!is_ignored_path(Path::new("templates/base.html")));
}

#[test]
fn a_source_directory_named_target_is_indexed_and_a_cargo_one_is_not() {
    let directory = tempfile::tempdir().unwrap();

    let app = directory.path().join("app").join("schedule").join("target");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("models.py"), "class ScheduleTarget:\n    pass\n").unwrap();

    let build = directory.path().join("rust").join("target");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(directory.path().join("rust").join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(build.join("generated.py"), "class Generated:\n    pass\n").unwrap();

    assert!(!is_ignored_path(&app), "a Django app named target is not a build directory");
    assert!(is_ignored_path(&build), "a target beside a Cargo.toml is a build directory");

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let names: Vec<String> = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .map(|node| node.name)
        .collect();

    assert!(
        names.iter().any(|name| name == "ScheduleTarget"),
        "a model under a source directory named target must be indexed, got {names:?}",
    );

    assert!(
        !names.iter().any(|name| name == "Generated"),
        "a cargo build directory must stay skipped, got {names:?}",
    );
}

#[test]
fn incremental_reindex_handles_change_and_deletion() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.py"), "def a():\n    return 1\n").unwrap();
    std::fs::write(directory.path().join("b.py"), "def b():\n    return 2\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    std::fs::write(directory.path().join("a.py"), "def a():\n    return 99\n").unwrap();
    std::fs::remove_file(directory.path().join("b.py")).unwrap();

    let second = index_project(&store, &project, "proj", directory.path()).unwrap();

    assert_eq!(second.files_indexed, 1, "only the changed file is re-parsed");
    assert_eq!(second.files_removed, 1, "the deleted file is removed");

    let names: Vec<String> = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .map(|node| node.name)
        .collect();

    assert!(names.iter().any(|name| name == "a"), "a survives");
    assert!(!names.iter().any(|name| name == "b"), "b is gone after deletion, got {names:?}");
}

#[test]
fn resolves_a_local_call_into_an_edge() {
    let directory = tempfile::tempdir().unwrap();
    let source = "class Thing:\n    pass\n\ndef make():\n    return Thing()\n";
    std::fs::write(directory.path().join("app.py"), source).unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    let stats = index_project(&store, &project, "proj", directory.path()).unwrap();

    assert!(stats.resolved_edges >= 1, "Thing() must resolve to the Thing class");
    assert!(store.count_edges().unwrap() >= 3, "the resolved edge must be persisted");
}

#[test]
fn synthesizes_override_edges_across_inheritance() {
    let directory = tempfile::tempdir().unwrap();
    let source = "class Base:\n    def save(self):\n        return 1\n\n\nclass Article(Base):\n    def save(self):\n        return 2\n";
    std::fs::write(directory.path().join("models.py"), source).unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    let base_save = nodes
        .iter()
        .find(|node| node.qualified_name.ends_with("Base.save"))
        .expect("Base.save method");

    let article_save = nodes
        .iter()
        .find(|node| node.qualified_name.ends_with("Article.save"))
        .expect("Article.save method");

    let overriders = store.callers(&base_save.id).unwrap();

    assert!(
        overriders
            .iter()
            .any(|(kind, node)| *kind == EdgeKind::Overrides && node.id == article_save.id),
        "Article.save must override Base.save, got {overriders:?}",
    );
}

#[test]
fn styles_gate_drops_unmatched_class_references() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("static")).unwrap();
    std::fs::create_dir(directory.path().join("templates")).unwrap();
    std::fs::write(directory.path().join("static").join("app.css"), ".card { color: red; }\n").unwrap();

    std::fs::write(
        directory.path().join("templates").join("page.html"),
        "<div class=\"card missing\"></div>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("site");

    index_project(&store, &project, "site", directory.path()).unwrap();

    let selector = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Selector && node.name == "card")
        .expect(".card selector node");

    let stylers = store.callers(&selector.id).unwrap();

    assert!(
        stylers.iter().any(|(kind, _)| *kind == EdgeKind::Styles),
        "class=\"card\" must resolve to the .card selector",
    );

    assert_eq!(
        store.count_unresolved(&project).unwrap(),
        0,
        "the unmatched `missing` class must be gated, leaving no pending styles refs",
    );
}

#[test]
fn links_cross_project_template_inheritance() {
    let spire = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(spire.path().join("templates").join("spire")).unwrap();

    std::fs::write(
        spire.path().join("templates").join("spire").join("base.html"),
        "<html>{% block content %}{% endblock %}</html>\n",
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("templates").join("workspace")).unwrap();

    std::fs::write(
        workspace.path().join("templates").join("workspace").join("page.html"),
        "{% extends 'spire/base.html' %}\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let spire_project = ProjectId::new("django-spire");

    index_project(&store, &spire_project, "django-spire", spire.path()).unwrap();
    index_project(&store, &ProjectId::new("workspace"), "workspace", workspace.path()).unwrap();
    link_constellation(&store).unwrap();

    let base = store
        .all_nodes(Some(&spire_project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Template && node.name == "spire/base.html")
        .expect("spire base template");

    let extenders = store.callers(&base.id).unwrap();

    assert!(
        extenders.iter().any(|(kind, node)| {
            *kind == EdgeKind::ExtendsTemplate && node.project_id.as_str() == "workspace"
        }),
        "workspace page extends the spire base template across projects, got {extenders:?}",
    );
}

#[test]
fn links_cross_project_model_relation() {
    let spire = tempfile::tempdir().unwrap();

    std::fs::write(
        spire.path().join("models.py"),
        "from django.db import models\n\n\nclass HistoryEvent(models.Model):\n    pass\n",
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();

    std::fs::write(
        workspace.path().join("models.py"),
        "from django.db import models\n\n\nclass Audit(models.Model):\n    event = models.ForeignKey('HistoryEvent', on_delete=models.CASCADE)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let spire_project = ProjectId::new("django-spire");

    index_project(&store, &spire_project, "django-spire", spire.path()).unwrap();
    index_project(&store, &ProjectId::new("workspace"), "workspace", workspace.path()).unwrap();
    link_constellation(&store).unwrap();

    let event = store
        .all_nodes(Some(&spire_project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "HistoryEvent")
        .expect("HistoryEvent model");

    let relaters = store.callers(&event.id).unwrap();

    assert!(
        relaters.iter().any(|(kind, node)| {
            *kind == EdgeKind::RelatesTo && node.project_id.as_str() == "workspace"
        }),
        "workspace Audit relates to spire HistoryEvent across projects, got {relaters:?}",
    );
}

#[test]
fn locates_the_innermost_symbol_at_a_line() {
    let directory = tempfile::tempdir().unwrap();
    let source = "class Service:\n    def run(self):\n        return helper()\n\n\ndef helper():\n    return 1\n";
    std::fs::write(directory.path().join("app.py"), source).unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let nodes = store.nodes_at("app.py", 3).unwrap();

    assert!(!nodes.is_empty(), "a symbol spans app.py:3");
    assert_eq!(nodes[0].name, "run", "innermost at line 3 is the run method, got {:?}", nodes[0].name);
}

#[test]
fn callers_located_reports_the_call_site_line() {
    let directory = tempfile::tempdir().unwrap();
    let source = "def target():\n    return 1\n\n\ndef caller():\n    return target()\n";
    std::fs::write(directory.path().join("app.py"), source).unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let target = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "target")
        .expect("target function");

    let callers = store.callers_located(&target.id).unwrap();

    let call = callers
        .iter()
        .find(|(kind, _, _)| *kind == EdgeKind::Calls)
        .expect("a calls edge into target");

    assert_eq!(call.2, 6, "target() is called on line 6, got {}", call.2);
}

#[test]
fn resolves_a_package_reexport_chain() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("blog")).unwrap();

    std::fs::write(
        directory.path().join("blog").join("__init__.py"),
        "from .models import Article\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("blog").join("models.py"),
        "from django.db import models\n\n\nclass Article(models.Model):\n    pass\n",
    )
    .unwrap();

    std::fs::write(directory.path().join("service.py"), "from blog import Article\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let article = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "Article" && node.kind == NodeKind::Model)
        .expect("Article model node");

    let importers = store.callers(&article.id).unwrap();

    assert!(
        importers
            .iter()
            .any(|(kind, node)| *kind == EdgeKind::Imports && node.file_path.ends_with("service.py")),
        "service.py must resolve `from blog import Article` through the package re-export",
    );
}

#[test]
fn resolves_a_multi_hop_package_reexport_chain() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("blog").join("models")).unwrap();
    std::fs::write(
        directory.path().join("blog").join("__init__.py"),
        "from .models import Article\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("blog").join("models").join("__init__.py"),
        "from .article import Article\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("blog").join("models").join("article.py"),
        "from django.db import models\n\n\nclass Article(models.Model):\n    pass\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("service.py"), "from blog import Article\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let article = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "Article" && node.kind == NodeKind::Model)
        .expect("Article model node");

    assert!(
        article.file_path.ends_with("article.py"),
        "the model resolves to its defining module, not an __init__ re-export",
    );

    let importers = store.callers(&article.id).unwrap();

    assert!(
        importers
            .iter()
            .any(|(kind, node)| *kind == EdgeKind::Imports && node.file_path.ends_with("service.py")),
        "service.py must resolve `from blog import Article` through two package re-export hops",
    );
}

#[test]
fn skips_gitignored_files_and_honors_negations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();

    std::fs::write(root.join(".gitignore"), "secret.py\ngenerated/\nbuild/*.py\n!build/keep.py\n").unwrap();
    std::fs::write(root.join("app.py"), "def handler():\n    pass\n").unwrap();
    std::fs::write(root.join("secret.py"), "def leaked():\n    pass\n").unwrap();
    std::fs::create_dir(root.join("generated")).unwrap();
    std::fs::write(root.join("generated").join("noise.py"), "def noise():\n    pass\n").unwrap();
    std::fs::create_dir(root.join("build")).unwrap();
    std::fs::write(root.join("build").join("noise.py"), "def dropped():\n    pass\n").unwrap();
    std::fs::write(root.join("build").join("keep.py"), "def kept():\n    pass\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let names: Vec<String> = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .map(|node| node.name)
        .collect();

    assert!(names.iter().any(|name| name == "handler"), "a normal file is indexed");
    assert!(names.iter().any(|name| name == "kept"), "a negated path is indexed back in");
    assert!(!names.iter().any(|name| name == "leaked"), "a gitignored file is skipped");
    assert!(!names.iter().any(|name| name == "noise"), "a gitignored directory is skipped");
    assert!(!names.iter().any(|name| name == "dropped"), "a glob-ignored file is skipped");
}

#[test]
fn call_resolution_is_scoped_to_imports() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("jira")).unwrap();
    std::fs::create_dir_all(root.join("hr")).unwrap();

    // A local class that happens to define order_by (the Jira-integration trap).
    std::fs::write(
        root.join("jira").join("builder.py"),
        "class JQLBuilder:\n    def order_by(self):\n        return self\n",
    )
    .unwrap();

    std::fs::write(root.join("hr").join("utils.py"), "def fetch_active():\n    return []\n").unwrap();

    // hr view imports fetch_active (resolves) but never imports JQLBuilder; its
    // .order_by() is a Django ORM call that must NOT bind to JQLBuilder.order_by.
    std::fs::write(
        root.join("hr").join("views.py"),
        "from hr.utils import fetch_active\n\n\ndef list_view():\n    fetch_active()\n    return Employee.objects.order_by('name')\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let view = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "list_view")
        .expect("list_view node");

    let callees: Vec<String> = store
        .callees(&view.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.name)
        .collect();

    assert!(
        !callees.iter().any(|name| name == "order_by"),
        "an unimported same-named method must not resolve, got {callees:?}",
    );

    assert!(
        callees.iter().any(|name| name == "fetch_active"),
        "an imported cross-file call still resolves, got {callees:?}",
    );
}

#[test]
fn queryset_method_dispatch_resolves_custom_methods() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("app").join("models.py"),
        "from django.db import models\n\n\nclass ArticleQuerySet:\n    def by_year(self, year):\n        return self.filter(year=year)\n\n\nclass Article(models.Model):\n    objects = ArticleQuerySet.as_manager()\n",
    )
    .unwrap();

    // The service imports the MODEL, never the QuerySet; import-scoped call
    // resolution would drop this, but queryset dispatch must catch the custom
    // method by_year (and leave the builtin .filter() unresolved).
    std::fs::write(
        root.join("app").join("service.py"),
        "from app.models import Article\n\n\ndef year_breakdown(year):\n    return Article.objects.by_year(year)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let service = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "year_breakdown")
        .expect("year_breakdown node");

    let callees: Vec<(String, String)> = store
        .callees(&service.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| (node.name, node.qualified_name))
        .collect();

    assert!(
        callees.iter().any(|(name, qualified)| name == "by_year" && qualified.contains("ArticleQuerySet")),
        "a Model.objects.custom() call resolves to the queryset method, got {callees:?}",
    );
}

#[test]
fn service_method_dispatch_disambiguates_by_the_receiving_model() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("app")).unwrap();

    // Two services defining the SAME method name, this codebase's dominant shape:
    // one service per model, method names repeated across them. Uniqueness cannot
    // pick one, so the receiving model is the only thing that can.
    std::fs::write(
        root.join("app").join("models.py"),
        concat!(
            "from django.db import models\n",
            "\n",
            "\n",
            "class TargetService:\n",
            "    def set_quantity(self, quantity):\n",
            "        return quantity\n",
            "\n",
            "\n",
            "class QuotaService:\n",
            "    def set_quantity(self, quantity):\n",
            "        return quantity\n",
            "\n",
            "\n",
            "class Target(models.Model):\n",
            "    services = TargetService()\n",
            "\n",
            "\n",
            "class Quota(models.Model):\n",
            "    services = QuotaService()\n",
        ),
    )
    .unwrap();

    std::fs::write(
        root.join("app").join("views.py"),
        concat!(
            "from app.models import Target\n",
            "\n",
            "\n",
            "def set_target(quantity):\n",
            "    return Target.services.set_quantity(quantity)\n",
        ),
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let view = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "set_target")
        .expect("set_target node");

    let callees: Vec<(String, String)> = store
        .callees(&view.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| (node.name, node.qualified_name))
        .collect();

    let bound_to_target = callees
        .iter()
        .any(|(name, owner)| name == "set_quantity" && owner.contains("TargetService"));

    assert!(bound_to_target, "the receiving model picks its own service, got {callees:?}");

    assert!(
        !callees.iter().any(|(_, owner)| owner.contains("QuotaService")),
        "and never binds to another model's service of the same name, got {callees:?}",
    );
}

#[test]
fn a_definition_passed_as_an_argument_is_referenced_rather_than_orphaned() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("app")).unwrap();

    // `crumbs` is never called here, only handed over. Without a reference edge it
    // has no incoming edge at all and reads as dead code, which is the dominant
    // false positive in orphan scanning.
    // Two views in one module, each nesting its own `crumbs`, is the ordinary
    // Django shape. A file-global name check would call that ambiguous and give
    // up on exactly the callbacks this is meant to see, so scope decides.
    std::fs::write(
        root.join("app").join("views.py"),
        concat!(
            "def list_view(request):\n",
            "    def crumbs(breadcrumbs):\n",
            "        return breadcrumbs\n",
            "\n",
            "    return build(request, breadcrumbs_func=crumbs)\n",
            "\n",
            "\n",
            "def detail_view(request):\n",
            "    def crumbs(breadcrumbs):\n",
            "        return breadcrumbs\n",
            "\n",
            "    return build(request, breadcrumbs_func=crumbs)\n",
        ),
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let callbacks: Vec<_> = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .filter(|node| node.name == "crumbs")
        .collect();

    assert_eq!(callbacks.len(), 2, "both nested callbacks were indexed");

    for callback in &callbacks {
        let referrers: Vec<String> = store
            .callers(&callback.id)
            .unwrap()
            .into_iter()
            .filter(|(kind, _)| *kind == EdgeKind::References)
            .map(|(_, node)| node.name)
            .collect();

        let owner = callback.qualified_name.rsplit("::").next().unwrap_or_default().to_string();

        assert_eq!(
            referrers.len(),
            1,
            "{owner} is referenced once, by its own view and no other, got {referrers:?}",
        );

        assert!(
            owner.starts_with(referrers[0].as_str()),
            "and by the view that nests it, got {owner} referenced by {referrers:?}",
        );
    }

    let orphans: Vec<String> = store
        .orphan_definitions(&project, 32)
        .unwrap()
        .into_iter()
        .map(|node| node.name)
        .collect();

    assert!(
        !orphans.iter().any(|name| name == "crumbs"),
        "so it is no longer a dead-code candidate, got {orphans:?}",
    );
}

#[test]
fn route_resolution_honors_the_view_module_import() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("employee").join("views")).unwrap();
    std::fs::create_dir_all(root.join("employee").join("urls")).unwrap();
    std::fs::create_dir_all(root.join("employment").join("views")).unwrap();

    // Two nested apps each define page_views.detail_view: a 2-way name collision.
    std::fs::write(
        root.join("employee").join("views").join("page_views.py"),
        "def detail_view(request):\n    return 1\n",
    )
    .unwrap();

    std::fs::write(
        root.join("employment").join("views").join("page_views.py"),
        "def detail_view(request):\n    return 2\n",
    )
    .unwrap();

    // The url imports employee's page_views by an ABSOLUTE module path whose top
    // package (myproject.human_resource) is ABOVE the index root, so the indexed
    // file paths (employee/views/...) do not contain those segments. The route
    // must still bind to employee's view, never employment's.
    std::fs::write(
        root.join("employee").join("urls").join("page_urls.py"),
        "from myproject.human_resource.employee.views import page_views\n\n\nurlpatterns = [path('d/', page_views.detail_view, name='detail')]\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", root).unwrap();

    let route = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Route && node.file_path.ends_with("page_urls.py"))
        .expect("route node");

    let views: Vec<String> = store
        .callees(&route.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::RoutesTo)
        .map(|(_, node)| node.file_path)
        .collect();

    assert!(
        views.iter().any(|path| path.contains("employee/views")),
        "route binds to the imported app's view, got {views:?}",
    );

    assert!(
        !views.iter().any(|path| path.contains("employment/views")),
        "route must not bind to a sibling app's same-named view, got {views:?}",
    );
}

#[test]
fn resolves_a_signal_receiver_to_its_model() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\nclass Article(models.Model):\n    pass\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("signals.py"),
        "from django.db.models.signals import post_save
from django.dispatch import receiver


@receiver(post_save, sender=Article)
def on_article_saved(sender, instance, **kwargs):
    pass
",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let article = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "Article" && node.kind == NodeKind::Model)
        .expect("Article model node");

    let receivers = store.callers(&article.id).unwrap();

    assert!(
        receivers
            .iter()
            .any(|(kind, node)| *kind == EdgeKind::Receives && node.name == "on_article_saved"),
        "the @receiver handler must resolve to a Receives edge into the Article model",
    );
}

#[test]
fn synthesizes_a_js_event_dispatch_to_its_handler() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("app.js"),
        "function notify() {\n    bus.emit('refresh');\n}\n\n\
function handleRefresh() {\n    return 1;\n}\n\n\
bus.on('refresh', handleRefresh);\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("ui");

    let stats = index_project(&store, &project, "ui", directory.path()).unwrap();

    assert!(stats.synthesized_edges >= 1, "the emit/on pair must synthesize one edge");

    let nodes = store.all_nodes(Some(&project)).unwrap();
    let handler = nodes.iter().find(|node| node.name == "handleRefresh").expect("handleRefresh node");
    let notify = nodes.iter().find(|node| node.name == "notify").expect("notify node");

    let callers = store.callers(&handler.id).unwrap();

    assert!(
        callers.iter().any(|(kind, node)| *kind == EdgeKind::Calls && node.id == notify.id),
        "notify must synthesize a Calls edge into handleRefresh",
    );
}

#[test]
fn synthesizes_an_alpine_dispatch_across_files() {
    let directory = tempfile::tempdir().unwrap();
    let templates = directory.path().join("templates");
    std::fs::create_dir_all(&templates).unwrap();

    std::fs::write(
        templates.join("widget.html"),
        "<button @click=\"$dispatch('cart-add')\">Add</button>\n\
<div @cart-add=\"addItem()\"></div>\n",
    )
    .unwrap();

    std::fs::write(directory.path().join("cart.js"), "function addItem() {\n    return 1;\n}\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("shop");

    let stats = index_project(&store, &project, "shop", directory.path()).unwrap();

    assert!(stats.synthesized_edges >= 1, "the $dispatch/@cart-add pair must synthesize an edge");

    let handler = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "addItem" && node.language == Language::JavaScript)
        .expect("addItem node");

    let callers = store.callers(&handler.id).unwrap();

    assert!(
        callers.iter().any(|(kind, node)| *kind == EdgeKind::Calls && node.kind == NodeKind::Template),
        "the template's $dispatch must synthesize a Calls edge into addItem",
    );
}

#[test]
fn count_stale_files_flags_disk_changes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.py"), "def a():\n    return 1\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let fresh = count_stale_files(&store, &project, directory.path()).unwrap();

    assert_eq!(fresh.changed, 0, "nothing is stale right after an index");
    assert_eq!(fresh.removed, 0, "nothing is removed right after an index");

    std::fs::write(directory.path().join("b.py"), "def b():\n    return 2\n").unwrap();
    std::fs::remove_file(directory.path().join("a.py")).unwrap();

    let stale = count_stale_files(&store, &project, directory.path()).unwrap();

    assert_eq!(stale.changed, 1, "the new b.py counts as a change");
    assert_eq!(stale.removed, 1, "the deleted a.py counts as removed");
}

#[test]
fn use_store_backed_only_for_large_sparse_projects() {
    assert!(!use_store_backed(10, 1_000), "a small project keeps the bulk load");
    assert!(!use_store_backed(40_000, 200_000), "many pending refs keep the bulk load");
    assert!(use_store_backed(100, 200_000), "few refs against many nodes use per-query lookups");
}

#[test]
fn synthesizes_external_edge_for_an_imported_library_base() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from external_lib.mixins import BaseThing\n\n\nclass Widget(BaseThing):\n    pass\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    let stats = index_project(&store, &project, "proj", directory.path()).unwrap();

    assert!(stats.external_edges >= 1, "the library base must synthesize an external edge");

    let external = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::External && node.name == "BaseThing")
        .expect("external BaseThing node");

    assert_eq!(external.qualified_name, "external_lib.mixins.BaseThing");

    let widget = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "Widget")
        .expect("Widget node");

    let callees = store.callees(&widget.id).unwrap();

    assert!(
        callees.iter().any(|(kind, node)| *kind == EdgeKind::Extends && node.id == external.id),
        "Widget must extend the external BaseThing via a synthesized edge",
    );
}

#[test]
fn synthesizes_external_edge_for_a_library_template() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    let templates = directory.path().join("templates").join("page");
    std::fs::create_dir_all(&templates).unwrap();

    std::fs::write(
        templates.join("index.html"),
        "{% extends 'django_spire/base.html' %}\n{% include 'page/_card.html' %}\n",
    )
    .unwrap();

    std::fs::write(templates.join("_card.html"), "<div>card</div>\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    let stats = index_project(&store, &project, "proj", directory.path()).unwrap();

    assert!(stats.external_edges >= 1, "the library template must synthesize an external edge");

    let nodes = store.all_nodes(Some(&project)).unwrap();

    assert!(
        nodes
            .iter()
            .any(|node| node.kind == NodeKind::External && node.name == "django_spire/base.html"),
        "the django_spire template must become an external node",
    );

    assert!(
        !nodes
            .iter()
            .any(|node| node.kind == NodeKind::External && node.name.contains("_card.html")),
        "a first-party template include must not be externalized",
    );
}

#[test]
fn externalizes_instantiation_and_return_then_clears_satisfied_pending_rows() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.http import JsonResponse\n\n\ndef view() -> JsonResponse:\n    return JsonResponse({})\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let external = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::External && node.name == "JsonResponse")
        .expect("external JsonResponse node from instantiation/return annotation");

    assert_eq!(external.qualified_name, "django.http.JsonResponse");

    link_constellation(&store).unwrap();

    let pending = store.load_unresolved(Some(&project)).unwrap();

    assert!(
        !pending.iter().any(|(_, reference)| reference.reference_name == "JsonResponse"),
        "externalized JsonResponse references must be cleared from the pending table after linking",
    );
}

#[test]
fn resolves_an_absolute_first_party_submodule_import_to_its_file() {
    let directory = tempfile::tempdir().unwrap();
    let asset = directory.path().join("app").join("asset");
    std::fs::create_dir_all(&asset).unwrap();

    std::fs::write(asset.join("__init__.py"), "").unwrap();
    std::fs::write(asset.join("models.py"), "class Inventory:\n    pass\n").unwrap();
    std::fs::write(asset.join("forms.py"), "from app.asset import models\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let models_file = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::File && node.file_path.ends_with("app/asset/models.py"))
        .expect("models.py file node");

    let callers = store.callers(&models_file.id).unwrap();

    assert!(
        callers.iter().any(|(kind, _)| *kind == EdgeKind::Imports),
        "the absolute `from app.asset import models` must resolve to the models.py file",
    );

    let pending = store.load_unresolved(Some(&project)).unwrap();

    assert!(
        !pending.iter().any(|(_, reference)| reference.reference_name == "models"),
        "the resolved submodule import must not linger in the pending table",
    );
}

#[test]
fn resolves_a_method_call_on_a_type_annotated_parameter() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("services.py"),
        "class Order:\n    \
def recalculate(self):\n        return 1\n\n\
def process(order: Order):\n    return order.recalculate()\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("shop");

    index_project(&store, &project, "shop", directory.path()).unwrap();

    let process = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "process" && node.kind == NodeKind::Function)
        .expect("process must be extracted as a function node");

    let resolved = store
        .callees(&process.id)
        .unwrap()
        .into_iter()
        .any(|(kind, node)| {
            kind == EdgeKind::Calls
                && node.name == "recalculate"
                && node.qualified_name.contains("Order")
        });

    assert!(
        resolved,
        "order.recalculate() must bind to Order.recalculate via the annotated parameter type",
    );
}

#[test]
fn resolves_a_get_queryset_chain_to_the_custom_queryset_method() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
class InventoryQuerySet(models.QuerySet):\n    \
def by_year(self):\n        return self\n\n\
class InventoryManager(models.Manager):\n    \
def get_queryset(self):\n        return InventoryQuerySet(self.model)\n\n    \
def located(self):\n        return self.get_queryset().by_year()\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("inventory");

    index_project(&store, &project, "inventory", directory.path()).unwrap();

    let located = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "located" && node.kind == NodeKind::Method)
        .expect("located must be extracted as a method node");

    let resolved = store
        .callees(&located.id)
        .unwrap()
        .into_iter()
        .any(|(kind, node)| kind == EdgeKind::Calls && node.name == "by_year");

    assert!(
        resolved,
        "self.get_queryset().by_year() must dispatch to the custom QuerySet method, \
         not dead-end at the get_queryset() call",
    );
}

#[test]
fn resolves_an_alpine_handler_to_its_x_data_method() {
    let directory = tempfile::tempdir().unwrap();
    let templates = directory.path().join("templates");
    std::fs::create_dir_all(&templates).unwrap();

    std::fs::write(
        templates.join("widget.html"),
        "<div x-data=\"{ count: 0, async advanceStatus(url) { this.count++ } }\">\n\
             <button @click=\"advanceStatus('/go')\">Go</button>\n\
             </div>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("ui");

    index_project(&store, &project, "ui", directory.path()).unwrap();

    let method = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "advanceStatus" && node.language == Language::JavaScript)
        .expect("advanceStatus must be extracted as a JS method node");

    let callers = store.callers(&method.id).unwrap();

    assert!(
        callers.iter().any(|(kind, _)| *kind == EdgeKind::Handles),
        "the @click handler must resolve to the x-data method via a Handles edge",
    );
}

#[test]
fn extracts_django_routes_and_links_the_view() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django entry point\n").unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "def article_list(request):\n    return 1\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("urls.py"),
        "from django.urls import path\nfrom . import views\n\n\
urlpatterns = [\n    path(\"articles/\", views.article_list, name=\"articles\"),\n]\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    let stats = index_project(&store, &project, "blog", directory.path()).unwrap();

    let routes = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .filter(|node| node.kind == NodeKind::Route)
        .count();

    assert_eq!(routes, 1, "the path() call must produce one route node");
    assert!(stats.resolved_edges >= 1, "the route must resolve to its view handler");
}

#[test]
fn links_imports_across_projects() {
    let store = Store::open_in_memory().unwrap();

    let shared = tempfile::tempdir().unwrap();
    std::fs::create_dir(shared.path().join("accounts")).unwrap();

    std::fs::write(shared.path().join("accounts").join("models.py"), "class User:\n    pass\n")
        .unwrap();

    index_project(&store, &ProjectId::new("shared"), "shared", shared.path()).unwrap();

    let app = tempfile::tempdir().unwrap();

    std::fs::write(
        app.path().join("service.py"),
        "from accounts.models import User\n\ndef build():\n    return User()\n",
    )
    .unwrap();

    index_project(&store, &ProjectId::new("app"), "app", app.path()).unwrap();

    let linked = link_constellation(&store).unwrap();

    assert!(linked >= 1, "the cross-project User import must link to shared");
}

#[test]
fn cross_project_linking_ignores_external_name_collisions() {
    let store = Store::open_in_memory().unwrap();

    // repoA defines a first-party `special_sum`, plus a first-party `path` that
    // baits a collision with the stdlib name another project imports.
    let repository_a = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository_a.path().join("shared")).unwrap();
    std::fs::write(repository_a.path().join("shared").join("calc.py"), "def special_sum(a, b):\n    return a + b\n").unwrap();
    std::fs::write(repository_a.path().join("shared").join("router.py"), "def path(route):\n    return route\n").unwrap();
    index_project(&store, &ProjectId::new("repo_a"), "repo_a", repository_a.path()).unwrap();

    // repoB genuinely imports repoA's module (must link) and also imports the
    // third-party `django.urls.path` (must NOT link to repoA's `path`).
    let repository_b = tempfile::tempdir().unwrap();
    std::fs::write(repository_b.path().join("use.py"), "from shared.calc import special_sum\n\n\ndef run():\n    return special_sum(1, 2)\n").unwrap();
    std::fs::write(repository_b.path().join("views.py"), "from django.urls import path\n\n\ndef noop():\n    return path\n").unwrap();
    index_project(&store, &ProjectId::new("repo_b"), "repo_b", repository_b.path()).unwrap();

    let linked = link_constellation(&store).unwrap();

    assert_eq!(
        linked, 1,
        "only the module-matched special_sum import links; the django.urls.path import must not collide with repoA's first-party path",
    );
}

#[test]
fn external_stubs_unify_into_cross_project_definitions() {
    let store = Store::open_in_memory().unwrap();

    // A library defines a base class.
    let library = tempfile::tempdir().unwrap();
    std::fs::create_dir(library.path().join("shared")).unwrap();
    std::fs::write(library.path().join("shared").join("mixins.py"), "class BaseMixin:\n    pass\n").unwrap();
    index_project(&store, &ProjectId::new("lib"), "lib", library.path()).unwrap();

    // An app imports and extends it; before linking, the app synthesizes an
    // `external BaseMixin` stub and its model extends that stub.
    let app = tempfile::tempdir().unwrap();
    std::fs::write(app.path().join("models.py"), "from shared.mixins import BaseMixin\n\n\nclass Thing(BaseMixin):\n    pass\n").unwrap();
    index_project(&store, &ProjectId::new("app"), "app", app.path()).unwrap();

    link_constellation(&store).unwrap();

    let nodes = store.all_nodes(None).unwrap();

    assert!(
        !nodes.iter().any(|node| node.kind == NodeKind::External && node.name == "BaseMixin"),
        "the external BaseMixin stub must be unified into the library definition",
    );

    let definition = nodes
        .iter()
        .find(|node| node.name == "BaseMixin" && node.kind != NodeKind::External)
        .expect("the library BaseMixin is indexed");

    let callers = store.callers(&definition.id).unwrap();

    assert!(
        callers.iter().any(|(kind, node)| *kind == EdgeKind::Extends && node.name == "Thing"),
        "the app model must extend the real cross-project BaseMixin, got {:?}",
        callers.iter().map(|(k, n)| (*k, n.name.clone())).collect::<Vec<_>>(),
    );
}

#[test]
fn minified_assets_are_not_indexed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();
    std::fs::create_dir_all(directory.path().join("static")).unwrap();

    std::fs::write(directory.path().join("static").join("vendor.min.js"), "function aa(){return 1};function bb(){return 2}\n").unwrap();
    std::fs::write(directory.path().join("static").join("app.js"), "export function boot() {\n    return 1;\n}\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("site");

    index_project(&store, &project, "site", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    assert!(
        !nodes.iter().any(|node| node.file_path.ends_with("vendor.min.js")),
        "a minified asset must not be indexed",
    );

    assert!(
        nodes.iter().any(|node| node.kind == NodeKind::Function && node.name == "boot"),
        "the real .js file must still index",
    );
}

#[test]
fn links_views_to_templates_and_template_inheritance() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import render\n\n\
def index(request):\n    return render(request, 'blog/index.html', {})\n",
    )
    .unwrap();

    let templates = directory.path().join("templates");
    std::fs::create_dir_all(templates.join("blog")).unwrap();

    std::fs::write(templates.join("base.html"), "<html>{% block c %}{% endblock %}</html>\n")
        .unwrap();

    std::fs::write(
        templates.join("blog").join("index.html"),
        "{% extends 'base.html' %}\n{% include 'blog/_card.html' %}\n",
    )
    .unwrap();

    std::fs::write(templates.join("blog").join("_card.html"), "<div>card</div>\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    let stats = index_project(&store, &project, "blog", directory.path()).unwrap();

    let template_nodes = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .filter(|node| node.kind == NodeKind::Template)
        .count();

    assert_eq!(template_nodes, 3, "base, index, and _card templates");

    assert!(
        stats.resolved_edges >= 3,
        "render + extends + include must resolve, got {}",
        stats.resolved_edges,
    );
}

#[test]
fn links_full_stack_across_template_css_and_js() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::create_dir_all(directory.path().join("static")).unwrap();

    std::fs::write(
        directory.path().join("static").join("app.js"),
        "export function boot() {\n  return 1;\n}\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("static").join("site.css"),
        ".card { color: red; }\n#main { width: 100%; }\n",
    )
    .unwrap();

    std::fs::create_dir_all(directory.path().join("templates")).unwrap();

    std::fs::write(
        directory.path().join("templates").join("page.html"),
        "<link href=\"site.css\" rel=\"stylesheet\">\n\
<div class=\"card\">x</div>\n\
<script src=\"app.js\"></script>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("site");

    index_project(&store, &project, "site", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    assert!(
        nodes.iter().any(|node| node.kind == NodeKind::Selector && node.name == "card"),
        "the .card CSS selector must be a node",
    );

    assert!(
        nodes.iter().any(|node| node.kind == NodeKind::Function && node.name == "boot"),
        "the JS boot function must be a node",
    );

    let template_id = NodeId::new(&project, "page.html");

    let related: Vec<(EdgeKind, String)> = store
        .callees(&template_id)
        .unwrap()
        .into_iter()
        .map(|(kind, node)| (kind, node.name))
        .collect();

    assert!(
        related.iter().any(|(kind, name)| *kind == EdgeKind::Styles && name == "card"),
        "class=\"card\" must link to the .card selector, got {related:?}",
    );

    assert!(
        related.iter().any(|(kind, name)| *kind == EdgeKind::References && name == "app.js"),
        "script src must link to app.js, got {related:?}",
    );

    assert!(
        related.iter().any(|(kind, name)| *kind == EdgeKind::References && name == "site.css"),
        "link href must link to site.css, got {related:?}",
    );
}

#[test]
fn links_alpine_directives_and_js_imports() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::create_dir_all(directory.path().join("static")).unwrap();

    std::fs::write(
        directory.path().join("static").join("app.js"),
        "import { fmt } from './util.js';\n\n\
Alpine.data('cartItem', () => ({ open: false }));\n\n\
function increment() {\n  return fmt(1);\n}\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("static").join("util.js"),
        "export function fmt(value) {\n  return value;\n}\n",
    )
    .unwrap();

    std::fs::create_dir_all(directory.path().join("templates")).unwrap();

    std::fs::write(
        directory.path().join("templates").join("p.html"),
        "<div x-data=\"cartItem()\">\n  <button @click=\"increment()\">+</button>\n</div>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("app");

    index_project(&store, &project, "app", directory.path()).unwrap();

    let template_related: Vec<(EdgeKind, String)> = store
        .callees(&NodeId::new(&project, "p.html"))
        .unwrap()
        .into_iter()
        .map(|(kind, node)| (kind, node.name))
        .collect();

    assert!(
        template_related.iter().any(|(k, n)| *k == EdgeKind::Handles && n == "cartItem"),
        "x-data must link to the Alpine.data('cartItem') component, got {template_related:?}",
    );

    assert!(
        template_related.iter().any(|(k, n)| *k == EdgeKind::Handles && n == "increment"),
        "@click must link to the increment function, got {template_related:?}",
    );

    let import_related: Vec<(EdgeKind, String)> = store
        .callees(&NodeId::new(&project, "static/app.js"))
        .unwrap()
        .into_iter()
        .map(|(kind, node)| (kind, node.name))
        .collect();

    assert!(
        import_related.iter().any(|(k, n)| *k == EdgeKind::Imports && n == "fmt"),
        "the named JS import must resolve to the fmt export, got {import_related:?}",
    );
}

#[test]
fn links_cbv_model_and_form_attributes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\nclass Article(models.Model):\n    pass\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.views.generic import ListView\nfrom .models import Article\n\n\
class ArticleList(ListView):\n    model = Article\n    queryset = Article.objects.all()\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let view = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "ArticleList")
        .unwrap();

    assert_eq!(view.kind, NodeKind::View, "ArticleList must be a View");

    let related: Vec<String> = store
        .callees(&view.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::RelatesTo)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        related.iter().any(|qualified| qualified.ends_with("models.py::Article")),
        "the CBV's model/queryset must relate it to the Article model, got {related:?}",
    );
}

#[test]
fn links_url_names_to_routes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::write(
        directory.path().join("urls.py"),
        "from django.urls import path\nfrom . import views\n\n\
urlpatterns = [\n    path(\"articles/\", views.article_list, name=\"article-list\"),\n]\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.urls import reverse\n\n\
def article_list(request):\n    return 1\n\n\
def go(request):\n    return reverse(\"article-list\")\n",
    )
    .unwrap();

    std::fs::create_dir_all(directory.path().join("templates")).unwrap();

    std::fs::write(
        directory.path().join("templates").join("nav.html"),
        "<a href=\"{% url 'article-list' %}\">articles</a>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let route = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Route && node.name == "article-list")
        .unwrap();

    let resolvers: Vec<String> = store
        .callers(&route.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Resolves)
        .map(|(_, node)| node.name)
        .collect();

    assert!(
        resolvers.iter().any(|name| name == "nav.html"),
        "{{% url %}} must resolve to the route, got {resolvers:?}",
    );

    assert!(
        resolvers.iter().any(|name| name == "go"),
        "reverse() must resolve to the route, got {resolvers:?}",
    );
}

#[test]
fn links_cbv_template_name_to_template() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.views.generic import ListView\n\n\
class ArticleList(ListView):\n    template_name = \"blog/list.html\"\n",
    )
    .unwrap();

    std::fs::create_dir_all(directory.path().join("templates").join("blog")).unwrap();

    std::fs::write(directory.path().join("templates").join("blog").join("list.html"), "<ul></ul>\n")
        .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let view = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "ArticleList")
        .unwrap();

    assert_eq!(view.kind, NodeKind::View, "ArticleList must be promoted to a View");

    let renders: Vec<String> = store
        .callees(&view.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Renders)
        .map(|(_, node)| node.name)
        .collect();

    assert!(
        renders.iter().any(|name| name == "blog/list.html"),
        "the CBV template_name must render blog/list.html, got {renders:?}",
    );
}

#[test]
fn resolves_aliased_import_calls() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("pkg")).unwrap();

    std::fs::write(directory.path().join("pkg").join("utils.py"), "def helper():\n    return 1\n")
        .unwrap();

    std::fs::write(
        directory.path().join("pkg").join("service.py"),
        "from .utils import helper as do_help\n\ndef run():\n    return do_help()\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("p");

    index_project(&store, &project, "p", directory.path()).unwrap();

    let run = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.qualified_name.ends_with("service.py::run"))
        .unwrap();

    let callees: Vec<String> = store
        .callees(&run.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        callees.iter().any(|qualified| qualified.ends_with("utils.py::helper")),
        "do_help() must resolve to helper through the import alias, got {callees:?}",
    );
}

#[test]
fn resolves_self_calls_to_the_enclosing_class() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("a.py"),
        "class Foo:\n    def run(self):\n        return self.helper()\n    \
def helper(self):\n        return 1\n\n\
class Bar:\n    def helper(self):\n        return 2\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("p");

    index_project(&store, &project, "p", directory.path()).unwrap();

    let run = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.qualified_name.ends_with("Foo.run"))
        .unwrap();

    let callees: Vec<String> = store
        .callees(&run.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        callees.iter().any(|qualified| qualified.ends_with("Foo.helper")),
        "self.helper() must bind to Foo.helper, got {callees:?}",
    );

    assert!(
        !callees.iter().any(|qualified| qualified.ends_with("Bar.helper")),
        "self.helper() must NOT bind to Bar.helper, got {callees:?}",
    );
}

#[test]
fn promotes_models_and_links_relations() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();
    std::fs::create_dir_all(directory.path().join("blog")).unwrap();

    std::fs::write(
        directory.path().join("blog").join("models.py"),
        "from django.db import models\n\n\
class Author(models.Model):\n    name = models.CharField(max_length=100)\n\n\
class Article(models.Model):\n    \
author = models.ForeignKey(Author, on_delete=models.CASCADE)\n    \
tags = models.ManyToManyField('Tag')\n\n\
class Tag(models.Model):\n    label = models.CharField(max_length=50)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();
    let models = nodes.iter().filter(|node| node.kind == NodeKind::Model).count();

    assert_eq!(models, 3, "Author, Article, Tag must be promoted to models, got {models}");

    let article = nodes
        .iter()
        .find(|node| node.name == "Article" && node.kind == NodeKind::Model)
        .unwrap();

    let related: Vec<(EdgeKind, String)> = store
        .callees(&article.id)
        .unwrap()
        .into_iter()
        .map(|(kind, node)| (kind, node.name))
        .collect();

    assert!(
        related.iter().any(|(k, n)| *k == EdgeKind::RelatesTo && n == "Author"),
        "Article must relate to Author (ForeignKey), got {related:?}",
    );

    assert!(
        related.iter().any(|(k, n)| *k == EdgeKind::RelatesTo && n == "Tag"),
        "Article must relate to Tag (ManyToManyField), got {related:?}",
    );

    assert!(
        nodes.iter().any(|node| node.kind == NodeKind::Field && node.name == "author"),
        "the author field must be a Field node",
    );
}

#[test]
fn reverse_relations_are_synthesized_on_the_target_model() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
class Author(models.Model):\n    name = models.CharField(max_length=50)\n\n\
class Article(models.Model):\n    \
author = models.ForeignKey(Author, on_delete=models.CASCADE)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    let author = nodes
        .iter()
        .find(|node| node.name == "Author" && node.kind == NodeKind::Model)
        .unwrap();

    let reverse = store
        .callees(&author.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, node)| *kind == EdgeKind::RelatesTo && node.name == "Article")
        .count();

    assert_eq!(
        reverse, 1,
        "Author relates back to Article exactly once via the synthesized reverse relation, got {reverse}",
    );
}

#[test]
fn strict_and_loose_kinds_never_resolve_to_a_field() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::create_dir_all(directory.path().join("app").join("core")).unwrap();
    std::fs::create_dir_all(directory.path().join("app").join("billing")).unwrap();

    // A plain `models.Model` base: the base name "Model" has no local class, so
    // the old fuzzy fallback bound `extends` onto a field named `model`.
    std::fs::write(
        directory.path().join("app").join("core").join("models.py"),
        "from django.db import models\n\n\nclass Widget(models.Model):\n    name = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // Fields literally named `model` and `date`: the collision targets.
    std::fs::write(
        directory.path().join("app").join("billing").join("models.py"),
        "from django.db import models\n\n\nclass Invoice(models.Model):\n    model = models.CharField(max_length=10)\n    date = models.DateField()\n",
    )
    .unwrap();

    // A `date(...)` call: stdlib constructor, not the `date` field above.
    std::fs::write(
        directory.path().join("app").join("billing").join("services.py"),
        "from datetime import date\n\n\ndef build():\n    return date(2020, 1, 1)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("shop");

    index_project(&store, &project, "shop", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    let junk = |kind: NodeKind| {
        matches!(
            kind,
            NodeKind::Field
                | NodeKind::Property
                | NodeKind::Variable
                | NodeKind::Constant
                | NodeKind::Parameter
        )
    };

    let reference_like = |kind: EdgeKind| {
        matches!(
            kind,
            EdgeKind::Calls
                | EdgeKind::Extends
                | EdgeKind::Instantiates
                | EdgeKind::RelatesTo
                | EdgeKind::Decorates
                | EdgeKind::Returns
                | EdgeKind::TypeOf
        )
    };

    for node in &nodes {
        let callees = store.callees(&node.id).unwrap();

        for (kind, target) in &callees {
            assert!(
                !(reference_like(*kind) && junk(target.kind)),
                "{} edge from {} resolved to a {:?} ({}): a name collision, not a real edge",
                kind.as_str(),
                node.name,
                target.kind,
                target.name,
            );
        }
    }
}

#[test]
fn a_file_that_panics_extraction_is_skipped_not_fatal() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\nclass Widget(models.Model):\n    name = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // A template nested far past the parser's NESTING_DEPTH_MAX (256): extracting
    // it panics. One pathological file must not abort the whole parallel index.
    let depth = 400;
    let mut deep = String::new();

    for index in 0..depth {
        deep.push_str(&format!("{{% if a{index} %}}"));
    }

    deep.push('x');

    for _ in 0..depth {
        deep.push_str("{% endif %}");
    }

    std::fs::create_dir_all(directory.path().join("templates")).unwrap();
    std::fs::write(directory.path().join("templates").join("deep.html"), &deep).unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("site");

    let stats = index_project(&store, &project, "site", directory.path()).unwrap();

    assert!(stats.files_skipped >= 1, "the panicking template must be skipped, not fatal");

    let nodes = store.all_nodes(Some(&project)).unwrap();

    assert!(
        nodes.iter().any(|node| node.kind == NodeKind::Model && node.name == "Widget"),
        "the healthy file must still index after a sibling file panics",
    );
}

#[test]
fn service_method_dispatch_binds_unique_names_and_drops_ambiguous() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("manage.py"), "# django\n").unwrap();
    std::fs::create_dir_all(directory.path().join("shop")).unwrap();

    std::fs::write(
        directory.path().join("shop").join("services.py"),
        "class WidgetProcessorService:\n    def frobnicate_widget(self):\n        return 1\n\n    def shared_op(self):\n        return 1\n\n    def save_model_obj(self):\n        return 1\n\n\nclass GadgetProcessorService:\n    def shared_op(self):\n        return 2\n",
    )
    .unwrap();

    // A view reaches the service through `obj.services.processor.<method>()`, the
    // chained-attribute dispatch the resolver can't follow statically.
    std::fs::write(
        directory.path().join("shop").join("views.py"),
        "def do_it(widget):\n    widget.services.processor.frobnicate_widget()\n    widget.services.processor.shared_op()\n    widget.services.processor.save_model_obj()\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("shop");

    index_project(&store, &project, "shop", directory.path()).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();
    let do_it = nodes.iter().find(|node| node.name == "do_it").expect("the view function indexes");

    let callees: Vec<(EdgeKind, String)> = store
        .callees(&do_it.id)
        .unwrap()
        .into_iter()
        .map(|(kind, node)| (kind, node.name))
        .collect();

    // Recall: a name unique to one *Service class binds.
    assert!(
        callees.iter().any(|(kind, name)| *kind == EdgeKind::Calls && name == "frobnicate_widget"),
        "a unique service method must resolve through .services dispatch, got {callees:?}",
    );

    // Reliability: an ambiguous name (two services define `shared_op`) stays
    // unresolved rather than guess, and a base builtin never binds.
    assert!(
        !callees.iter().any(|(_, name)| name == "shared_op"),
        "an ambiguous service method must not bind, got {callees:?}",
    );

    assert!(
        !callees.iter().any(|(_, name)| name == "save_model_obj"),
        "a base service builtin must not bind, got {callees:?}",
    );
}

#[test]
fn synthesizes_type_scoped_template_member_access() {
    let directory = tempfile::tempdir().unwrap();

    // Widget has `color`; Gadget has `weight`. Only Widget is bound into the
    // view's context, via get_object_or_404.
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
         class Widget(models.Model):\n    color = models.CharField(max_length=10)\n\n\
         class Gadget(models.Model):\n    weight = models.IntegerField(default=0)\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import get_object_or_404, render\n\
         from .models import Widget\n\n\
         def detail_view(request, pk):\n    \
             widget = get_object_or_404(Widget, pk=pk)\n    \
             return render(request, 'widget/detail.html', {'widget': widget})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates").join("widget");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("detail.html"),
        "<div>{{ widget.color }}</div>\n<span>{{ widget.weight }}</span>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "widget/detail.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Widget.color")),
        "the color access resolves to Widget.color via the view's get_object_or_404 type, got {accessed:?}",
    );
    assert!(
        !accessed.iter().any(|qualified| qualified.ends_with("weight")),
        "the weight access drops: Widget has no `weight`, even though Gadget.weight exists globally. \
         type-scoped, not name-matched, got {accessed:?}",
    );
}

#[test]
fn synthesizes_loop_variable_member_access() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\nclass Row(models.Model):\n    label = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // The view binds a queryset collection `rows`; the template loops it and
    // accesses a member on the loop variable.
    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import render\n\
         from .models import Row\n\n\
         def list_view(request):\n    \
             rows = Row.objects.all()\n    \
             return render(request, 'rows.html', {'rows': rows})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("rows.html"),
        "{% for row in rows %}<td>{{ row.label }}</td>{% endfor %}\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "rows.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Row.label")),
        "the loop variable `row` over `rows = Row.objects.all()` types as Row, so row.label \
         resolves to Row.label, got {accessed:?}",
    );
}

#[test]
fn synthesizes_inherited_member_access() {
    let directory = tempfile::tempdir().unwrap();

    // An abstract base declares is_active; Thing inherits it and the template
    // accesses the inherited field on a typed instance.
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
         class Base(models.Model):\n    is_active = models.BooleanField(default=True)\n\n    \
             class Meta:\n        abstract = True\n\n\
         class Thing(Base):\n    name = models.CharField(max_length=10)\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import get_object_or_404, render\n\
         from .models import Thing\n\n\
         def detail_view(request, pk):\n    \
             thing = get_object_or_404(Thing, pk=pk)\n    \
             return render(request, 'thing.html', {'thing': thing})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(template_dir.join("thing.html"), "<div>{{ thing.is_active }}</div>\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "thing.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Base.is_active")),
        "thing.is_active resolves up the Extends chain to the inherited Base.is_active, got {accessed:?}",
    );
}

#[test]
fn synthesizes_reverse_accessor_loop_member_access() {
    let directory = tempfile::tempdir().unwrap();

    // Child FKs Parent with related_name='children'; the detail view binds the
    // parent, the template loops `parent.children` and accesses a child member.
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
         class Parent(models.Model):\n    title = models.CharField(max_length=10)\n\n\
         class Child(models.Model):\n    \
             parent = models.ForeignKey(Parent, on_delete=models.CASCADE, related_name='children')\n    \
             code = models.CharField(max_length=10)\n",
    )
    .unwrap();

    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import get_object_or_404, render\n\
         from .models import Parent\n\n\
         def detail_view(request, pk):\n    \
             parent = get_object_or_404(Parent, pk=pk)\n    \
             return render(request, 'parent.html', {'parent': parent})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("parent.html"),
        "{% for child in parent.children %}<td>{{ child.code }}</td>{% endfor %}\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "parent.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Child.code")),
        "the loop over parent.children (a related_name reverse accessor) types `child` as Child, \
         so child.code resolves to Child.code, got {accessed:?}",
    );
}

#[test]
fn synthesizes_derived_collection_loop_member_access() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\
         class Parent(models.Model):\n    title = models.CharField(max_length=10)\n\n\
         class Child(models.Model):\n    \
             parent = models.ForeignKey(Parent, on_delete=models.CASCADE, related_name='children')\n    \
             code = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // `children` is a multi-line (parenthesized) local queryset off the bound
    // parent's reverse accessor, exercising both paren-unwrapping and the
    // derived-collection typing.
    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import get_object_or_404, render\n\
         from .models import Parent\n\n\
         def detail_view(request, pk):\n    \
             parent = get_object_or_404(Parent, pk=pk)\n    \
             children = (\n        parent.children.all()\n    )\n    \
             return render(request, 'list.html', {'children': children})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("list.html"),
        "{% for child in children %}<td>{{ child.code }}</td>{% endfor %}\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "list.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Child.code")),
        "the multi-line local `children = (parent.children.all())` types as a collection of Child, \
         so the loop's child.code resolves to Child.code, got {accessed:?}",
    );
}

#[test]
fn synthesizes_glue_model_field_member_access() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\nclass Widget(models.Model):\n    color = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // The form view binds the widget under a django-glue name (== its local); the
    // form template binds a glue widget field; it resolves to the model member.
    std::fs::write(
        directory.path().join("views.py"),
        "import django_glue as dg\n\
         from django.shortcuts import get_object_or_404, render\n\
         from .models import Widget\n\n\
         def form_view(request, pk):\n    \
             widget = get_object_or_404(Widget, pk=pk)\n    \
             dg.glue_model_object(request, 'widget', widget, 'view')\n    \
             return render(request, 'widget_form.html', {})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("widget_form.html"),
        "{% include 'django_glue/form/field/char_field.html' with glue_model_field='widget.color' %}\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "widget_form.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Widget.color")),
        "glue_model_field='widget.color' resolves through the view's `widget` glue-name local to \
         Widget.color, got {accessed:?}",
    );
}

#[test]
fn synthesizes_glue_js_field_member_access() {
    let directory = tempfile::tempdir().unwrap();

    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\nclass Inventory(models.Model):\n    sku = models.CharField(max_length=10)\n",
    )
    .unwrap();

    // The rewrite's Glue.model registration binds the instance under a name; the
    // template reads a field in JS via Glue.model.<name>.<field>.
    std::fs::write(
        directory.path().join("views.py"),
        "from django.shortcuts import get_object_or_404, render\n\
         from django_glue.shortcuts.glue import Glue\n\
         from .models import Inventory\n\n\
         def detail_view(request, pk):\n    \
             inventory = get_object_or_404(Inventory, pk=pk)\n    \
             Glue.model(request, unique_name='inventory', target=inventory)\n    \
             return render(request, 'detail.html', {})\n",
    )
    .unwrap();

    let template_dir = directory.path().join("templates");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::write(
        template_dir.join("detail.html"),
        "<span x-text=\"Glue.model.inventory.sku\"></span>\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("blog");

    index_project(&store, &project, "blog", directory.path()).unwrap();

    let template = store
        .nodes_kind_in(&project, NodeKind::Template)
        .unwrap()
        .into_iter()
        .find(|node| node.name == "detail.html")
        .expect("the template node");

    let accessed: Vec<String> = store
        .callees(&template.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::AccessesMember)
        .map(|(_, node)| node.qualified_name)
        .collect();

    assert!(
        accessed.iter().any(|qualified| qualified.ends_with("Inventory.sku")),
        "the rewrite's Glue.model.inventory.sku JS access resolves through the view's `inventory` \
         glue-name local to Inventory.sku, got {accessed:?}",
    );
}

#[test]
fn template_owner_maps_namespace_to_canonical_project() {
    // A namespaced template belongs to its leading segment's project, so a
    // vendored copy under a workspace cannot shadow the django-spire origin.
    assert_eq!(template_owner("django_spire/page/full_page.html"), "django-spire");
    assert_eq!(template_owner("django_glue/widget.html"), "django-glue");
    // A bare name maps to itself, matching no project id, so it stays ambiguous.
    assert_eq!(template_owner("base.html"), "base.html");
}

#[test]
fn module_of_maps_files_and_packages() {
    assert_eq!(module_of("app/partner/urls/page_urls.py"), "app.partner.urls.page_urls");
    assert_eq!(module_of("app/partner/urls/__init__.py"), "app.partner.urls");
    assert_eq!(module_of("app\\partner\\urls\\page_urls.py"), "app.partner.urls.page_urls");
}

#[test]
fn namespace_chain_disambiguates_reused_inner_namespace() {
    let mut includes: FxHashMap<String, (Option<String>, String)> = FxHashMap::default();

    // Root includes the partner app; partner includes its own page urls and,
    // deeper, an agreement->client_contact subtree that ALSO has `page` urls.
    includes.insert("app.partner.urls".into(), (Some("partner".into()), "system.urls".into()));
    includes.insert("app.partner.urls.page_urls".into(), (Some("page".into()), "app.partner.urls".into()));
    includes.insert("app.partner.agreement.urls".into(), (Some("agreement".into()), "app.partner.urls".into()));

    includes.insert(
        "app.partner.agreement.client_contact.urls".into(),
        (Some("client_contact".into()), "app.partner.agreement.urls".into()),
    );

    includes.insert(
        "app.partner.agreement.client_contact.urls.page_urls".into(),
        (Some("page".into()), "app.partner.agreement.client_contact.urls".into()),
    );

    let no_app_names: FxHashMap<String, String> = FxHashMap::default();

    // The same inner namespace `page` resolves to distinct chains, so a
    // `reverse('partner:page:detail')` can no longer hit the client_contact one.
    assert_eq!(
        namespace_chain("app.partner.urls.page_urls", &includes, &no_app_names),
        Some(vec!["partner".to_string(), "page".to_string()]),
    );

    assert_eq!(
        namespace_chain("app.partner.agreement.client_contact.urls.page_urls", &includes, &no_app_names),
        Some(vec![
            "partner".to_string(),
            "agreement".to_string(),
            "client_contact".to_string(),
            "page".to_string(),
        ]),
    );

    assert_eq!(namespace_chain("app.unincluded.urls", &includes, &no_app_names), None);
}

#[test]
fn reverse_names_carry_the_root_app_name_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let sub = directory.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    std::fs::write(
        directory.path().join("urls.py"),
        "from django.urls import include, path\n\n\
         app_name = 'myproj'\n\
         urlpatterns = [path('sub/', include('sub.urls', namespace='sub'))]\n",
    )
    .unwrap();

    std::fs::write(sub.join("__init__.py"), "").unwrap();
    std::fs::write(
        sub.join("urls.py"),
        "from django.urls import path\n\nfrom . import views\n\n\
         app_name = 'subapp'\n\
         urlpatterns = [path('detail/', views.detail, name='detail')]\n",
    )
    .unwrap();

    std::fs::write(sub.join("views.py"), "def detail(request):\n    return 1\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let names: Vec<String> =
        store.route_reverse_names().unwrap().into_iter().map(|(_, name, _)| name).collect();

    assert!(
        names.iter().any(|name| name == "myproj:sub:detail"),
        "the route's reverse name carries the root app_name then the include namespace, got {names:?}",
    );
}

#[test]
fn reverse_names_get_root_prefix_even_when_the_root_include_is_dynamic() {
    let directory = tempfile::tempdir().unwrap();
    let sub = directory.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    // The root urlconf includes its apps dynamically (a comprehension), so no static
    // include connects sub.urls to the root: the chain cannot walk up, and only the
    // project-root app_name prefix can supply `myproj`.
    std::fs::write(
        directory.path().join("urls.py"),
        "from django.apps import apps\nfrom django.urls import include, path\n\n\
         app_name = 'myproj'\n\
         urlpatterns = [path(f'{c}/', include(c.urls)) for c in apps.get_app_configs()]\n",
    )
    .unwrap();

    std::fs::write(sub.join("__init__.py"), "").unwrap();
    std::fs::write(
        sub.join("urls.py"),
        "from django.urls import path\n\nfrom . import views\n\n\
         app_name = 'subapp'\n\
         urlpatterns = [path('detail/', views.detail, name='detail')]\n",
    )
    .unwrap();

    std::fs::write(sub.join("views.py"), "def detail(request):\n    return 1\n").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("proj");

    index_project(&store, &project, "proj", directory.path()).unwrap();

    let names: Vec<String> =
        store.route_reverse_names().unwrap().into_iter().map(|(_, name, _)| name).collect();

    assert!(
        names.iter().any(|name| name == "myproj:subapp:detail"),
        "the root app_name prefixes the reverse name even without a static include, got {names:?}",
    );
}

#[test]
fn namespace_chain_folds_in_app_name_at_the_root_and_for_unnamespaced_includes() {
    // The root urlconf (`urls`) declares app_name='django_spire'; the auth include
    // gives no namespace= kwarg, so its level comes from the auth module's app_name.
    let mut includes: FxHashMap<String, (Option<String>, String)> = FxHashMap::default();
    includes.insert("auth.urls".into(), (Some("auth".into()), "urls".into()));
    includes.insert("auth.user.urls.page_urls".into(), (None, "auth.urls".into()));

    let mut app_names: FxHashMap<String, String> = FxHashMap::default();
    app_names.insert("urls".into(), "django_spire".into());

    // A route in page_urls: the unnamespaced include adds no level, auth contributes
    // `auth`, and the root app_name prepends `django_spire`.
    assert_eq!(
        namespace_chain("auth.user.urls.page_urls", &includes, &app_names),
        Some(vec!["django_spire".to_string(), "auth".to_string()]),
    );

    // A route directly in the root urlconf reverses under the bare app_name.
    assert_eq!(
        namespace_chain("urls", &includes, &app_names),
        Some(vec!["django_spire".to_string()]),
    );
}

#[test]
fn reference_only_versions_are_not_link_targets_but_stay_queryable() {
    let store = Store::open_in_memory().unwrap();

    // The canonical library (what the client actually installs) and a second copy
    // indexed as a reference-only version, both exporting the same symbol from the
    // same module path, so an unfiltered linker would pick one arbitrarily.
    let canonical = tempfile::tempdir().unwrap();
    std::fs::create_dir(canonical.path().join("accounts")).unwrap();
    std::fs::write(canonical.path().join("accounts").join("models.py"), "class User:\n    pass\n").unwrap();
    index_project(&store, &ProjectId::new("django-spire"), "django-spire", canonical.path()).unwrap();

    let next = tempfile::tempdir().unwrap();
    std::fs::create_dir(next.path().join("accounts")).unwrap();
    std::fs::write(next.path().join("accounts").join("models.py"), "class User:\n    pass\n").unwrap();

    let next_project = ProjectId::new("django-spire@next");

    index_project(&store, &next_project, "django-spire@next", next.path()).unwrap();
    store.set_reference_only(&next_project, true).unwrap();

    let client = tempfile::tempdir().unwrap();
    std::fs::write(
        client.path().join("service.py"),
        "from accounts.models import User\n\n\ndef build():\n    return User()\n",
    )
    .unwrap();
    index_project(&store, &ProjectId::new("client"), "client", client.path()).unwrap();

    link_constellation(&store).unwrap();

    let importers_of = |project: &ProjectId| -> Vec<String> {
        let user = store
            .all_nodes(Some(project))
            .unwrap()
            .into_iter()
            .find(|node| node.name == "User" && node.kind == NodeKind::Class)
            .expect("a User class in the project");

        store
            .callers(&user.id)
            .unwrap()
            .into_iter()
            .filter(|(kind, _)| *kind == EdgeKind::Imports)
            .map(|(_, node)| node.project_id.as_str().to_string())
            .collect()
    };

    assert!(
        importers_of(&ProjectId::new("django-spire")).iter().any(|project| project == "client"),
        "the client import binds to the canonical version",
    );

    assert!(
        importers_of(&next_project).is_empty(),
        "the reference-only version is never a cross-project link target",
    );

    assert!(
        store.all_nodes(Some(&next_project)).unwrap().iter().any(|node| node.name == "User"),
        "the reference-only version's symbols remain queryable",
    );
}

#[test]
fn a_reference_only_version_still_links_out_to_canonical() {
    let store = Store::open_in_memory().unwrap();

    // The canonical library defines a base class.
    let canonical = tempfile::tempdir().unwrap();
    std::fs::create_dir(canonical.path().join("core")).unwrap();
    std::fs::write(canonical.path().join("core").join("base.py"), "class BaseThing:\n    pass\n").unwrap();
    index_project(&store, &ProjectId::new("lib"), "lib", canonical.path()).unwrap();

    // A reference-only consumer imports that base; as a link source (not target)
    // its import must still resolve across the boundary.
    let next = tempfile::tempdir().unwrap();
    std::fs::write(
        next.path().join("consumer.py"),
        "from core.base import BaseThing\n\n\ndef use():\n    return BaseThing()\n",
    )
    .unwrap();

    let next_project = ProjectId::new("consumer@next");

    index_project(&store, &next_project, "consumer@next", next.path()).unwrap();
    store.set_reference_only(&next_project, true).unwrap();

    let linked = link_constellation(&store).unwrap();

    assert!(linked >= 1, "a reference-only project still links out to a canonical target");

    let base = store
        .all_nodes(Some(&ProjectId::new("lib")))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "BaseThing")
        .expect("BaseThing in the canonical lib");

    let importers: Vec<String> = store
        .callers(&base.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Imports)
        .map(|(_, node)| node.project_id.as_str().to_string())
        .collect();

    assert!(
        importers.iter().any(|project| project == "consumer@next"),
        "the reference-only consumer's import resolves to the canonical base, got {importers:?}",
    );
}

#[test]
fn a_call_binds_to_the_method_a_cross_project_base_class_defines() {
    let store = Store::open_in_memory().unwrap();

    // The library owns the base queryset and the base test case, under the
    // package directory its consumers spell in an import.
    let library = tempfile::tempdir().unwrap();
    std::fs::create_dir(library.path().join("spire")).unwrap();
    std::fs::write(
        library.path().join("spire").join("querysets.py"),
        "class HistoryQuerySet:\n    def active(self):\n        return self\n",
    )
    .unwrap();
    std::fs::write(
        library.path().join("spire").join("cases.py"),
        "class BaseTestCase:\n    def setUp(self):\n        return 1\n",
    )
    .unwrap();

    index_project(&store, &ProjectId::new("spire"), "spire", library.path()).unwrap();

    // The app subclasses both and calls the inherited methods three ways.
    let app = tempfile::tempdir().unwrap();
    std::fs::write(
        app.path().join("querysets.py"),
        "from spire.querysets import HistoryQuerySet\n\n\nclass ArticleQuerySet(HistoryQuerySet):\n\
         \x20   def recent(self):\n        return self.active()\n",
    )
    .unwrap();
    std::fs::write(
        app.path().join("views.py"),
        "import models\n\n\ndef listing(request):\n\
         \x20   return models.Article.objects.active()\n",
    )
    .unwrap();
    std::fs::write(
        app.path().join("tests.py"),
        "from spire.cases import BaseTestCase\n\n\nclass ArticleTestCase(BaseTestCase):\n\
         \x20   def setUp(self):\n        super().setUp()\n",
    )
    .unwrap();
    std::fs::write(app.path().join("models.py"), "class Article:\n    pass\n").unwrap();

    index_project(&store, &ProjectId::new("app"), "app", app.path()).unwrap();
    link_constellation(&store).unwrap();

    let library_project = ProjectId::new("spire");
    let nodes = store.all_nodes(Some(&library_project)).unwrap();

    let active = nodes
        .iter()
        .find(|node| node.name == "active" && node.kind == NodeKind::Method)
        .expect("HistoryQuerySet.active");

    let set_up = nodes
        .iter()
        .find(|node| node.name == "setUp" && node.kind == NodeKind::Method)
        .expect("BaseTestCase.setUp");

    let callers_of = |id: &NodeId| -> Vec<String> {
        store
            .callers(id)
            .unwrap()
            .into_iter()
            .filter(|(kind, _)| *kind == EdgeKind::Calls)
            .map(|(_, node)| node.qualified_name.clone())
            .collect()
    };

    let active_callers = callers_of(&active.id);

    assert!(
        active_callers.iter().any(|name| name.ends_with("ArticleQuerySet.recent")),
        "self.active() binds to the base queryset's method, got {active_callers:?}",
    );

    assert!(
        active_callers.iter().any(|name| name.ends_with("listing")),
        "models.Article.objects.active() binds through the model's queryset, got {active_callers:?}",
    );

    let set_up_callers = callers_of(&set_up.id);

    assert!(
        set_up_callers.iter().any(|name| name.ends_with("ArticleTestCase.setUp")),
        "super().setUp() binds to the base, not to the overriding method, got {set_up_callers:?}",
    );
}

#[test]
fn an_ambiguous_base_leaves_an_inherited_call_unresolved() {
    let store = Store::open_in_memory().unwrap();

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("mixins.py"),
        "class LeftMixin:\n    def render(self):\n        return 1\n\n\n\
         class RightMixin:\n    def render(self):\n        return 2\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("views.py"),
        "from mixins import LeftMixin, RightMixin\n\n\n\
         class PageView(LeftMixin, RightMixin):\n    def get(self):\n        return super().render()\n",
    )
    .unwrap();

    let project = ProjectId::new("app");

    index_project(&store, &project, "app", directory.path()).unwrap();
    link_constellation(&store).unwrap();

    let nodes = store.all_nodes(Some(&project)).unwrap();

    let renders: Vec<&NodeId> = nodes
        .iter()
        .filter(|node| node.name == "render" && node.kind == NodeKind::Method)
        .map(|node| &node.id)
        .collect();

    assert_eq!(renders.len(), 2, "both mixins define render");

    for render in renders {
        let callers: Vec<String> = store
            .callers(render)
            .unwrap()
            .into_iter()
            .filter(|(kind, _)| *kind == EdgeKind::Calls)
            .map(|(_, node)| node.qualified_name.clone())
            .collect();

        assert!(
            callers.is_empty(),
            "two mixins at the same depth define render, so neither may be bound, got {callers:?}",
        );
    }
}

#[test]
fn a_module_qualified_call_binds_to_the_function_that_module_defines() {
    let store = Store::open_in_memory().unwrap();

    let library = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(library.path().join("spire/generic")).unwrap();
    std::fs::write(
        library.path().join("spire/generic/portal_views.py"),
        "def template_view(request):\n    return 1\n",
    )
    .unwrap();
    // A same-named function elsewhere must not win; only the named module counts.
    std::fs::write(
        library.path().join("spire/decoy.py"),
        "def template_view(request):\n    return 2\n",
    )
    .unwrap();

    index_project(&store, &ProjectId::new("spire"), "spire", library.path()).unwrap();

    let app = tempfile::tempdir().unwrap();
    std::fs::write(
        app.path().join("views.py"),
        "from spire.generic import portal_views\n\n\ndef listing(request):\n\
         \x20   return portal_views.template_view(request)\n",
    )
    .unwrap();

    index_project(&store, &ProjectId::new("app"), "app", app.path()).unwrap();
    link_constellation(&store).unwrap();

    let target = store
        .all_nodes(Some(&ProjectId::new("spire")))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "template_view" && node.file_path.ends_with("portal_views.py"))
        .expect("template_view in portal_views");

    let callers: Vec<String> = store
        .callers(&target.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name.clone())
        .collect();

    assert!(
        callers.iter().any(|name| name.ends_with("listing")),
        "portal_views.template_view() binds to the function that module defines, got {callers:?}",
    );

    let decoy = store
        .all_nodes(Some(&ProjectId::new("spire")))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "template_view" && node.file_path.ends_with("decoy.py"))
        .expect("decoy template_view");

    let decoy_callers: Vec<String> = store
        .callers(&decoy.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name.clone())
        .collect();

    assert!(
        decoy_callers.is_empty(),
        "the same-named function in another module must not be bound, got {decoy_callers:?}",
    );
}

#[test]
fn a_call_through_a_model_relation_binds_to_the_related_models_queryset() {
    let store = Store::open_in_memory().unwrap();

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("querysets.py"),
        "class LocationQuerySet:\n    def lots(self):\n        return self\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\n\
         class Location(models.Model):\n    name = models.CharField(max_length=10)\n\n\n\
         class HarvestLoad(models.Model):\n\
         \x20   locations = models.ManyToManyField('location.Location', related_name='harvest_loads')\n\n\
         \x20   @property\n\
         \x20   def lot_display(self):\n        return self.locations.lots()\n",
    )
    .unwrap();

    let project = ProjectId::new("app");

    index_project(&store, &project, "app", directory.path()).unwrap();
    link_constellation(&store).unwrap();

    let lots = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "lots" && node.kind == NodeKind::Method)
        .expect("LocationQuerySet.lots");

    let callers: Vec<String> = store
        .callers(&lots.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name.clone())
        .collect();

    assert!(
        callers.iter().any(|name| name.ends_with("HarvestLoad.lot_display")),
        "self.locations.lots() types the field as Location and binds its queryset, got {callers:?}",
    );
}

#[test]
fn a_call_through_an_annotated_local_and_related_name_binds() {
    let store = Store::open_in_memory().unwrap();

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("querysets.py"),
        "class ContactQuerySet:\n    def without_location(self):\n        return self\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("models.py"),
        "from django.db import models\n\n\n\
         class Company(models.Model):\n    name = models.CharField(max_length=10)\n\n\n\
         class Contact(models.Model):\n\
         \x20   company = models.ForeignKey('company.Company', related_name='contacts', on_delete=models.CASCADE)\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("views.py"),
        "from models import Company\n\n\n\
         def listing(request, company: Company):\n\
         \x20   return company.contacts.without_location()\n",
    )
    .unwrap();

    let project = ProjectId::new("app");

    index_project(&store, &project, "app", directory.path()).unwrap();
    link_constellation(&store).unwrap();

    let target = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "without_location")
        .expect("ContactQuerySet.without_location");

    let callers: Vec<String> = store
        .callers(&target.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::Calls)
        .map(|(_, node)| node.qualified_name.clone())
        .collect();

    assert!(
        callers.iter().any(|name| name.ends_with("listing")),
        "an annotated local plus a related_name types the receiver, got {callers:?}",
    );
}

#[test]
fn a_cross_project_base_survives_a_re_index() {
    let store = Store::open_in_memory().unwrap();

    let library = tempfile::tempdir().unwrap();
    let package = library.path().join("django_spire");
    std::fs::create_dir_all(package.join("history")).unwrap();

    std::fs::write(
        package.join("history").join("mixins.py"),
        "from django.db import models


class HistoryModelMixin(models.Model):
    is_active = models.BooleanField(default=True)

    class Meta:
        abstract = True
",
    )
    .unwrap();

    index_project(&store, &ProjectId::new("django-spire"), "django-spire", &package).unwrap();

    let app = tempfile::tempdir().unwrap();
    let models = app.path().join("app").join("inventory").join("models.py");
    std::fs::create_dir_all(models.parent().unwrap()).unwrap();
    std::fs::write(app.path().join("manage.py"), "# django
").unwrap();

    let source = "from django.db import models

from django_spire.history.mixins import HistoryModelMixin


class Inventory(HistoryModelMixin):
    name = models.CharField(max_length=255)
";

    std::fs::write(&models, source).unwrap();

    index_project(&store, &ProjectId::new("portal"), "portal", app.path()).unwrap();

    link_constellation(&store).unwrap();

    // The edit a watcher serves. Re-indexing re-derives the portal's external stubs,
    // so `Inventory` extends a fresh un-unified stub again; the first link already
    // consumed the reference rows that produced it, so an un-relinked re-index does
    // not delay this edge, it loses it permanently.
    std::fs::write(&models, format!("{source}    sku = models.CharField(max_length=64)
")).unwrap();

    index_project(&store, &ProjectId::new("portal"), "portal", app.path()).unwrap();
    link_constellation(&store).unwrap();

    let nodes = store.all_nodes(None).unwrap();

    let base = nodes
        .iter()
        .find(|node| node.name == "HistoryModelMixin" && node.kind != NodeKind::External)
        .expect("the companion HistoryModelMixin is indexed");

    let callers = store.callers(&base.id).unwrap();

    assert!(
        callers.iter().any(|(kind, node)| *kind == EdgeKind::Extends && node.name == "Inventory"),
        "a re-indexed model must still extend the companion base, got {:?}",
        callers.iter().map(|(k, n)| (*k, n.name.clone())).collect::<Vec<_>>(),
    );
}

/// Django's `path(route, view, ...)` takes its handler by keyword as readily as
/// positionally, and django-spire writes it that way. Reading only the
/// positional slot emitted no reference at all, so the route rendered
/// unresolved with nothing to say about what it had named: the one shape the
/// diagnostic cannot explain, and the reason this is a test rather than a note.
#[test]
fn a_handler_passed_as_the_view_keyword_still_binds() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();

    std::fs::create_dir_all(root.join("app/notification/views")).unwrap();
    std::fs::create_dir_all(root.join("app/notification/urls")).unwrap();

    std::fs::write(
        root.join("app/notification/views/page_views.py"),
        "def notification_list_view(request):\n    return None\n",
    )
    .unwrap();

    std::fs::write(
        root.join("app/notification/urls/page_urls.py"),
        "from django.urls import path\n\n\
         from app.notification.views import page_views\n\n\n\
         urlpatterns = [\n\
         \x20   path('list/',\n\
         \x20       view=page_views.notification_list_view,\n\
         \x20       name='list')\n\
         ]\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("portal");

    index_project(&store, &project, "portal", root).unwrap();

    let route = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Route && node.name == "list")
        .expect("the route node");

    let views: Vec<String> = store
        .callees(&route.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, _)| *kind == EdgeKind::RoutesTo)
        .map(|(_, node)| node.name)
        .collect();

    assert_eq!(views, ["notification_list_view"], "the keyword handler binds like a positional one");
}
