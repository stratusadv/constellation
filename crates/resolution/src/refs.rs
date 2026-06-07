use constellation_graph::{EdgeKind, Language, NodeId};

/// A reference recorded during extraction that has not yet been tied to a
/// target node. The resolution pipeline consumes these and, on success, emits
/// an edge of `reference_kind` from `from_node_id` to the resolved target.
#[derive(Clone, Debug)]
pub struct UnresolvedRef {
    pub from_node_id: NodeId,
    pub reference_name: String,
    pub reference_kind: EdgeKind,
    pub line: u32,
    pub column: u32,
    pub file_path: String,
    pub language: Language,
    pub candidates: Vec<String>,
}

impl UnresolvedRef {
    /// An unresolved reference built with a 1-based source location.
    pub fn new(
        from_node_id: NodeId,
        reference_name: impl Into<String>,
        reference_kind: EdgeKind,
        line: u32,
        column: u32,
        file_path: impl Into<String>,
        language: Language,
    ) -> Self {
        let reference_name = reference_name.into();
        let file_path = file_path.into();

        assert!(!reference_name.is_empty(), "reference_name must not be empty");
        assert!(!file_path.is_empty(), "reference file_path must not be empty");
        assert!(line >= 1, "reference line is 1-based");

        Self {
            from_node_id,
            reference_name,
            reference_kind,
            line,
            column,
            file_path,
            language,
            candidates: Vec::new(),
        }
    }
}

/// The strategy a reference was resolved by, ordered roughly from most to least certain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResolvedBy {
    ExactMatch,
    QualifiedName,
    Import,
    InstanceMethod,
    Framework,
    FilePath,
    Fuzzy,
}

impl ResolvedBy {
    /// The kebab-case label for this resolution strategy.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedBy::ExactMatch => "exact-match",
            ResolvedBy::QualifiedName => "qualified-name",
            ResolvedBy::Import => "import",
            ResolvedBy::InstanceMethod => "instance-method",
            ResolvedBy::Framework => "framework",
            ResolvedBy::FilePath => "file-path",
            ResolvedBy::Fuzzy => "fuzzy",
        }
    }
}

/// A reference successfully tied to a target node, with the confidence and
/// strategy behind the match. Carries only the originating fields the resulting
/// edge needs (`from_node_id`, `line`, `column`, `reference_kind`) rather than the
/// whole [`UnresolvedRef`], so resolving an edge does not clone the reference's
/// name, file path, and candidate list: built once per resolved edge.
#[derive(Clone, Debug)]
pub struct ResolvedRef {
    pub from_node_id: NodeId,
    pub line: u32,
    pub column: u32,
    pub reference_kind: EdgeKind,
    pub target_node_id: NodeId,
    pub confidence: f32,
    pub resolved_by: ResolvedBy,
}

impl ResolvedRef {
    /// A resolved reference built from an unresolved one, binding the target and confidence.
    pub fn new(
        reference: &UnresolvedRef,
        target_node_id: NodeId,
        confidence: f32,
        resolved_by: ResolvedBy,
    ) -> Self {
        assert!(confidence >= 0.0, "confidence must be non-negative");
        assert!(confidence <= 1.0, "confidence must not exceed one");

        Self {
            from_node_id: reference.from_node_id.clone(),
            line: reference.line,
            column: reference.column,
            reference_kind: reference.reference_kind,
            target_node_id,
            confidence,
            resolved_by,
        }
    }
}
