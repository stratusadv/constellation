#![forbid(unsafe_code)]

//! constellation-store: the SQLite persistence layer for the unified graph.
//! Holds the schema and the [`Store`] handle with its batched, idempotent
//! per-file write path. The schema is rebuilt from scratch when it changes
//! (detected by a fingerprint), rather than migrated in place.
//!
//! The handle is one type with one connection, and the query families are
//! `impl Store` blocks spread across [`query`]'s modules, so a caller sees a
//! single API while each family keeps its own SQL, row caps, and row mapping.

mod error;
mod limits;
mod mapping;
mod pool;
mod query;
mod rows;
mod sql;
mod store;
mod time;
mod write;

pub use error::StoreError;
pub use pool::{READERS_MAX, StorePool};
pub use rows::{
    AsOfSymbol, CommitFile, CommitRecord, FileIndex, FileRow, FileTouch, FlowMember, FlowRecord,
    FlowRow, FlowSort, HistoryHit, IncomingRef, LinkEdge, OutgoingRef, ProjectRow, SymbolChange,
    SymbolHistoryHit, SymbolRevision, UnresolvedRoute,
};
pub use store::Store;
