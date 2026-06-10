use crate::ids::NodeId;

/// The kind of relationship an edge encodes. Generic call/containment/type
/// relationships plus the Django-specific links (routing, template rendering
/// and inheritance, and model relations) that constellation tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Calls,
    Contains,
    Decorates,
    Extends,
    Imports,
    Instantiates,
    Overrides,
    References,
    Returns,
    TypeOf,
    ExtendsTemplate,
    Handles,
    IncludesTemplate,
    Receives,
    RelatesTo,
    Renders,
    Resolves,
    RoutesTo,
    Styles,
    AdminOf,
    OverridesTemplate,
    Tests,
    Reads,
    AccessesMember,
    ContextType,
    LoopBinding,
    ReverseAccessor,
    DerivedCollection,
    UsesTag,
}

impl EdgeKind {
    /// The snake_case label for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Contains => "contains",
            EdgeKind::Decorates => "decorates",
            EdgeKind::Extends => "extends",
            EdgeKind::Imports => "imports",
            EdgeKind::Instantiates => "instantiates",
            EdgeKind::Overrides => "overrides",
            EdgeKind::References => "references",
            EdgeKind::Returns => "returns",
            EdgeKind::TypeOf => "type_of",
            EdgeKind::ExtendsTemplate => "extends_template",
            EdgeKind::Handles => "handles",
            EdgeKind::IncludesTemplate => "includes_template",
            EdgeKind::Receives => "receives",
            EdgeKind::RelatesTo => "relates_to",
            EdgeKind::Renders => "renders",
            EdgeKind::Resolves => "resolves",
            EdgeKind::RoutesTo => "routes_to",
            EdgeKind::Styles => "styles",
            EdgeKind::AdminOf => "admin_of",
            EdgeKind::OverridesTemplate => "overrides_template",
            EdgeKind::Tests => "tests",
            EdgeKind::Reads => "reads",
            EdgeKind::AccessesMember => "accesses_member",
            EdgeKind::ContextType => "context_type",
            EdgeKind::LoopBinding => "loop_binding",
            EdgeKind::ReverseAccessor => "reverse_accessor",
            EdgeKind::DerivedCollection => "derived_collection",
            EdgeKind::UsesTag => "uses_tag",
        }
    }

    /// The kind parsed from its snake_case label, or `None` if unknown.
    pub fn from_str_label(label: &str) -> Option<EdgeKind> {
        let kind = match label {
            "calls" => EdgeKind::Calls,
            "contains" => EdgeKind::Contains,
            "decorates" => EdgeKind::Decorates,
            "extends" => EdgeKind::Extends,
            "imports" => EdgeKind::Imports,
            "instantiates" => EdgeKind::Instantiates,
            "overrides" => EdgeKind::Overrides,
            "references" => EdgeKind::References,
            "returns" => EdgeKind::Returns,
            "type_of" => EdgeKind::TypeOf,
            "extends_template" => EdgeKind::ExtendsTemplate,
            "handles" => EdgeKind::Handles,
            "includes_template" => EdgeKind::IncludesTemplate,
            "receives" => EdgeKind::Receives,
            "relates_to" => EdgeKind::RelatesTo,
            "renders" => EdgeKind::Renders,
            "resolves" => EdgeKind::Resolves,
            "routes_to" => EdgeKind::RoutesTo,
            "styles" => EdgeKind::Styles,
            "admin_of" => EdgeKind::AdminOf,
            "overrides_template" => EdgeKind::OverridesTemplate,
            "tests" => EdgeKind::Tests,
            "reads" => EdgeKind::Reads,
            "accesses_member" => EdgeKind::AccessesMember,
            "context_type" => EdgeKind::ContextType,
            "loop_binding" => EdgeKind::LoopBinding,
            "reverse_accessor" => EdgeKind::ReverseAccessor,
            "derived_collection" => EdgeKind::DerivedCollection,
            "uses_tag" => EdgeKind::UsesTag,
            _ => return None,
        };

        Some(kind)
    }
}

/// A directed relationship between two nodes. `source` and `target` carry
/// project prefixes in their ids, so an edge whose endpoints differ by project
/// is a cross-project link without needing a separate type.
#[derive(Clone, Debug)]
pub struct Edge {
    pub source: NodeId,
    pub target: NodeId,
    pub kind: EdgeKind,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub provenance: Option<String>,
}

impl Edge {
    /// An edge of the given kind between two nodes.
    pub fn new(source: NodeId, target: NodeId, kind: EdgeKind) -> Self {
        debug_assert!(!source.as_str().is_empty(), "edge source id must not be empty");
        debug_assert!(!target.as_str().is_empty(), "edge target id must not be empty");

        Self {
            source,
            target,
            kind,
            line: None,
            column: None,
            provenance: None,
        }
    }

    /// The edge, tagged with the 1-based source location the relationship was observed at.
    pub fn at(mut self, line: u32, column: u32) -> Self {
        assert!(line >= 1, "edge line is 1-based");

        self.line = Some(line);
        self.column = Some(column);

        self
    }

    /// The edge, tagged with the resolver or pass that produced it, useful for auditing
    /// cross-project links, which are inferred rather than read off the source.
    pub fn with_provenance(mut self, provenance: impl Into<String>) -> Self {
        let provenance = provenance.into();

        assert!(!provenance.is_empty(), "provenance label must not be empty");

        self.provenance = Some(provenance);

        self
    }

    /// Whether the endpoints live in different projects, the defining shape
    /// of a constellation link.
    pub fn is_cross_project(&self) -> bool {
        self.source.project_prefix() != self.target.project_prefix()
    }
}
