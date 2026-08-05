//! Questions whose answer is two or three graph hops from the query's anchor.
//!
//! Route to view to template; model to reverse accessor to consuming view.
//! These are what the graph exists for, and the ones a text search cannot
//! answer at all, so they are scored separately from the direct lookups rather
//! than averaged in with them.

use crate::benchmarks::{Context, LOCATE_FETCH, satisfies};
use crate::report::BenchmarkRow;
use crate::score::mean_reciprocal_rank;

use constellation_mcp::ConstellationServer;

/// The name this benchmark reports under.
const NAME: &str = "multi_hop";

/// The minimum hop count that makes a goldset question a multi-hop one.
const HOPS_MIN: u32 = 2;

/// The files one explore call is allowed to return while being judged.
const EXPLORE_FILES: u32 = 12;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    let questions: Vec<&crate::config::Question> =
        context.goldset.question.iter().filter(|question| question.hops >= HOPS_MIN).collect();

    if questions.is_empty() {
        return vec![
            BenchmarkRow::failed(
                NAME,
                "mrr",
                "goldset",
                format!("the goldset holds no question with hops >= {HOPS_MIN}"),
            ),
        ];
    }

    let mut rows: Vec<BenchmarkRow> = Vec::new();
    let mut ranks: Vec<Option<usize>> = Vec::new();

    for question in questions {
        match answered(context, &question.query, &question.expected) {
            Ok(found) => {
                // A multi-hop question is answered or it is not: the graded
                // question is whether one call surfaced the answering symbol at
                // all, so the rank is one or nothing.
                let rank = found.then_some(1);

                ranks.push(rank);

                rows.push(
                    BenchmarkRow::ok(
                        NAME,
                        "answered",
                        question.query.clone(),
                        f64::from(u8::from(found)),
                    )
                    .with_mode(format!("{} hops", question.hops))
                    .with_detail(if found { "in one explore call" } else { "not surfaced" }),
                );
            }
            Err(reason) => {
                rows.push(BenchmarkRow::failed(NAME, "answered", question.query.clone(), reason));
            }
        }
    }

    // An aggregate over zero successful measurements is an absent measurement,
    // not a zero score, and is reported as such.
    rows.push(if ranks.is_empty() {
        BenchmarkRow::failed(NAME, "mrr", "all multi-hop questions", "no question measured")
    } else {
        BenchmarkRow::ok(NAME, "mrr", "all multi-hop questions", mean_reciprocal_rank(&ranks))
    });

    rows
}

/// Whether one `explore` response contains the expected symbol.
fn answered(context: &Context<'_>, query: &str, expected: &str) -> Result<bool, String> {
    // Existence against the graph, location by search. Conflating the two would
    // report a ranking failure as a stale goldset and drop the question from the
    // aggregate, which is the opposite of measuring it.
    if !context.store.node_exists_named(expected).map_err(|error| error.to_string())? {
        return Err(format!("{expected:?} is not in the index; the goldset is stale"));
    }

    let indexed = context
        .store
        .search_nodes(expected, LOCATE_FETCH)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|node| satisfies(&node.qualified_name, expected));

    let Some(target) = indexed else {
        return Err(format!("{expected:?} is indexed but unreachable by name search"));
    };

    let store = constellation_store::Store::open(&context.database)
        .map_err(|error| error.to_string())?;

    let server = ConstellationServer::new(store);
    let text = server.explore_text(query, EXPLORE_FILES).map_err(|error| error.to_string())?;

    Ok(mentions(&text, &target.name, &target.file_path))
}

/// Whether a response names a symbol, by its name appearing on a line that also
/// names its file. Matching both guards against a same-named symbol in another
/// file counting as an answer.
fn mentions(text: &str, name: &str, file_path: &str) -> bool {
    text.lines().any(|line| line.contains(name) && line.contains(file_path))
}

#[cfg(test)]
mod tests {
    use super::{HOPS_MIN, mentions};

    #[test]
    fn a_mention_needs_both_the_name_and_its_file() {
        let text = "# [blog] view detail_view (app/views.py:42)\n";

        assert!(mentions(text, "detail_view", "app/views.py"));

        assert!(
            !mentions(text, "detail_view", "other/views.py"),
            "a same-named symbol in another file is not the answer",
        );

        assert!(!mentions(text, "list_view", "app/views.py"));
    }

    #[test]
    fn a_direct_lookup_is_not_a_multi_hop_question() {
        const { assert!(HOPS_MIN > 1, "one hop is a direct lookup, measured by search_quality instead") };
    }
}
