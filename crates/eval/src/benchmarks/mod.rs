//! The benchmarks a run may select from.
//!
//! Every benchmark returns `Vec<BenchmarkRow>` and never panics on a failed
//! tool call: a call that could not run becomes a row with
//! [`crate::report::Status::Error`] and is excluded from aggregates, so an
//! unmeasurable case never masquerades as a bad score.

pub mod agent_baseline;
pub mod build_performance;
pub mod flow_completeness;
pub mod impact_accuracy;
pub mod multi_hop;
pub mod search_quality;
pub mod token_efficiency;

use crate::config::{Config, Goldset};
use crate::report::BenchmarkRow;

use constellation_store::Store;

use std::path::PathBuf;

/// The context one benchmark needs: the store, the parsed config, and the
/// goldset.
pub struct Context<'a> {
    pub config: &'a Config,
    /// The database path *resolved* against the config's own directory. The
    /// benchmarks that open their own connection must use this, never
    /// `config.database`, which is relative to the config file rather than to
    /// the working directory the harness runs from.
    pub database: PathBuf,
    pub goldset: &'a Goldset,
    pub store: &'a Store,
}

/// The result count used only to locate an already-confirmed symbol, never to
/// grade anything. Generous on purpose: existence is settled against the graph
/// by [`constellation_store::Store::node_exists_named`] before this runs, so a
/// small limit here would only reintroduce the ranking-versus-existence
/// confusion it exists to avoid.
pub const LOCATE_FETCH: u32 = 500;

/// The benchmarks, by the name a `--benchmark` argument selects.
pub const BENCHMARK_NAMES: &[&str] = &[
    "agent_baseline",
    "build_performance",
    "flow_completeness",
    "impact_accuracy",
    "multi_hop",
    "search_quality",
    "token_efficiency",
];

/// A named benchmark run, or `None` when the name matches none.
pub fn run(name: &str, context: &Context<'_>) -> Option<Vec<BenchmarkRow>> {
    let rows = match name {
        "agent_baseline" => agent_baseline::run(context),
        "build_performance" => build_performance::run(context),
        "flow_completeness" => flow_completeness::run(context),
        "impact_accuracy" => impact_accuracy::run(context),
        "multi_hop" => multi_hop::run(context),
        "search_quality" => search_quality::run(context),
        "token_efficiency" => token_efficiency::run(context),
        _ => return None,
    };

    Some(rows)
}

/// The project a project-scoped benchmark runs against: the configured one, or
/// the only indexed project when the config names none.
///
/// The failure is returned as the reason to report, rather than a bare `None`.
/// A store that could not be read and an ambiguous constellation are different
/// facts, and reporting both as "set project = in the config" sent the reader
/// after a config that was never the problem.
pub fn target_project(
    context: &Context<'_>,
) -> Result<constellation_graph::ProjectId, String> {
    let projects = context.store.all_projects().map_err(|error| error.to_string())?;

    if let Some(named) = &context.config.project {
        return projects
            .into_iter()
            .find(|project| project.id.as_str() == named || &project.name == named)
            .map(|project| project.id)
            .ok_or_else(|| format!("no indexed project matches project = {named:?}"));
    }

    if projects.len() == 1 {
        return projects
            .into_iter()
            .next()
            .map(|project| project.id)
            .ok_or_else(|| "the index holds no project".to_string());
    }

    if projects.is_empty() {
        return Err("the index holds no project; run `constellation init`".to_string());
    }

    Err("several projects are indexed; set project = in the config".to_string())
}

/// Whether a qualified name satisfies an expectation: an exact match, or a
/// suffix at a name boundary, so a goldset may name `Order.total` without
/// writing the whole `app/models.py::Order.total`.
pub fn satisfies(qualified_name: &str, expected: &str) -> bool {
    if qualified_name == expected {
        return true;
    }

    match qualified_name.strip_suffix(expected) {
        Some(head) => head.is_empty() || head.ends_with('.') || head.ends_with("::"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{BENCHMARK_NAMES, satisfies};

    #[test]
    fn an_expectation_matches_at_a_name_boundary_only() {
        assert!(satisfies("app/models.py::Order.total", "Order.total"));
        assert!(satisfies("app/models.py::Order", "Order"));
        assert!(satisfies("Order", "Order"));

        assert!(
            !satisfies("app/models.py::Reorder", "Order"),
            "a suffix landing mid-identifier is not a match",
        );
    }

    #[test]
    fn every_named_benchmark_is_unique() {
        let mut sorted = BENCHMARK_NAMES.to_vec();

        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(sorted.len(), BENCHMARK_NAMES.len(), "no benchmark name collides");
    }
}
