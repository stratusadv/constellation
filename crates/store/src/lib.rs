#![forbid(unsafe_code)]

//! constellation-store: the SQLite persistence layer for the unified graph.
//! Holds the schema and the [`Store`] handle with its batched, idempotent
//! per-file write path. The schema is rebuilt from scratch when it changes
//! (detected by a fingerprint), rather than migrated in place.

mod error;
mod store;
mod time;

pub use error::StoreError;
pub use store::{
    AsOfSymbol, CommitFile, CommitRecord, FileIndex, FileRow, FileTouch, HistoryHit, LinkEdge,
    ProjectRow, Store, SymbolChange, SymbolHistoryHit, SymbolRevision,
};
