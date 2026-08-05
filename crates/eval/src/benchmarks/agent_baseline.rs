//! The comparison that matters: constellation against the incumbent.
//!
//! Benchmarking a tool against its own past tells you whether it improved.
//! Benchmarking it against what a developer or agent would otherwise do tells
//! you whether it is worth having. This scripts a grep-and-read-top-three
//! baseline over the same goldset and reports tokens-to-answer and wall-clock
//! for both.
//!
//! It is a scripted approximation of a grep loop, not a real agent, and the
//! report's Limits section says so. It bounds the comparison; it does not
//! settle it.

use std::path::Path;
use std::time::Instant;

use crate::benchmarks::{Context, LOCATE_FETCH, satisfies, target_project};
use crate::report::BenchmarkRow;
use crate::score::approximate_tokens;

use constellation_mcp::ConstellationServer;
use constellation_store::Store;

/// The name this benchmark reports under.
const NAME: &str = "agent_baseline";

/// The files a baseline grep reads in full, mirroring an agent that greps and
/// then opens the top few hits.
const BASELINE_FILES_READ: usize = 3;

/// The files one explore call is allowed to return.
const EXPLORE_FILES: u32 = 8;

/// The fail-fast bound on files one baseline grep scans.
const BASELINE_SCAN_MAX: usize = 20_000;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    let project = match target_project(context) {
        Ok(project) => project,
        Err(reason) => {
            return vec![BenchmarkRow::failed(NAME, "tokens", "project", reason)];
        }
    };

    let root = match context.store.project_root(&project) {
        Ok(Some(root)) => root,
        Ok(None) => {
            return vec![BenchmarkRow::failed(NAME, "tokens", "project", "the project has no root")];
        }
        Err(error) => {
            return vec![BenchmarkRow::failed(NAME, "tokens", "project", error.to_string())];
        }
    };

    if context.goldset.question.is_empty() {
        return vec![BenchmarkRow::failed(NAME, "tokens", "goldset", "the goldset holds no questions")];
    }

    let paths = match context.store.project_file_paths(&project) {
        Ok(paths) => paths,
        Err(error) => {
            return vec![BenchmarkRow::failed(NAME, "tokens", "project", error.to_string())];
        }
    };

    let mut rows: Vec<BenchmarkRow> = Vec::new();

    for question in &context.goldset.question {
        rows.extend(compare(context, Path::new(&root), &paths, &question.query, &question.expected));
    }

    rows
}

/// A question measured both ways.
fn compare(
    context: &Context<'_>,
    root: &Path,
    paths: &[String],
    query: &str,
    expected: &str,
) -> Vec<BenchmarkRow> {
    let mut rows: Vec<BenchmarkRow> = Vec::new();

    let baseline_started = Instant::now();
    let baseline = baseline_tokens(root, paths, query);
    let baseline_ms = baseline_started.elapsed().as_millis() as f64;

    rows.push(
        BenchmarkRow::ok(NAME, "tokens", query.to_string(), baseline.tokens as f64)
            .with_mode("grep plus read top 3"),
    );
    rows.push(
        BenchmarkRow::ok(NAME, "wall_clock_ms", query.to_string(), baseline_ms)
            .with_mode("grep plus read top 3"),
    );
    rows.push(
        BenchmarkRow::ok(NAME, "answered", query.to_string(), f64::from(u8::from(baseline.answered(expected))))
            .with_mode("grep plus read top 3"),
    );

    let graph_started = Instant::now();

    match explore_tokens(context, query, expected) {
        Ok((tokens, answered)) => {
            let graph_ms = graph_started.elapsed().as_millis() as f64;

            rows.push(
                BenchmarkRow::ok(NAME, "tokens", query.to_string(), tokens as f64)
                    .with_mode("one constellation_explore call"),
            );
            rows.push(
                BenchmarkRow::ok(NAME, "wall_clock_ms", query.to_string(), graph_ms)
                    .with_mode("one constellation_explore call"),
            );
            rows.push(
                BenchmarkRow::ok(NAME, "answered", query.to_string(), f64::from(u8::from(answered)))
                    .with_mode("one constellation_explore call"),
            );
        }
        Err(reason) => rows.push(
            BenchmarkRow::failed(NAME, "tokens", query.to_string(), reason)
                .with_mode("one constellation_explore call"),
        ),
    }

    rows
}

/// The cost of a scripted grep-and-read baseline, and what it read.
struct Baseline {
    read: Vec<String>,
    tokens: usize,
}

impl Baseline {
    /// Whether anything the baseline read names the expected symbol.
    fn answered(&self, expected: &str) -> bool {
        let needle = expected.rsplit(['.', ':']).next().unwrap_or(expected);

        self.read.iter().any(|source| source.contains(needle))
    }
}

/// The baseline: scan every indexed file for the query's first token, then read
/// the top few hits in full, exactly as an agent grepping and opening files
/// would.
fn baseline_tokens(root: &Path, paths: &[String], query: &str) -> Baseline {
    let needle = query.split_whitespace().next().unwrap_or(query).to_lowercase();

    let mut read: Vec<String> = Vec::new();
    let mut tokens: usize = 0;
    let mut scanned: usize = 0;

    for path in paths {
        scanned += 1;

        if scanned > BASELINE_SCAN_MAX || read.len() >= BASELINE_FILES_READ {
            break;
        }

        let Ok(source) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };

        if !source.to_lowercase().contains(&needle) {
            continue;
        }

        tokens += approximate_tokens(source.len());
        read.push(source);
    }

    assert!(read.len() <= BASELINE_FILES_READ, "the baseline reads a bounded number of files");

    Baseline { read, tokens }
}

/// The graph's cost and whether its one call answered the question.
fn explore_tokens(context: &Context<'_>, query: &str, expected: &str) -> Result<(usize, bool), String> {
    let store = Store::open(&context.database).map_err(|error| error.to_string())?;
    let server = ConstellationServer::new(store);

    let text = server.explore_text(query, EXPLORE_FILES).map_err(|error| error.to_string())?;

    // Existence against the graph, location by search: a symbol the ranker
    // cannot surface is a result worth recording, not a broken question.
    if !context.store.node_exists_named(expected).map_err(|error| error.to_string())? {
        return Err(format!("{expected:?} is not in the index; the goldset is stale"));
    }

    let target = context
        .store
        .search_nodes(expected, LOCATE_FETCH)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|node| satisfies(&node.qualified_name, expected));

    let answered = match target {
        Some(node) => text.contains(node.name.as_str()),
        None => return Err(format!("{expected:?} is indexed but unreachable by name search")),
    };

    Ok((approximate_tokens(text.len()), answered))
}

#[cfg(test)]
mod tests {
    use super::{BASELINE_FILES_READ, Baseline};

    #[test]
    fn the_baseline_answers_when_something_it_read_names_the_symbol() {
        let baseline = Baseline {
            read: vec!["def generate_order_number():\n    pass\n".to_string()],
            tokens: 10,
        };

        assert!(baseline.answered("services.py::OrderService.generate_order_number"));
        assert!(!baseline.answered("services.py::OrderService.recalculate_totals"));
    }

    #[test]
    fn a_baseline_that_read_nothing_answers_nothing() {
        let baseline = Baseline { read: Vec::new(), tokens: 0 };

        assert!(!baseline.answered("anything"));
    }

    #[test]
    fn the_baseline_reads_a_realistic_number_of_files() {
        const { assert!(BASELINE_FILES_READ >= 1, "an agent reads at least the top hit") };
        const { assert!(BASELINE_FILES_READ <= 5, "and does not open twenty files before answering") };
    }
}
