use std::sync::Arc;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_linking::{
    ImportLinker, LinkContext, PendingImport, ProjectLink, is_linkable, module_matches,
};

const LINKABLE_KINDS: [NodeKind; 8] = [
    NodeKind::Class,
    NodeKind::Constant,
    NodeKind::File,
    NodeKind::Function,
    NodeKind::Method,
    NodeKind::Model,
    NodeKind::Variable,
    NodeKind::View,
];

const UNLINKABLE_KINDS: [NodeKind; 9] = [
    NodeKind::Field,
    NodeKind::Import,
    NodeKind::Module,
    NodeKind::Parameter,
    NodeKind::Property,
    NodeKind::Route,
    NodeKind::Template,
    NodeKind::Selector,
    NodeKind::External,
];

struct FakeContext {
    nodes: Vec<Arc<Node>>,
    packages: Vec<(String, String)>,
}

impl FakeContext {
    fn new(nodes: Vec<Arc<Node>>) -> Self {
        Self { nodes, packages: Vec::new() }
    }

    fn with_package(mut self, package: &str, project: &str) -> Self {
        self.packages.push((package.to_string(), project.to_string()));

        self
    }
}

impl LinkContext for FakeContext {
    fn exports_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.name == name).cloned().collect()
    }

    fn project_for_package(&self, package: &str) -> Option<&str> {
        self.packages.iter().find(|(pkg, _)| pkg == package).map(|(_, project)| project.as_str())
    }
}

fn export(project: &str, file: &str, name: &str, kind: NodeKind) -> Arc<Node> {
    let project_id = ProjectId::new(project);
    let id = NodeId::new(&project_id, &format!("{file}::{name}"));

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: format!("{file}::{name}"),
        file_path: file.to_string(),
        language: Language::Python,
    };

    Arc::new(Node::new(id, project_id, kind, identity, Span::new(1, 1, 0, 0), 0))
}

fn pending(project: &str, reference_name: &str, module: &str) -> PendingImport {
    let project_id = ProjectId::new(project);
    let from_node_id = NodeId::new(&project_id, "caller.py::caller");

    PendingImport {
        project_id,
        from_node_id,
        reference_name: reference_name.to_string(),
        module: module.to_string(),
        line: 3,
        column: 0,
    }
}

#[test]
fn is_linkable_admits_importable_kinds_only() {
    for kind in LINKABLE_KINDS {
        assert!(is_linkable(kind), "{kind:?} is something another project imports");
    }

    for kind in UNLINKABLE_KINDS {
        assert!(!is_linkable(kind), "{kind:?} is never a cross-project import target");
    }
}

#[test]
fn module_matches_when_the_file_path_carries_the_import_module() {
    assert!(
        module_matches("app.models", "src/app/models.py"),
        "the dotted file path ends with the imported module",
    );

    assert!(
        module_matches("models", "src/app/models.py"),
        "a short module still matches as a suffix of the file path",
    );
}

#[test]
fn module_matches_in_either_direction_so_differing_roots_do_not_defeat_it() {
    assert!(
        module_matches("src.app.models", "app/models.py"),
        "the import path may itself end with the rooted file module",
    );
}

#[test]
fn module_matches_drops_a_trailing_init() {
    assert!(
        module_matches("src.app", "src/app/__init__.py"),
        "a package __init__ reduces to the package module",
    );

    assert!(
        !module_matches("src.app.models", "src/app/__init__.py"),
        "the package module does not match an unrelated submodule",
    );
}

#[test]
fn module_matches_rejects_unrelated_paths() {
    assert!(
        !module_matches("other.thing", "src/app/models.py"),
        "neither path is a suffix of the other",
    );
}

#[test]
#[should_panic(expected = "must not be empty")]
fn module_matches_rejects_an_empty_module() {
    module_matches("", "src/app/models.py");
}

#[test]
fn project_link_keeps_its_edge_and_confidence() {
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");
    let edge = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&shop, "b"), EdgeKind::Imports);

    let link = ProjectLink::new(edge, 0.85);

    assert_eq!(link.confidence, 0.85, "the confidence is preserved");
    assert!(link.edge.is_cross_project(), "the wrapped edge crosses a boundary");
}

#[test]
fn project_link_admits_the_confidence_boundaries() {
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");
    let make = || Edge::new(NodeId::new(&blog, "a"), NodeId::new(&shop, "b"), EdgeKind::Imports);

    assert_eq!(ProjectLink::new(make(), 0.0).confidence, 0.0, "zero is an admissible confidence");
    assert_eq!(ProjectLink::new(make(), 1.0).confidence, 1.0, "one is an admissible confidence");
}

#[test]
#[should_panic(expected = "must cross a project boundary")]
fn project_link_rejects_a_same_project_edge() {
    let blog = ProjectId::new("blog");
    let edge = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&blog, "b"), EdgeKind::Imports);

    ProjectLink::new(edge, 0.85);
}

#[test]
#[should_panic(expected = "must not exceed one")]
fn project_link_rejects_confidence_above_one() {
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");
    let edge = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&shop, "b"), EdgeKind::Imports);

    ProjectLink::new(edge, 1.5);
}

#[test]
#[should_panic(expected = "non-negative")]
fn project_link_rejects_a_negative_confidence() {
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");
    let edge = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&shop, "b"), EdgeKind::Imports);

    ProjectLink::new(edge, -0.1);
}

#[test]
fn link_joins_a_pending_import_to_a_matching_export_in_another_project() {
    let context = FakeContext::new(vec![export("shop", "shop/billing/models.py", "Invoice", NodeKind::Model)]);
    let import = pending("blog", "Invoice", "shop.billing.models");

    let link = ImportLinker.link(&import, &context).expect("a module-matched export links");

    assert_eq!(link.confidence, 0.85, "a module-evidenced link carries the fixed confidence");
    assert_eq!(link.edge.kind, EdgeKind::Imports, "the synthesized edge is an import");
    assert!(link.edge.is_cross_project(), "the link spans the two projects");
    assert_eq!(link.edge.source, import.from_node_id, "the edge starts at the importing node");
    assert_eq!(link.edge.line, Some(3), "the import site location rides onto the edge");

    assert_eq!(
        link.edge.provenance.as_deref(),
        Some("link:blog->shop"),
        "provenance records the crossed boundary",
    );
}

#[test]
fn link_declines_when_no_other_project_exports_the_name() {
    let context = FakeContext::new(vec![export("blog", "blog/models.py", "Invoice", NodeKind::Model)]);
    let import = pending("blog", "Invoice", "blog.models");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "a same-project export is not a cross-project link",
    );
}

#[test]
fn link_declines_without_module_evidence() {
    let context = FakeContext::new(vec![export("shop", "shop/billing/models.py", "Invoice", NodeKind::Model)]);
    let import = pending("blog", "Invoice", "");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "a bare same-name match with no module path stays unlinked",
    );
}

#[test]
fn link_declines_when_the_module_path_disagrees() {
    let context = FakeContext::new(vec![export("shop", "shop/billing/models.py", "Invoice", NodeKind::Model)]);
    let import = pending("blog", "Invoice", "warehouse.inventory");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "a name match whose module path disagrees is not linked",
    );
}

#[test]
fn link_uses_package_to_project_evidence_for_a_reexported_symbol() {
    // The defining file sits deeper than the imported package (a re-export) and
    // under a stripped package root, so module_matches finds no overlap; the
    // package name still pins the project.
    let context = FakeContext::new(vec![export(
        "django-spire",
        "contrib/seeding/model/django/seeder.py",
        "DjangoModelSeeder",
        NodeKind::Class,
    )])
    .with_package("django_spire", "django-spire");

    let import = pending("shop", "DjangoModelSeeder", "django_spire.contrib.seeding");

    let link = ImportLinker.link(&import, &context).expect("package evidence links the re-export");

    assert_eq!(link.confidence, 0.8, "a package-evidenced link carries the lower confidence");
    assert!(link.edge.is_cross_project(), "the link spans the two projects");

    assert_eq!(
        link.edge.provenance.as_deref(),
        Some("link:shop->django-spire"),
        "provenance records the crossed boundary",
    );
}

#[test]
fn link_declines_package_evidence_when_the_project_has_two_same_named_exports() {
    let context = FakeContext::new(vec![
        export("django-spire", "a/x.py", "Thing", NodeKind::Class),
        export("django-spire", "b/y.py", "Thing", NodeKind::Class),
    ])
    .with_package("django_spire", "django-spire");

    let import = pending("shop", "Thing", "django_spire.contrib");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "two same-named exports in the named project are ambiguous and stay unlinked",
    );
}

#[test]
fn link_skips_an_unlinkable_export_kind() {
    let context = FakeContext::new(vec![export("shop", "shop/billing/models.py", "Invoice", NodeKind::Import)]);
    let import = pending("blog", "Invoice", "shop.billing.models");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "an import-kind export is filtered before matching",
    );
}

#[test]
fn link_skips_a_generated_target_even_with_a_module_match() {
    let context = FakeContext::new(vec![export("shop", "shop/migrations/0001_initial.py", "Invoice", NodeKind::Model)]);
    let import = pending("blog", "Invoice", "shop.migrations.0001_initial");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "a symbol defined in a migrations directory is never a real import target",
    );
}

#[test]
fn link_skips_a_minified_target() {
    let context = FakeContext::new(vec![export("shop", "shop/static/Chart.min.js", "path", NodeKind::Function)]);
    let import = pending("blog", "path", "shop.static.Chart.min");

    assert!(
        ImportLinker.link(&import, &context).is_none(),
        "a symbol parsed out of a minified bundle is filtered",
    );
}

#[test]
fn link_picks_the_module_matched_export_among_several_candidates() {
    let context = FakeContext::new(vec![
        export("warehouse", "warehouse/inventory/models.py", "Invoice", NodeKind::Model),
        export("shop", "shop/billing/models.py", "Invoice", NodeKind::Model),
    ]);

    let import = pending("blog", "Invoice", "shop.billing.models");

    let link = ImportLinker.link(&import, &context).expect("the module-matched candidate wins");

    assert_eq!(link.edge.target.project_prefix(), "shop", "the edge targets the project whose path matched");
}

#[test]
#[should_panic(expected = "must not be empty")]
fn link_rejects_an_empty_reference_name() {
    let context = FakeContext::new(Vec::new());
    let import = pending("blog", "", "shop.billing.models");

    let _ = ImportLinker.link(&import, &context);
}
