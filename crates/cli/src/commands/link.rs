//! `link`: several repositories indexed into one shared database and bound
//! together.

use std::path::Path;

use anyhow::{Result, bail};
use constellation_graph::ProjectId;
use constellation_index::{index_project_reporting, link_constellation};
use constellation_store::Store;

use crate::commands::index::index_companions;
use crate::progress;
use crate::summary::print_store_summary;
use crate::workspace::{project_name, resolve_root};

/// The `constellation link <database> <repo> [repo ...]` command indexes every
/// repository into one shared constellation database, links imports across them,
/// and prints the summary.
pub(crate) fn link_command(rest: &[String]) -> Result<()> {
    let Some((database, repositories)) = rest.split_first() else {
        bail!("usage: constellation link <database> <repo> [repo ...]");
    };

    if repositories.is_empty() {
        bail!("usage: constellation link <database> <repo> [repo ...]");
    }

    let store = Store::open(Path::new(database))?;

    for repository in repositories {
        let root = Path::new(repository);

        if !root.is_dir() {
            bail!("not a directory: {repository}");
        }

        let canonical = resolve_root(root)?;
        let root = canonical.as_path();

        let name = project_name(root);
        let project = ProjectId::new(name.as_str());

        let mut progress = progress::Progress::new(&format!("indexing {name}"));

        index_project_reporting(&store, &project, &name, root, |phase| {
            progress.on_phase(phase);
        })?;

        progress.finish();

        index_companions(&store, root)?;
    }

    link_constellation(&store)?;

    print_store_summary(&store, Path::new(database))
}
