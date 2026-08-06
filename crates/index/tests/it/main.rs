//! The integration suite for `constellation-index`, as one test binary.
//!
//! Cargo builds and links every file directly under `tests/` as its own
//! binary, so eleven files were eleven links and eleven compiled copies of the
//! convergence fixture. As modules of one binary they link once and share the
//! fixture by construction.
//!
//! The watcher modules are all one invariant stressed differently, so the
//! oracle they check lives in [`common`] rather than being restated per file.

mod common;

mod dir_index_consistency;
mod flows;
mod fuzz_file_operations;
mod fuzz_watcher_stress;
mod index;
mod profile;
mod reindex_inbound_edges;
mod watcher_delete;
mod watcher_git_checkout;
mod watcher_lifecycle;
mod watcher_new_directory;
mod watcher_new_package;
mod watcher_rename_storm;
