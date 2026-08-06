//! The read side of the store, one module per query family.
//!
//! Each module holds an `impl Store` block, so a caller still reaches every
//! query through one handle and the split is invisible from outside the crate.
//! What the split buys is inside: a query family and the SQL, row caps, and
//! private helpers it needs sit together, and adding a family adds a file
//! rather than growing one.

mod edges;
mod files;
mod flows;
mod git;
mod nodes;
pub(crate) mod unresolved;
