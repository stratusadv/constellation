//! Deciding whether a stored index is still valid.
//!
//! A change to the extractors invalidates every stored node, so the
//! extractor set is fingerprinted and the fingerprint is stored beside the
//! index. A mismatch re-indexes from scratch rather than merging output
//! from two different parsers.

use std::path::Path;
use std::time::UNIX_EPOCH;



/// A fingerprint of the running binary (its size and modification time)
/// to detect that the extractor changed since a project was last indexed. A
/// rebuilt binary has a new fingerprint, so the next index re-extracts every
/// file instead of keeping nodes the old extractor produced. Returns `None` when
/// the executable cannot be stat'd, which leaves the incremental skip in force.
pub(crate) fn index_fingerprint() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let metadata = std::fs::metadata(&path).ok()?;

    let size = metadata.len();
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0);

    Some(format!("{size}:{modified_ms}"))
}

/// The running binary's fingerprint (see [`index_fingerprint`]), exposed so the
/// history pass can re-ingest when the extractor changes. Empty when the
/// executable cannot be stat'd.
pub fn extractor_fingerprint() -> String {
    index_fingerprint().unwrap_or_default()
}

/// The current `HEAD` commit hash of the git repository at `root`, or `None` when
/// `root` is not a git work tree. Lets the history pass skip a repository whose
/// HEAD has not changed since the last ingest.
pub fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if head.is_empty() { None } else { Some(head) }
}
