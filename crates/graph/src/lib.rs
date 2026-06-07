#![forbid(unsafe_code)]

//! constellation-graph: the shared vocabulary of the cross-project knowledge
//! graph. Projects, nodes, edges, and the identifiers that keep them distinct
//! across every indexed repository.

mod edge;
mod ids;
mod language;
mod node;

pub use edge::{Edge, EdgeKind};
pub use ids::{NodeId, ProjectId};
pub use language::Language;
pub use node::{Node, NodeIdentity, NodeKind, Span, Visibility};
