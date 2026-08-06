#![forbid(unsafe_code)]

//! constellation-graph: the shared vocabulary of the cross-project knowledge
//! graph. Projects, nodes, edges, and the identifiers that keep them distinct
//! across every indexed repository.

mod clock;
mod edge;
mod framework;
mod ids;
mod language;
mod node;
mod paths;
mod profile;
mod security;

pub use clock::{now_unix_millis, now_unix_secs};
pub use edge::{Edge, EdgeKind};
pub use framework::{
    FRAMEWORK_CLASS_SUFFIXES, RELATION_FIELDS, has_framework_class_suffix, is_covering_ref,
    is_dunder_name, is_framework_hook_name, is_framework_reached, relation_field_target,
};
pub use ids::{NodeId, ProjectId};
pub use language::Language;
pub use node::{Node, NodeIdentity, NodeKind, Span, Visibility};
pub use paths::{
    app_segment, base_name, is_generated_path, is_management_command_path, is_migration_path,
    is_minified_path, is_test_path,
};
pub use profile::{DJANGO_HOOK_NAMES, PROFILE_NAME_DEFAULT, PROFILE_NAMES, Profile};
pub use security::{SECURITY_KEYWORDS, is_security_sensitive, security_keyword};
