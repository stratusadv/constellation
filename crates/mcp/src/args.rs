//! The argument struct behind every tool.
//!
//! The `///` doc on each field is functional, not decorative: `JsonSchema`
//! turns it into the parameter description the agent reads when deciding what
//! to pass. Keeping them together makes the whole agent-facing surface one
//! file to review.

use schemars::JsonSchema;
use serde::Deserialize;

/// The arguments for `search`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Symbol name to search for (e.g. "Article", "auth"). Matching is
    /// substring/fuzzy; exact then prefix matches rank first.
    pub query: String,
    /// Maximum results to return.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for tools that operate on a named symbol.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolArgs {
    /// Symbol name to look up. A bare name (`save_model_obj`) matches every
    /// definition with that name; pass `Owner.member`
    /// (`OrderService.save_model_obj`) to target one overload.
    pub symbol: String,
    /// Maximum related symbols to list.
    pub limit: Option<u32>,
}

/// The arguments for `subclasses`, which pages: a widely used mixin has hundreds of
/// subclasses, and a listing that silently stops at the limit reads as the whole set.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubclassesArgs {
    /// Base class or mixin whose subclasses to list (e.g. `HistoryModelMixin`).
    pub symbol: String,
    /// Maximum subclasses to list.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `impact`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactArgs {
    /// Exact symbol name whose blast radius to compute.
    pub symbol: String,
    /// How many caller levels to traverse.
    pub depth: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `explore`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreArgs {
    /// Symbol/file names or concrete domain words to explore (e.g. "Order
    /// OrderService order_number"). Matched against names, docstrings, and source
    /// bodies; use real code identifiers, not abstract prose.
    pub query: String,
    /// Maximum distinct files to include source from.
    pub max_files: Option<u32>,
    /// Outline mode: return signature-only outlines for every matched file (no
    /// bodies), a cheap wide survey. Each file lists its symbols most relevant to
    /// the query, not all of them; a trailing `(+N more symbol(s) here)` says when
    /// a file holds more than the outline shows. Default false: the top files come
    /// back in full source.
    pub outline: Option<bool>,
}

/// The arguments for `files`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilesArgs {
    /// Restrict to one project by its id or display name; omit to list every
    /// project in the constellation.
    pub project: Option<String>,
    /// List the files whose path contains this substring (case-insensitive),
    /// instead of the aggregated package summary (e.g. "models.py" for every
    /// models file, "billing/" for one app). Combine with `project` to scope it.
    pub pattern: Option<String>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `links`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinksArgs {
    /// Restrict to links whose source or target is this project (its id or
    /// display name); omit to list every cross-project link.
    pub project: Option<String>,
    /// Maximum link edges to list.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `overview`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewArgs {
    /// Restrict the digest to one project (its id or display name); omit to
    /// summarize every project in the constellation.
    pub project: Option<String>,
}

/// The arguments for `routes`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RoutesArgs {
    /// Restrict to one project by its id or display name; omit to list every
    /// project's routes.
    pub project: Option<String>,
    /// Show only routes whose URL pattern, view name, rendered template, or full
    /// route name contains this substring (case-insensitive), e.g. "detail" for
    /// the detail routes, "inventory/" for one app's. Omit for the whole map.
    pub pattern: Option<String>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `path`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PathArgs {
    /// The starting symbol (a name, or `Owner.member` to disambiguate).
    pub from: String,
    /// The destination symbol to reach.
    pub to: String,
}

/// The arguments for `history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryArgs {
    /// The file or app to trace over time, as a path substring (e.g.
    /// "orders/models.py" for one file, "orders/" for an app, "models.py" for
    /// every models file). Omit to list the most recent commits across the
    /// whole constellation.
    pub target: Option<String>,
    /// Restrict to one project by its id or display name.
    pub project: Option<String>,
    /// Maximum commits to list, newest first.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `symbol_history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolHistoryArgs {
    /// The symbol to trace: a bare name ("Order", "list_orders") or a qualified
    /// name ("orders.Order.total"). Matches a definition's name or qualified name,
    /// or a longer qualified name ending in it (so "Order" finds "orders.Order").
    pub symbol: String,
    /// Restrict to one project by its id or display name.
    pub project: Option<String>,
    /// Maximum change rows to list, newest first.
    pub limit: Option<u32>,
}

/// The arguments for `as_of`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AsOfArgs {
    /// The point in time to reconstruct: a commit hash (full or a prefix) or a
    /// date "YYYY-MM-DD". The symbols alive at that point are returned.
    pub at: String,
    /// Restrict to one project by its id or display name. Recommended with a
    /// commit hash, which is only meaningful within its own repository.
    pub project: Option<String>,
    /// Restrict to files whose path contains this substring (a file or an app).
    pub path: Option<String>,
    /// Maximum symbols to list.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `at`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AtArgs {
    /// File path as constellation prints it; a suffix is enough (`views.py` or
    /// `app/views.py`).
    pub file: String,
    /// 1-based line number (e.g. from a traceback frame or a grep hit).
    pub line: u32,
}

/// The arguments for `orphans`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OrphansArgs {
    /// The project to scan (its id or display name). Required: dead-code candidates
    /// are scoped to one project so the scan stays bounded and meaningful.
    pub project: Option<String>,
    /// Maximum candidates to list.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// The arguments for `changed`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChangedArgs {
    /// The git base to diff the working tree against. Defaults to `HEAD` (uncommitted
    /// and staged edits); pass a branch or ref (e.g. `main`) for a whole-branch diff.
    pub base: Option<String>,
    /// Maximum changed symbols to list per project.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail instead of narrowing. Expires when the index changes, which is
    /// reported rather than silently paging a shifted result set.
    pub cursor: Option<String>,
}

/// One criterion of a `winnow` query.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WinnowCriterionArg {
    /// The property to filter on. One of: kind, language, project, name, file,
    /// decorator, calls, called_by, extends, relates_to, renders, lines,
    /// callers, churn, changed_since, tested, in_flow, risk.
    pub axis: String,
    /// The comparison. One of: eq, in, contains, matches (glob), >, >=, <, <=
    /// (word forms gt, gte, lt, lte, == also accepted).
    pub op: String,
    /// The value to compare against. Comma-separate for `in` and for multiple
    /// alternatives (e.g. "model,view"). `matches` takes a glob with `*` and
    /// `?`, not a regular expression.
    pub value: String,
    /// For the `churn` axis only: the window in days to count commits over.
    /// Defaults to 90.
    pub window_days: Option<u32>,
}

/// The arguments for `winnow`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WinnowArgs {
    /// The criteria, ANDed together. The order you pass them is semantic only:
    /// the evaluator reorders by cost, so put them in whatever order reads best.
    pub criteria: Vec<WinnowCriterionArg>,
    /// The result order: risk (default), churn, callers, lines, criticality,
    /// or name.
    pub rank: Option<String>,
    /// Maximum results to return.
    pub limit: Option<u32>,
    /// The `cursor=` value from a previous truncated response, to page into the
    /// tail. Expires when the index changes.
    pub cursor: Option<String>,
}

/// The arguments for `flows`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowsArgs {
    /// Restrict to one project by its id or display name; omit to list every
    /// project's flows.
    pub project: Option<String>,
    /// Show only flows whose name or entry kind contains this substring
    /// (case-insensitive), e.g. "checkout", "route", "celery_task".
    pub pattern: Option<String>,
    /// Ordering: "criticality" (default, most critical first), "size" (widest
    /// reach first), or "name".
    pub sort: Option<String>,
    /// Maximum flows to list.
    pub limit: Option<u32>,
}

/// The arguments for `affected_flows`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AffectedFlowsArgs {
    /// The git base to diff the working tree against when `files` is omitted.
    /// Defaults to `HEAD` (uncommitted and staged edits); pass a branch or ref
    /// (e.g. `main`) for a whole-branch diff.
    pub base: Option<String>,
    /// An explicit list of file paths to check instead of running git, as
    /// constellation prints them (e.g. "orders/views.py").
    pub files: Option<Vec<String>>,
    /// Maximum flows to list.
    pub limit: Option<u32>,
}
