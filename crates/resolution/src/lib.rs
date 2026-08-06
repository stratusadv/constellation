#![forbid(unsafe_code)]

//! Reference resolution: turn the unresolved references extraction emits into
//! edges by matching each reference to the node it names. Framework-aware
//! resolvers (Django first) plug in to handle patterns static parsing cannot
//! see on its own: URL routing, template rendering, ORM dynamic dispatch.

mod context;
mod framework;
mod frameworks;
mod refs;
mod resolver;

pub use context::{EventRecord, EventRole, ImportMapping, ResolutionContext};
pub use framework::{FrameworkExtractionResult, FrameworkResolver};
pub use frameworks::DjangoResolver;
pub use refs::{ResolvedBy, ResolvedRef, UnresolvedRef};
pub use resolver::{
    COLLECTION_CONTEXT, MANAGER_SUFFIXES, QUERYSET_BUILTINS, QUERYSET_DISPATCH, RECEIVER_ROOT,
    RETURNS_OF, SENTINEL_PREFIX, SERVICE_DISPATCH, SERVICE_SUFFIXES, SUPER_DISPATCH,
    TYPED_RECEIVER, edge_from_resolved, resolve_reference,
};

#[doc(hidden)]
pub use frameworks::is_pascal_word;

#[doc(hidden)]
pub use resolver::shared_directory_depth;
