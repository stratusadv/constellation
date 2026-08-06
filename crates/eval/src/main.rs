#![forbid(unsafe_code)]

//! constellation-eval: does the graph answer *better*, not just faster.
//!
//! The speed benches (`extraction/benches/parse.rs`, `mcp/benches/rank.rs`,
//! `index/examples/index_time.rs`) measure how fast constellation is. Nothing
//! measured whether it was right, which made every change to `explore`'s
//! ranking unfalsifiable. This harness fixes that, and deliberately ships its
//! own limits alongside its numbers.
//!
//! Kept out of `crates/cli` so the shipped binary does not grow an eval
//! subcommand: this is a development tool, not part of the product surface.
//!
//! ```text
//! constellation-eval --config eval/configs/<repo>.toml [--benchmark <name>] [--out eval/results]
//! ```

mod benchmarks;
mod config;
mod report;
mod score;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use constellation_store::Store;

use crate::benchmarks::{BENCHMARK_NAMES, Context};
use crate::report::BenchmarkRow;

/// The default directory results are written to.
const RESULTS_DIRECTORY: &str = "eval/results";

/// The dispatch: parse the arguments, load the config and goldset, run the
/// selected benchmarks, and write both outputs.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("constellation-eval: {message}");

            ExitCode::FAILURE
        }
    }
}

/// An evaluation run.
fn run(arguments: &[String]) -> Result<(), String> {
    if arguments.iter().any(|argument| argument == "--help" || argument == "-h") {
        print_usage();

        return Ok(());
    }

    let config_path = flag_value(arguments, "--config")
        .map(PathBuf::from)
        .ok_or("pass --config <path to a toml config>; see --help")?;

    let selected = flag_value(arguments, "--benchmark");
    let output = flag_value(arguments, "--out").unwrap_or_else(|| RESULTS_DIRECTORY.to_string());

    let config = config::load_config(&config_path).map_err(|error| error.to_string())?;
    let goldset = config::load_goldset(&config, &config_path).map_err(|error| error.to_string())?;

    let database = config::resolve_relative(&config_path, &config.database);

    if !database.is_file() {
        return Err(format!(
            "no constellation database at {}; run `constellation init` in the target project",
            database.display(),
        ));
    }

    let store = Store::open(&database).map_err(|error| error.to_string())?;
    let context =
        Context { config: &config, database: database.clone(), goldset: &goldset, store: &store };

    let names: Vec<&str> = match &selected {
        Some(name) => {
            if !BENCHMARK_NAMES.contains(&name.as_str()) {
                return Err(format!(
                    "unknown benchmark {name:?}; valid benchmarks: {}",
                    BENCHMARK_NAMES.join(", "),
                ));
            }

            vec![name.as_str()]
        }
        None => BENCHMARK_NAMES.to_vec(),
    };

    let mut rows: Vec<BenchmarkRow> = Vec::new();

    for name in names {
        eprintln!("constellation-eval: running {name}");

        let produced = benchmarks::run(name, &context)
            .ok_or_else(|| format!("benchmark {name:?} is registered but has no runner"))?;

        rows.extend(produced);
    }

    write_outputs(Path::new(&output), &config.name, &rows)
}

/// The CSV and markdown written side by side, with the directory created if
/// absent.
fn write_outputs(directory: &Path, name: &str, rows: &[BenchmarkRow]) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("creating {}: {error}", directory.display()))?;

    let stem = name.replace(|character: char| !character.is_alphanumeric(), "-");

    let csv_path = directory.join(format!("{stem}.csv"));
    let markdown_path = directory.join(format!("{stem}.md"));

    std::fs::write(&csv_path, report::to_csv(rows))
        .map_err(|error| format!("writing {}: {error}", csv_path.display()))?;

    std::fs::write(&markdown_path, report::to_markdown(name, rows))
        .map_err(|error| format!("writing {}: {error}", markdown_path.display()))?;

    println!("{} rows", rows.len());
    println!("{}", csv_path.display());
    println!("{}", markdown_path.display());

    Ok(())
}

/// The value following `flag`, or `None` when the flag is absent or trails with
/// no value.
fn flag_value(arguments: &[String], flag: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == flag)?;

    arguments.get(position + 1).filter(|value| !value.starts_with("--")).cloned()
}

/// The usage text.
fn print_usage() {
    println!("constellation-eval --config <path> [--benchmark <name>] [--out <directory>]");
    println!();
    println!("Benchmarks: {}", BENCHMARK_NAMES.join(", "));
    println!("Results default to {RESULTS_DIRECTORY}/<config name>.csv and .md");
    println!();
    println!("Every report ends with a Limits section. Read it before quoting a number.");
}

#[cfg(test)]
mod tests {
    use super::flag_value;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_flag_yields_the_value_after_it() {
        let given = arguments(&["--config", "eval/configs/w.toml", "--benchmark", "search_quality"]);

        assert_eq!(flag_value(&given, "--config").as_deref(), Some("eval/configs/w.toml"));
        assert_eq!(flag_value(&given, "--benchmark").as_deref(), Some("search_quality"));
        assert_eq!(flag_value(&given, "--out"), None, "an absent flag has no value");
    }

    #[test]
    fn a_trailing_flag_with_no_value_is_not_a_value() {
        let given = arguments(&["--config"]);

        assert_eq!(flag_value(&given, "--config"), None);

        let followed = arguments(&["--config", "--benchmark", "search_quality"]);

        assert_eq!(
            flag_value(&followed, "--config"),
            None,
            "the next flag is not mistaken for this one's value",
        );
    }
}
