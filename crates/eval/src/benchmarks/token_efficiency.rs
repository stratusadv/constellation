//! How many tokens each way of answering a change question costs.
//!
//! Three approaches over the same commits: reading every changed file in full,
//! reading the diff, and one `explore` call seeded from the commit message. The
//! ratios are what matter; the absolute figures are a four-bytes-per-token
//! approximation and the report says so.
//!
//! A failed call is recorded with `status = error` and excluded from the
//! aggregates. It must never be scored as zero tokens, which would read as a
//! spectacular efficiency win.

use std::path::Path;

use crate::benchmarks::{Context, target_project};
use crate::report::BenchmarkRow;
use crate::score::approximate_tokens;

use constellation_graph::ProjectId;
use constellation_mcp::ConstellationServer;
use constellation_store::Store;

/// The name this benchmark reports under.
const NAME: &str = "token_efficiency";

/// The largest commit sampled.
const COMMIT_FILES_MAX: usize = 25;

/// The files one explore call is allowed to return.
const EXPLORE_FILES: u32 = 8;

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

    let commits = match context
        .store
        .history_for_path(Some(&project), "%", context.config.commits_max)
    {
        Ok(commits) if !commits.is_empty() => commits,
        Ok(_) => {
            return vec![BenchmarkRow::failed(
                NAME,
                "tokens",
                "history",
                "no ingested history; run `constellation history`",
            )];
        }
        Err(error) => {
            return vec![BenchmarkRow::failed(NAME, "tokens", "history", error.to_string())];
        }
    };

    let mut rows: Vec<BenchmarkRow> = Vec::new();

    for commit in commits {
        let case = format!("{} {}", &commit.commit_hash[..commit.commit_hash.len().min(8)], commit.summary);

        match measure(context, &project, Path::new(&root), &commit.commit_hash, &commit.summary) {
            Ok(measurement) => {
                rows.push(BenchmarkRow::ok(NAME, "tokens", case.clone(), measurement.full_read as f64)
                    .with_mode("read every changed file"));
                rows.push(BenchmarkRow::ok(NAME, "tokens", case.clone(), measurement.diff as f64)
                    .with_mode("git diff"));
                rows.push(BenchmarkRow::ok(NAME, "tokens", case.clone(), measurement.explore as f64)
                    .with_mode("one constellation_explore call"));

                rows.push(
                    BenchmarkRow::ok(NAME, "ratio_vs_full_read", case, measurement.ratio())
                        .with_detail("explore tokens over full-read tokens; lower is better"),
                );
            }
            Err(reason) => rows.push(BenchmarkRow::failed(NAME, "tokens", case, reason)),
        }
    }

    rows
}

/// A commit's three token counts.
struct Measurement {
    diff: usize,
    explore: usize,
    full_read: usize,
}

impl Measurement {
    /// The explore cost as a fraction of the full-read cost, one when the
    /// full read cost nothing (an empty commit), so a degenerate case cannot
    /// look like an infinite win.
    fn ratio(&self) -> f64 {
        if self.full_read == 0 {
            return 1.0;
        }

        self.explore as f64 / self.full_read as f64
    }
}

/// A commit measured all three ways.
fn measure(
    context: &Context<'_>,
    project: &ProjectId,
    root: &Path,
    commit_hash: &str,
    summary: &str,
) -> Result<Measurement, String> {
    let files = context
        .store
        .files_touched_by(project, commit_hash)
        .map_err(|error| error.to_string())?;

    if files.is_empty() || files.len() > COMMIT_FILES_MAX {
        return Err(format!("{} files touched; outside the sampled window", files.len()));
    }

    let full_read = files
        .iter()
        .filter_map(|path| std::fs::read_to_string(root.join(path)).ok())
        .map(|source| approximate_tokens(source.len()))
        .sum();

    let diff = approximate_tokens(diff_bytes(root, commit_hash)?);

    let store = Store::open(&context.database).map_err(|error| error.to_string())?;
    let server = ConstellationServer::new(store);

    let text = server
        .explore_text(summary, EXPLORE_FILES)
        .map_err(|error| error.to_string())?;

    Ok(Measurement { diff, explore: approximate_tokens(text.len()), full_read })
}

/// The byte length of `git show` for one commit, or an error when git is
/// unavailable or the commit is unknown.
fn diff_bytes(root: &Path, commit_hash: &str) -> Result<usize, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg("--no-color")
        .arg(commit_hash)
        .output()
        .map_err(|error| format!("running git: {error}"))?;

    if !output.status.success() {
        return Err(format!("git show failed for {commit_hash}"));
    }

    Ok(output.stdout.len())
}

#[cfg(test)]
mod tests {
    use super::Measurement;

    #[test]
    fn the_ratio_is_explore_over_full_read() {
        let measurement = Measurement { diff: 200, explore: 500, full_read: 5_000 };

        assert!((measurement.ratio() - 0.1).abs() < 1e-9, "got {}", measurement.ratio());
    }

    #[test]
    fn an_empty_full_read_reports_parity_rather_than_an_infinite_win() {
        let measurement = Measurement { diff: 0, explore: 500, full_read: 0 };

        assert_eq!(measurement.ratio(), 1.0, "a degenerate case never looks like a win");
    }
}
