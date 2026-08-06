//! `cargo xtask probe`: call one tool on the freshly built binary and print what
//! an agent would see.
//!
//! The loop this exists for is editing constellation and checking the result. A
//! connected agent is the wrong instrument for that: it answers from whatever
//! worker its session holds, which may predate the edit. This drives a private
//! server over the same stdio transport a client uses, so the output is the
//! product's, not an approximation of it, and it needs no client at all.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::artifact::{host_target, release_artifact};
use crate::{Result, workspace_root};

/// The tool called when none is named: the one that says whether the index the
/// probe is about to read is even there.
const TOOL_DEFAULT: &str = "status";

/// The id of the probe's initialize, kept distinct from the call's.
const INITIALIZE_ID: u32 = 1;

/// The id of the probe's tool call.
const CALL_ID: u32 = 2;

/// The release binary probed with one tool, its text printed.
///
/// `cargo xtask probe [tool] [json-arguments]`, e.g.
/// `cargo xtask probe explore '{"query":"HarvestLoad","max_files":2}'`. With no
/// tool it calls [`TOOL_DEFAULT`]; with no arguments it passes an empty object.
///
/// The worker discovers its graph the way it always does, from the working
/// directory, which here is the constellation checkout and holds no Django
/// index. Point it at a real one with `CONSTELLATION_DB=<path to index.db>`,
/// which the child inherits.
pub fn probe(tool: Option<&str>, arguments: Option<&str>) -> Result {
    let tool = tool.unwrap_or(TOOL_DEFAULT);
    let arguments = arguments.unwrap_or("{}");

    if serde_json::from_str::<serde_json::Value>(arguments).is_err() {
        return Err(format!("the tool arguments are not JSON: {arguments}").into());
    }

    let root = workspace_root()?;
    let binary = release_artifact(&root, &host_target()?);

    if !binary.is_file() {
        return Err(format!(
            "no binary at {}; run `cargo xtask build` first",
            binary.display(),
        )
        .into());
    }

    let text = call(&binary, tool, arguments)?;

    println!("{text}");

    Ok(())
}

/// An initialize-then-call exchange against a worker started for this probe.
fn call(binary: &Path, tool: &str, arguments: &str) -> Result<String> {
    let mut child = Command::new(binary)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let mut stdin = child.stdin.take().ok_or("the server exposes no stdin")?;
    let stdout = child.stdout.take().ok_or("the server exposes no stdout")?;
    let mut reader = BufReader::new(stdout);

    let client = r#"{"name":"xtask-probe","version":"1"}"#;
    let parameters =
        format!(r#"{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{client}}}"#);

    let initialize = format!(
        r#"{{"jsonrpc":"2.0","id":{INITIALIZE_ID},"method":"initialize","params":{parameters}}}"#,
    );

    writeln!(stdin, "{initialize}")?;
    writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#)?;

    let call_parameters = format!(r#"{{"name":"{tool}","arguments":{arguments}}}"#);

    let call = format!(
        r#"{{"jsonrpc":"2.0","id":{CALL_ID},"method":"tools/call","params":{call_parameters}}}"#,
    );

    writeln!(stdin, "{call}")?;
    stdin.flush()?;

    let answer = read_answer(&mut reader, CALL_ID);

    // The transport closes on stdin, which is how the worker learns to exit; the
    // kill is only for one that ignores it.
    drop(stdin);

    let _ = child.kill();
    let _ = child.wait();

    answer
}

/// The response carrying `id`, rendered as the text a client would show, with a
/// tool error reported rather than printed as an empty result.
fn read_answer(reader: &mut impl BufRead, id: u32) -> Result<String> {
    let mut line = String::new();

    loop {
        line.clear();

        if reader.read_line(&mut line)? == 0 {
            return Err("the server closed before answering".into());
        }

        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        if message["id"] != serde_json::json!(id) {
            continue;
        }

        if let Some(error) = message["error"].as_object() {
            return Err(format!("the tool call failed: {error:?}").into());
        }

        return Ok(rendered(&message));
    }
}

/// The text blocks of a tool result, joined; the raw JSON when the result is
/// shaped some other way, so a probe never silently prints nothing.
fn rendered(message: &serde_json::Value) -> String {
    let Some(content) = message["result"]["content"].as_array() else {
        return message["result"].to_string();
    };

    let text: Vec<&str> = content.iter().filter_map(|block| block["text"].as_str()).collect();

    if text.is_empty() {
        return message["result"].to_string();
    }

    text.join("\n")
}
