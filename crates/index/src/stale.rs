//! Counting how far the index has drifted from the working tree.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use constellation_graph::{Language, ProjectId};
use constellation_store::Store;
use ignore::WalkState;
use rustc_hash::FxHashSet;

use crate::IndexError;
use crate::limits::FILE_COUNT_MAX;
use crate::walk::{hash_hex, is_minified, modified_ms, relative_path, to_u32, walk_parallel};

/// The working-tree staleness for one project relative to the last index: how many
/// source files now have a newer modification time (or are new), and how many
/// indexed files have since been removed. Stat-only (never reads file contents),
/// so it is cheap enough for a status check.
#[derive(Clone, Copy, Debug, Default)]
pub struct StaleFiles {
    pub changed: u32,
    pub removed: u32,
}

/// The [`StaleFiles`] for a project, computed by walking its root and comparing each
/// source file's modification time to the stored baseline.
pub fn count_stale_files(
    store: &Store,
    project: &ProjectId,
    root: &Path,
) -> Result<StaleFiles, IndexError> {
    let stored = Arc::new(store.file_mtimes(project)?);
    let hashes = Arc::new(store.file_hashes(project)?);

    // Walk and stat in parallel: both the gitignore traversal and the per-file
    // mtime syscall (slow on Windows) dominate the stale check, and there is no
    // shared mutable index state to serialize on. Workers tally into shared
    // counters; the visit count is bounded, quitting the walk gracefully on
    // overflow rather than panicking inside a worker thread.
    let changed = Arc::new(AtomicU32::new(0));
    let visited = Arc::new(AtomicU32::new(0));
    let seen: Arc<Mutex<FxHashSet<String>>> = Arc::new(Mutex::new(FxHashSet::default()));

    // Clones the walk consumes; the originals stay live to read after run().
    let stored_walk = Arc::clone(&stored);
    let hashes_walk = Arc::clone(&hashes);
    let changed_walk = Arc::clone(&changed);
    let seen_walk = Arc::clone(&seen);
    let root_owned = root.to_path_buf();

    walk_parallel(root).run(move || {
        let stored = Arc::clone(&stored_walk);
        let hashes = Arc::clone(&hashes_walk);
        let changed = Arc::clone(&changed_walk);
        let visited = Arc::clone(&visited);
        let seen = Arc::clone(&seen_walk);
        let root = root_owned.clone();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };

            if visited.fetch_add(1, Ordering::Relaxed) >= FILE_COUNT_MAX {
                return WalkState::Quit;
            }

            if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
                return WalkState::Continue;
            }

            let path = entry.path();

            let supported = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(Language::from_extension)
                .is_some();

            if !supported {
                return WalkState::Continue;
            }

            // The index skips minified/bundled vendor assets (`is_minified`), so
            // they are never stored; the stale check must skip them too, or each,
            // forever absent from the index, counts as a phantom "changed" on
            // every status call.
            if is_minified(path) {
                return WalkState::Continue;
            }

            let Some(relative) = relative_path(&root, path) else {
                return WalkState::Continue;
            };

            // mtime is the cheap pre-filter; a bumped mtime (a checkout, a
            // formatter, a sync) is confirmed against the content hash (what
            // indexing actually keys re-extraction on), so a touched-but-unchanged
            // file is not reported stale.
            let stale = match stored.get(&relative) {
                Some(&stored_ms) if modified_ms(path) <= stored_ms => false,
                Some(_) => match std::fs::read(path) {
                    Ok(bytes) => hashes.get(&relative).map(String::as_str) != Some(hash_hex(&bytes).as_str()),
                    Err(_) => true,
                },
                None => true,
            };

            if stale {
                changed.fetch_add(1, Ordering::Relaxed);
            }

            seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(relative);

            WalkState::Continue
        })
    });

    let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let changed = changed.load(Ordering::Relaxed);

    assert!(changed <= to_u32(seen.len()), "changed files cannot exceed source files seen");

    let removed = stored.keys().filter(|path| !seen.contains(path.as_str())).count();

    Ok(StaleFiles { changed, removed: to_u32(removed) })
}
