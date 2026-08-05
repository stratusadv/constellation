//! Index size and shape, folded in so one run reports both speed and quality.
//!
//! The existing `index_time` and `index_mem` examples measure a build from
//! scratch. What a quality run needs alongside its retrieval numbers is the
//! shape of the index those numbers were produced against: without it, an MRR
//! is uncomparable between two runs whose indexes differed.

use std::time::Instant;

use crate::benchmarks::{Context, target_project};
use crate::report::BenchmarkRow;

use constellation_graph::NodeKind;

/// The name this benchmark reports under.
const NAME: &str = "build_performance";

/// The queries timed to characterize read latency.
const LATENCY_QUERIES: &[&str] = &["Order", "save", "view", "Service"];

/// The results fetched per timed query.
const LATENCY_FETCH: u32 = 20;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    let project = match target_project(context) {
        Ok(project) => project,
        Err(reason) => {
            return vec![BenchmarkRow::failed(NAME, "nodes", "project", reason)];
        }
    };

    let mut rows: Vec<BenchmarkRow> = Vec::new();

    match context.store.count_nodes(&project) {
        Ok(nodes) => rows.push(BenchmarkRow::ok(NAME, "nodes", "index", f64::from(nodes))),
        Err(error) => rows.push(BenchmarkRow::failed(NAME, "nodes", "index", error.to_string())),
    }

    match context.store.count_files(&project) {
        Ok(files) => rows.push(BenchmarkRow::ok(NAME, "files", "index", f64::from(files))),
        Err(error) => rows.push(BenchmarkRow::failed(NAME, "files", "index", error.to_string())),
    }

    match context.store.count_edges() {
        Ok(edges) => rows.push(BenchmarkRow::ok(NAME, "edges", "constellation", f64::from(edges))),
        Err(error) => rows.push(BenchmarkRow::failed(NAME, "edges", "constellation", error.to_string())),
    }

    match context.store.count_links() {
        Ok(links) => rows.push(BenchmarkRow::ok(NAME, "links", "constellation", f64::from(links))),
        Err(error) => rows.push(BenchmarkRow::failed(NAME, "links", "constellation", error.to_string())),
    }

    match context.store.kind_counts(&project) {
        Ok(counts) => rows.extend(django_surface_rows(&counts)),
        Err(error) => {
            rows.push(BenchmarkRow::failed(NAME, "django_surface", "index", error.to_string()));
        }
    }

    rows.push(search_latency(context));

    rows
}

/// The Django surface rows, one per model, view, route, and template count. The
/// other kinds are counted too, but they are the general symbol total the `nodes`
/// row already carries.
fn django_surface_rows(counts: &[(NodeKind, u32)]) -> Vec<BenchmarkRow> {
    const SURFACE: [NodeKind; 4] =
        [NodeKind::Model, NodeKind::View, NodeKind::Route, NodeKind::Template];

    counts
        .iter()
        .filter(|(kind, _)| SURFACE.contains(kind))
        .map(|(kind, count)| {
            let label = kind.as_str().to_string();

            BenchmarkRow::ok(NAME, "django_surface", label, f64::from(*count))
        })
        .collect()
}

/// The mean wall-clock of a bounded search, the read latency every other
/// benchmark's numbers were produced at.
fn search_latency(context: &Context<'_>) -> BenchmarkRow {
    let started = Instant::now();
    let mut ran: u32 = 0;

    for query in LATENCY_QUERIES {
        if context.store.search_nodes(query, LATENCY_FETCH).is_err() {
            return BenchmarkRow::failed(NAME, "search_latency_us", "mean", "a search query failed");
        }

        ran += 1;
    }

    assert!(ran as usize == LATENCY_QUERIES.len(), "every timed query ran");

    let mean = started.elapsed().as_micros() as f64 / f64::from(ran);

    BenchmarkRow::ok(NAME, "search_latency_us", "mean", mean)
        .with_detail(format!("mean over {ran} bounded searches, warm cache"))
}

#[cfg(test)]
mod tests {
    use super::{LATENCY_FETCH, LATENCY_QUERIES};

    #[test]
    fn the_latency_probe_is_a_small_fixed_set() {
        assert!(!LATENCY_QUERIES.is_empty(), "the mean needs at least one query");
        assert!(LATENCY_QUERIES.len() <= 8, "timing stays cheap enough to run every time");
        const { assert!(LATENCY_FETCH > 0, "a search fetches something") };
    }
}
