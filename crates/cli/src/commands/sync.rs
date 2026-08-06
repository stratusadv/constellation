//! `sync`: the one-shot catch-up for a constellation with no server attached.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use constellation_index::{link_constellation, refresh_constellation};
use constellation_store::Store;

use crate::commands::history::index_history_if_enabled;
use crate::commands::index::index_companions;
use crate::summary::print_store_summary;
use crate::workspace::discover_database;

/// The `constellation sync [database]` command re-indexes every project in the
/// constellation from disk (incrementally, skipping unchanged files), re-links it,
/// and prints the summary. A one-shot catch-up: a running `serve` already does this
/// in the background on every change, so `sync` is for refreshing the graph when no
/// server is attached. Git history is refreshed too when `[history]` is enabled.
/// The database is discovered as for `serve`.
pub(crate) fn sync_command(rest: &[String]) -> Result<()> {
    let database = match rest.first() {
        Some(path) => PathBuf::from(path),
        None => discover_database()?,
    };

    if !database.is_file() {
        bail!(
            "no constellation database at {}; run `constellation init` first",
            database.display(),
        );
    }

    let store = Store::open(&database)?;

    refresh_constellation(&store)?;

    // Pick up companions or versions newly added to the config since the last
    // index, then bind them. The workspace root is the directory that holds the
    // discovered `.constellation/`.
    if let Some(workspace_root) = database.parent().and_then(Path::parent)
        && workspace_root.is_dir()
    {
        index_companions(&store, workspace_root)?;
    }

    if store.all_projects()?.len() > 1 {
        link_constellation(&store)?;
    }

    print_store_summary(&store, &database)?;

    if let Some(workspace_root) = database.parent().and_then(Path::parent) {
        index_history_if_enabled(&store, workspace_root)?;
    }

    Ok(())
}
