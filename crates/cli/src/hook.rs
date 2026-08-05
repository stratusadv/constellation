//! The agent hook: graph context injected into a file-search tool call.
//!
//! An instruction file can only ask an agent to prefer the graph over grep.
//! This does it mechanically: a `PreToolUse` hook on `Grep`, `Glob`, `Read`,
//! and `Bash`-with-`rg` reads the pattern the agent was about to search for,
//! looks it up in the graph, and hands back what the graph knows that a text
//! search cannot.
//!
//! # Hard constraints
//!
//! These are the difference between a useful hook and one that gets uninstalled
//! on day two, so they are enforced rather than intended.
//!
//! 1. **Always exit 0.** Any error, missing index, malformed input, or blown
//!    deadline emits nothing and succeeds. The hook must never block or fail a
//!    tool call. Every fallible step here degrades to "no output".
//! 2. **Budget the latency.** [`HOOK_BUDGET_MS`] is a self-imposed deadline
//!    checked between steps. Opening SQLite and running a bounded search is
//!    single-digit milliseconds on a warm page cache; when it is not (a cold
//!    open on a large index), the deadline fires and the hook stays silent
//!    rather than making every search wait.
//! 3. **Bound the output.** [`HOOK_CONTEXT_BYTES_MAX`] caps the injected text.
//!    It goes into every matching tool call, so a verbose hook is worse than no
//!    hook.
//! 4. **Removable.** Registered per project, by `constellation init` and by a
//!    `constellation install` run from inside an indexed project; skipped by
//!    `--no-hooks` on either. Removed by deleting the project's
//!    `.claude/settings.local.json` entry, and `constellation uninstall` clears
//!    the legacy user-scope one an earlier version wrote. On by default: a rule
//!    the user has to find and enable does not stop the failure this exists to
//!    stop. Rules 1 to 3 are what earn that default.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use constellation_graph::{EdgeKind, Node, is_covering_ref};
use constellation_mcp::truncate_at_boundary;
use constellation_store::Store;
use serde_json::Value;

/// The wall-clock budget for one hook invocation. Past this the hook emits
/// nothing, so a slow index degrades the hook rather than the agent.
const HOOK_BUDGET_MS: u64 = 150;

/// The byte cap on the injected context.
const HOOK_CONTEXT_BYTES_MAX: usize = 2_000;

/// The symbols one hook invocation reports on.
const HOOK_SYMBOLS_MAX: usize = 4;

/// The flows one hook invocation names per symbol.
const HOOK_FLOWS_MAX: u32 = 2;

/// The fail-fast bound on bytes read from stdin, so a malformed or hostile hook
/// payload cannot make the hook allocate without limit.
const HOOK_STDIN_BYTES_MAX: u64 = 1_048_576;

/// The fail-fast bound on tokens scanned while extracting a pattern from a shell
/// command line.
const COMMAND_TOKENS_MAX: usize = 512;

/// The shortest pattern worth looking up. One or two characters match half the
/// codebase, so the lookup would be noise.
const PATTERN_CHARS_MIN: usize = 3;

/// The `rg` and `grep` flags whose value is the *next* token, which must
/// therefore be skipped rather than mistaken for the pattern. Getting this wrong
/// is the classic bug in this kind of extraction: `rg -t py Foo` would otherwise
/// report `py` as the search pattern.
const VALUE_TAKING_FLAGS: &[&str] = &[
    "--after-context",
    "--before-context",
    "--color",
    "--colors",
    "--context",
    "--context-separator",
    "--file",
    "--glob",
    "--iglob",
    "--ignore-file",
    "--max-columns",
    "--max-count",
    "--max-depth",
    "--max-filesize",
    "--path-separator",
    "--pre",
    "--regexp",
    "--replace",
    "--sort",
    "--sortr",
    "--threads",
    "--type",
    "--type-add",
    "--type-not",
    "-A",
    "-B",
    "-C",
    "-E",
    "-M",
    "-T",
    "-e",
    "-f",
    "-g",
    "-j",
    "-m",
    "-r",
    "-t",
];

/// The `constellation hook <event>` dispatch. Unknown events succeed silently,
/// so a future hook wired into an older binary is a no-op rather than an error.
pub fn hook_command(rest: &[String]) -> Result<()> {
    let event = rest.first().map(String::as_str).unwrap_or("pre-tool-use");

    if event != "pre-tool-use" {
        return Ok(());
    }

    let deadline = Instant::now() + Duration::from_millis(HOOK_BUDGET_MS);

    if let Some(context) = enrich(deadline) {
        print!("{}", hook_output(&context));
    }

    Ok(())
}

/// The graph context for the tool call described on stdin, or `None` whenever
/// anything at all prevents producing one: unreadable input, no pattern, no
/// index, no match, or the deadline.
fn enrich(deadline: Instant) -> Option<String> {
    let payload = read_stdin()?;
    let request: Value = serde_json::from_str(&payload).ok()?;

    let tool = request.get("tool_name")?.as_str()?;
    let input = request.get("tool_input")?;
    let pattern = extract_pattern(tool, input)?;

    if pattern.chars().count() < PATTERN_CHARS_MIN {
        return None;
    }

    if Instant::now() >= deadline {
        return None;
    }

    let working_directory = request
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;

    let database = discover_database(&working_directory)?;
    let store = Store::open(&database).ok()?;

    render_context(&store, &pattern, deadline)
}

/// The whole of stdin, bounded. `None` when stdin is closed, unreadable, or
/// empty.
fn read_stdin() -> Option<String> {
    let mut payload = String::new();

    std::io::stdin()
        .take(HOOK_STDIN_BYTES_MAX)
        .read_to_string(&mut payload)
        .ok()?;

    if payload.trim().is_empty() {
        return None;
    }

    Some(payload)
}

/// The search pattern a tool call is about to run, per tool. `None` when the
/// tool is not one the hook enriches, or carries no usable pattern.
fn extract_pattern(tool: &str, input: &Value) -> Option<String> {
    let field = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_string);

    match tool {
        "Grep" | "Glob" => field("pattern"),
        "Read" => field("file_path").map(|path| basename(&path).to_string()),
        "Bash" => field("command").as_deref().and_then(command_pattern),
        _ => None,
    }
}

/// The final path segment of a path, which is the useful lookup key for a
/// `Read` (the graph indexes symbols, not absolute paths).
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The search pattern inside a shell command line that runs `rg` or `grep`, or
/// `None` when the command is neither. Flags are skipped, and the flags that
/// consume the following token skip it too, so `rg -t py --glob '*.py' Foo`
/// yields `Foo`.
pub fn command_pattern(command: &str) -> Option<String> {
    let tokens: Vec<&str> = command.split_whitespace().take(COMMAND_TOKENS_MAX).collect();

    assert!(tokens.len() <= COMMAND_TOKENS_MAX, "command scanning stays bounded");

    let start = tokens
        .iter()
        .position(|token| matches!(trim_path(token), "rg" | "grep" | "egrep" | "fgrep" | "ripgrep"))?;

    let mut index = start + 1;
    let mut scanned: usize = 0;

    while index < tokens.len() {
        scanned += 1;

        assert!(scanned <= COMMAND_TOKENS_MAX, "flag scanning stays bounded");

        let token = tokens[index];

        // `-e PATTERN` and `--regexp=PATTERN` name the pattern explicitly, which
        // beats positional inference: with `-e` present the first bare token is
        // a path, not the pattern.
        if let Some(value) = token.strip_prefix("--regexp=") {
            return unquote(value);
        }

        if token == "-e" || token == "--regexp" {
            return tokens.get(index + 1).copied().and_then(unquote);
        }

        if VALUE_TAKING_FLAGS.contains(&token) {
            index += 2;

            continue;
        }

        if token.starts_with('-') {
            index += 1;

            continue;
        }

        return unquote(token);
    }

    None
}

/// A token with one layer of surrounding quotes removed, or `None` when nothing
/// usable remains.
fn unquote(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(['"', '\'']);

    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A command token reduced to its program name, so `/usr/bin/rg` and `rg.exe`
/// both register as ripgrep.
fn trim_path(token: &str) -> &str {
    let base = token.rsplit(['/', '\\']).next().unwrap_or(token);

    base.strip_suffix(".exe").unwrap_or(base)
}

/// The nearest `.constellation/index.db` at or above `start`, or the
/// `CONSTELLATION_DB` override. Duplicated from the main binary's discovery
/// deliberately: the hook must never inherit a failure path that can error.
fn discover_database(start: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CONSTELLATION_DB") {
        let path = PathBuf::from(path);

        return path.is_file().then_some(path);
    }

    let mut directory = start.to_path_buf();
    let mut depth: u32 = 0;

    loop {
        depth += 1;

        assert!(depth <= 4_096, "directory walk stays bounded");

        let candidate = directory.join(".constellation").join("index.db");

        if candidate.is_file() {
            return Some(candidate);
        }

        if !directory.pop() {
            return None;
        }
    }
}

/// The rendered context for one pattern: the matching symbols with their
/// location, fan-in, test coverage, and flow membership. `None` when nothing
/// matched or the deadline fired mid-render.
fn render_context(store: &Store, pattern: &str, deadline: Instant) -> Option<String> {
    let nodes = store.search_nodes(pattern, 32).ok()?;

    let mut ranked: Vec<Node> = nodes.into_iter().filter(is_reportable).collect();

    ranked.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    ranked.truncate(HOOK_SYMBOLS_MAX);

    if ranked.is_empty() {
        return None;
    }

    let mut out = format!(
        "constellation already indexes {pattern:?}. Prefer its `explore` tool \
         over reading these by hand:\n",
    );

    for node in &ranked {
        if Instant::now() >= deadline {
            break;
        }

        out.push_str(&describe(store, node));

        if out.len() >= HOOK_CONTEXT_BYTES_MAX {
            break;
        }
    }

    Some(truncate_at_boundary(&out, HOOK_CONTEXT_BYTES_MAX).into_owned())
}

/// Whether a matched node is worth reporting: a definition, not an import row or
/// a local variable that happens to share the name.
fn is_reportable(node: &Node) -> bool {
    use constellation_graph::NodeKind;

    matches!(
        node.kind,
        NodeKind::Class
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Model
            | NodeKind::Route
            | NodeKind::View
    )
}

/// A symbol's line of context: where it is, how many things reference it,
/// whether a test covers it, and the most critical flow it sits in.
fn describe(store: &Store, node: &Node) -> String {
    let callers = store.callers(&node.id).unwrap_or_default();

    let referencing =
        callers.iter().filter(|(kind, _)| *kind != EdgeKind::Contains).count();

    let covered = callers.iter().any(|(kind, caller)| is_covering_ref(*kind, &caller.file_path));
    let coverage = if covered { "" } else { ", NO covering tests" };

    let flow = store
        .flows_for_nodes(std::slice::from_ref(&node.id.as_str().to_string()), HOOK_FLOWS_MAX)
        .ok()
        .and_then(|flows| flows.into_iter().next())
        .map(|flow| format!(", in {:?} flow (criticality {:.2})", flow.name, flow.criticality))
        .unwrap_or_default();

    format!(
        "  {} {} ({}:{}) - {referencing} references{coverage}{flow}\n",
        node.kind.as_str(),
        node.name,
        node.file_path,
        node.span.start_line,
    )
}

/// The hook's stdout contract: a `PreToolUse` response carrying the additional
/// context, which the agent sees alongside the tool result.
fn hook_output(context: &str) -> String {
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context,
        }
    });

    payload.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        HOOK_CONTEXT_BYTES_MAX, basename, command_pattern, extract_pattern, hook_output, trim_path,
        truncate_at_boundary,
    };

    use serde_json::json;

    #[test]
    fn value_taking_flags_do_not_become_the_pattern() {
        let cases = [
            ("rg Foo", "Foo"),
            ("rg -n Foo", "Foo"),
            ("rg -t py Foo", "Foo"),
            ("rg --type py Foo", "Foo"),
            ("rg -g '*.py' Foo", "Foo"),
            ("rg --glob '*.py' Foo src/", "Foo"),
            ("rg -A 3 -B 3 Foo", "Foo"),
            ("rg --max-count 2 Foo", "Foo"),
            ("grep -rn Foo src/", "Foo"),
            ("/usr/bin/rg -i Foo", "Foo"),
            ("rg.exe -i Foo", "Foo"),
            ("cd src && rg -l Foo", "Foo"),
        ];

        for (command, expected) in cases {
            assert_eq!(
                command_pattern(command).as_deref(),
                Some(expected),
                "extracting from {command:?}",
            );
        }
    }

    #[test]
    fn an_explicit_pattern_flag_beats_the_first_bare_token() {
        assert_eq!(command_pattern("rg -e Foo src/").as_deref(), Some("Foo"));
        assert_eq!(command_pattern("rg --regexp Foo src/").as_deref(), Some("Foo"));
        assert_eq!(command_pattern("rg --regexp=Foo src/").as_deref(), Some("Foo"));
    }

    #[test]
    fn a_command_that_runs_no_search_tool_yields_nothing() {
        assert_eq!(command_pattern("cargo test"), None);
        assert_eq!(command_pattern("ls -la"), None);
        assert_eq!(command_pattern("rg"), None, "a bare invocation names no pattern");
        assert_eq!(command_pattern("rg -n"), None, "flags alone name no pattern");
    }

    #[test]
    fn each_tool_reads_its_own_pattern_field() {
        assert_eq!(
            extract_pattern("Grep", &json!({ "pattern": "Order" })).as_deref(),
            Some("Order"),
        );
        assert_eq!(
            extract_pattern("Glob", &json!({ "pattern": "**/models.py" })).as_deref(),
            Some("**/models.py"),
        );
        assert_eq!(
            extract_pattern("Read", &json!({ "file_path": "/repo/app/models.py" })).as_deref(),
            Some("models.py"),
            "a Read looks up its basename, since the graph indexes symbols not paths",
        );
        assert_eq!(
            extract_pattern("Bash", &json!({ "command": "rg -n Order" })).as_deref(),
            Some("Order"),
        );
        assert_eq!(extract_pattern("Write", &json!({ "pattern": "Order" })), None, "not enriched");
    }

    #[test]
    fn trim_path_reduces_a_program_to_its_name() {
        assert_eq!(trim_path("/usr/bin/rg"), "rg");
        assert_eq!(trim_path("C:\\tools\\rg.exe"), "rg");
        assert_eq!(trim_path("rg"), "rg");
    }

    #[test]
    fn basename_takes_the_final_segment() {
        assert_eq!(basename("/repo/app/models.py"), "models.py");
        assert_eq!(basename("app\\models.py"), "models.py");
        assert_eq!(basename("models.py"), "models.py");
    }

    #[test]
    fn truncation_respects_the_byte_cap_on_a_char_boundary() {
        let text = "e\u{301}".repeat(4_000);
        let cut = truncate_at_boundary(&text, HOOK_CONTEXT_BYTES_MAX);
        let body = cut.strip_suffix(constellation_mcp::ELLIPSIS).unwrap_or(&cut);

        assert!(cut.len() <= HOOK_CONTEXT_BYTES_MAX, "the cap is respected, marker included");
        assert!(text.starts_with(body), "truncation only removes a suffix");
        assert!(cut.ends_with(constellation_mcp::ELLIPSIS), "and a cut blurb says it was cut");
    }

    #[test]
    fn hook_output_carries_the_pre_tool_use_contract() {
        let output = hook_output("context");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(parsed["hookSpecificOutput"]["additionalContext"], "context");
    }
}
