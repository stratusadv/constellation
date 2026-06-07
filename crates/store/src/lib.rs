#![forbid(unsafe_code)]

//! constellation-store: the SQLite persistence layer for the unified graph.
//! Holds the schema, the migration runner, and the [`Store`] handle with its
//! batched, idempotent per-file write path.

mod error;
mod migrations;
mod store;
mod time;

pub use error::StoreError;
pub use store::{FileIndex, FileRow, LinkEdge, ProjectRow, Store};
