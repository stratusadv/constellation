//! Mean reciprocal rank over a curated goldset.
//!
//! The two retrieval paths are measured separately and reported as separate
//! rows, because they fail differently: `Store::search_nodes` is name and
//! docstring matching, while `explore` adds body content, inverse document
//! frequency, structural ranking, and recency on top. A change that helps one
//! can hurt the other, and a single blended number would hide that.

use crate::benchmarks::{Context, LOCATE_FETCH, satisfies};
use crate::report::BenchmarkRow;
use crate::score::{RECIPROCAL_RANK_CUTOFF, mean_reciprocal_rank, reciprocal_rank};

use constellation_mcp::ConstellationServer;

/// The name this benchmark reports under.
const NAME: &str = "search_quality";

/// The results fetched per query before ranking is judged.
const FETCH: u32 = 40;

/// The files one explore call is allowed to return while being judged.
const EXPLORE_FILES: u32 = 12;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    if context.goldset.question.is_empty() {
        return vec![BenchmarkRow::failed(NAME, "mrr", "goldset", "the goldset holds no questions")];
    }

    let mut rows: Vec<BenchmarkRow> = Vec::new();
    let mut search_ranks: Vec<Option<usize>> = Vec::new();
    let mut explore_ranks: Vec<Option<usize>> = Vec::new();

    for question in &context.goldset.question {
        match search_rank(context, &question.query, &question.expected) {
            Ok(rank) => {
                search_ranks.push(rank);

                rows.push(
                    BenchmarkRow::ok(NAME, "reciprocal_rank", question.query.clone(), reciprocal_rank(rank))
                        .with_mode("store.search_nodes")
                        .with_detail(rank_detail(rank)),
                );
            }
            Err(reason) => rows.push(
                BenchmarkRow::failed(NAME, "reciprocal_rank", question.query.clone(), reason)
                    .with_mode("store.search_nodes"),
            ),
        }

        match explore_rank(context, &question.query, &question.expected) {
            Ok(rank) => {
                explore_ranks.push(rank);

                rows.push(
                    BenchmarkRow::ok(NAME, "reciprocal_rank", question.query.clone(), reciprocal_rank(rank))
                        .with_mode("explore file ranking")
                        .with_detail(rank_detail(rank)),
                );
            }
            Err(reason) => rows.push(
                BenchmarkRow::failed(NAME, "reciprocal_rank", question.query.clone(), reason)
                    .with_mode("explore file ranking"),
            ),
        }
    }

    rows.push(aggregate("store.search_nodes", &search_ranks));
    rows.push(aggregate("explore file ranking", &explore_ranks));

    rows
}

/// The mean reciprocal rank over one retrieval path, or an error row when every
/// question failed to measure. An aggregate over zero successful measurements
/// is not zero quality; reporting it as zero would read as a catastrophic
/// regression rather than as an absent measurement.
fn aggregate(mode: &'static str, ranks: &[Option<usize>]) -> BenchmarkRow {
    if ranks.is_empty() {
        return BenchmarkRow::failed(NAME, "mrr", "all questions", "no question measured")
            .with_mode(mode);
    }

    BenchmarkRow::ok(NAME, "mrr", "all questions", mean_reciprocal_rank(ranks)).with_mode(mode)
}

/// A human note for one graded rank.
fn rank_detail(rank: Option<usize>) -> String {
    match rank {
        Some(rank) => format!("rank {rank}"),
        None => format!("not in the top {RECIPROCAL_RANK_CUTOFF}"),
    }
}

/// The one-based rank of the expected symbol in a plain search, or `None` when
/// it did not appear.
///
/// An expectation naming a symbol that is not in the index at all fails the run
/// loudly rather than scoring zero: a stale goldset is a bug, not a result. But
/// existence is checked against the graph directly, never against a search:
/// asking a ranked, truncated search whether something exists conflates "absent
/// from the index" with "absent from the first N results", and the second is
/// precisely the failure this benchmark is here to measure. Reported as an
/// error it would be excluded from the aggregate, quietly deleting the hardest
/// questions from the score.
fn search_rank(context: &Context<'_>, query: &str, expected: &str) -> Result<Option<usize>, String> {
    require_indexed(context, expected)?;

    let nodes = context.store.search_nodes(query, FETCH).map_err(|error| error.to_string())?;

    Ok(nodes
        .iter()
        .position(|node| satisfies(&node.qualified_name, expected))
        .map(|position| position + 1))
}

/// An error unless `expected` names a symbol the graph actually holds.
fn require_indexed(context: &Context<'_>, expected: &str) -> Result<(), String> {
    let exists =
        context.store.node_exists_named(expected).map_err(|error| error.to_string())?;

    if exists {
        return Ok(());
    }

    Err(format!("{expected:?} is not in the index; the goldset is stale"))
}

/// The one-based rank of the file holding the expected symbol among the files
/// `explore` returned, or `None` when it returned none of them.
fn explore_rank(context: &Context<'_>, query: &str, expected: &str) -> Result<Option<usize>, String> {
    require_indexed(context, expected)?;

    // A generous fetch purely to locate the symbol's file, not to grade
    // anything: the grading happens against what `explore` returns, below.
    let target = context
        .store
        .search_nodes(expected, LOCATE_FETCH)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|node| satisfies(&node.qualified_name, expected))
        .ok_or_else(|| format!("{expected:?} is indexed but unreachable by name search"))?;

    // A fresh server per question, so no session-recency memory carries between
    // graded questions and inflates the later ones.
    let store = constellation_store::Store::open(&context.database)
        .map_err(|error| error.to_string())?;

    let server = ConstellationServer::new(store);
    let text = server.explore_text(query, EXPLORE_FILES).map_err(|error| error.to_string())?;

    Ok(file_rank(&text, &target.file_path))
}

/// The one-based rank of a file among the `# [project] kind name (path:line)`
/// headers `explore` emits, counting each distinct file once in first-seen
/// order.
fn file_rank(text: &str, file_path: &str) -> Option<usize> {
    let mut seen: Vec<&str> = Vec::new();

    for line in text.lines() {
        if !line.starts_with("# [") {
            continue;
        }

        let Some(path) = header_path(line) else {
            continue;
        };

        if !seen.contains(&path) {
            seen.push(path);
        }

        if path == file_path {
            return Some(seen.len());
        }
    }

    None
}

/// The file path inside an explore header line's trailing `(path:line)`.
fn header_path(line: &str) -> Option<&str> {
    let inside = line.rsplit_once('(')?.1;
    let inside = inside.split_once(')')?.0;

    Some(inside.rsplit_once(':')?.0)
}

#[cfg(test)]
mod tests {
    use super::{file_rank, header_path, rank_detail};

    #[test]
    fn a_header_line_yields_its_file_path() {
        assert_eq!(
            header_path("# [blog] view detail_view (app/views.py:42)"),
            Some("app/views.py"),
        );

        assert_eq!(header_path("not a header"), None);
    }

    #[test]
    fn a_file_ranks_by_first_appearance_counting_each_file_once() {
        let text = concat!(
            "# [blog] view list_view (app/views.py:10)\n",
            "1\tdef list_view():\n",
            "# [blog] view detail_view (app/views.py:42)\n",
            "# [blog] model Order (app/models.py:5)\n",
        );

        assert_eq!(file_rank(text, "app/views.py"), Some(1), "the first distinct file is rank one");
        assert_eq!(file_rank(text, "app/models.py"), Some(2), "the second distinct file is rank two");
        assert_eq!(file_rank(text, "app/forms.py"), None, "an absent file has no rank");
    }

    #[test]
    fn a_miss_reports_the_cutoff_rather_than_a_bare_zero() {
        assert!(rank_detail(None).contains("not in the top"), "{}", rank_detail(None));
        assert_eq!(rank_detail(Some(3)), "rank 3");
    }
}
