//! Reading the working tree's diff, for the tools that answer about change.
//!
//! Every invocation here goes through [`constellation_index::run_git`], which
//! bounds it in time and output. These run inside `serve`, on a request an
//! agent triggered, so an unbounded one is a hung session.

use std::path::Path;

use constellation_index::run_git;
use rustc_hash::FxHashMap;

/// A bound on the untracked files folded into one diff, so a working tree full
/// of unignored scratch output cannot turn a review listing unbounded.
const UNTRACKED_FILES_MAX: usize = 512;

/// A bound on a caller-supplied revision's length. Any real branch name, tag,
/// hash, or `HEAD~n` expression is far shorter.
const REVISION_CHARS_MAX: usize = 256;

/// The reasons a caller-supplied git base was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionError {
    Empty,
    LooksLikeAnOption,
    TooLong,
    HasWhitespace,
}

impl std::fmt::Display for RevisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            RevisionError::Empty => "it is empty",
            RevisionError::LooksLikeAnOption => {
                "it begins with '-', which git reads as an option rather than a revision"
            }
            RevisionError::TooLong => "it is longer than any real revision",
            RevisionError::HasWhitespace => "it contains whitespace or a control character",
        };

        formatter.write_str(message)
    }
}

/// A caller-supplied git base checked for shape before it is handed to git.
///
/// `git diff` takes options as well as revisions, and any value beginning with
/// a dash is read as one. `--output=<path>` writes the diff to a file of the
/// caller's choosing, so an agent-supplied base that is never checked is not
/// merely a wrong revision, it is a write to an arbitrary path. Passing it as an
/// argument rather than through a shell does not help, and neither does `--`:
/// that separator divides revisions from paths, not options from revisions.
/// Checking the shape is what is left.
///
/// Deliberately narrow. It admits every branch, tag, hash, `HEAD~3`,
/// `origin/main`, and `@{upstream}` form; it rejects the handful of exotic
/// revisions carrying spaces (`main@{2 days ago}`), and says so, rather than
/// widening the rule to fit them.
pub fn check_revision(base: &str) -> Result<&str, RevisionError> {
    if base.is_empty() {
        return Err(RevisionError::Empty);
    }

    if base.len() > REVISION_CHARS_MAX {
        return Err(RevisionError::TooLong);
    }

    if base.starts_with('-') {
        return Err(RevisionError::LooksLikeAnOption);
    }

    if base.chars().any(|character| character.is_whitespace() || character.is_control()) {
        return Err(RevisionError::HasWhitespace);
    }

    Ok(base)
}

/// The count of lines inside the 1-based span `[start, end]` that any diff hunk
/// touched, saturating at the span's own length so overlapping hunks cannot
/// report more changed lines than the symbol has.
pub(crate) fn overlapping_lines(ranges: &[(u32, u32)], start: u32, end: u32) -> u32 {
    assert!(start >= 1, "a node span is 1-based");
    assert!(start <= end, "a node span is well-formed");

    let span = end.saturating_sub(start).saturating_add(1);
    let mut covered: u32 = 0;

    for &(hunk_start, hunk_end) in ranges {
        let low = hunk_start.max(start);
        let high = hunk_end.min(end);

        if low <= high {
            covered = covered.saturating_add(high - low + 1);
        }
    }

    let covered = covered.min(span);

    assert!(covered <= span, "changed lines never exceed the symbol's own span");

    covered
}

/// The current UTC time in epoch seconds, or zero if the clock predates the
/// epoch (which would make every churn window empty rather than panic).
pub(crate) use constellation_graph::now_unix_secs;

/// The new-side hunk ranges of `git -C root diff --unified=0 <base>`, keyed by
/// file so a caller can compute per-symbol overlap without re-parsing. Empty
/// when git is unavailable, the path is not a repo, the revision is refused, or
/// nothing changed.
pub(crate) fn git_diff_hunks(root: &str, base: Option<&str>) -> FxHashMap<String, Vec<(u32, u32)>> {
    let Ok(reference) = check_revision(base.unwrap_or("HEAD")) else {
        return FxHashMap::default();
    };

    let Some(run) =
        run_git(Path::new(root), &["diff", "--unified=0", "--no-color", reference])
    else {
        return FxHashMap::default();
    };

    parse_diff_hunks(&run.stdout)
}

/// The files git neither tracks nor ignores, repo-relative. A new file never
/// appears in `git diff`, so without these a branch whose work sits in files not
/// yet added reads as a clean tree. Bounded by [`UNTRACKED_FILES_MAX`]; empty
/// when git is unavailable or the path is not a repo.
pub(crate) fn git_untracked_files(root: &str) -> Vec<String> {
    let Some(run) =
        run_git(Path::new(root), &["ls-files", "--others", "--exclude-standard"])
    else {
        return Vec::new();
    };

    let files: Vec<String> = run
        .stdout
        .lines()
        .filter(|line| !line.is_empty())
        .take(UNTRACKED_FILES_MAX)
        .map(|line| line.to_string())
        .collect();

    assert!(files.len() <= UNTRACKED_FILES_MAX, "the untracked listing is capped");

    files
}

/// The new-side line ranges parsed from a unified diff, grouped by file: each
/// `+++ b/<path>` sets the current file, each `@@ -a,b +c,d @@` yields
/// `(c, c+d-1)` (a zero-length hunk, a pure deletion, maps to its anchor line
/// `c`). Ranges keep the order git emitted them, which is ascending per file.
#[doc(hidden)]
pub fn parse_diff_hunks(diff: &str) -> FxHashMap<String, Vec<(u32, u32)>> {
    let mut ranges: FxHashMap<String, Vec<(u32, u32)>> = FxHashMap::default();
    let mut current_file: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = (path != "/dev/null").then(|| path.to_string());
        } else if line.starts_with("@@")
            && let Some(file) = &current_file
            && let Some((start, len)) = parse_hunk_new_range(line)
        {
            let start = start.max(1);
            let end = if len == 0 { start } else { start + len - 1 };

            ranges.entry(file.clone()).or_default().push((start, end));
        }
    }

    ranges
}

/// The `(start, length)` of a hunk header's new side: `@@ -3,2 +5,4 @@` -> `(5, 4)`,
/// `@@ -3 +5 @@` -> `(5, 1)`. `None` when the header is malformed.
fn parse_hunk_new_range(hunk: &str) -> Option<(u32, u32)> {
    let spec = hunk.split('+').nth(1)?.split(' ').next()?;
    let mut parts = spec.split(',');

    let start: u32 = parts.next()?.parse().ok()?;
    let length: u32 = parts.next().and_then(|value| value.parse().ok()).unwrap_or(1);

    Some((start, length))
}

#[cfg(test)]
mod tests {
    use super::{REVISION_CHARS_MAX, RevisionError, check_revision, git_diff_hunks};

    #[test]
    fn ordinary_revisions_are_accepted() {
        for revision in [
            "HEAD",
            "main",
            "origin/main",
            "HEAD~3",
            "HEAD^2",
            "v1.2.3",
            "3f7a91c",
            "release/2026-01",
            "@{upstream}",
            "feature/PROJ-123_add-widget",
        ] {
            assert_eq!(check_revision(revision), Ok(revision), "{revision:?} is a real revision");
        }
    }

    #[test]
    fn a_revision_that_git_would_read_as_an_option_is_refused() {
        // The one that matters: `git diff --output=<path>` writes the diff to a
        // file of the caller's choosing. An unchecked agent-supplied base is a
        // write to an arbitrary path, not just a wrong answer.
        for option in [
            "--output=/home/user/.bashrc",
            "--output=x",
            "-p",
            "--ext-diff",
            "--no-index",
            "-G.",
        ] {
            assert_eq!(
                check_revision(option),
                Err(RevisionError::LooksLikeAnOption),
                "{option:?} must never reach git as a revision",
            );
        }
    }

    #[test]
    fn empty_overlong_and_whitespace_revisions_are_refused() {
        assert_eq!(check_revision(""), Err(RevisionError::Empty));

        let overlong = "a".repeat(REVISION_CHARS_MAX + 1);

        assert_eq!(check_revision(&overlong), Err(RevisionError::TooLong));

        assert_eq!(check_revision("main; rm -rf /"), Err(RevisionError::HasWhitespace));
        assert_eq!(check_revision("main\nHEAD"), Err(RevisionError::HasWhitespace));
        assert_eq!(check_revision("main\0"), Err(RevisionError::HasWhitespace));
    }

    #[test]
    fn a_refused_revision_never_reaches_git() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_string_lossy().into_owned();
        let bait = directory.path().join("written-by-git");

        let hunks = git_diff_hunks(
            &root,
            Some(&format!("--output={}", bait.display())),
        );

        assert!(hunks.is_empty(), "the call is refused before git runs");
        assert!(!bait.exists(), "and git never wrote the file the option named");
    }

    #[test]
    fn every_refusal_reason_explains_itself() {
        for error in [
            RevisionError::Empty,
            RevisionError::LooksLikeAnOption,
            RevisionError::TooLong,
            RevisionError::HasWhitespace,
        ] {
            assert!(!error.to_string().is_empty(), "{error:?} tells the agent what was wrong");
        }
    }
}
