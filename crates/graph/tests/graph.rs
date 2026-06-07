use std::collections::HashSet;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span, Visibility,
};

const NODE_KINDS: [NodeKind; 17] = [
    NodeKind::Class,
    NodeKind::Constant,
    NodeKind::Field,
    NodeKind::File,
    NodeKind::Function,
    NodeKind::Import,
    NodeKind::Method,
    NodeKind::Module,
    NodeKind::Parameter,
    NodeKind::Property,
    NodeKind::Variable,
    NodeKind::Model,
    NodeKind::Route,
    NodeKind::Template,
    NodeKind::View,
    NodeKind::Selector,
    NodeKind::External,
];

const EDGE_KINDS: [EdgeKind; 28] = [
    EdgeKind::Calls,
    EdgeKind::Contains,
    EdgeKind::Decorates,
    EdgeKind::Extends,
    EdgeKind::Imports,
    EdgeKind::Instantiates,
    EdgeKind::Overrides,
    EdgeKind::References,
    EdgeKind::Returns,
    EdgeKind::TypeOf,
    EdgeKind::ExtendsTemplate,
    EdgeKind::Handles,
    EdgeKind::IncludesTemplate,
    EdgeKind::Receives,
    EdgeKind::RelatesTo,
    EdgeKind::Renders,
    EdgeKind::Resolves,
    EdgeKind::RoutesTo,
    EdgeKind::Styles,
    EdgeKind::AdminOf,
    EdgeKind::OverridesTemplate,
    EdgeKind::Tests,
    EdgeKind::Reads,
    EdgeKind::AccessesMember,
    EdgeKind::ContextType,
    EdgeKind::LoopBinding,
    EdgeKind::ReverseAccessor,
    EdgeKind::DerivedCollection,
];

const LANGUAGES: [Language; 4] = [
    Language::Css,
    Language::HtmlDjango,
    Language::JavaScript,
    Language::Python,
];

const VISIBILITIES: [Visibility; 3] = [
    Visibility::Private,
    Visibility::Protected,
    Visibility::Public,
];

fn identity(name: &str) -> NodeIdentity {
    NodeIdentity {
        name: name.to_string(),
        qualified_name: format!("module.{name}"),
        file_path: "module.py".to_string(),
        language: Language::Python,
    }
}

fn sample_node(name: &str) -> Node {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, &format!("module.{name}"));

    Node::new(id, project, NodeKind::Function, identity(name), Span::new(1, 1, 0, 0), 0)
}

#[test]
fn project_id_round_trips_through_as_str_and_display() {
    let project = ProjectId::new("blog");

    assert_eq!(project.as_str(), "blog", "as_str returns the stored name");
    assert_eq!(project.to_string(), "blog", "Display writes the bare name");
    assert_eq!(format!("{project}"), "blog", "the format machinery agrees with to_string");
}

#[test]
fn project_id_accepts_an_owned_string() {
    let owned = String::from("shop");

    assert_eq!(ProjectId::new(owned).as_str(), "shop", "Into<String> takes an owned name");
}

#[test]
fn project_id_supports_equality_hashing_and_ordering() {
    let mut set = HashSet::new();

    set.insert(ProjectId::new("alpha"));
    set.insert(ProjectId::new("alpha"));
    set.insert(ProjectId::new("beta"));

    assert_eq!(set.len(), 2, "equal ids collapse to one entry in a hash set");
    assert!(ProjectId::new("alpha") < ProjectId::new("beta"), "ids order lexically");
}

#[test]
#[should_panic(expected = "must not be empty")]
fn project_id_rejects_an_empty_name() {
    ProjectId::new("");
}

#[test]
#[should_panic(expected = "must not contain")]
fn project_id_rejects_an_embedded_separator() {
    ProjectId::new("blog::shop");
}

#[test]
fn node_id_new_prefixes_the_qualified_name() {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, "app.py::handler");

    assert_eq!(id.as_str(), "blog::app.py::handler", "the project prefixes the qualified name");
    assert_eq!(id.project_prefix(), "blog", "the prefix is everything before the first separator");
    assert_eq!(id.to_string(), "blog::app.py::handler", "Display writes the full id");
}

#[test]
fn node_id_from_raw_passes_a_prefixed_id_through_unchanged() {
    let id = NodeId::from_raw("shop::lib.py::Widget");

    assert_eq!(id.as_str(), "shop::lib.py::Widget", "the raw id is stored verbatim");
    assert_eq!(id.project_prefix(), "shop", "the prefix splits on the first separator only");
}

#[test]
fn node_id_supports_equality_hashing_and_ordering() {
    let mut set = HashSet::new();

    set.insert(NodeId::from_raw("p::a"));
    set.insert(NodeId::from_raw("p::a"));

    assert_eq!(set.len(), 1, "equal node ids collapse in a hash set");
    assert!(NodeId::from_raw("p::a") < NodeId::from_raw("p::b"), "node ids order by their string");
}

#[test]
#[should_panic(expected = "must not be empty")]
fn node_id_new_rejects_an_empty_qualified_name() {
    NodeId::new(&ProjectId::new("blog"), "");
}

#[test]
#[should_panic(expected = "must not begin with")]
fn node_id_new_rejects_a_leading_separator() {
    NodeId::new(&ProjectId::new("blog"), "::handler");
}

#[test]
#[should_panic(expected = "must not be empty")]
fn node_id_from_raw_rejects_an_empty_string() {
    NodeId::from_raw("");
}

#[test]
#[should_panic(expected = "must carry a project prefix")]
fn node_id_from_raw_rejects_a_missing_separator() {
    NodeId::from_raw("noseparator");
}

#[test]
fn language_as_str_covers_every_variant_distinctly() {
    let labels: HashSet<&str> = LANGUAGES.iter().map(|language| language.as_str()).collect();

    assert_eq!(labels.len(), LANGUAGES.len(), "every language has a distinct label");
    assert_eq!(Language::HtmlDjango.as_str(), "htmldjango", "the Django template label carries no underscore");
}

#[test]
fn language_from_str_label_round_trips_and_rejects_unknown() {
    for language in LANGUAGES {
        assert_eq!(
            Language::from_str_label(language.as_str()),
            Some(language),
            "{language:?} round trips through its label",
        );
    }

    assert_eq!(Language::from_str_label("rust"), None, "an unindexed language has no label");
    assert_eq!(Language::from_str_label("HTMLDJANGO"), None, "labels are case sensitive");
}

#[test]
fn language_from_extension_maps_the_target_stack() {
    assert_eq!(Language::from_extension("css"), Some(Language::Css));
    assert_eq!(Language::from_extension("htm"), Some(Language::HtmlDjango), "htm shares the html mapping");
    assert_eq!(Language::from_extension("html"), Some(Language::HtmlDjango));
    assert_eq!(Language::from_extension("js"), Some(Language::JavaScript));
    assert_eq!(Language::from_extension("mjs"), Some(Language::JavaScript), "mjs shares the js mapping");
    assert_eq!(Language::from_extension("py"), Some(Language::Python));
    assert_eq!(Language::from_extension("pyi"), Some(Language::Python), "stub files map to Python");
}

#[test]
fn language_from_extension_rejects_unknown_and_uppercase() {
    assert_eq!(Language::from_extension("rs"), None, "an out-of-stack extension has no language");
    assert_eq!(Language::from_extension("ts"), None, "TypeScript is out of scope");
    assert_eq!(Language::from_extension(""), None, "an empty extension has no language");
    assert_eq!(Language::from_extension("CSS"), None, "extensions match case sensitively");
}

#[test]
fn node_kind_as_str_covers_every_variant_distinctly() {
    let labels: HashSet<&str> = NODE_KINDS.iter().map(|kind| kind.as_str()).collect();

    assert_eq!(labels.len(), NODE_KINDS.len(), "every node kind has a distinct label");
}

#[test]
fn node_kind_from_str_label_round_trips_and_rejects_unknown() {
    for kind in NODE_KINDS {
        assert_eq!(NodeKind::from_str_label(kind.as_str()), Some(kind), "{kind:?} round trips");
    }

    assert_eq!(NodeKind::from_str_label("widget"), None, "an unknown label parses to nothing");
    assert_eq!(NodeKind::from_str_label("Model"), None, "labels are lowercase only");
}

#[test]
fn visibility_round_trips_and_rejects_unknown() {
    let labels: HashSet<&str> = VISIBILITIES.iter().map(|visibility| visibility.as_str()).collect();

    assert_eq!(labels.len(), VISIBILITIES.len(), "every visibility has a distinct label");

    for visibility in VISIBILITIES {
        assert_eq!(
            Visibility::from_str_label(visibility.as_str()),
            Some(visibility),
            "{visibility:?} round trips",
        );
    }

    assert_eq!(Visibility::from_str_label("internal"), None, "an unknown visibility parses to nothing");
}

#[test]
fn edge_kind_as_str_covers_every_variant_distinctly() {
    let labels: HashSet<&str> = EDGE_KINDS.iter().map(|kind| kind.as_str()).collect();

    assert_eq!(labels.len(), EDGE_KINDS.len(), "every edge kind has a distinct label");
}

#[test]
fn edge_kind_uses_snake_case_for_multiword_labels() {
    assert_eq!(EdgeKind::TypeOf.as_str(), "type_of");
    assert_eq!(EdgeKind::ExtendsTemplate.as_str(), "extends_template");
    assert_eq!(EdgeKind::IncludesTemplate.as_str(), "includes_template");
    assert_eq!(EdgeKind::RelatesTo.as_str(), "relates_to");
    assert_eq!(EdgeKind::RoutesTo.as_str(), "routes_to");
    assert_eq!(EdgeKind::AccessesMember.as_str(), "accesses_member");
    assert_eq!(EdgeKind::ReverseAccessor.as_str(), "reverse_accessor");
    assert_eq!(EdgeKind::DerivedCollection.as_str(), "derived_collection");
}

#[test]
fn edge_kind_from_str_label_round_trips_and_rejects_unknown() {
    for kind in EDGE_KINDS {
        assert_eq!(EdgeKind::from_str_label(kind.as_str()), Some(kind), "{kind:?} round trips");
    }

    assert_eq!(EdgeKind::from_str_label("frobnicates"), None, "an unknown label parses to nothing");
    assert_eq!(EdgeKind::from_str_label("typeOf"), None, "labels are snake_case, not camelCase");
}

#[test]
fn span_stores_its_bounds_and_compares_by_value() {
    let span = Span::new(3, 7, 4, 9);

    assert_eq!(span.start_line, 3);
    assert_eq!(span.end_line, 7);
    assert_eq!(span.start_column, 4);
    assert_eq!(span.end_column, 9);
    assert_eq!(span, Span::new(3, 7, 4, 9), "spans compare by value");
    assert_ne!(span, Span::new(3, 8, 4, 9), "a differing end line makes a different span");
}

#[test]
fn span_allows_a_single_line() {
    let span = Span::new(5, 5, 0, 10);

    assert_eq!(span.start_line, span.end_line, "start may equal end for a one-line span");
}

#[test]
#[should_panic(expected = "1-based")]
fn span_rejects_a_zero_start_line() {
    Span::new(0, 1, 0, 0);
}

#[test]
#[should_panic(expected = "must not exceed end_line")]
fn span_rejects_a_start_line_past_the_end() {
    Span::new(4, 2, 0, 0);
}

#[test]
fn node_new_clears_every_optional_attribute() {
    let node = sample_node("handler");

    assert_eq!(node.name, "handler");
    assert_eq!(node.qualified_name, "module.handler");
    assert_eq!(node.kind, NodeKind::Function);
    assert_eq!(node.language, Language::Python);
    assert_eq!(node.docstring, None, "the docstring starts cleared");
    assert_eq!(node.signature, None, "the signature starts cleared");
    assert_eq!(node.visibility, None, "the visibility starts cleared");
    assert!(!node.is_exported, "exported starts false");
    assert!(!node.is_async, "async starts false");
    assert!(!node.is_static, "static starts false");
    assert!(!node.is_abstract, "abstract starts false");
    assert!(node.decorators.is_empty(), "decorators start empty");
    assert_eq!(node.updated_at_ms, 0);
}

#[test]
fn node_new_admits_a_zero_timestamp() {
    let node = sample_node("at_epoch");

    assert_eq!(node.updated_at_ms, 0, "a zero timestamp is the non-negative boundary");
}

#[test]
#[should_panic(expected = "name must not be empty")]
fn node_new_rejects_an_empty_name() {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, "module.x");
    let identity = NodeIdentity {
        name: String::new(),
        qualified_name: "module.x".to_string(),
        file_path: "module.py".to_string(),
        language: Language::Python,
    };

    Node::new(id, project, NodeKind::Function, identity, Span::new(1, 1, 0, 0), 0);
}

#[test]
#[should_panic(expected = "file_path must not be empty")]
fn node_new_rejects_an_empty_file_path() {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, "module.x");
    let identity = NodeIdentity {
        name: "x".to_string(),
        qualified_name: "module.x".to_string(),
        file_path: String::new(),
        language: Language::Python,
    };

    Node::new(id, project, NodeKind::Function, identity, Span::new(1, 1, 0, 0), 0);
}

#[test]
#[should_panic(expected = "non-negative")]
fn node_new_rejects_a_negative_timestamp() {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, "module.x");

    Node::new(id, project, NodeKind::Function, identity("x"), Span::new(1, 1, 0, 0), -1);
}

#[test]
fn edge_new_leaves_location_and_provenance_unset() {
    let source = NodeId::from_raw("blog::a");
    let target = NodeId::from_raw("blog::b");
    let edge = Edge::new(source, target, EdgeKind::Calls);

    assert_eq!(edge.kind, EdgeKind::Calls);
    assert_eq!(edge.line, None, "a fresh edge has no recorded line");
    assert_eq!(edge.column, None, "a fresh edge has no recorded column");
    assert_eq!(edge.provenance, None, "a fresh edge has no provenance");
}

#[test]
fn edge_builders_attach_location_and_provenance() {
    let source = NodeId::from_raw("blog::a");
    let target = NodeId::from_raw("blog::b");
    let edge = Edge::new(source, target, EdgeKind::Imports).at(12, 4).with_provenance("resolver");

    assert_eq!(edge.line, Some(12), "at records the 1-based line");
    assert_eq!(edge.column, Some(4), "at records the column");
    assert_eq!(edge.provenance.as_deref(), Some("resolver"), "with_provenance tags the producing pass");
}

#[test]
fn edge_is_cross_project_compares_endpoint_prefixes() {
    let blog = ProjectId::new("blog");
    let shop = ProjectId::new("shop");

    let across = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&shop, "b"), EdgeKind::Imports);
    let within = Edge::new(NodeId::new(&blog, "a"), NodeId::new(&blog, "b"), EdgeKind::Calls);

    assert!(across.is_cross_project(), "endpoints in differing projects make a cross-project edge");
    assert!(!within.is_cross_project(), "endpoints in one project are not cross-project");
}

#[test]
#[should_panic(expected = "1-based")]
fn edge_at_rejects_a_zero_line() {
    let source = NodeId::from_raw("blog::a");
    let target = NodeId::from_raw("blog::b");

    Edge::new(source, target, EdgeKind::Calls).at(0, 0);
}

#[test]
#[should_panic(expected = "provenance label must not be empty")]
fn edge_with_provenance_rejects_an_empty_label() {
    let source = NodeId::from_raw("blog::a");
    let target = NodeId::from_raw("blog::b");

    Edge::new(source, target, EdgeKind::Calls).with_provenance("");
}
