//! `serve`: the MCP server, and the watcher that keeps its graph current.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::args::positional;
use crate::commands::supervise::{SUPERVISE_FLAG, supervise_command};
use crate::workspace::discover_database_optional;

/// The `constellation serve [database]` command serves the constellation graph to an
/// agent over MCP (stdio) and, in the background, watches every indexed project and
/// re-indexes and re-links on each change, so the graph stays current mid-session.
/// With no argument it discovers the database from the `CONSTELLATION_DB`
/// environment variable, or by walking up from the working directory for a
/// `.constellation/index.db`, so one registration serves every project.
///
/// `--supervise` serves through a proxy that replaces this process's replaceable
/// half whenever the binary is rebuilt, so a client keeps its session across an
/// install. `--worker` is that replaceable half and is passed by the proxy, never
/// by a person.
pub(crate) fn serve_command(rest: &[String]) -> Result<()> {
    if rest.iter().any(|argument| argument == SUPERVISE_FLAG) {
        return supervise_command(rest);
    }

    let database = match positional(rest) {
        Some(path) => Some(PathBuf::from(path)),
        None => discover_database_optional()?,
    };

    match database {
        Some(database) => {
            if !database.is_file() {
                bail!(
                    "no constellation database at {}; run `constellation init` in the project",
                    database.display(),
                );
            }

            constellation_mcp::serve(&database)?;
        }
        None => constellation_mcp::serve_unavailable()?,
    }

    Ok(())
}
