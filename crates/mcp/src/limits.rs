//! Every tuning constant the server answers by, in one place.
//!
//! These are the numbers that decide how much an agent gets back: how many
//! results, how deep a traversal runs, how many bytes a response may spend.
//! Scattered through the tools they read as arbitrary; together they read as
//! a budget, which is what they are.

/// The default number of results returned by search.
pub(crate) const SEARCH_LIMIT_DEFAULT: u32 = 20;

/// A hard cap on changed symbols one `changed` call scores. Scoring costs a
/// handful of graph queries per symbol, so an enormous branch diff is capped and
/// the remainder reported explicitly rather than silently dropped.
pub(crate) const CHANGED_SCORED_MAX: usize = 500;

/// The strongest reasons rendered beside a changed symbol's risk score. A bare
/// number is not actionable; four reasons stop being read.
pub(crate) const CHANGED_REASONS_MAX: usize = 3;

/// A hard cap on rows a paginated query fetches to satisfy an offset. The
/// cursor's own offset bound is far larger; this is what keeps a deep page from
/// materializing an unbounded result set just to skip most of it.
pub(crate) const PAGED_FETCH_MAX: u32 = 5_000;

/// The default number of routes listed per page.
pub(crate) const ROUTES_LIMIT_DEFAULT: u32 = 250;

/// A cap on the ranked positions one explore call records against the session.
/// The head of the ranking is what the agent actually read; the tail is context
/// it never saw, and recording it would dilute the signal.
pub(crate) const EXPLORE_SESSION_RECORD_MAX: usize = 32;

/// The seconds in one day, for turning a day-denominated window into the epoch
/// seconds the history tables store.
pub(crate) const SECONDS_PER_DAY: i64 = 86_400;

/// The default number of flows one `flows` call lists.
pub(crate) const FLOWS_LIMIT_DEFAULT: u32 = 25;

/// The hard cap a requested flow limit is clamped to, shared by `flows` and
/// `affected_flows` so neither can be asked for an unbounded listing.
pub(crate) const FLOWS_LIMIT_MAX: u32 = 500;

/// The over-fetch a pattern-filtered flow listing performs, so filtering to a
/// handful of matches still has candidates to filter from.
pub(crate) const FLOWS_FETCH_MAX: u32 = 4_000;

/// The default number of flows `affected_flows` lists.
pub(crate) const AFFECTED_FLOWS_LIMIT_DEFAULT: u32 = 20;

/// A hard cap on the changed nodes `affected_flows` looks flows up for, so an
/// enormous branch diff stays one bounded query.
pub(crate) const AFFECTED_FLOW_NODES_MAX: usize = 5_000;

/// The search over-fetches by this factor, then reorders so hand-written source
/// outranks test and generated files before truncating to the requested limit.
pub(crate) const SEARCH_OVERFETCH: u32 = 4;

/// A hard cap on rows fetched for one search, regardless of the requested limit.
pub(crate) const SEARCH_FETCH_MAX: u32 = 200;

/// A floor on rows fetched before re-ranking, so a small requested limit still
/// pulls enough candidates for the source/kind/match re-sort to surface the best
/// few. Without it, `limit=6` fetches only 24 rows and a strong prefix match can
/// sit outside that window, never reordered into view.
pub(crate) const SEARCH_FETCH_MIN: u32 = 64;

/// The definitions one orphan scan examines. The framework filter rejects most
/// of what the query returns, so a fetch sized to the page (the old `limit * 6`)
/// scanned the first few dozen files alphabetically and reported whatever
/// survived as the project's dead code. This is a scan bound, not a page size.
pub(crate) const ORPHAN_SCAN_MAX: u32 = 50_000;

/// A bound on the node names scanned by the fuzzy fallback, which only runs when
/// an exact and substring search both returned nothing.
pub(crate) const SEARCH_FUZZY_SCAN_MAX: usize = 200_000;

/// The largest name-length excess a fuzzy candidate may carry over the query
/// before it stops being "the same thing, misspelled" and becomes noise.
pub(crate) const SEARCH_FUZZY_SLACK_MAX: usize = 12;

/// The default number of callers/callees listed per symbol.
pub(crate) const RELATED_LIMIT_DEFAULT: u32 = 25;

/// The breadth-first hop bound when walking incoming `Extends` edges for
/// `constellation_subclasses`, far past any real inheritance depth.
pub(crate) const SUBCLASS_HOPS_MAX: u32 = 16;

/// The number of definition seeds `constellation_explore` checks for test coverage
/// before flagging the uncovered ones, kept small so the note stays cheap.
pub(crate) const EXPLORE_COVERAGE_CHECK_MAX: usize = 8;

/// The innermost-symbol results `constellation_at` shows for one file:line.
pub(crate) const AT_RESULTS_MAX: usize = 5;

/// A cap on a call-site snippet's length, in characters.
pub(crate) const CALL_SITE_SNIPPET_CHARS_MAX: usize = 160;

/// The default number of cross-project link edges `links` lists before truncating.
pub(crate) const LINKS_LIMIT_DEFAULT: u32 = 100;

/// The default number of commits `history` lists (newest first) before truncating.
pub(crate) const HISTORY_LIMIT_DEFAULT: u32 = 40;

/// The default number of symbols `as_of` lists before truncating: a whole app's
/// surface at a point in time, so larger than the per-commit limit.
pub(crate) const AS_OF_LIMIT_DEFAULT: u32 = 200;

/// A hard cap on link edges fetched for one `links` call.
pub(crate) const LINKS_FETCH_MAX: u32 = 2_000;

/// The top packages an `overview` lists per project (enough to convey the shape, not
/// the whole tree; use `files` for that).
pub(crate) const OVERVIEW_PACKAGES_MAX: usize = 6;

/// A hard cap on base-class hops `model` walks up the inheritance chain when
/// assembling a model's effective fields (a bound on the MRO traversal).
pub(crate) const MODEL_MRO_DEPTH_MAX: u32 = 16;

/// A hard cap on nodes one `model` traversal visits across the inheritance chain.
pub(crate) const MODEL_NODES_MAX: usize = 2_000;

/// A cap on how many same-named nodes `node` details in full before summarizing
/// the rest (usually import sites) as a count.
pub(crate) const NODE_DETAIL_MAX: usize = 8;

/// A cap on callers `node` lists inline for an unambiguous symbol, so the common
/// "who uses this" question is answered without a second `callers` call.
pub(crate) const NODE_CALLERS_INLINE_MAX: usize = 5;

/// A cap on how many same-named definitions `tests` reports coverage for.
///
/// `limit` bounds the tests listed per symbol, not the symbols: a house-style name
/// every service redeclares (`save_model_obj`) matches dozens of definitions, and
/// their sum is what the response costs. The overflow is reported, never dropped
/// silently.
pub(crate) const TESTS_SYMBOLS_MAX: usize = 12;

/// The default and hard-cap depth for impact traversal.
pub(crate) const IMPACT_DEPTH_DEFAULT: u32 = 2;

pub(crate) const IMPACT_DEPTH_MAX: u32 = 8;

/// A hard cap on nodes visited during one impact traversal.
pub(crate) const IMPACT_NODES_MAX: usize = 5_000;

/// A cap on impact-result lines rendered before truncating with a "(+N more)"
/// note: the blast radius is still counted past this; only the listing stops.
pub(crate) const IMPACT_LINES_MAX: usize = 200;

/// A per-level cap on impact lines. A hub symbol (a base mixin, a shared util) has
/// hundreds of direct callers; without a per-level bound L1 alone consumes the
/// whole budget and the deeper, often more informative levels never print. The
/// traversal still counts every caller past this; only the L1 listing is
/// sampled, with the remainder rolled into the "(+N more)" tail.
pub(crate) const IMPACT_LEVEL_LINES_MAX: usize = 40;

/// The default number of distinct files explore includes source from.
pub(crate) const EXPLORE_FILES_DEFAULT: u32 = 8;

/// A hard cap on symbols explore considers for a query.
pub(crate) const EXPLORE_SYMBOLS_MAX: u32 = 200;

/// The files whose body content explore samples for extra structural seeds.
pub(crate) const CONTENT_FILES_MAX: u32 = 5;

/// A cap on definition nodes drawn from content-matched files as explore seeds.
pub(crate) const CONTENT_SEED_NODES_MAX: usize = 30;

/// The output byte budget for one explore call, scaled to the graph size between a
/// floor (small project) and a hard cap (large project). The cap stays small
/// enough for the whole result to come back in-band: an MCP host spills an
/// oversized tool result to a file, where it is far less useful than inline.
#[doc(hidden)]
pub const EXPLORE_BYTES_BASE: usize = 40_000;

pub(crate) const EXPLORE_BYTES_PER_NODE: usize = 12;

/// A hard cap on the explore output byte budget.
#[doc(hidden)]
pub const EXPLORE_BYTES_MAX: usize = 64_000;

/// A hard cap on source lines one explore call emits, independent of the byte
/// budget: a second bound so a few very long symbols cannot dominate.
pub(crate) const EXPLORE_LINES_MAX: usize = 1_500;

/// A cap on lines rendered for a single symbol body: a long class/report renders
/// its head, not its whole 100+ lines, so one symbol cannot crowd out the rest.
pub(crate) const NODE_BODY_LINES_MAX: u32 = 60;

/// A hard cap on ranked positions explore groups into files. The ranking is
/// relevance-descending, so the tail past this bound is near-zero signal; the
/// bound keeps the grouping walk explicitly finite.
pub(crate) const EXPLORE_RANKED_MAX: usize = 4_096;

/// A cap on how many of a file's symbols explore renders. A flat file of 20
/// independent views or routes should not dump all of them because two matched
/// the query; the top few by relevance carry the answer. Members past this are
/// dropped from that file's render (the file still appears), keeping one big file
/// from crowding out the other relevant files.
pub(crate) const EXPLORE_SYMBOLS_PER_FILE_MAX: usize = 6;

/// The number of top-ranked files explore renders in full source. Files past this
/// rank are outlined (their symbols' headers and signatures only, no bodies) so
/// the most relevant code comes back verbatim while less-relevant files stay
/// visible as cheap pointers (an agent can `explore`/`node` them for full source).
pub(crate) const EXPLORE_FULL_FILES_MAX: usize = 4;

/// The unnamed neighbours a file may contribute alongside the symbols the
/// query named by identifier: enough to read the code around the answer, not so
/// many that the file's other functions crowd it out.
pub(crate) const EXPLORE_NEIGHBOURS_MAX: usize = 2;

/// The deepest indentation an outline renders, so a pathological nesting chain
/// cannot walk the signature column off the page.
pub(crate) const OUTLINE_DEPTH_MAX: usize = 4;

/// The distinct named symbols (query words that exactly name a symbol) explore treats
/// as call-path endpoints, the hop bound on each traced path, the BFS visit cap,
/// and the number of paths rendered; these bounds keep flow tracing finite.
pub(crate) const FLOW_ENDPOINTS_MAX: usize = 4;

pub(crate) const FLOW_HOPS_MAX: u32 = 6;

pub(crate) const FLOW_NODES_MAX: usize = 8_000;

pub(crate) const FLOW_PATHS_MAX: usize = 6;

/// The definitions of one named endpoint a flow trace will try before giving up on
/// that name. A query word like `_modal_view` names a dozen same-named definitions
/// across apps; trying only the first-ranked one reports "no call path" for a pair
/// that a sibling definition connects in a single hop, which is a false negative at
/// the top of explore's output.
pub(crate) const FLOW_CANDIDATES_MAX: usize = 12;

/// A fail-fast bound on routes listed per project, so the URL map of a large repo
/// stays a readable digest rather than dumping every route.
pub(crate) const ROUTES_PER_PROJECT_MAX: usize = 250;

/// A fail-fast bound on the unbound route handlers the URL map names in full. Past
/// it the footer reports the count alone, so a project mid-refactor cannot turn the
/// diagnostic into the longer half of the table.
pub(crate) const ROUTES_UNBOUND_NAMED_MAX: usize = 10;

/// A fail-fast bound on the symbols a feature slice gathers, so a hub model cannot
/// drag in the whole graph.
pub(crate) const FEATURE_NODES_MAX: usize = 60;

/// The depth to which the feature walk follows the structural chain (route→view→template→
/// includes is three hops).
pub(crate) const FEATURE_DEPTH_MAX: u32 = 3;

/// The threshold above which slicing all same-named definitions interleaves
/// unrelated features (every `detail_view` in every app) into one undifferentiated
/// dump, so the slice is replaced by a disambiguation listing (name one with its
/// `file::name`). At or below it, each seed is sliced (a model and its handful of
/// overloads stay sliceable).
pub(crate) const FEATURE_SEED_DISAMBIGUATION_MAX: usize = 3;

/// The maximum signature characters appended to a rendered node line before
/// truncation, so a multi-line or very long signature cannot blow up a listing.
pub(crate) const NODE_LINE_SIGNATURE_CHARS_MAX: usize = 120;
