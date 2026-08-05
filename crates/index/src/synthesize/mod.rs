//! The synthesis passes: edges derived from the resolved graph rather than read
//! from source.
//!
//! Each pass runs after resolution, reads the graph the parsers and the resolver
//! produced, and writes edges no single file could have shown. They are
//! separated by what they derive rather than run as one pass, because each is
//! independently testable and independently skippable.

pub(crate) mod events;
pub(crate) mod external;
pub(crate) mod overrides;
pub(crate) mod relations;
pub(crate) mod templates;
