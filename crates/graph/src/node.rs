use crate::ids::{NodeId, ProjectId};
use crate::language::Language;

/// The kind of symbol a node represents. Generic Python constructs plus the
/// Django-specific structure constellation promotes to first-class nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Class,
    Constant,
    Field,
    File,
    Function,
    Import,
    Method,
    Module,
    Parameter,
    Property,
    Variable,
    Model,
    Route,
    Template,
    View,
    Selector,
    External,
}

impl NodeKind {
    /// The lowercase label for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Class => "class",
            NodeKind::Constant => "constant",
            NodeKind::Field => "field",
            NodeKind::File => "file",
            NodeKind::Function => "function",
            NodeKind::Import => "import",
            NodeKind::Method => "method",
            NodeKind::Module => "module",
            NodeKind::Parameter => "parameter",
            NodeKind::Property => "property",
            NodeKind::Variable => "variable",
            NodeKind::Model => "model",
            NodeKind::Route => "route",
            NodeKind::Template => "template",
            NodeKind::View => "view",
            NodeKind::Selector => "selector",
            NodeKind::External => "external",
        }
    }

    /// The kind parsed from its lowercase label, or `None` if unknown.
    pub fn from_str_label(label: &str) -> Option<NodeKind> {
        let kind = match label {
            "class" => NodeKind::Class,
            "constant" => NodeKind::Constant,
            "field" => NodeKind::Field,
            "file" => NodeKind::File,
            "function" => NodeKind::Function,
            "import" => NodeKind::Import,
            "method" => NodeKind::Method,
            "module" => NodeKind::Module,
            "parameter" => NodeKind::Parameter,
            "property" => NodeKind::Property,
            "variable" => NodeKind::Variable,
            "model" => NodeKind::Model,
            "route" => NodeKind::Route,
            "template" => NodeKind::Template,
            "view" => NodeKind::View,
            "selector" => NodeKind::Selector,
            "external" => NodeKind::External,
            _ => return None,
        };

        Some(kind)
    }
}

/// The Python access intent inferred from naming convention: a leading underscore
/// is protected, a leading double underscore is private, everything else public.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    Private,
    Protected,
    Public,
}

impl Visibility {
    /// The lowercase label for this visibility.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Protected => "protected",
            Visibility::Public => "public",
        }
    }

    /// The visibility parsed from its lowercase label, or `None` if unknown.
    pub fn from_str_label(label: &str) -> Option<Visibility> {
        let visibility = match label {
            "private" => Visibility::Private,
            "protected" => Visibility::Protected,
            "public" => Visibility::Public,
            _ => return None,
        };

        Some(visibility)
    }
}

/// A 1-based source span. Lines and columns originate at one so the values
/// match what an editor shows; extraction converts from any 0-based parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start_line: u32,
    pub end_line: u32,
    pub start_column: u32,
    pub end_column: u32,
}

impl Span {
    /// A span over 1-based lines, with start not past the end.
    pub fn new(start_line: u32, end_line: u32, start_column: u32, end_column: u32) -> Self {
        assert!(start_line >= 1, "span lines are 1-based");
        assert!(start_line <= end_line, "span start_line must not exceed end_line");

        Self {
            start_line,
            end_line,
            start_column,
            end_column,
        }
    }
}

/// The four fields that, together, identify a symbol. Grouped so they cannot be
/// transposed at a call site, since three are strings that would otherwise be
/// easy to swap.
#[derive(Clone, Debug)]
pub struct NodeIdentity {
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub language: Language,
}

/// A symbol in the knowledge graph.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub project_id: ProjectId,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub language: Language,
    pub span: Span,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    pub visibility: Option<Visibility>,
    pub is_exported: bool,
    pub is_async: bool,
    pub is_static: bool,
    pub is_abstract: bool,
    pub decorators: Vec<String>,
    pub updated_at_ms: i64,
}

impl Node {
    /// A node built from its identity. Optional attributes (docstring,
    /// signature, visibility, flags, decorators) start cleared and are set on
    /// the returned value by the caller.
    pub fn new(
        id: NodeId,
        project_id: ProjectId,
        kind: NodeKind,
        identity: NodeIdentity,
        span: Span,
        updated_at_ms: i64,
    ) -> Self {
        assert!(!identity.name.is_empty(), "node name must not be empty");

        assert!(
            !identity.qualified_name.is_empty(),
            "node qualified_name must not be empty",
        );

        assert!(!identity.file_path.is_empty(), "node file_path must not be empty");
        assert!(updated_at_ms >= 0, "updated_at_ms must be non-negative");

        Self {
            id,
            project_id,
            kind,
            name: identity.name,
            qualified_name: identity.qualified_name,
            file_path: identity.file_path,
            language: identity.language,
            span,
            docstring: None,
            signature: None,
            visibility: None,
            is_exported: false,
            is_async: false,
            is_static: false,
            is_abstract: false,
            decorators: Vec::new(),
            updated_at_ms,
        }
    }
}
