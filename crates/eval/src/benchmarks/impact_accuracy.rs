//! How well the graph predicts what a change will touch, in two modes.
//!
//! The two ground truths are emitted side by side in a `ground_truth_mode`
//! column, and the circular one is labelled circular in the column value itself:
//!
//! - **co-change**: seed the prediction with one file from a commit and grade it
//!   against the *other* files the author touched in that same commit. The
//!   ground truth is git history, not the graph, so this is evidence.
//!   constellation reads it straight from `git_commit_file` with no subprocess,
//!   which is a direct advantage over pattern-matching implementations.
//! - **graph-derived**: the same prediction graded against graph neighbours.
//!   Circular by construction. A ceiling, not evidence.

use rustc_hash::FxHashSet;

use crate::benchmarks::{Context, target_project};
use crate::report::BenchmarkRow;
use crate::score::accuracy;

use constellation_graph::{EdgeKind, ProjectId};
use constellation_store::Store;

/// The name this benchmark reports under.
const NAME: &str = "impact_accuracy";

/// The mode label for the git-history ground truth.
const MODE_CO_CHANGE: &str = "co-change (same commit, seed excluded)";

/// The mode label for the graph ground truth, which names its own circularity.
const MODE_GRAPH: &str = "graph-derived (circular, upper bound)";

/// The largest commit graded. A sweeping refactor touching two hundred files
/// says nothing about impact prediction, only about how the branch was merged.
const COMMIT_FILES_MAX: usize = 25;

/// The smallest commit graded: a single-file commit has no co-changed file to
/// predict.
const COMMIT_FILES_MIN: usize = 2;

/// The benchmark run.
pub fn run(context: &Context<'_>) -> Vec<BenchmarkRow> {
    let project = match target_project(context) {
        Ok(project) => project,
        Err(reason) => {
            return vec![BenchmarkRow::failed(NAME, "f1", "project", reason)];
        }
    };

    let commits = match sample_commits(context.store, &project, context.config.commits_max) {
        Ok(commits) if !commits.is_empty() => commits,
        Ok(_) => {
            return vec![BenchmarkRow::failed(
                NAME,
                "f1",
                "history",
                "no ingested history; run `constellation history`",
            )];
        }
        Err(reason) => return vec![BenchmarkRow::failed(NAME, "f1", "history", reason)],
    };

    let mut rows: Vec<BenchmarkRow> = Vec::new();

    for (commit, files) in commits {
        match grade(context.store, &project, &files) {
            Ok((co_change, graph)) => {
                rows.push(
                    BenchmarkRow::ok(NAME, "f1", commit.clone(), co_change.f1).with_mode(MODE_CO_CHANGE),
                );
                rows.push(
                    BenchmarkRow::ok(NAME, "precision", commit.clone(), co_change.precision)
                        .with_mode(MODE_CO_CHANGE),
                );
                rows.push(
                    BenchmarkRow::ok(NAME, "recall", commit.clone(), co_change.recall)
                        .with_mode(MODE_CO_CHANGE),
                );
                rows.push(BenchmarkRow::ok(NAME, "f1", commit, graph.f1).with_mode(MODE_GRAPH));
            }
            Err(reason) => rows.push(BenchmarkRow::failed(NAME, "f1", commit, reason)),
        }
    }

    rows
}

/// The commits worth grading: those touching between [`COMMIT_FILES_MIN`] and
/// [`COMMIT_FILES_MAX`] files, newest first, capped at `commits_max`.
fn sample_commits(
    store: &Store,
    project: &ProjectId,
    commits_max: u32,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let hits = store
        .history_for_path(Some(project), "%", commits_max.saturating_mul(4).max(commits_max))
        .map_err(|error| error.to_string())?;

    // The project's indexed file set does not change while sampling, so it is
    // read once here rather than rebuilt inside the per-commit loop.
    let indexed: FxHashSet<String> =
        store.project_file_paths(project).map_err(|error| error.to_string())?.into_iter().collect();

    let mut sampled: Vec<(String, Vec<String>)> = Vec::new();

    for hit in hits {
        if sampled.len() >= commits_max as usize {
            break;
        }

        let files = commit_files(store, project, &hit.commit_hash, &indexed)?;

        if (COMMIT_FILES_MIN..=COMMIT_FILES_MAX).contains(&files.len()) {
            sampled.push((hit.commit_hash, files));
        }
    }

    assert!(sampled.len() <= commits_max as usize, "sampling respects its cap");

    Ok(sampled)
}

/// The indexed files one commit touched. Read from `git_commit_file`, not from
/// a `git show` subprocess. `indexed` is the project's file set, passed in
/// because it is the same for every commit in a sample.
fn commit_files(
    store: &Store,
    project: &ProjectId,
    commit_hash: &str,
    indexed: &FxHashSet<String>,
) -> Result<Vec<String>, String> {
    let files: Vec<String> = store
        .files_touched_by(project, commit_hash)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|path| indexed.contains(path))
        .collect();

    Ok(files)
}

/// A commit graded in both modes: the first file seeds the prediction, the
/// rest are the co-change ground truth.
fn grade(
    store: &Store,
    project: &ProjectId,
    files: &[String],
) -> Result<(crate::score::Accuracy, crate::score::Accuracy), String> {
    let (seed, rest) = files.split_first().ok_or("the commit touched no indexed file")?;

    let predicted = predict(store, project, seed)?;

    let expected: FxHashSet<&String> = rest.iter().collect();
    let overlap = predicted.iter().filter(|path| expected.contains(path)).count();

    let co_change = accuracy(predicted.len(), expected.len(), overlap);

    // The graph mode grades the same prediction against itself, the graph's own
    // neighbours, which is why it is an upper bound rather than a measurement:
    // by construction it scores 1.0. Graded from `predicted` rather than from a
    // second identical query, so the code says what the number means.
    let graph = accuracy(predicted.len(), predicted.len(), predicted.len());

    Ok((co_change, graph))
}

/// The files the graph predicts a change to `seed` reaches: the files holding
/// anything that references a symbol defined in it.
fn predict(store: &Store, project: &ProjectId, seed: &str) -> Result<FxHashSet<String>, String> {
    let nodes = store.nodes_file_in(project, seed).map_err(|error| error.to_string())?;

    let ids: Vec<String> = nodes.iter().map(|node| node.id.as_str().to_string()).collect();

    let mut files: FxHashSet<String> = FxHashSet::default();

    for reference in store.incoming_refs(&ids).map_err(|error| error.to_string())? {
        if reference.kind == EdgeKind::Contains || reference.source_file_path == seed {
            continue;
        }

        files.insert(reference.source_file_path);
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::{COMMIT_FILES_MAX, COMMIT_FILES_MIN, MODE_CO_CHANGE, MODE_GRAPH};

    #[test]
    fn the_circular_mode_names_its_own_circularity() {
        assert!(
            MODE_GRAPH.contains("circular"),
            "the column value itself warns the reader, not a footnote elsewhere",
        );
        assert!(MODE_GRAPH.contains("upper bound"), "{MODE_GRAPH}");
    }

    #[test]
    fn the_evidence_mode_names_its_ground_truth() {
        assert!(MODE_CO_CHANGE.contains("same commit"), "{MODE_CO_CHANGE}");
        assert!(!MODE_CO_CHANGE.contains("circular"), "git history is not circular with the graph");
    }

    #[test]
    fn the_commit_size_window_excludes_the_useless_extremes() {
        const { assert!(COMMIT_FILES_MIN >= 2, "a one-file commit has nothing to predict") };
        const { assert!(COMMIT_FILES_MAX > COMMIT_FILES_MIN, "the window is non-empty") };
    }
}
