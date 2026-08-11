//! Extraction and persistence: source in, one file's rows out.
//!
//! This is the parallel half of an index run. Files are extracted across
//! threads and persisted serially, because extraction is pure and the store
//! is not.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use constellation_extraction::{
    CssExtractor, ExtractionOutput, Extractor, JavaScriptExtractor, PythonExtractor,
    SOURCE_BYTES_MAX, TemplateExtractor,
};
use constellation_graph::{Edge, EdgeKind, Language, NodeId, ProjectId, is_minified_source};
use constellation_resolution::{
    DjangoResolver, FrameworkResolver,
};
use constellation_store::{FileIndex, Store};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{IndexError, IndexOutcome, IndexPhase, IndexStats};
use crate::context::FsContext;
use crate::fingerprint::index_fingerprint;
use crate::limits::{FILE_COUNT_MAX, extract_batch_size};
use crate::resolve::run_resolution_phase;
use crate::walk::{collect_file_paths, hash_hex, is_minified, modified_ms, relative_path, to_u32};

/// The roll-back guard for the bulk write transaction: disarmed on success, rolls
/// back on drop when the indexing walk returns early with an error.
struct BulkGuard<'store> {
    store: &'store Store,
    armed: bool,
}

impl Drop for BulkGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.store.bulk_rollback();
        }
    }
}

/// The way a run decides which stored files no longer exist.
///
/// This is the whole difference between a full index and a watcher's
/// path-scoped one, and it is a correctness distinction rather than a
/// performance one: absence only proves deletion to a run that looked
/// everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sweep {
    /// The sweep removing every stored file the run did not see. Available only to a
    /// run that walked the whole tree, which therefore knows the complete
    /// on-disk set.
    Everything,
    /// The sweep considering only the paths the caller named, of which only those that no
    /// longer exist are removed. A path-scoped run has looked at a handful of
    /// files and knows nothing about the rest, so it must not read their
    /// absence from its own result set as deletion.
    Named,
}

/// The [`index_project_reporting`] run, also returning the project-relative paths the
/// run rewrote or removed.
pub fn index_project_tracked(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    on_phase: impl FnMut(IndexPhase),
) -> Result<IndexOutcome, IndexError> {
    assert!(root.is_dir(), "project root must be a directory: {root:?}");

    let root_absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_absolute.as_path();

    let paths = collect_file_paths(root)?;

    index_files(store, project, name, root, &paths, Sweep::Everything, on_phase)
}

/// The index of exactly `paths`, for a watcher that knows which files a burst
/// touched and should not re-walk the project to find out.
///
/// `paths` are absolute and need not all exist: one that is gone has its rows
/// removed, which is how a delete or the losing half of a rename is applied.
/// Paths outside `root`, and directories, are ignored.
///
/// Falls back to a full walk when the extractor fingerprint has moved, because
/// a fingerprint change means every file's stored nodes are stale, not just the
/// ones in this burst.
pub fn index_paths_tracked(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    paths: &[PathBuf],
) -> Result<IndexOutcome, IndexError> {
    assert!(root.is_dir(), "project root must be a directory: {root:?}");

    let root_absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_absolute.as_path();

    if extractor_changed(store, project)? {
        return index_project_tracked(store, project, name, root, |_phase| {});
    }

    let owned: Vec<PathBuf> =
        paths.iter().filter(|path| path.starts_with(root)).cloned().collect();

    if owned.is_empty() {
        return Ok(IndexOutcome::default());
    }

    index_files(store, project, name, root, &owned, Sweep::Named, |_phase| {})
}

/// Whether the binary's extractor fingerprint differs from the one stamped on
/// the project, which makes every stored file's nodes stale.
fn extractor_changed(store: &Store, project: &ProjectId) -> Result<bool, IndexError> {
    let Some(current) = index_fingerprint() else {
        return Ok(false);
    };

    Ok(store.index_version(project)? != current)
}

/// The shared body of every index run: extract the given paths in parallel,
/// persist them serially, sweep removals according to `sweep`, then resolve.
fn index_files(
    store: &Store,
    project: &ProjectId,
    name: &str,
    root: &Path,
    paths: &[PathBuf],
    sweep: Sweep,
    mut on_phase: impl FnMut(IndexPhase),
) -> Result<IndexOutcome, IndexError> {
    assert!(!name.is_empty(), "project name must not be empty");

    store.upsert_project(project, name, &root.to_string_lossy())?;

    // A change to the extractor (a rebuilt binary) leaves every source file's
    // content hash unchanged, so the per-file skip would keep the old extractor's
    // nodes. Compare the binary's fingerprint to the project's stamp and, on a
    // mismatch, re-extract every file by extracting against an empty hash baseline.
    let fingerprint = index_fingerprint();
    let force_full = match fingerprint.as_deref() {
        Some(current) => store.index_version(project)? != current,
        None => false,
    };

    let extractors: Vec<Box<dyn Extractor>> = vec![
        Box::new(PythonExtractor::new()),
        Box::new(TemplateExtractor::new()),
        Box::new(JavaScriptExtractor::new()),
        Box::new(CssExtractor::new()),
    ];
    let frameworks = detect_frameworks(root);

    // The full stored file set is always the removal baseline: a file the walk no
    // longer yields (deleted, or under a newly-excluded directory like migrations)
    // must drop its stale nodes even on a force-full pass. The extraction skip is
    // separate: an empty baseline on force_full so every surviving file re-extracts.
    let stored = store.file_hashes(project)?;
    let empty: FxHashMap<String, String> = FxHashMap::default();
    let extract_baseline = if force_full { &empty } else { &stored };

    let mut stats = IndexStats::default();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut changed_paths: Vec<String> = Vec::new();
    let mut files_done: u32 = 0;

    let files_total = to_u32(paths.len());

    store.bulk_begin()?;
    let mut bulk = BulkGuard { store, armed: true };

    let batch_size = extract_batch_size();

    for chunk in paths.chunks(batch_size) {
        let outcomes: Vec<ExtractOutcome> = chunk
            .par_iter()
            .map(|path| extract_one(project, &extractors, &frameworks, extract_baseline, root, path))
            .collect();

        for outcome in outcomes {
            persist_outcome(store, project, outcome, &mut stats, &mut seen, &mut changed_paths)?;

            files_done += 1;

            on_phase(IndexPhase::Extracting { files_done, files_total: files_total.max(files_done) });
        }
    }

    stats.files_removed = match sweep {
        Sweep::Everything => remove_missing(store, project, &stored, &seen, &mut changed_paths)?,
        Sweep::Named => {
            remove_named(store, project, root, paths, &stored, &seen, &mut changed_paths)?
        }
    };

    bulk.armed = false;
    store.bulk_commit()?;

    if stats.files_indexed > 0 || stats.files_removed > 0 {
        on_phase(IndexPhase::Resolving);

        run_resolution_phase(store, project, root, &frameworks, &mut stats)?;
    }

    // Stamp the project with the binary that indexed it, so the next run with the
    // same binary trusts the content-hash skip again. Only after a successful
    // index; a failed run rolls back its writes and must re-extract next time.
    if let Some(current) = fingerprint.as_deref() {
        store.set_index_version(project, current)?;
    }

    assert!(
        changed_paths.len() as u32 <= FILE_COUNT_MAX,
        "changed paths cannot exceed the walk bound",
    );

    Ok(IndexOutcome { changed_paths, stats })
}

/// The deletion of the named paths that are recorded in the store but no longer
/// on disk, returning how many were removed.
///
/// The path-scoped counterpart of [`remove_missing`]. It considers only the
/// paths the caller named, because a burst says which files changed and nothing
/// at all about the ones it does not mention.
fn remove_named(
    store: &Store,
    project: &ProjectId,
    root: &Path,
    paths: &[PathBuf],
    existing: &FxHashMap<String, String>,
    seen: &FxHashSet<String>,
    changed_paths: &mut Vec<String>,
) -> Result<u32, IndexError> {
    let mut removed: u32 = 0;
    let mut count: u32 = 0;

    for path in paths {
        count += 1;

        assert!(count <= FILE_COUNT_MAX, "removal scan exceeded {FILE_COUNT_MAX} files");

        let Some(relative) = relative_path(root, path) else {
            continue;
        };

        // Seen means this run indexed it or found its content unchanged. Not
        // seen and not stored means it was never ours (an unsupported file, a
        // directory).
        if seen.contains(&relative) || !existing.contains_key(&relative) {
            continue;
        }

        // Not seen, and stored: a deletion, but only if the file is really
        // gone. A file that exists and merely could not be read this pass
        // (caught mid-rename, mid-write, or briefly locked) reaches here too,
        // and dropping its rows would lose them permanently: unlike the full
        // walk, which re-examines everything on its next run, a path-scoped
        // pass has already consumed the only event that named this file. Stale
        // rows are recoverable; deleted ones are not.
        if path.exists() {
            continue;
        }

        store.remove_file(project, &relative)?;
        changed_paths.push(relative);
        removed += 1;
    }

    assert!(removed <= count, "removed no more files than were named");

    Ok(removed)
}

/// The deletion of files recorded in the store that the walk no longer found on disk,
/// returning how many were removed.
fn remove_missing(
    store: &Store,
    project: &ProjectId,
    existing: &FxHashMap<String, String>,
    seen: &FxHashSet<String>,
    changed_paths: &mut Vec<String>,
) -> Result<u32, IndexError> {
    let mut removed: u32 = 0;
    let mut count: u32 = 0;

    for path in existing.keys() {
        count += 1;

        assert!(count <= FILE_COUNT_MAX, "removal scan exceeded {FILE_COUNT_MAX} files");

        if !seen.contains(path) {
            store.remove_file(project, path)?;
            changed_paths.push(path.clone());
            removed += 1;
        }
    }

    assert!(removed <= count, "removed no more files than were scanned");

    Ok(removed)
}

/// The framework resolvers that apply to the project at `root`.
fn detect_frameworks(root: &Path) -> Vec<Box<dyn FrameworkResolver>> {
    let context = FsContext::new(root);
    let mut active: Vec<Box<dyn FrameworkResolver>> = Vec::new();

    let django = DjangoResolver;

    if django.detect(&context) {
        active.push(Box::new(django));
    }

    assert!(active.len() <= 1, "only the django resolver is registered");

    active
}

/// The result of extracting one file off the store thread, the parallel half of
/// indexing. The store-touching half ([`persist_outcome`]) runs serially.
enum ExtractOutcome {
    /// A graph awaiting persistence, with the file metadata to
    /// record alongside it.
    Indexed {
        relative: String,
        content_hash: String,
        language: Language,
        size_bytes: u64,
        modified_at_ms: i64,
        source: String,
        output: ExtractionOutput,
    },
    /// The content hash matched the stored hash; left untouched.
    Unchanged { relative: String },
    /// A file of an unsupported language, unreadable, or over the parse cap.
    Ignored,
}

/// The extraction of one file into its graph without touching the store, so it can run in
/// parallel. Skips re-extraction when the content hash is unchanged; ignores
/// unsupported, unreadable, or oversized files.
fn extract_one(
    project: &ProjectId,
    extractors: &[Box<dyn Extractor>],
    frameworks: &[Box<dyn FrameworkResolver>],
    existing: &FxHashMap<String, String>,
    root: &Path,
    path: &Path,
) -> ExtractOutcome {
    assert!(!extractors.is_empty(), "at least one extractor must be available");

    let Some(language) = path.extension().and_then(|extension| extension.to_str()).and_then(Language::from_extension)
    else {
        return ExtractOutcome::Ignored;
    };

    // Minified/bundled vendor assets parse into thousands of mangled one-letter
    // "symbols" that pollute search, files, and counts (a `Chart.min.js` alone
    // yields hundreds) and never help an agent; skip them entirely.
    if is_minified(path) {
        return ExtractOutcome::Ignored;
    }

    let Some(extractor) = extractors.iter().find(|extractor| extractor.language() == language) else {
        return ExtractOutcome::Ignored;
    };

    let Ok(source) = std::fs::read_to_string(path) else {
        return ExtractOutcome::Ignored;
    };

    if source.len() > SOURCE_BYTES_MAX {
        return ExtractOutcome::Ignored;
    }

    // The same exclusion as `is_minified`, decided from the content instead of the
    // name, because a vendored bundle is routinely shipped under an ordinary one
    // (`robit/html/alpine.js`). Left in, one such file contributes more mangled
    // one-letter "symbols" than the library it sits beside contributes real ones.
    //
    // JavaScript only: minification mangles identifiers, so nothing readable
    // survives, while a minified stylesheet keeps its selector names intact and a
    // template's `class="btn-primary"` still resolves into it. Minified CSS is
    // dropped by name (`.min.css`) where that is the author's own labelling, and
    // left alone otherwise.
    if language == Language::JavaScript && is_minified_source(&source) {
        return ExtractOutcome::Ignored;
    }

    let Some(relative) = relative_path(root, path) else {
        return ExtractOutcome::Ignored;
    };

    let content_hash = hash_hex(source.as_bytes());

    if existing.get(&relative).is_some_and(|stored| stored == &content_hash) {
        return ExtractOutcome::Unchanged { relative };
    }

    // Isolate each file: a parser assert or tree-sitter edge case must skip that
    // one file, not abort the whole parallel index. The release profile is
    // panic=unwind, so this catches; the extractors create fresh parser state per
    // call (they are Sync), so a caught panic cannot corrupt a sibling file.
    let extracted = catch_unwind(AssertUnwindSafe(|| {
        let mut output = extractor.extract(project, &relative, &source);
        merge_frameworks(project, frameworks, language, &relative, &source, &mut output);

        output
    }));

    let output = match extracted {
        Ok(output) => output,
        Err(_) => {
            eprintln!("constellation: skipped {relative}: extraction panicked");

            return ExtractOutcome::Ignored;
        }
    };

    ExtractOutcome::Indexed {
        relative,
        content_hash,
        language,
        size_bytes: source.len() as u64,
        modified_at_ms: modified_ms(path),
        source,
        output,
    }
}

/// The persistence of one extraction outcome, folded into the running stats. Runs on
/// the indexing thread, serializing every store write.
fn persist_outcome(
    store: &Store,
    project: &ProjectId,
    outcome: ExtractOutcome,
    stats: &mut IndexStats,
    seen: &mut FxHashSet<String>,
    changed_paths: &mut Vec<String>,
) -> Result<(), IndexError> {
    match outcome {
        ExtractOutcome::Indexed {
            relative,
            content_hash,
            language,
            size_bytes,
            modified_at_ms,
            source,
            output,
        } => {
            let file = FileIndex {
                path: &relative,
                content_hash: &content_hash,
                language,
                size_bytes,
                modified_at_ms,
                source: &source,
            };

            store.persist_file(
                project,
                &file,
                &output.nodes,
                &output.edges,
                &output.unresolved_refs,
                &output.import_mappings,
                &output.events,
            )?;

            stats.files_indexed += 1;
            stats.nodes = stats.nodes.saturating_add(to_u32(output.nodes.len()));
            stats.edges = stats.edges.saturating_add(to_u32(output.edges.len()));
            stats.unresolved_refs =
                stats.unresolved_refs.saturating_add(to_u32(output.unresolved_refs.len()));

            changed_paths.push(relative.clone());
            seen.insert(relative);
        }
        ExtractOutcome::Unchanged { relative } => {
            stats.files_unchanged += 1;
            seen.insert(relative);
        }
        ExtractOutcome::Ignored => stats.files_skipped += 1,
    }

    Ok(())
}

/// The run of each applicable framework's extractor over the file, merging its
/// route nodes and references into `output`, linking each new node to the file.
fn merge_frameworks(
    project: &ProjectId,
    frameworks: &[Box<dyn FrameworkResolver>],
    language: Language,
    relative: &str,
    source: &str,
    output: &mut ExtractionOutput,
) {
    assert!(!relative.is_empty(), "relative path must not be empty");

    let file_id = NodeId::new(project, relative);

    for framework in frameworks {
        if !framework.languages().contains(&language) {
            continue;
        }

        let extra = framework.extract(project, relative, source);

        for node in &extra.nodes {
            let edge = Edge::new(file_id.clone(), node.id.clone(), EdgeKind::Contains)
                .with_provenance("framework");

            output.edges.push(edge);
        }

        output.nodes.extend(extra.nodes);
        output.unresolved_refs.extend(extra.references);
    }
}
