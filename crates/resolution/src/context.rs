use std::path::Path;
use std::sync::Arc;

use constellation_graph::{Language, Node, NodeKind};

/// The read-only view of a single project's graph that resolvers query while
/// turning references into edges. Lookups return `Arc<Node>` handles: a resolver
/// never holds a borrow into the underlying store across a mutation, and a
/// lookup that matches many same-named nodes (a common method name) clones
/// reference counts rather than deep-copying every ~200-byte node.
pub trait ResolutionContext {
    /// The nodes whose simple name equals `name`, matched
    /// case-sensitively. Empty when nothing matches.
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>>;

    /// The nodes whose simple name lowercases to `lower_name`, the
    /// case-insensitive fallback used after an exact-name lookup misses.
    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>>;

    /// The nodes whose fully qualified name equals `qualified_name`.
    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>>;

    /// The nodes of the given kind across the project.
    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>>;

    /// The nodes defined in `file_path`.
    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>>;

    /// Whether the project contains a file at `file_path`.
    fn file_exists(&self, file_path: &str) -> bool;

    /// The source of a project file, or `None` when it is absent or unreadable.
    fn read_file(&self, file_path: &str) -> Option<String>;

    /// The path of every file in the project.
    fn all_files(&self) -> Vec<String>;

    /// The absolute filesystem root the project was indexed from.
    fn project_root(&self) -> &Path;

    /// The import bindings declared in `file_path` for `language`, each
    /// mapping a local name to the origin it was imported from.
    fn import_mappings(&self, file_path: &str, language: Language) -> Vec<ImportMapping>;
}

/// A name brought into a file by an import statement, resolved to its origin
/// where the origin is local to the project.
#[derive(Clone, Debug)]
pub struct ImportMapping {
    pub local_name: String,
    pub exported_name: String,
    pub source: String,
    pub is_default: bool,
    pub is_namespace: bool,
    pub resolved_path: Option<String>,
}

/// Whether an [`EventRecord`] dispatches an event or registers a listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventRole {
    Dispatch,
    Listen,
}

/// An event-channel observation from JS/Alpine source: a dispatch site
/// (`emit`/`fire`/`$dispatch`) or a listener registration (`on`/`once`/
/// `addEventListener`/`@event`/`x-on:`). Correlated by `event` name across a
/// project to synthesize the dispatcher -> handler edges static parsing misses.
#[derive(Clone, Debug)]
pub struct EventRecord {
    pub role: EventRole,
    pub event: String,
    /// The id of the dispatching node for a `Dispatch`, or the handler's name for a `Listen`.
    pub symbol: String,
    pub line: u32,
    pub column: u32,
}
