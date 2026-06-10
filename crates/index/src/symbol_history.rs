//! Symbol-level git history (Tier 2): for each file that ever changed, extract
//! its trackable symbols at every commit that touched it and diff against the
//! prior revision, yielding the per-commit added / modified / removed rows. Blobs
//! are read from one long-lived `git cat-file --batch` process, fed in lockstep
//! so the pipe never deadlocks; the same extractors the live index uses parse the
//! historical source (which is just a string, so no working-tree checkout).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use constellation_extraction::{
    CssExtractor, Extractor, JavaScriptExtractor, PythonExtractor, SOURCE_BYTES_MAX,
    TemplateExtractor,
};
use constellation_graph::{Language, NodeKind, ProjectId};
use constellation_store::{FileTouch, SymbolChange, SymbolRevision};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::IndexError;

/// A fail-fast bound on the file-touches loaded for one symbol-history pass.
pub(crate) const TOUCHES_MAX: u32 = 2_000_000;

/// The file-revisions read and parsed per batch within one file's history. It
/// bounds peak memory (a batch's blob sources and symbol sets are held at once)
/// while keeping the parse work parallel. Most files have fewer revisions than
/// this, so their whole history parses in a single parallel batch.
const REVISION_BATCH_MAX: usize = 512;

/// The symbol-change rows for a project, derived by diffing each touched file's
/// trackable symbols across the commits that touched it, in the chronological
/// order `touches` arrives in. `touches` must be grouped by file (the store
/// orders it so) and non-empty.
pub(crate) fn diff_history(
    root: &Path,
    project: &ProjectId,
    touches: &[FileTouch],
    mut on_progress: impl FnMut(u32, u32),
) -> Result<Vec<SymbolRevision>, IndexError> {
    assert!(!touches.is_empty(), "touches must not be empty");
    assert!(!root.as_os_str().is_empty(), "root must not be empty");

    let extractors = extractors();
    let mut reader = BlobReader::open(root)?;

    let total = touches.len() as u32;
    let mut rows: Vec<SymbolRevision> = Vec::new();
    let mut done: u32 = 0;
    let mut start: usize = 0;

    // Touches arrive grouped by file, each group in chronological order. Process
    // one file's run at a time: read its revisions serially (cat-file is lockstep)
    // then parse them in parallel (the dominant cost), then diff in order against
    // the running previous revision. A run is sub-chunked so a file with a very
    // long history bounds peak memory; `prev` carries across the sub-chunks.
    while start < touches.len() {
        let file = touches[start].file_path.as_str();

        let mut end = start + 1;

        while end < touches.len() && touches[end].file_path == file {
            end += 1;
        }

        let mut prev: FxHashMap<String, SymbolEntry> = FxHashMap::default();

        for window in touches[start..end].chunks(REVISION_BATCH_MAX) {
            let sources = read_blobs(&mut reader, window)?;

            let parsed: Vec<FxHashMap<String, SymbolEntry>> = sources
                .par_iter()
                .map(|source| match source {
                    Some(source) => symbols_of(&extractors, project, file, source),
                    None => FxHashMap::default(),
                })
                .collect();

            for (touch, curr) in window.iter().zip(parsed) {
                diff_into(&prev, &curr, &touch.commit_hash, &touch.file_path, &mut rows);

                prev = curr;
                done += 1;

                on_progress(done, total);
            }
        }

        start = end;
    }

    assert!(done == total, "every touch is diffed exactly once");

    Ok(rows)
}

/// The blob source for each touch in `window`, read serially through the shared
/// `cat-file` process (its pipe is lockstep, so reads cannot overlap). `None` for
/// a revision git reports missing or one that exceeds the size cap.
fn read_blobs(
    reader: &mut BlobReader,
    window: &[FileTouch],
) -> Result<Vec<Option<String>>, IndexError> {
    let mut sources: Vec<Option<String>> = Vec::with_capacity(window.len());

    for touch in window {
        sources.push(reader.blob(&touch.commit_hash, &touch.file_path)?);
    }

    Ok(sources)
}

/// The extractors the live index uses, rebuilt for the history pass so the
/// historical source parses exactly as the current tree does.
fn extractors() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(PythonExtractor::new()),
        Box::new(TemplateExtractor::new()),
        Box::new(JavaScriptExtractor::new()),
        Box::new(CssExtractor::new()),
    ]
}

/// One entry in a file revision's symbol set: the attributes that decide whether
/// the symbol changed between revisions.
struct SymbolEntry {
    name: String,
    kind: &'static str,
    signature: Option<String>,
}

/// The trackable symbols of `source` at `file_path`, keyed by qualified name. A
/// file of an unparsed language, an oversized source, or an extractor panic
/// yields an empty set. Non-definition kinds (files, imports, parameters, locals)
/// are dropped as noise.
fn symbols_of(
    extractors: &[Box<dyn Extractor>],
    project: &ProjectId,
    file_path: &str,
    source: &str,
) -> FxHashMap<String, SymbolEntry> {
    let mut symbols = FxHashMap::default();

    let language = Path::new(file_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(Language::from_extension);

    let Some(language) = language else {
        return symbols;
    };

    let Some(extractor) = extractors.iter().find(|extractor| extractor.language() == language)
    else {
        return symbols;
    };

    if source.len() > SOURCE_BYTES_MAX {
        return symbols;
    }

    // Historical source can be malformed in ways the current tree is not; isolate
    // a parser edge case to this one revision rather than abort the whole pass.
    let Ok(output) = catch_unwind(AssertUnwindSafe(|| extractor.extract(project, file_path, source)))
    else {
        return symbols;
    };

    for node in output.nodes {
        if !is_trackable_kind(node.kind) {
            continue;
        }

        symbols.insert(
            node.qualified_name,
            SymbolEntry { name: node.name, kind: node.kind.as_str(), signature: node.signature },
        );
    }

    symbols
}

/// The changes between a file's previous and current revision, appended to
/// `rows`: symbols present now but not before are added, those gone now are
/// removed, and those whose signature changed are modified. The signature
/// recorded is the new one for added/modified, the prior one for removed.
fn diff_into(
    prev: &FxHashMap<String, SymbolEntry>,
    curr: &FxHashMap<String, SymbolEntry>,
    commit_hash: &str,
    file_path: &str,
    rows: &mut Vec<SymbolRevision>,
) {
    for (qualified_name, entry) in curr {
        let change = match prev.get(qualified_name) {
            None => SymbolChange::Added,
            Some(before) if before.signature != entry.signature => SymbolChange::Modified,
            Some(_) => continue,
        };

        rows.push(revision(commit_hash, file_path, qualified_name, entry, change));
    }

    for (qualified_name, entry) in prev {
        if !curr.contains_key(qualified_name) {
            rows.push(revision(commit_hash, file_path, qualified_name, entry, SymbolChange::Removed));
        }
    }
}

/// One [`SymbolRevision`] built from a diffed symbol entry.
fn revision(
    commit_hash: &str,
    file_path: &str,
    qualified_name: &str,
    entry: &SymbolEntry,
    change: SymbolChange,
) -> SymbolRevision {
    SymbolRevision {
        commit_hash: commit_hash.to_string(),
        file_path: file_path.to_string(),
        qualified_name: qualified_name.to_string(),
        name: entry.name.clone(),
        kind: entry.kind.to_string(),
        change,
        signature: entry.signature.clone(),
    }
}

/// Whether a node kind is a definition worth tracking across history: the
/// structural symbols whose appearance, signature change, or removal tells the
/// transformation story. Files, imports, modules, parameters, locals, templates,
/// CSS selectors, and synthetic boundary nodes are excluded as noise.
fn is_trackable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Constant
            | NodeKind::Field
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Model
            | NodeKind::Property
            | NodeKind::Route
            | NodeKind::View
    )
}

/// A long-lived `git cat-file --batch` process for one repository, fed blob
/// requests (`<commit>:<path>`) in lockstep: write one request, read its full
/// response, before the next, so neither pipe ever fills and deadlocks.
struct BlobReader {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BlobReader {
    /// A `cat-file --batch` process rooted at `root`.
    fn open(root: &Path) -> Result<Self, IndexError> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("cat-file")
            .arg("--batch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| IndexError::Git(format!("starting git cat-file: {error}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| IndexError::Git("git cat-file stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| IndexError::Git("git cat-file stdout unavailable".to_string()))?;

        Ok(Self { child, stdin: Some(stdin), stdout: BufReader::new(stdout) })
    }

    /// The blob at `<commit_hash>:<file_path>`, or `None` when git reports it
    /// missing (the path did not exist at that commit, e.g. before it was added or
    /// after it was deleted) or it exceeds [`SOURCE_BYTES_MAX`].
    fn blob(&mut self, commit_hash: &str, file_path: &str) -> Result<Option<String>, IndexError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| IndexError::Git("cat-file stdin closed".to_string()))?;

        writeln!(stdin, "{commit_hash}:{file_path}")
            .and_then(|()| stdin.flush())
            .map_err(|error| IndexError::Git(format!("writing cat-file request: {error}")))?;

        let mut header = String::new();
        let read = self
            .stdout
            .read_line(&mut header)
            .map_err(|error| IndexError::Git(format!("reading cat-file header: {error}")))?;

        if read == 0 {
            return Err(IndexError::Git("git cat-file closed early".to_string()));
        }

        let header = header.trim_end();

        if header.ends_with("missing") {
            return Ok(None);
        }

        let size =
            parse_batch_size(header).ok_or_else(|| IndexError::Git(format!("bad cat-file header: {header:?}")))?;

        if size > SOURCE_BYTES_MAX {
            self.discard(size + 1)?;

            return Ok(None);
        }

        let mut buffer = vec![0u8; size + 1];
        self.stdout
            .read_exact(&mut buffer)
            .map_err(|error| IndexError::Git(format!("reading blob: {error}")))?;

        buffer.truncate(size);

        Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
    }

    /// The next `count` bytes of output read and dropped, to keep the stream in
    /// sync after skipping an oversized blob.
    fn discard(&mut self, count: usize) -> Result<(), IndexError> {
        let mut limited = self.stdout.by_ref().take(count as u64);

        io::copy(&mut limited, &mut io::sink())
            .map_err(|error| IndexError::Git(format!("skipping oversized blob: {error}")))?;

        Ok(())
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        // Close stdin first so git sees EOF and exits, then reap it; reaping while
        // stdin is still open would hang waiting for a process that waits for us.
        drop(self.stdin.take());

        let _ = self.child.wait();
    }
}

/// The blob byte size parsed from a `cat-file --batch` header line
/// `<oid> <type> <size>`, or `None` when the trailing field is not a number.
fn parse_batch_size(header: &str) -> Option<usize> {
    header.rsplit(' ').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(signature: &str) -> SymbolEntry {
        SymbolEntry {
            name: "Order".to_string(),
            kind: "class",
            signature: Some(signature.to_string()),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> FxHashMap<String, SymbolEntry> {
        pairs.iter().map(|(name, signature)| (name.to_string(), entry(signature))).collect()
    }

    #[test]
    fn diff_into_reports_added_modified_and_removed() {
        let prev = map(&[("orders.Order.total", "int"), ("orders.Order.note", "str")]);
        let curr = map(&[("orders.Order.total", "Decimal"), ("orders.Order.created", "datetime")]);

        let mut rows = Vec::new();
        diff_into(&prev, &curr, "c0ffee", "orders/models.py", &mut rows);

        let added: Vec<_> = rows.iter().filter(|r| r.change == SymbolChange::Added).collect();
        let modified: Vec<_> = rows.iter().filter(|r| r.change == SymbolChange::Modified).collect();
        let removed: Vec<_> = rows.iter().filter(|r| r.change == SymbolChange::Removed).collect();

        assert_eq!(added.len(), 1);
        assert_eq!(added[0].qualified_name, "orders.Order.created");
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].qualified_name, "orders.Order.total");
        assert_eq!(modified[0].signature.as_deref(), Some("Decimal"), "the new signature is recorded");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].qualified_name, "orders.Order.note");
    }

    #[test]
    fn diff_into_is_silent_when_nothing_changed() {
        let prev = map(&[("orders.Order.total", "int")]);
        let curr = map(&[("orders.Order.total", "int")]);

        let mut rows = Vec::new();
        diff_into(&prev, &curr, "c0ffee", "orders/models.py", &mut rows);

        assert!(rows.is_empty());
    }

    #[test]
    fn parse_batch_size_reads_the_size_field() {
        assert_eq!(parse_batch_size("deadbeef blob 42"), Some(42));
        assert_eq!(parse_batch_size("deadbeef blob 0"), Some(0));
        assert_eq!(parse_batch_size("not-a-header"), None);
    }

    #[test]
    fn trackable_kinds_keep_definitions_and_drop_noise() {
        assert!(is_trackable_kind(NodeKind::Field));
        assert!(is_trackable_kind(NodeKind::Model));
        assert!(is_trackable_kind(NodeKind::Method));
        assert!(!is_trackable_kind(NodeKind::File));
        assert!(!is_trackable_kind(NodeKind::Import));
        assert!(!is_trackable_kind(NodeKind::Parameter));
    }
}
