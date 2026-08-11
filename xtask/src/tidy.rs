//! The style ratchets: counts that must fall and may never rise.
//!
//! Three rules constellation is written against are not met by the tree today.
//! Gating on the rule itself would make CI permanently red, and dropping the
//! rule would make it a lie, so each is gated on its current count instead.
//! Fixing an offender lowers the number; adding one fails the build.
//!
//! The check is deliberately strict in both directions. A count that comes in
//! under its ratchet fails too, with the new number to write down, because a
//! ratchet nobody tightens is a ceiling that quietly stops meaning anything.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Result, workspace_root};

/// The column limit both style guides constellation is written against set.
const LINE_COLUMNS_MAX: usize = 100;

/// The length past which a module has stopped being one unit of code and has
/// become somewhere things are put. Nothing in the graph, resolution, or
/// linking layers approaches it; the modules over it are the ones grown by
/// accretion.
const MODULE_LINES_MAX: usize = 1_000;

/// The number of lines currently wider than [`LINE_COLUMNS_MAX`].
const LINES_OVER_COLUMNS_RATCHET: usize = 423;

/// The number of modules currently longer than [`MODULE_LINES_MAX`].
const MODULES_OVER_LINES_RATCHET: usize = 5;

/// The hard limit TigerStyle sets on one function's length.
const FUNCTION_LINES_MAX: usize = 70;

/// The number of functions currently longer than [`FUNCTION_LINES_MAX`].
///
/// The rule was written down and never measured, so the tree drifted well past
/// it. Splitting all of them at once is a large mechanical change across the
/// parsers, the resolver, and the renderers, with real risk and no behavioural
/// benefit, so it is recorded here instead: a new long function fails the build,
/// and every one paid off lowers the number.
const FUNCTIONS_OVER_LINES_RATCHET: usize = 17;

/// The directories walked for Rust sources.
const SOURCE_ROOTS: [&str; 2] = ["crates", "xtask"];

/// A cap on how many directory entries one walk visits. The workspace is three
/// orders of magnitude below it, so reaching it means the walk escaped the
/// source roots.
const WALK_ENTRIES_MAX: u32 = 100_000;

/// The offenders a failing ratchet names before it stops listing.
const REPORT_ENTRIES_MAX: usize = 20;

/// The ratchets measured against the tree and compared to their recorded counts.
pub fn check() -> Result {
    let root = workspace_root()?;
    let sources = rust_sources(&root)?;

    let mut wide_lines: Vec<(String, usize)> = Vec::new();
    let mut long_modules: Vec<(String, usize)> = Vec::new();
    let mut long_functions: Vec<(String, usize)> = Vec::new();

    for source in &sources {
        let text = fs::read_to_string(source)?;
        let name = relative_name(&root, source);

        let over_columns = text
            .lines()
            .filter(|line| line.chars().count() > LINE_COLUMNS_MAX)
            .count();

        if over_columns > 0 {
            wide_lines.push((name.clone(), over_columns));
        }

        let line_count = text.lines().count();

        if line_count > MODULE_LINES_MAX {
            long_modules.push((name.clone(), line_count));
        }

        for (function, length) in long_functions_in(&text) {
            long_functions.push((format!("{name}::{function}"), length));
        }
    }

    let wide_total: usize = wide_lines.iter().map(|(_, count)| *count).sum();

    let stale = enforce(
        Ratchet {
            label: "lines over 100 columns",
            constant: "LINES_OVER_COLUMNS_RATCHET",
            measured: wide_total,
            recorded: LINES_OVER_COLUMNS_RATCHET,
        },
        &wide_lines,
    ) + enforce(
        Ratchet {
            label: "modules over 1000 lines",
            constant: "MODULES_OVER_LINES_RATCHET",
            measured: long_modules.len(),
            recorded: MODULES_OVER_LINES_RATCHET,
        },
        &long_modules,
    ) + enforce(
        Ratchet {
            label: "functions over 70 lines",
            constant: "FUNCTIONS_OVER_LINES_RATCHET",
            measured: long_functions.len(),
            recorded: FUNCTIONS_OVER_LINES_RATCHET,
        },
        &long_functions,
    );

    if stale > 0 {
        return Err(format!("{stale} ratchet(s) no longer match the tree").into());
    }

    Ok(())
}

/// A measured count and the count recorded for it in this module.
struct Ratchet<'text> {
    label: &'text str,
    constant: &'text str,
    measured: usize,
    recorded: usize,
}

/// The number of ratchets that failed, which is one when this one did.
fn enforce(ratchet: Ratchet<'_>, offenders: &[(String, usize)]) -> u32 {
    let Ratchet { label, constant, measured, recorded } = ratchet;

    if measured == recorded {
        println!("tidy: {label}: {measured}");

        return 0;
    }

    if measured < recorded {
        println!("tidy: {label}: {measured}, down from {recorded}");
        println!("      lower {constant} to {measured} so the ground gained is held");

        return 1;
    }

    println!("tidy: {label}: {measured}, up from {recorded}");
    report(offenders);

    1
}

/// The worst offenders named, most first, so a failure points somewhere.
fn report(offenders: &[(String, usize)]) {
    let mut ranked: Vec<&(String, usize)> = offenders.iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    for (name, count) in ranked.iter().take(REPORT_ENTRIES_MAX) {
        println!("      {count:>5}  {name}");
    }

    if ranked.len() > REPORT_ENTRIES_MAX {
        println!("      and {} more", ranked.len() - REPORT_ENTRIES_MAX);
    }
}

/// The `(name, line count)` of every function in `text` longer than
/// [`FUNCTION_LINES_MAX`].
///
/// Brace counting, not parsing. It is enough because the thing being measured
/// is shape rather than meaning, and because a ratchet only has to be
/// consistent with itself: a count that is a little off in the same way on
/// every run still refuses to rise. Braces inside strings, comments, and char
/// literals would confuse it, so lines whose brace balance comes from those are
/// the known imprecision, and the one case that matters (a function body that
/// grows) moves the count regardless.
fn long_functions_in(text: &str) -> Vec<(String, usize)> {
    let mut long: Vec<(String, usize)> = Vec::new();
    let mut open: Option<(String, usize, i32)> = None;

    for (number, line) in text.lines().enumerate() {
        let balance = line.matches('{').count() as i32 - line.matches('}').count() as i32;

        if let Some((name, start, depth)) = open.take() {
            let depth = depth + balance;

            if depth > 0 {
                open = Some((name, start, depth));

                continue;
            }

            let length = number - start + 1;

            if length > FUNCTION_LINES_MAX {
                long.push((name, length));
            }

            continue;
        }

        let Some(name) = function_name(line) else {
            continue;
        };

        // A signature spanning several lines opens no block on its own line, so
        // the body starts counting from the signature either way.
        if balance > 0 {
            open = Some((name.to_string(), number, balance));
        } else if !line.trim_end().ends_with(';') {
            open = Some((name.to_string(), number, 0));
        }
    }

    long
}

/// The name of the function a line declares, or `None` when it declares none.
fn function_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();

    let rest = trimmed
        .strip_prefix("pub fn ")
        .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
        .or_else(|| trimmed.strip_prefix("pub(super) fn "))
        .or_else(|| trimmed.strip_prefix("fn "))
        .or_else(|| trimmed.strip_prefix("async fn "))
        .or_else(|| trimmed.strip_prefix("pub async fn "))?;

    let end = rest.find(['(', '<', ' ']).unwrap_or(rest.len());

    (end > 0).then(|| &rest[..end])
}

/// The Rust sources under the workspace's source roots, sorted, so a report
/// reads the same on every machine.
fn rust_sources(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending: Vec<PathBuf> = SOURCE_ROOTS.iter().map(|name| root.join(name)).collect();
    let mut sources: Vec<PathBuf> = Vec::new();
    let mut examined: u32 = 0;

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            examined += 1;

            assert!(examined <= WALK_ENTRIES_MAX, "the source walk stays inside the workspace");

            let path = entry?.path();

            if path.is_dir() {
                pending.push(path);
                continue;
            }

            if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }

    assert!(!sources.is_empty(), "the workspace always holds rust sources");

    sources.sort();

    Ok(sources)
}

/// A source path written relative to the workspace root, with forward slashes
/// so a Windows run and a Linux run produce the same report.
fn relative_name(root: &Path, source: &Path) -> String {
    let relative = source.strip_prefix(root).unwrap_or(source);

    relative.to_string_lossy().replace('\\', "/")
}
