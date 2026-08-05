//! One module per tool family: the work behind each `constellation_*` tool.
//!
//! Every function here takes a `&Store` and returns the `String` an agent
//! reads. None of them know about MCP, `rmcp`, or the async runtime, which is
//! what lets the integration suite call them directly and what keeps the tool
//! declarations in [`crate::server`] down to a lock and a call.

pub(crate) mod changed;
pub(crate) mod feature;
pub(crate) mod flows;
pub(crate) mod history;
pub(crate) mod impact;
pub(crate) mod project;
pub(crate) mod search;
pub(crate) mod status;
pub(crate) mod symbol;
pub(crate) mod winnow;
