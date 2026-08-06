//! Pinned tool output: what an agent is actually handed, one snapshot per tool.
//!
//! The rendered text is the contract. An agent never sees a `Node`, a row, or a
//! rank; it sees the lines this crate writes, and every decision behind them
//! (which symbols made the cut, what order they came in, which detail was worth
//! a line and which was dropped to stay inside a budget) reaches it only as
//! formatting. That makes the whole of `render`, `rank`, and `tools` one
//! user-visible surface, and it is a surface no reasonable number of
//! hand-written assertions covers: an assertion checks the line someone thought
//! to check, and a ranking change moves the ones nobody did.
//!
//! So these pin the text itself, against a graph that came out of the real
//! indexer (see [`crate::fixture`]) rather than one assembled by hand. A
//! refactor of ranking or rendering that was meant to change nothing shows up
//! as an empty diff or does not pass, and one that was meant to change
//! something shows exactly what it changed, in the form the agent will read it.
//!
//! Updating a snapshot is not a way to make a test pass. Every moved line is a
//! change in what an agent gets told about the code, and it is accepted only
//! once someone has read it and agrees.
//!
//! ```text
//! cargo test -p constellation-mcp        # writes .snap.new beside each miss
//! cargo insta review                     # read every diff, accept or reject
//! ```

use constellation_mcp::cursor::Page;
use constellation_mcp::winnow::RawCriterion;
use constellation_mcp::{
    affected_flows_text, as_of_text, at_text, callees_text, callers_text, changed_text,
    feature_text, files_text, flows_text, history_text, impact_text, links_text, model_text,
    node_text, orphans_text, overview_text, routes_text, search_text, status_text,
    subclasses_text, symbol_history_text, tests_text, winnow_text,
};

use crate::fixture::{Fixture, HistoryFixture};

/// The graph generation every snapshot renders under. Zero, because a cursor
/// encodes an offset and a generation that say nothing about the text pinned.
const GENERATION: u64 = 0;

/// The page every snapshot renders: the first one.
fn first() -> Page {
    Page::default()
}

/// The row cap every listing renders under, high enough that the fixture never
/// reaches it: a snapshot that stopped at its limit would pin the limit rather
/// than the ranking.
const LIMIT: u32 = 25;

/// The file cap `explore` renders under, high enough to reach both projects, so
/// the snapshot covers a cross-repository answer rather than one repository's
/// half of it.
const EXPLORE_FILES: u32 = 8;

#[test]
fn status() {
    let fixture = Fixture::build();
    let text = status_text(&fixture.store).expect("rendering status");

    insta::assert_snapshot!("status", fixture.render(&text));
}

#[test]
fn overview() {
    let fixture = Fixture::build();
    let text = overview_text(&fixture.store, None).expect("rendering overview");

    insta::assert_snapshot!("overview", fixture.render(&text));
}

/// The same tool with a project filter, which is a different path through the
/// renderer and the one an agent takes once it knows which repository it is in.
#[test]
fn overview_one_project() {
    let fixture = Fixture::build();

    let text = overview_text(&fixture.store, Some(fixture.platform.as_str()))
        .expect("rendering a scoped overview");

    insta::assert_snapshot!("overview_one_project", fixture.render(&text));
}

#[test]
fn files() {
    let fixture = Fixture::build();

    let text =
        files_text(&fixture.store, None, None, &first(), GENERATION).expect("rendering files");

    insta::assert_snapshot!("files", fixture.render(&text));
}

#[test]
fn search() {
    let fixture = Fixture::build();

    let text = search_text(&fixture.store, "Order", LIMIT, &first(), GENERATION)
        .expect("rendering search");

    insta::assert_snapshot!("search", fixture.render(&text));
}

#[test]
fn node() {
    let fixture = Fixture::build();
    let text = node_text(&fixture.store, "Order.recalculate_totals").expect("rendering node");

    insta::assert_snapshot!("node", fixture.render(&text));
}

#[test]
fn at_line() {
    let fixture = Fixture::build();
    let text = at_text(&fixture.store, "orders/models.py", 11).expect("rendering at");

    insta::assert_snapshot!("at_line", fixture.render(&text));
}

#[test]
fn callers() {
    let fixture = Fixture::build();
    let text =
        callers_text(&fixture.store, "recalculate_totals", LIMIT).expect("rendering callers");

    insta::assert_snapshot!("callers", fixture.render(&text));
}

#[test]
fn callees() {
    let fixture = Fixture::build();
    let text = callees_text(&fixture.store, "checkout_view", LIMIT).expect("rendering callees");

    insta::assert_snapshot!("callees", fixture.render(&text));
}

#[test]
fn model() {
    let fixture = Fixture::build();
    let text = model_text(&fixture.store, "Order").expect("rendering model");

    insta::assert_snapshot!("model", fixture.render(&text));
}

#[test]
fn routes() {
    let fixture = Fixture::build();

    let text = routes_text(&fixture.store, None, None, LIMIT, &first(), GENERATION)
        .expect("rendering routes");

    insta::assert_snapshot!("routes", fixture.render(&text));
}

#[test]
fn links() {
    let fixture = Fixture::build();

    let text =
        links_text(&fixture.store, None, LIMIT, &first(), GENERATION).expect("rendering links");

    insta::assert_snapshot!("links", fixture.render(&text));
}

#[test]
fn impact() {
    let fixture = Fixture::build();

    let text = impact_text(&fixture.store, "recalculate_totals", 2, &first(), GENERATION)
        .expect("rendering impact");

    insta::assert_snapshot!("impact", fixture.render(&text));
}

#[test]
fn subclasses() {
    let fixture = Fixture::build();

    let text = subclasses_text(&fixture.store, "TimeStamped", LIMIT, &first(), GENERATION)
        .expect("rendering subclasses");

    insta::assert_snapshot!("subclasses", fixture.render(&text));
}

#[test]
fn tests() {
    let fixture = Fixture::build();
    let text = tests_text(&fixture.store, "recalculate_totals", LIMIT).expect("rendering tests");

    insta::assert_snapshot!("tests", fixture.render(&text));
}

#[test]
fn orphans() {
    let fixture = Fixture::build();

    let scope = Some(fixture.shop.as_str());

    let profile = constellation_graph::Profile::default();

    let text = orphans_text(&fixture.store, &profile, scope, LIMIT, &first(), GENERATION)
        .expect("rendering orphans");

    insta::assert_snapshot!("orphans", fixture.render(&text));
}

#[test]
fn feature() {
    let fixture = Fixture::build();
    let text = feature_text(&fixture.store, "Order").expect("rendering feature");

    insta::assert_snapshot!("feature", fixture.render(&text));
}

#[test]
fn flows() {
    let fixture = Fixture::build();
    // Scoped, because the fixture only traces the app project: an unscoped
    // listing would pin the absence of platform flows rather than any flow.
    let text = flows_text(&fixture.store, Some(fixture.shop.as_str()), None, None, LIMIT)
        .expect("rendering flows");

    insta::assert_snapshot!("flows", fixture.render(&text));
}

#[test]
fn winnow() {
    let fixture = Fixture::build();

    let criteria =
        [RawCriterion { axis: "kind", op: "is", value: "method", window_days: None }];

    let text = winnow_text(&fixture.store, &criteria, None, LIMIT, &first(), GENERATION)
        .expect("rendering winnow");

    insta::assert_snapshot!("winnow", fixture.render(&text));
}

/// The primary tool, and the one with the largest rendered surface: a file
/// grouping, a per-file symbol selection, line-numbered bodies, a budget, and
/// the trailing hint. Rendered through a server rather than a free function
/// because explore's ranking runs off a graph cache the server owns.
#[test]
fn explore() {
    let fixture = Fixture::build();
    let server = fixture.server();

    let text = server.explore_text("recalculate_totals", EXPLORE_FILES).expect("rendering explore");

    insta::assert_snapshot!("explore", fixture.render(&text));
}

/// The case of two symbols named at once, the other half of explore: the chain
/// from one to the other, traced across files.
///
/// A route and a base template, because the chain between them runs through
/// three different edge kinds (a route resolving to its view, the view
/// rendering a template, that template extending another). A path pinned over
/// one repeated edge would say nothing about how the other kinds render.
#[test]
fn path() {
    let fixture = Fixture::build();
    let server = fixture.server();

    let text = server.path_text("checkout", "orders/base.html").expect("rendering path");

    insta::assert_snapshot!("path", fixture.render(&text));
}

/// The same rendering over a chain that leaves the repository, which is the
/// case constellation exists for and the one no single-repository index can
/// answer at all.
#[test]
fn path_cross_project() {
    let fixture = Fixture::build();
    let server = fixture.server();

    let text = server.path_text("audit", "AuditLog").expect("rendering path");

    insta::assert_snapshot!("path_cross_project", fixture.render(&text));
}

/// The answer when there is no chain, which is a different rendering and the
/// one an agent hits when it guesses two unrelated symbols. Pinned so a change
/// to the traversal that quietly stopped finding paths would surface as this
/// message replacing a chain, rather than as a chain nobody was watching.
#[test]
fn path_unconnected() {
    let fixture = Fixture::build();
    let server = fixture.server();

    let text = server.path_text("checkout_view", "AuditLog").expect("rendering path");

    insta::assert_snapshot!("path_unconnected", fixture.render(&text));
}

/// The flows a change touches, seeded from an explicit file list rather than a
/// diff: the file list is the deterministic half of the tool, and the diff half
/// is pinned by [`changed`], which reads the same working tree.
#[test]
fn affected_flows() {
    let fixture = Fixture::build();

    let files = ["orders/models.py".to_string()];

    let text = affected_flows_text(&fixture.store, None, Some(&files), LIMIT)
        .expect("rendering affected flows");

    insta::assert_snapshot!("affected_flows", fixture.render(&text));
}

#[test]
fn history() {
    let Some(fixture) = HistoryFixture::build() else {
        eprintln!("history: git is not on PATH; skipping");

        return;
    };

    let text = history_text(&fixture.store, None, None, LIMIT, &first(), GENERATION)
        .expect("rendering history");

    insta::assert_snapshot!("history", fixture.render(&text));
}

#[test]
fn symbol_history() {
    let Some(fixture) = HistoryFixture::build() else {
        eprintln!("symbol_history: git is not on PATH; skipping");

        return;
    };

    let text = symbol_history_text(&fixture.store, "recalculate_totals", None, LIMIT)
        .expect("rendering symbol history");

    insta::assert_snapshot!("symbol_history", fixture.render(&text));
}

/// The case addressed by commit hash rather than by date, because the hash is the harder
/// path: a date parses on its own, while a hash has to resolve against the
/// ingested history before anything can be listed.
#[test]
fn as_of() {
    let Some(fixture) = HistoryFixture::build() else {
        eprintln!("as_of: git is not on PATH; skipping");

        return;
    };

    let text = as_of_text(
        &fixture.store,
        fixture.first_commit(),
        Some(fixture.shop.as_str()),
        None,
        LIMIT,
        &first(),
        GENERATION,
    )
    .expect("rendering as of");

    insta::assert_snapshot!("as_of", fixture.render(&text));
}

/// The risk ranking over a real working-tree diff, with the history the score
/// wants actually present, so the snapshot pins a scored row rather than the
/// note explaining which factors were missing.
#[test]
fn changed() {
    let Some(fixture) = HistoryFixture::build() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    let text = changed_text(&fixture.store, None, LIMIT, &first(), GENERATION)
        .expect("rendering changed");

    insta::assert_snapshot!("changed", fixture.render(&text));
}

