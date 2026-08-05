//! The integration suite for `constellation-mcp`, as one test binary.
//!
//! One module per tool family. Each builds a store, writes a fixture graph
//! into it, and asserts on the rendered text an agent would receive, because
//! the rendering is the contract: an agent reads the text, not the rows.
//!
//! [`snapshot`] takes that contract literally and pins the text itself, one
//! snapshot per tool, against the indexed two-project constellation in
//! [`fixture`]. The two approaches answer different questions: a hand-built
//! graph and a named assertion say why a line is what it is, while a snapshot
//! says whether anything moved. A refactor needs both.

mod changed;
mod fixture;
mod mcp;
mod regressions;
mod snapshot;
mod tool_names;
mod winnow;
