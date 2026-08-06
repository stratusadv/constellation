//! One module per subcommand, each exposing a single `*_command` entry point
//! that takes the arguments after the subcommand word.
//!
//! Dispatch stays in `main.rs` and the work stays here, so adding a subcommand
//! is a module plus one match arm rather than a change to anything existing.

pub(crate) mod flows;
pub(crate) mod history;
pub(crate) mod index;
pub(crate) mod link;
pub(crate) mod serve;
pub(crate) mod supervise;
pub(crate) mod sync;
pub(crate) mod tools;
