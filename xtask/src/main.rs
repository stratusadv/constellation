//! Build automation for constellation, invoked as `cargo xtask <task>`.
//!
//! Every build, install, and check task lives here rather than in a shell
//! script, a batch file, or a make-alike. One implementation compiles and runs
//! identically on every platform a contributor uses, the compiler checks it,
//! and there is no second toolchain to install before the first build.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod artifact;
mod fixtures;
mod probe;
mod tidy;

/// The result every task returns.
///
/// An xtask failure is printed and the process exits, so nothing inspects the
/// error and a boxed one keeps the tasks free of a bespoke error enum they
/// would never match on.
pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The tasks `cargo xtask` accepts, printed when it is handed one it does not.
const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
    build [target]   compile the release binary for one target triple into
                     target/<triple>/release; target defaults to the host
    dist             build for the host and for Windows, so one checkout
                     produces both deliverables
    install [dir]    build, then place the binary on PATH; dir defaults to
                     $CONSTELLATION_INSTALL_DIR, else ~/.local/bin
    probe [tool] [json]
                     call one tool on the built binary and print its output,
                     e.g. cargo xtask probe explore '{\"query\":\"Order\"}'
                     set CONSTELLATION_DB to probe another project's graph
    fixtures survey  report what the real indexed repositories contain, so a
                     test fixture can be written to match. Reads counts and
                     shapes only, never source. Configured by fixtures.toml
    tidy             check the style ratchets, which may only fall
";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    let outcome = match arguments.first().map(String::as_str) {
        Some("build") => artifact::build_task(arguments.get(1).map(String::as_str)),
        Some("dist") => artifact::dist(),
        Some("install") => artifact::install(arguments.get(1).map(String::as_str)),
        Some("probe") => probe::probe(
            arguments.get(1).map(String::as_str),
            arguments.get(2).map(String::as_str),
        ),
        Some("fixtures") => match arguments.get(1).map(String::as_str) {
            Some("survey") => fixtures::survey(arguments.get(2).map(String::as_str)),
            _ => Err("usage: cargo xtask fixtures survey [rows]".into()),
        },
        Some("tidy") => tidy::check(),
        _ => {
            eprint!("{USAGE}");

            return ExitCode::FAILURE;
        }
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");

            ExitCode::FAILURE
        }
    }
}

/// The workspace root, resolved at compile time from this crate's location.
///
/// xtask sits one level below the root by construction, so the path holds
/// wherever the binary is launched from. Deriving it from the current directory
/// instead would make every task depend on where it was invoked.
pub fn workspace_root() -> Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    let root = manifest
        .parent()
        .ok_or("the xtask manifest directory has no parent")?;

    assert!(root.join("Cargo.toml").is_file(), "the workspace root holds a manifest");
    assert!(root.join("crates").is_dir(), "the workspace root holds crates/");

    Ok(root.to_path_buf())
}
