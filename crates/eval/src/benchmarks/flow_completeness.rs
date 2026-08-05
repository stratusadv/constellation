//! How much of the Django surface the precomputed flows actually cover.
//!
//! Two questions, both answerable from the graph alone:
//!
//! - Does every route that *resolves to something* appear as a flow entry point?
//!   Anything less is a flow-detection gap, since a route is the one entry point
//!   constellation knows precisely rather than heuristically.
//! - What fraction of routes resolve to anything at all? A Django `include()`
//!   mount (`path('json/', include(...))`) is indexed as a route but reaches no
//!   view, so it can never seed a flow. Counting those against flow detection
//!   blames the wrong pass: they are an edge-extraction gap, or not a gap at
//!   all. The two are reported separately for that reason.
//! - What fraction of route flows reach at least one template? A route flow that
//!   never reaches a template has either a broken `Renders` edge or a genuinely
//!   headless endpoint, and the ratio is how that is noticed.

use rustc_hash::FxHashSet;

use crate::benchmarks::{Context, target_project};
use crate::report::BenchmarkRow;

use constellation_graph::{NodeKind, ProjectId};
use constellation_store::{FlowSort, Store};

/// The name this benchmark reports under.
const NAME: &str = "flow_completeness";

/// The flows one run examines.
const FLOWS_MAX: u32 = 20_000;

/// The reach-set members one flow is examined through.
const MEMBERS_MAX: u32 = 2_000;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    let project = match target_project(context) {
        Ok(project) => project,
        Err(reason) => {
            return vec![BenchmarkRow::failed(NAME, "coverage", "project", reason)];
        }
    };

    match measure(context.store, &project) {
        Ok(rows) => rows,
        Err(reason) => vec![BenchmarkRow::failed(NAME, "coverage", "flows", reason)],
    }
}

/// The two coverage ratios, or the reason neither could be measured.
fn measure(store: &Store, project: &ProjectId) -> Result<Vec<BenchmarkRow>, String> {
    let flows = store
        .flows(Some(project), FlowSort::Criticality, FLOWS_MAX)
        .map_err(|error| error.to_string())?;

    if flows.is_empty() {
        return Err("no flows computed; run `constellation flows`".to_string());
    }

    let routes = store
        .nodes_kind_in(project, NodeKind::Route)
        .map_err(|error| error.to_string())?;

    let entries: FxHashSet<&str> = flows.iter().map(|flow| flow.entry_node_id.as_str()).collect();

    // A route with no outgoing flow edge resolves to nothing: an `include()`
    // mount prefix, or a route whose view the resolver could not follow. Either
    // way no flow can start there, so grading flow detection against it measures
    // the resolver, not the flows.
    let mut resolving: Vec<&constellation_graph::Node> = Vec::with_capacity(routes.len());

    for route in &routes {
        let outgoing =
            store.flow_edge_count(&route.id).map_err(|error| error.to_string())?;

        if outgoing > 0 {
            resolving.push(route);
        }
    }

    let resolution_coverage = ratio(resolving.len(), routes.len());

    let covered = resolving.iter().filter(|route| entries.contains(route.id.as_str())).count();
    let route_coverage = ratio(covered, resolving.len());

    let route_flows: Vec<&constellation_store::FlowRow> =
        flows.iter().filter(|flow| flow.entry_kind == "route").collect();

    let mut reaching: usize = 0;

    for flow in &route_flows {
        let members = store.flow_members(flow.id, MEMBERS_MAX).map_err(|error| error.to_string())?;

        if members.iter().any(|(node, _)| node.kind == NodeKind::Template) {
            reaching += 1;
        }
    }

    let template_coverage = ratio(reaching, route_flows.len());

    Ok(vec![
        BenchmarkRow::ok(NAME, "route_entry_coverage", "every resolving route", route_coverage)
            .with_detail(format!(
                "{covered} of {} routes that resolve to something are flow entry points",
                resolving.len(),
            )),
        BenchmarkRow::ok(NAME, "route_resolution_coverage", "every indexed route", resolution_coverage)
            .with_detail(format!(
                "{} of {} routes resolve to anything; the rest are include() mounts or unresolved views",
                resolving.len(),
                routes.len(),
            )),
        BenchmarkRow::ok(NAME, "template_reach", "route flows", template_coverage)
            .with_detail(format!(
                "{reaching} of {} route flows reach at least one template",
                route_flows.len(),
            )),
        BenchmarkRow::ok(NAME, "flows", "total", flows.len() as f64),
        BenchmarkRow::ok(
            NAME,
            "truncated_reach_sets",
            "total",
            flows.iter().filter(|flow| flow.truncated).count() as f64,
        )
        .with_detail("reach sets cut short at the node cap; their counts are a floor"),
    ])
}

/// A `numerator / denominator` ratio, one when the denominator is zero (nothing
/// to cover is fully covered, not zero-covered).
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 1.0;
    }

    let value = numerator as f64 / denominator as f64;

    assert!((0.0..=1.0).contains(&value), "a coverage ratio lands in 0..=1");

    value
}

#[cfg(test)]
mod tests {
    use super::ratio;

    #[test]
    fn coverage_of_nothing_is_complete_rather_than_zero() {
        assert_eq!(ratio(0, 0), 1.0, "a project with no routes has no route gap");
    }

    #[test]
    fn coverage_is_the_plain_fraction() {
        assert_eq!(ratio(3, 4), 0.75);
        assert_eq!(ratio(4, 4), 1.0);
        assert_eq!(ratio(0, 4), 0.0);
    }
}
