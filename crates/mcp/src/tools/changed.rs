//! `constellation_changed`: the symbols a branch touched, ranked by
//! review risk rather than by diff order.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, Node, ProjectId, app_segment, is_covering_ref, is_test_path,
};
use constellation_store::{Store, StoreError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::git::{git_diff_hunks, git_untracked_files, now_unix_secs, overlapping_lines};
use crate::limits::{CHANGED_REASONS_MAX, CHANGED_SCORED_MAX, SECONDS_PER_DAY};
use crate::render::{dedup_related, node_line};
use crate::tools::impact::project_roots;
use crate::{cursor, risk};

/// A changed symbol and the diff overlap that surfaced it, before scoring.
pub(crate) struct ChangedCandidate {
    changed_lines: u32,
    pub(crate) node: Node,
}

/// A scored changed symbol: the factors behind its risk, its rendered
/// location line, and the weight set that produced the score. The weights are
/// carried per row because they differ by project: a project with no ingested
/// history drops the churn factor and renormalizes the rest.
struct ScoredChange {
    factors: risk::RiskFactors,
    line: String,
    weights: risk::RiskWeights,
}

/// The per-project inputs a risk score needs beyond the node itself: the churn
/// counts read from indexed history, the weight set that history's presence (or
/// absence) implies, and the notes that explain any dropped factor.
struct ChangedContext {
    churn: FxHashMap<String, FxHashMap<String, u32>>,
    notes: Vec<String>,
    weights: FxHashMap<String, risk::RiskWeights>,
}

impl ChangedContext {
    /// The context loaded for every project named in `project_ids`, one pair of
    /// aggregate queries each, so per-symbol scoring issues no further churn
    /// queries.
    fn load(store: &Store, project_ids: &[String]) -> Result<Self, StoreError> {
        let since = now_unix_secs().saturating_sub(risk::CHURN_WINDOW_DAYS * SECONDS_PER_DAY);

        let mut context = Self {
            churn: FxHashMap::default(),
            notes: Vec::new(),
            weights: FxHashMap::default(),
        };

        for project_id in project_ids {
            let project = ProjectId::new(project_id.clone());

            let availability = risk::FactorAvailability {
                churn: store.count_history_commits(&project)? > 0,
                flow_participation: store.count_flows(&project)? > 0,
            };

            if availability.churn {
                context.churn.insert(project_id.clone(), store.file_commit_counts(&project, since)?);
            }

            // One note per project, naming every dropped factor and the pass that
            // would supply it: a renormalized score is comparable but blind to
            // whatever is missing, and a reader must be told which.
            let mut missing: Vec<&str> = Vec::new();

            if !availability.churn {
                missing.push("churn (run `constellation history`)");
            }

            if !availability.flow_participation {
                missing.push("flow participation (run `constellation flows`)");
            }

            if !missing.is_empty() {
                context.notes.push(format!(
                    "note: {project_id} scored without {}; remaining weights renormalized",
                    missing.join(" and "),
                ));
            }

            context.weights.insert(project_id.clone(), risk::RISK_WEIGHTS.renormalized(availability));
        }

        assert!(context.weights.len() == project_ids.len(), "every project carries a weight set");

        Ok(context)
    }

    /// The commits within the churn window that touched one project's file, or
    /// zero when that project has no ingested history.
    fn churn_commits(&self, project_id: &str, file_path: &str) -> u32 {
        self.churn
            .get(project_id)
            .and_then(|counts| counts.get(file_path))
            .copied()
            .unwrap_or(0)
    }

    /// The weight set for one project, falling back to the tuned defaults for a
    /// project that was not loaded (which cannot happen for a scored symbol).
    fn weights_for(&self, project_id: &str) -> risk::RiskWeights {
        self.weights.get(project_id).copied().unwrap_or(risk::RISK_WEIGHTS)
    }
}

/// The changed symbols ranked by review risk: the definitions overlapping the
/// working-tree diff against `base` (default `HEAD`), scored on test coverage,
/// security sensitivity, fan-in, cross-app and cross-project reach, churn, diff
/// size, and flow participation, highest risk first. Combines `git diff` with
/// the graph, the edit-impact view git alone cannot give.
#[doc(hidden)]
pub fn changed_text(
    store: &Store,
    base: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let roots = project_roots(store)?;

    let mut project_ids: Vec<String> = roots.keys().cloned().collect();
    project_ids.sort_unstable();

    let mut candidates: Vec<ChangedCandidate> = Vec::new();

    for project_id in &project_ids {
        let root = roots.get(project_id).expect("every listed project has a root");
        let project = ProjectId::new(project_id.clone());

        candidates.extend(changed_candidates(store, &project, root, base)?);
    }

    if candidates.is_empty() {
        return Ok("no changed symbols (nothing modified, staged, or untracked vs the diff \
                   base, or not a git repo)"
            .to_string());
    }

    let discovered = candidates.len();
    candidates.truncate(CHANGED_SCORED_MAX);

    assert!(candidates.len() <= CHANGED_SCORED_MAX, "scoring respects its per-call cap");

    // Only the projects that actually contributed a changed symbol: a note about
    // renormalized weights for a project with nothing in the diff is noise, and
    // one per indexed project buries the listing it explains.
    let mut scored_project_ids: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.node.project_id.as_str().to_string())
        .collect();

    scored_project_ids.sort_unstable();
    scored_project_ids.dedup();

    let context = ChangedContext::load(store, &scored_project_ids)?;
    let mut scored: Vec<ScoredChange> = Vec::with_capacity(candidates.len());

    for candidate in &candidates {
        scored.push(score_change(store, candidate, &context)?);
    }

    // Descending risk, then descending fan-in, then the rendered line, so two
    // runs over an unchanged working tree emit byte-identical output.
    scored.sort_by(|left, right| {
        let by_risk = right.factors.total.total_cmp(&left.factors.total);
        let by_callers =
            right.factors.inputs.caller_count.cmp(&left.factors.inputs.caller_count);

        by_risk.then(by_callers).then_with(|| left.line.cmp(&right.line))
    });

    Ok(render_changed(&scored, &context, discovered, limit, page, generation))
}

/// The changed symbols of one project: every definition whose span overlaps a
/// diff hunk, deduplicated, each carrying the count of changed lines inside it.
pub(crate) fn changed_candidates(
    store: &Store,
    project: &ProjectId,
    root: &str,
    base: Option<&str>,
) -> Result<Vec<ChangedCandidate>, StoreError> {
    let mut hunks = git_diff_hunks(root, base);

    // An untracked file is entirely new, so every line in it is a changed line.
    // `overlapping_lines` saturates at each symbol's own span, so the open range
    // costs a symbol exactly its length rather than an invented number.
    for file in git_untracked_files(root) {
        hunks.entry(file).or_insert_with(|| vec![(1, u32::MAX)]);
    }

    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut candidates: Vec<ChangedCandidate> = Vec::new();

    for (file, ranges) in &hunks {
        for &(start, end) in ranges {
            for node in store.nodes_in_range(project, file, start, end)? {
                if !seen.insert(node.id.as_str().to_string()) {
                    continue;
                }

                let changed_lines =
                    overlapping_lines(ranges, node.span.start_line, node.span.end_line);

                candidates.push(ChangedCandidate { changed_lines, node });
            }
        }
    }

    Ok(candidates)
}

/// Whether a rendered node line carries a working-tree marker, the trigger for
/// printing the legend that explains them.
fn has_working_tree_marker(line: &str) -> bool {
    ["[M]", "[A]", "[D]", "[?]"].iter().any(|marker| line.ends_with(marker))
}

/// The score for one changed symbol: its callers are read once, split into the
/// coverage, fan-in, cross-app, and cross-project counts the score needs, and
/// blended with the project's churn and flow inputs.
fn score_change(
    store: &Store,
    candidate: &ChangedCandidate,
    context: &ChangedContext,
) -> Result<ScoredChange, StoreError> {
    let node = &candidate.node;
    let project_id = node.project_id.as_str();

    let mut callers = store.callers(&node.id)?;
    callers.retain(|(kind, _)| *kind != EdgeKind::Contains);

    let covering =
        callers.iter().filter(|(kind, caller)| is_covering_ref(*kind, &caller.file_path)).count();

    let related = dedup_related(callers);
    let home_app = app_segment(&node.file_path);

    let mut cross_app: u32 = 0;
    let mut cross_project: u32 = 0;

    for (_, caller, _) in &related {
        if app_segment(&caller.file_path) != home_app {
            cross_app = cross_app.saturating_add(1);
        }

        if caller.project_id.as_str() != project_id {
            cross_project = cross_project.saturating_add(1);
        }
    }

    // A symbol that lives in a test file is its own coverage; scoring it as
    // untested would rank the test suite above the code it guards.
    let covering_tests = if is_test_path(&node.file_path) {
        risk::TEST_SATURATION
    } else {
        u32::try_from(covering).unwrap_or(u32::MAX)
    };

    let (flow_criticality_total, flow_name_top) = store.flow_participation(&node.id)?;

    let inputs = risk::RiskInputs {
        caller_count: u32::try_from(related.len()).unwrap_or(u32::MAX),
        changed_lines: candidate.changed_lines,
        churn_commits: context.churn_commits(project_id, &node.file_path),
        covering_tests,
        cross_app_callers: cross_app,
        cross_project_callers: cross_project,
        flow_criticality_total,
        flow_name_top,
        // A test *about* permissions is not a permission change. The keyword scan
        // reads names, and a test file's names describe what they exercise, so
        // `test_harvest_permission_codename` scored as security-sensitive and
        // review-ranked above the production code it covers. Nothing in a test path
        // ships, so the factor does not apply there.
        security_keyword: if is_test_path(&node.file_path) {
            None
        } else {
            risk::security_keyword(&node.name, &node.qualified_name)
        },
    };

    let weights = context.weights_for(project_id);

    Ok(ScoredChange { factors: risk::score(inputs, &weights), line: node_line(node), weights })
}

/// The ranked listing rendered: one line per shown symbol with its score and the
/// strongest reasons behind it, then the explicit truncation and dropped-factor
/// notes. Nothing is ever truncated silently.
fn render_changed(
    scored: &[ScoredChange],
    context: &ChangedContext,
    discovered: usize,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> String {
    let window = cursor::slice(scored, page.offset, limit as usize);

    assert!(window.len() <= scored.len(), "a page never exceeds what was scored");

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out,
        "changed symbols by review risk ({} of {discovered}):",
        window.len(),
    );

    // The working-tree markers `node_line` appends are the point of this tool, so
    // the one place they must be explained is here. Emitted only when a row
    // actually carries one, so the legend never costs bytes it does not earn.
    if window.iter().any(|change| has_working_tree_marker(&change.line)) {
        out.push_str("  ([M] modified, [A] added, [D] deleted, [?] untracked)\n");
    }

    for change in window {
        let summary = change.factors.summary(&change.weights, CHANGED_REASONS_MAX);
        let reasons = if summary.is_empty() { String::new() } else { format!(" ({summary})") };

        let _ = writeln!(out, "  {}  risk {:.2}{reasons}", change.line, change.factors.total);
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), scored.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    if discovered > scored.len() {
        let _ = writeln!(out,
            "(+{} changed symbols not scored; narrow with base=)",
            discovered - scored.len(),
        );
    }

    for note in &context.notes {
        out.push_str(note);
        out.push('\n');
    }

    out
}
