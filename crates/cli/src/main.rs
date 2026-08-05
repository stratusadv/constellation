#![forbid(unsafe_code)]

//! constellation CLI: index, sync, link, and serve the cross-project knowledge graph.
//!
//! This module is dispatch and nothing else. Each subcommand's work lives in
//! its own module under [`commands`], and the pieces they share live in
//! [`args`], [`workspace`], and [`summary`], so a change to one command cannot
//! reach into another.

mod args;
mod bootstrap;
mod commands;
mod hook;
mod progress;
mod summary;
mod workspace;

use anyhow::{Result, bail};
use constellation_store::Store;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub(crate) const NAME: &str = "constellation";
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The command list, printed for `help` and for an argument that cannot be one.
const USAGE: &str = "\
Usage: constellation <command> [arguments]
       constellation <path>                    index that repository

Commands:
  init [path] [--no-hooks]                     create and index .constellation/index.db
  sync [db]                                    re-index every project and re-link
  link <db> <repo>...                          index several repos into one graph, linked
  serve [db] [--supervise]                     serve the graph over MCP (stdio) and watch;
                                               --supervise survives a rebuild, no reconnect
  history [db] [--symbols]                     ingest git history, for reads over time
  flows [db] [--project id] [--depth n]        trace and rank Django execution flows
  install [--no-hooks] / uninstall             register the MCP server with your agents
  hook pre-tool-use                            the Claude Code hook entry point

Options:
  -h, --help                                   print this help
  -V, --version                                print the version

With no argument at all, an in-memory smoke check runs.";

/// The dispatch of a subcommand (`init`, `sync`, `link`, `serve`, `history`,
/// `install`, `uninstall`). A bare path argument indexes that repository; with no
/// argument, run an in-memory smoke check.
fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.split_first() {
        Some((command, _)) if is_help(command) => print_usage(),
        Some((command, _)) if is_version(command) => print_version(),
        Some((command, rest)) if command == "init" => commands::index::init_command(rest),
        Some((command, rest)) if command == "sync" => commands::sync::sync_command(rest),
        Some((command, rest)) if command == "link" => commands::link::link_command(rest),
        Some((command, rest)) if command == "serve" => commands::serve::serve_command(rest),
        Some((command, rest)) if command == "history" => commands::history::history_command(rest),
        Some((command, rest)) if command == "flows" => commands::flows::flows_command(rest),
        Some((command, rest)) if command == "hook" => hook::hook_command(rest),
        Some((command, rest)) if command == "install" => bootstrap::install(rest),
        Some((command, _)) if command == "uninstall" => bootstrap::uninstall(),

        // Before the bare-path arm, so a mistyped flag is named as one rather
        // than reported as a missing directory.
        Some((argument, _)) if argument.starts_with('-') => {
            bail!("unknown option {argument:?}. Run `{NAME} --help` for usage")
        }

        Some((root, _)) => commands::index::index_command(root),
        None => smoke_check(),
    }
}

/// Whether an argument asks for the usage text.
fn is_help(argument: &str) -> bool {
    matches!(argument, "help" | "-h" | "--help")
}

/// Whether an argument asks for the version.
fn is_version(argument: &str) -> bool {
    matches!(argument, "version" | "-V" | "--version")
}

fn print_usage() -> Result<()> {
    println!("{NAME} {VERSION}");
    println!("A cross-project knowledge graph of Django codebases, served to an agent over MCP.");
    println!();
    println!("{USAGE}");

    Ok(())
}

fn print_version() -> Result<()> {
    println!("{NAME} {VERSION}");

    Ok(())
}

/// The smoke check that verifies the binary links correctly by opening an
/// in-memory store and printing the schema version.
fn smoke_check() -> Result<()> {
    let store = Store::open_in_memory()?;
    let fingerprint = store.schema_version()?;

    assert!(
        fingerprint != 0,
        "an initialized store carries a schema fingerprint"
    );

    println!("{NAME} {VERSION}: in-memory store ready (schema {fingerprint:#010x})");
    println!("pass a repository path to index it");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_help, is_version};

    #[test]
    fn help_and_version_are_recognized_in_every_spelling() {
        for argument in ["help", "-h", "--help"] {
            assert!(is_help(argument), "{argument} asks for usage");
            assert!(!is_version(argument), "{argument} is not a version request");
        }

        for argument in ["version", "-V", "--version"] {
            assert!(is_version(argument), "{argument} asks for the version");
            assert!(!is_help(argument), "{argument} is not a help request");
        }
    }

    #[test]
    fn a_repository_path_is_neither() {
        for argument in [".", "../app", "/srv/checkout", "help_desk"] {
            assert!(!is_help(argument), "{argument} is a path to index");
            assert!(!is_version(argument), "{argument} is a path to index");
        }
    }
}
