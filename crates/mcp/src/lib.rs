#![forbid(unsafe_code)]

//! MCP server: serves the unified, cross-project constellation graph to an
//! agent over stdio using the official `rmcp` SDK. Tools answer structural
//! questions (search, symbol detail, callers, callees, and transitive impact)
//! across every indexed project in one database.
//!
//! The crate is layered the way a request flows. [`server`] declares the tool
//! surface and owns the session state; each tool is a shell that locks the
//! store and calls a renderer in [`tools`]. Those renderers answer in text,
//! built from [`render`] and ordered by [`rank`]. Everything they agree on
//! (the response budget, what counts as a definition, how a date parses) lives
//! in [`limits`], [`symbols`], and [`dates`] so it is decided once.

mod args;
mod dates;
mod error;
mod git;
mod limits;
mod rank;
mod render;
mod server;
mod symbols;
mod text;
mod tools;

pub mod cursor;
pub mod hints;
pub mod recency;
pub mod risk;
pub mod winnow;

pub use args::{
    AffectedFlowsArgs, AsOfArgs, AtArgs, ChangedArgs, ExploreArgs, FilesArgs, FlowsArgs,
    HistoryArgs, ImpactArgs, LinksArgs, OrphansArgs, OverviewArgs, PathArgs, RoutesArgs,
    SearchArgs, SubclassesArgs, SymbolArgs, SymbolHistoryArgs, WinnowArgs, WinnowCriterionArg,
};
pub use error::McpError;
pub use git::{RevisionError, check_revision, parse_diff_hunks};
pub use limits::{EXPLORE_BYTES_BASE, EXPLORE_BYTES_MAX};
pub use rank::{explore_budget, path_penalty, rank_by_structure};
pub use server::{ConstellationServer, serve, serve_unavailable};
pub use symbols::qualified_name_ends_with;
pub use text::{ELLIPSIS, truncate_at_boundary};
pub use tools::changed::changed_text;
pub use tools::feature::feature_text;
pub use tools::flows::{flow_section, is_flow_edge, shortest_flow_path};
pub use tools::impact::{impact_text, subclasses_text, tests_text};
pub use tools::project::routes_text;
pub use tools::symbol::{callers_text, model_text};
pub use tools::winnow::winnow_text;

// The remaining renderers, exported for the integration suite rather than for
// callers: `cli` reaches the tool surface through [`ConstellationServer`], and
// a renderer is only interesting on its own to a test that wants the text
// without the transport. Each is `#[doc(hidden)]` at its definition, so none of
// them widens the documented API.
pub use tools::flows::{affected_flows_text, flows_text};
pub use tools::history::{as_of_text, history_text, symbol_history_text};
pub use tools::impact::orphans_text;
pub use tools::project::{files_text, links_text, overview_text};
pub use tools::search::search_text;
pub use tools::status::status_text;
pub use tools::symbol::{at_text, callees_text, node_text};
