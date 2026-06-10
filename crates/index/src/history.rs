//! Git commit history read from a working tree by shelling out to `git log`,
//! parsed into the store's [`CommitRecord`]s. Tier-1 history: the commits and the
//! files they touched (with line churn), without per-symbol diffing.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use constellation_store::{CommitFile, CommitRecord};

use crate::IndexError;

/// A fail-fast bound on the commits read from one repository's history.
pub(crate) const HISTORY_COMMITS_MAX: u32 = 20_000;

/// A fail-fast bound on the files recorded per commit, so a single sweeping
/// commit (a mass reformat, a vendored import) is truncated rather than allowed
/// to balloon the read.
const FILES_PER_COMMIT_MAX: usize = 4_096;

/// The unit-separator byte delimiting a commit header's fields, chosen because it
/// cannot occur in a git hash, author name, unix timestamp, or single-line
/// subject, so a header line is unambiguous against the tab-delimited numstat
/// lines that follow it.
const FIELD_SEPARATOR: char = '\u{1f}';

/// One repository's commit history, newest first, capped at `max` commits,
/// reporting `(done, total)` commits through `on_progress` as they stream in so a
/// caller can draw a progress bar. Empty when `root` is not the top of its own git
/// repository: a non-git directory, or a subdirectory of a larger repo (a `.venv`
/// companion copy lives inside the workspace repo, so indexing its history there
/// would misattribute the workspace's commits to it). A missing `git` binary is an
/// error.
pub(crate) fn read_history(
    root: &Path,
    max: u32,
    mut on_progress: impl FnMut(u32, u32),
) -> Result<Vec<CommitRecord>, IndexError> {
    assert!(!root.as_os_str().is_empty(), "root must not be empty");
    assert!(max > 0, "commit cap must be positive");

    if !is_own_git_root(root) {
        return Ok(Vec::new());
    }

    let total = commit_count(root, max);
    let format =
        format!("--format=tformat:%H{FIELD_SEPARATOR}%an{FIELD_SEPARATOR}%ct{FIELD_SEPARATOR}%s");

    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg(format!("--max-count={max}"))
        .arg("--no-renames")
        .arg("--numstat")
        .arg(format)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| IndexError::Git(format!("running git log: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| IndexError::Git("git log stdout unavailable".to_string()))?;

    let lines = BufReader::new(stdout).lines().map_while(Result::ok);
    let commits = parse_lines(lines, max, |done| on_progress(done.min(total), total));

    let status = child.wait().map_err(|error| IndexError::Git(format!("waiting on git log: {error}")))?;

    if !status.success() {
        return Ok(Vec::new());
    }

    Ok(commits)
}

/// Whether `root` is the top of its own git repository, the test for indexing its
/// history: `git rev-parse --show-toplevel` must resolve to `root` itself, not an
/// ancestor (which a `.venv` companion under the workspace would otherwise inherit).
fn is_own_git_root(root: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output();

    let Ok(output) = output else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    let toplevel = String::from_utf8_lossy(&output.stdout);

    match (std::fs::canonicalize(toplevel.trim()), std::fs::canonicalize(root)) {
        (Ok(toplevel), Ok(root)) => toplevel == root,
        _ => false,
    }
}

/// The number of commits reachable from `HEAD`, capped at `max`, used as the
/// progress-bar total. Zero when the count cannot be taken (an empty repo, or no
/// `HEAD`), which leaves the bar showing complete immediately rather than failing.
fn commit_count(root: &Path, max: u32) -> u32 {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-list")
        .arg("--count")
        .arg(format!("--max-count={max}"))
        .arg("HEAD")
        .output();

    let Ok(output) = output else {
        return 0;
    };

    if !output.status.success() {
        return 0;
    }

    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
}

/// The commits parsed from a whole `git log --numstat` output string via
/// [`parse_lines`]: the convenience entry the unit tests drive, where the live
/// read instead streams through `parse_lines` line by line.
#[cfg(test)]
fn parse_log(text: &str, max: u32) -> Vec<CommitRecord> {
    parse_lines(text.lines().map(|line| line.to_string()), max, |_done| {})
}

/// The shared streaming parser behind [`parse_log`] and [`read_history`]: the same
/// state machine over a line iterator (a string's lines, or a child process's
/// stdout), calling `on_commit` with the running count each time a commit
/// completes so a streaming caller can report progress.
fn parse_lines(
    lines: impl Iterator<Item = String>,
    max: u32,
    mut on_commit: impl FnMut(u32),
) -> Vec<CommitRecord> {
    assert!(max > 0, "commit cap must be positive");

    let mut commits: Vec<CommitRecord> = Vec::new();
    let mut current: Option<CommitRecord> = None;
    let mut count: u32 = 0;

    for line in lines {
        count = count.saturating_add(1);

        assert!(count < u32::MAX, "log line count must not overflow");

        if line.is_empty() {
            continue;
        }

        match parse_header(line.as_str()) {
            Some(header) => {
                if let Some(done) = current.replace(header) {
                    commits.push(done);
                    on_commit(commits.len() as u32);

                    if commits.len() as u32 >= max {
                        commits.truncate(max as usize);

                        return commits;
                    }
                }
            }
            None => append_numstat(current.as_mut(), line.as_str()),
        }
    }

    if let Some(done) = current.take()
        && (commits.len() as u32) < max
    {
        commits.push(done);
        on_commit(commits.len() as u32);
    }

    commits
}

/// One numstat line appended to the in-progress commit, if any and under the
/// per-commit file cap. A line before the first header (no current commit) or a
/// malformed entry is ignored.
fn append_numstat(current: Option<&mut CommitRecord>, line: &str) {
    let Some(commit) = current else {
        return;
    };

    if commit.files.len() >= FILES_PER_COMMIT_MAX {
        return;
    }

    if let Some(file) = parse_numstat(line) {
        commit.files.push(file);
    }
}

/// A commit header parsed from a sentinel-delimited line, or `None` when the line
/// carries no field separator (so it is a numstat line) or its timestamp does not
/// parse.
fn parse_header(line: &str) -> Option<CommitRecord> {
    let mut fields = line.splitn(4, FIELD_SEPARATOR);

    let commit_hash = fields.next()?;
    let author = fields.next()?;
    let committed = fields.next()?;
    let summary = fields.next().unwrap_or("");

    let committed_at: i64 = committed.parse().ok()?;

    assert!(!commit_hash.is_empty(), "a parsed commit hash is never empty");

    Some(CommitRecord {
        commit_hash: commit_hash.to_string(),
        author: author.to_string(),
        committed_at,
        summary: summary.to_string(),
        files: Vec::new(),
    })
}

/// A numstat entry (`insertions TAB deletions TAB path`) parsed into a
/// [`CommitFile`]. A binary file reports `-` for the counts, recorded as zero.
/// `None` for a line missing the path field.
fn parse_numstat(line: &str) -> Option<CommitFile> {
    let mut fields = line.splitn(3, '\t');

    let insertions = fields.next()?;
    let deletions = fields.next()?;
    let file_path = fields.next()?;

    if file_path.is_empty() {
        return None;
    }

    Some(CommitFile {
        file_path: file_path.to_string(),
        insertions: insertions.parse().unwrap_or(0),
        deletions: deletions.parse().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_commit_log() -> String {
        let separator = FIELD_SEPARATOR;

        format!(
            "{first}{separator}Ada{separator}1700000000{separator}add orders model\n\
             10\t0\torders/models.py\n\
             3\t1\torders/views.py\n\
             \n\
             {second}{separator}Bob{separator}1700100000{separator}tweak views\n\
             0\t5\torders/views.py\n",
            first = "a".repeat(40),
            second = "b".repeat(40),
        )
    }

    #[test]
    fn parse_log_reads_headers_and_numstat() {
        let commits = parse_log(&two_commit_log(), 100);

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].committed_at, 1_700_000_000);
        assert_eq!(commits[0].summary, "add orders model");
        assert_eq!(commits[0].files.len(), 2);
        assert_eq!(commits[0].files[0].file_path, "orders/models.py");
        assert_eq!(commits[0].files[0].insertions, 10);
        assert_eq!(commits[0].files[0].deletions, 0);
        assert_eq!(commits[1].author, "Bob");
        assert_eq!(commits[1].files[0].deletions, 5);
    }

    #[test]
    fn parse_log_honors_the_commit_cap() {
        let commits = parse_log(&two_commit_log(), 1);

        assert_eq!(commits.len(), 1, "the cap stops after one commit");
        assert_eq!(commits[0].author, "Ada");
    }

    #[test]
    fn parse_log_records_binary_churn_as_zero() {
        let separator = FIELD_SEPARATOR;
        let text = format!(
            "{hash}{separator}Ada{separator}1700000000{separator}add logo\n-\t-\tlogo.png\n",
            hash = "c".repeat(40),
        );

        let commits = parse_log(&text, 100);

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files[0].file_path, "logo.png");
        assert_eq!(commits[0].files[0].insertions, 0);
        assert_eq!(commits[0].files[0].deletions, 0);
    }
}
