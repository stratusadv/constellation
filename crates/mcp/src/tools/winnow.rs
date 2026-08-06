//! `constellation_winnow`: a multi-axis filter over the graph.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, Node, ProjectId, app_segment, is_covering_ref, is_test_path,
};
use constellation_store::{Store, StoreError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::git::now_unix_secs;
use crate::limits::SECONDS_PER_DAY;
use crate::render::{file_key, node_line};
use crate::{cursor, risk, winnow};

/// A candidate under winnow evaluation, carrying the derived facts a
/// criterion may test. The facts are loaded in bulk once the cheap column
/// criteria have narrowed the set, so a query never issues one lookup per
/// candidate.
struct WinnowCandidate {
    caller_count: u32,
    churn: u32,
    covered: bool,
    criticality: f64,
    flow_names: Vec<String>,
    node: Node,
    outgoing: Vec<(EdgeKind, String)>,
    caller_names: Vec<String>,
    changed_since: bool,
    risk: f64,
}

impl WinnowCandidate {
    /// A candidate with no derived facts loaded yet.
    fn new(node: Node) -> Self {
        Self {
            caller_count: 0,
            caller_names: Vec::new(),
            changed_since: false,
            churn: 0,
            covered: false,
            criticality: 0.0,
            flow_names: Vec::new(),
            node,
            outgoing: Vec::new(),
            risk: 0.0,
        }
    }

    /// The count of lines the symbol spans.
    fn lines(&self) -> i64 {
        i64::from(self.node.span.end_line.saturating_sub(self.node.span.start_line)) + 1
    }

    /// The names this symbol references through the given edge kind.
    fn targets(&self, kind: EdgeKind) -> impl Iterator<Item = &str> {
        self.outgoing
            .iter()
            .filter(move |(edge, _)| *edge == kind)
            .map(|(_, name)| name.as_str())
    }
}

/// The composed, multi-axis filter behind `constellation_winnow`.
#[doc(hidden)]
pub fn winnow_text(
    store: &Store,
    raw: &[winnow::RawCriterion<'_>],
    rank: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let criteria = match winnow::parse(raw) {
        Ok(criteria) => criteria,
        Err(error) => return Ok(format!("winnow: {error}")),
    };

    let rank = match rank.map(winnow::Rank::from_str_label) {
        Some(Some(rank)) => rank,
        Some(None) => {
            return Ok(format!(
                "winnow: unknown rank {:?}. Valid ranks: {}",
                rank.unwrap_or_default(),
                winnow::Rank::valid_labels(),
            ));
        }
        None => winnow::Rank::Risk,
    };

    let project = winnow_project(store, &criteria)?;

    let all = store.all_nodes(project.as_ref())?;
    let scanned = all.len();
    let truncated = scanned > winnow::WINNOW_CANDIDATES_MAX;

    let mut candidates: Vec<WinnowCandidate> = all
        .into_iter()
        .take(winnow::WINNOW_CANDIDATES_MAX)
        .map(WinnowCandidate::new)
        .collect();

    assert!(
        candidates.len() <= winnow::WINNOW_CANDIDATES_MAX,
        "the candidate scan respects its cap",
    );

    // The surviving count after each criterion, in the order they were actually
    // applied. Naming only the criterion that happened to empty the set blames
    // whichever one the cost reorder put last, which is rarely the restrictive
    // one: a criterion matching 14 symbols on its own reads as the culprit when
    // an earlier one had already cut the field to a disjoint 13. The cascade
    // shows where the intersection collapsed instead of guessing.
    let mut cascade: Vec<(String, usize)> = Vec::new();

    // Phase one: the indexed node columns, evaluated in memory with no query.
    for criterion in criteria.iter().filter(|criterion| is_column_axis(criterion.axis)) {
        candidates.retain(|candidate| matches_column(candidate, criterion));

        cascade.push((criterion.describe(), candidates.len()));
    }

    // Phase two: one bulk read per derived fact, over whatever survived.
    load_winnow_facts(store, &mut candidates, &criteria)?;

    // Phase three: the derived scalars and the edge joins.
    for criterion in criteria.iter().filter(|criterion| !is_column_axis(criterion.axis)) {
        candidates.retain(|candidate| matches_derived(candidate, criterion));

        cascade.push((criterion.describe(), candidates.len()));
    }

    if candidates.is_empty() {
        return Ok(empty_winnow_text(&cascade));
    }

    sort_winnow(&mut candidates, rank);

    render_winnow(&candidates, &criteria, rank, limit, page, generation, truncated, scanned)
}

/// The empty-result text: how many candidates survived each criterion, in the
/// order they were applied.
///
/// The first row whose count is zero is where the intersection collapsed, and the
/// rows above it say what the field already looked like by then, which is the
/// difference between "this criterion matches nothing" and "this criterion
/// matches plenty, just nothing the others left".
fn empty_winnow_text(cascade: &[(String, usize)]) -> String {
    if cascade.is_empty() {
        return "no symbols match (the constellation holds no candidates)".to_string();
    }

    let mut out = String::from(
        "no symbols match. Surviving candidates after each criterion, in evaluation order \
         (cost-ordered, not the order passed):\n",
    );

    for (criterion, surviving) in cascade {
        let _ = writeln!(out, "  {criterion} -> {surviving}");
    }

    out.push_str("drop or loosen the criterion whose row reaches 0.\n");

    out
}

/// The project a `project eq` criterion names, so the candidate load reads one
/// project rather than the whole constellation. `None` when the query does not
/// pin a project, or names one that does not exist (which the criterion itself
/// then eliminates everything for, honestly).
fn winnow_project(
    store: &Store,
    criteria: &[winnow::Criterion],
) -> Result<Option<ProjectId>, StoreError> {
    let named = criteria.iter().find_map(|criterion| match (&criterion.axis, &criterion.value) {
        (winnow::Axis::Project, winnow::Value::Strings(values)) if values.len() == 1 => {
            values.first().cloned()
        }
        _ => None,
    });

    let Some(named) = named else {
        return Ok(None);
    };

    let projects = store.all_projects()?;

    Ok(projects
        .into_iter()
        .find(|project| {
            project.id.as_str().eq_ignore_ascii_case(&named)
                || project.name.eq_ignore_ascii_case(&named)
        })
        .map(|project| project.id))
}

/// Whether an axis reads a node column already in memory, and so costs nothing.
fn is_column_axis(axis: winnow::Axis) -> bool {
    matches!(
        axis,
        winnow::Axis::Decorator
            | winnow::Axis::File
            | winnow::Axis::Kind
            | winnow::Axis::Language
            | winnow::Axis::Lines
            | winnow::Axis::Name
            | winnow::Axis::Project
    )
}

/// Whether a candidate satisfies a column criterion.
fn matches_column(candidate: &WinnowCandidate, criterion: &winnow::Criterion) -> bool {
    let node = &candidate.node;

    match (&criterion.axis, &criterion.value) {
        (winnow::Axis::Kind, winnow::Value::Kinds(kinds)) => kinds.contains(&node.kind),
        (winnow::Axis::Language, winnow::Value::Languages(languages)) => {
            languages.contains(&node.language)
        }
        (winnow::Axis::Project, winnow::Value::Strings(values)) => {
            winnow::string_matches(criterion.op, values, node.project_id.as_str())
        }
        (winnow::Axis::Name, winnow::Value::Strings(values)) => {
            winnow::string_matches(criterion.op, values, &node.name)
        }
        (winnow::Axis::File, winnow::Value::Strings(values)) => {
            winnow::string_matches(criterion.op, values, &node.file_path)
        }
        (winnow::Axis::Decorator, winnow::Value::Strings(values)) => node
            .decorators
            .iter()
            .any(|decorator| winnow::string_matches(criterion.op, values, decorator)),
        (winnow::Axis::Lines, winnow::Value::Number(threshold)) => {
            winnow::number_matches(criterion.op, *threshold, candidate.lines())
        }
        _ => false,
    }
}

/// Whether a candidate satisfies a derived or edge criterion.
fn matches_derived(candidate: &WinnowCandidate, criterion: &winnow::Criterion) -> bool {
    match (&criterion.axis, &criterion.value) {
        (winnow::Axis::Callers, winnow::Value::Number(threshold)) => {
            winnow::number_matches(criterion.op, *threshold, i64::from(candidate.caller_count))
        }
        (winnow::Axis::Churn, winnow::Value::Number(threshold)) => {
            winnow::number_matches(criterion.op, *threshold, i64::from(candidate.churn))
        }
        (winnow::Axis::Risk, winnow::Value::Fraction(threshold)) => {
            winnow::fraction_matches(criterion.op, *threshold, candidate.risk)
        }
        (winnow::Axis::Tested, winnow::Value::Truth(expected)) => candidate.covered == *expected,
        (winnow::Axis::ChangedSince, winnow::Value::Date(_)) => candidate.changed_since,
        (winnow::Axis::InFlow, winnow::Value::Truth(expected)) => {
            !candidate.flow_names.is_empty() == *expected
        }
        (winnow::Axis::InFlow, winnow::Value::Strings(values)) => candidate
            .flow_names
            .iter()
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        (winnow::Axis::CalledBy, winnow::Value::Strings(values)) => candidate
            .caller_names
            .iter()
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        (winnow::Axis::Calls, winnow::Value::Strings(values)) => candidate
            .targets(EdgeKind::Calls)
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        (winnow::Axis::Extends, winnow::Value::Strings(values)) => candidate
            .targets(EdgeKind::Extends)
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        (winnow::Axis::RelatesTo, winnow::Value::Strings(values)) => candidate
            .targets(EdgeKind::RelatesTo)
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        (winnow::Axis::Renders, winnow::Value::Strings(values)) => candidate
            .targets(EdgeKind::Renders)
            .any(|name| winnow::string_matches(criterion.op, values, name)),
        _ => false,
    }
}

/// The derived facts loaded in bulk for the narrowed candidate set: one query
/// per fact rather than one per candidate. Facts no criterion and no ranking
/// needs are skipped entirely.
fn load_winnow_facts(
    store: &Store,
    candidates: &mut [WinnowCandidate],
    criteria: &[winnow::Criterion],
) -> Result<(), StoreError> {
    if candidates.is_empty() {
        return Ok(());
    }

    let ids: Vec<String> =
        candidates.iter().map(|candidate| candidate.node.id.as_str().to_string()).collect();

    let mut caller_counts: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    let mut caller_names: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut covered: FxHashSet<String> = FxHashSet::default();

    for reference in store.incoming_refs(&ids)? {
        if is_covering_ref(reference.kind, &reference.source_file_path) {
            covered.insert(reference.target_id.clone());
        }

        if reference.kind == EdgeKind::Contains {
            continue;
        }

        caller_counts.entry(reference.target_id.clone()).or_default().insert(reference.source_id);
        caller_names.entry(reference.target_id).or_default().push(reference.source_name);
    }

    let mut outgoing: FxHashMap<String, Vec<(EdgeKind, String)>> = FxHashMap::default();

    for reference in store.outgoing_refs(&ids)? {
        outgoing.entry(reference.source_id).or_default().push((reference.kind, reference.target_name));
    }

    let mut flows: FxHashMap<String, Vec<(String, f64)>> = FxHashMap::default();

    for (node_id, name, criticality) in store.flow_membership_for(&ids)? {
        flows.entry(node_id).or_default().push((name, criticality));
    }

    let churn = winnow_churn(store, candidates, criteria)?;
    let changed = winnow_changed_since(store, criteria)?;

    for candidate in candidates.iter_mut() {
        let id = candidate.node.id.as_str();

        candidate.caller_count =
            u32::try_from(caller_counts.get(id).map_or(0, FxHashSet::len)).unwrap_or(u32::MAX);
        candidate.caller_names = caller_names.remove(id).unwrap_or_default();
        candidate.covered = covered.contains(id) || is_test_path(&candidate.node.file_path);
        candidate.outgoing = outgoing.remove(id).unwrap_or_default();

        let memberships = flows.remove(id).unwrap_or_default();

        candidate.criticality =
            memberships.iter().map(|(_, criticality)| *criticality).fold(0.0, f64::max);
        candidate.flow_names = memberships.into_iter().map(|(name, _)| name).collect();

        candidate.churn = churn
            .get(&file_key(candidate.node.project_id.as_str(), &candidate.node.file_path))
            .copied()
            .unwrap_or(0);

        candidate.changed_since = changed.contains(&candidate.node.qualified_name);
        candidate.risk = winnow_risk(candidate);
    }

    Ok(())
}

/// The per-file churn counts for the candidate set, over the window the `churn`
/// criterion names (or the default when it names none, since ranking by churn
/// still needs a window).
fn winnow_churn(
    store: &Store,
    candidates: &[WinnowCandidate],
    criteria: &[winnow::Criterion],
) -> Result<FxHashMap<String, u32>, StoreError> {
    let window_days = criteria
        .iter()
        .find(|criterion| criterion.axis == winnow::Axis::Churn)
        .map_or(winnow::CHURN_WINDOW_DAYS_DEFAULT, |criterion| criterion.window_days);

    let since = now_unix_secs().saturating_sub(i64::from(window_days).saturating_mul(SECONDS_PER_DAY));

    let mut projects: Vec<String> =
        candidates.iter().map(|candidate| candidate.node.project_id.as_str().to_string()).collect();

    projects.sort_unstable();
    projects.dedup();

    let mut churn: FxHashMap<String, u32> = FxHashMap::default();

    for project_id in projects {
        let project = ProjectId::new(project_id.clone());

        for (path, count) in store.file_commit_counts(&project, since)? {
            churn.insert(file_key(&project_id, &path), count);
        }
    }

    Ok(churn)
}

/// The qualified names changed since the date a `changed_since` criterion names,
/// or an empty set when the query carries no such criterion.
fn winnow_changed_since(
    store: &Store,
    criteria: &[winnow::Criterion],
) -> Result<FxHashSet<String>, StoreError> {
    let since = criteria.iter().find_map(|criterion| match (&criterion.axis, &criterion.value) {
        (winnow::Axis::ChangedSince, winnow::Value::Date(epoch)) => Some(*epoch),
        _ => None,
    });

    let Some(since) = since else {
        return Ok(FxHashSet::default());
    };

    Ok(store.qualified_names_changed_since(None, since)?.into_iter().collect())
}

/// The review risk of one winnow candidate. Diff size is necessarily zero here:
/// a winnow query is not scoped to a diff, so the factor has nothing to measure
/// and contributes nothing rather than being invented.
fn winnow_risk(candidate: &WinnowCandidate) -> f64 {
    let home_app = app_segment(&candidate.node.file_path);

    let inputs = risk::RiskInputs {
        caller_count: candidate.caller_count,
        changed_lines: 0,
        churn_commits: candidate.churn,
        covering_tests: if candidate.covered { risk::TEST_SATURATION } else { 0 },
        cross_app_callers: 0,
        cross_project_callers: 0,
        flow_criticality_total: candidate.criticality,
        flow_name_top: candidate.flow_names.first().cloned(),
        security_keyword: risk::security_keyword(
            &candidate.node.name,
            &candidate.node.qualified_name,
        ),
    };

    debug_assert!(!home_app.is_empty(), "every path has a leading segment");

    risk::score(inputs, &risk::RISK_WEIGHTS).total
}

/// The candidates ordered by the requested rank axis, ties broken by the
/// rendered location so two runs over one index emit identical output.
fn sort_winnow(candidates: &mut [WinnowCandidate], rank: winnow::Rank) {
    candidates.sort_by(|left, right| {
        let primary = match rank {
            winnow::Rank::Callers => right.caller_count.cmp(&left.caller_count),
            winnow::Rank::Churn => right.churn.cmp(&left.churn),
            winnow::Rank::Criticality => right.criticality.total_cmp(&left.criticality),
            winnow::Rank::Lines => right.lines().cmp(&left.lines()),
            winnow::Rank::Name => left.node.name.cmp(&right.node.name),
            winnow::Rank::Risk => right.risk.total_cmp(&left.risk),
        };

        primary
            .then_with(|| left.node.file_path.cmp(&right.node.file_path))
            .then(left.node.span.start_line.cmp(&right.node.span.start_line))
    });
}

/// The winnow result rendered: the criteria that produced it, one line per
/// symbol with the facts the query filtered on, and the paging and truncation
/// notes.
#[allow(clippy::too_many_arguments)]
fn render_winnow(
    candidates: &[WinnowCandidate],
    criteria: &[winnow::Criterion],
    rank: winnow::Rank,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
    truncated: bool,
    scanned: usize,
) -> Result<String, StoreError> {
    let window = cursor::slice(candidates, page.offset, limit as usize);

    let described: Vec<String> = criteria.iter().map(winnow::Criterion::describe).collect();

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out,
        "winnow [{}] ranked by {}: {} of {} matching",
        described.join(" AND "),
        rank.as_str(),
        window.len(),
        candidates.len(),
    );

    for candidate in window {
        let _ = writeln!(out,
            "  {}  risk {:.2}, {} callers, {} lines, churn {}{}{}",
            node_line(&candidate.node),
            candidate.risk,
            candidate.caller_count,
            candidate.lines(),
            candidate.churn,
            if candidate.covered { "" } else { ", NO tests" },
            match candidate.flow_names.first() {
                Some(name) => format!(", in {name:?} flow"),
                None => String::new(),
            },
        );
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), candidates.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    if truncated {
        let _ = writeln!(out,
            "(scan capped at {} of {scanned} symbols; narrow with project= or kind=)",
            winnow::WINNOW_CANDIDATES_MAX,
        );
    }

    Ok(out)
}
