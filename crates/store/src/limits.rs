//! Every fail-fast bound the store enforces, in one place.
//!
//! A query that can load an unbounded number of rows is a query that can be
//! handed an unbounded database. Each constant below caps one such load and
//! is asserted against at the site that reads it, so exceeding it fails loudly
//! rather than degrading into a slow answer.

/// The fail-fast bound on the rows written for a single file in one call.
pub(crate) const ROWS_PER_FILE_MAX: u32 = 5_000_000;

/// The fail-fast bound on the rows materialized by a single read.
pub(crate) const ROWS_LOADED_MAX: u32 = 50_000_000;

/// The fail-fast bound on per-file churn rows one [`Store::file_commit_counts`]
/// call materializes, far past the file count of any repository we index.
pub(crate) const FILE_CHURN_ROWS_MAX: u32 = 100_000;

/// The most values bound into one `IN (...)` clause. SQLite's own variable
/// ceiling is far higher; chunking here keeps a single prepared statement small
/// and its plan cacheable regardless of how many ids a caller passes.
pub(crate) const BULK_PARAMS_MAX: usize = 500;

/// The fail-fast bound on the flows one node may participate in before the
/// participation read stops summing. Far past any real entry-point fan-in.
pub(crate) const FLOW_PARTICIPATION_ROWS_MAX: u32 = 64;

/// The fail-fast bound on rows one unresolved-per-source aggregate materializes.
pub(crate) const UNRESOLVED_SOURCE_ROWS_MAX: u32 = 500_000;

/// The fail-fast bound on qualified names one changed-since read materializes.
pub(crate) const CHANGED_SINCE_ROWS_MAX: u32 = 500_000;

/// The fail-fast bound on files one commit-file read materializes.
pub(crate) const COMMIT_FILES_ROWS_MAX: u32 = 100_000;

/// The cap on how many rows a `LIMIT`-bounded read pre-allocates for, so a caller
/// passing an enormous limit cannot reserve a huge Vec for a tiny result set.
pub(crate) const PREALLOC_ROWS_MAX: usize = 4_096;

/// The most terms one full-text query carries. FTS5 plans a sub-query per term,
/// and a search naming more distinct identifiers than this is prose rather than
/// a query; the tail is dropped so the leading terms still answer.
pub(crate) const FTS_QUERY_TOKENS_MAX: usize = 32;
