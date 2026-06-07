use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The registration of `constellation serve` as an MCP server in every supported agent.
/// The registered command takes no database argument (`serve` discovers the
/// project's `.constellation/index.db` from the working directory), so one
/// registration covers every project.
pub fn install() -> Result<()> {
    install_claude_code();
    install_opencode();

    println!("Then, in each project: `constellation init`");

    Ok(())
}

/// The removal of `constellation` from every supported agent, the inverse of
/// `install`. Each project's `.constellation/` index is left untouched; only
/// the agent registrations are undone.
pub fn uninstall() -> Result<()> {
    uninstall_claude_code();
    uninstall_opencode();

    println!("Project indexes are kept; delete each `.constellation/` to remove them");

    Ok(())
}

/// The absolute path to the running `constellation` binary, registered with each
/// agent so launching the server never depends on the bare name being on PATH
/// (which fails intermittently, while a rebuild swaps the file, or in a shell
/// without the install directory on PATH). Falls back to the bare name only when
/// the path cannot be determined.
fn server_command() -> String {
    match std::env::current_exe() {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => "constellation".to_string(),
    }
}

/// The registration of Claude Code via its own CLI (user scope), so its config stays valid.
fn install_claude_code() {
    let executable = server_command();

    let status = claude_command(&[
        "mcp", "add", "--scope", "user", "constellation", "--", executable.as_str(), "serve",
    ])
    .status();

    match status {
        Ok(status) if status.success() => {
            println!("Claude Code: registered constellation (user scope)");
        }
        _ => {
            println!(
                "Claude Code: add manually -> \
                 claude mcp add --scope user constellation -- {executable} serve",
            );
        }
    }
}

/// The removal of the Claude Code registration via its own CLI (user scope).
fn uninstall_claude_code() {
    let status = claude_command(&["mcp", "remove", "--scope", "user", "constellation"]).status();

    match status {
        Ok(status) if status.success() => {
            println!("Claude Code: removed constellation (user scope)");
        }
        _ => {
            println!(
                "Claude Code: remove manually -> \
                 claude mcp remove --scope user constellation",
            );
        }
    }
}

/// A `claude` CLI invocation. On Windows the `claude` entry point is an npm
/// shim (`claude.cmd`) that `Command::new` cannot launch directly, so it is run
/// through `cmd /c`; on other platforms `claude` is invoked directly.
fn claude_command(arguments: &[&str]) -> Command {
    assert!(!arguments.is_empty(), "a claude invocation needs arguments");

    #[cfg(windows)]
    let mut command = {
        let mut shim = Command::new("cmd");
        shim.arg("/c").arg("claude");
        shim
    };

    #[cfg(not(windows))]
    let mut command = Command::new("claude");

    command.args(arguments);

    command
}

/// The OpenCode registration, best-effort: a config that cannot be parsed (for
/// example one with comments, which this strict reader will not rewrite without
/// losing them) or written falls back to printed manual instructions, so a failure
/// here never aborts the rest of `install`.
fn install_opencode() {
    match register_opencode() {
        Ok(path) => println!("OpenCode: registered constellation in {}", path.display()),
        Err(error) => {
            eprintln!("OpenCode: could not update the config automatically: {error}");

            print_opencode_manual();
        }
    }
}

/// The merge of a `constellation` entry into OpenCode's global config, preserving
/// existing settings. OpenCode deep-merges its global files (`config.json`,
/// `opencode.json`, `opencode.jsonc`) with project config, so this single
/// `opencode.json` entry registers constellation for every project. Returns the
/// config path on success.
fn register_opencode() -> Result<PathBuf> {
    let path = opencode_config_path()?;
    let mut config = read_config(&path)?;

    let root = config
        .as_object_mut()
        .context("opencode config root is not a JSON object")?;

    root.entry("$schema")
        .or_insert_with(|| json!("https://opencode.ai/config.json"));

    let servers = root
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("opencode `mcp` is not a JSON object")?;

    servers.insert(
        "constellation".to_string(),
        json!({
            "type": "local",
            "command": [server_command(), "serve"],
            "enabled": true
        }),
    );

    write_config(&path, &config)?;

    Ok(path)
}

/// The manual `mcp` entry to merge into OpenCode's config, printed when automatic
/// registration cannot proceed.
fn print_opencode_manual() {
    let executable = server_command();

    println!("OpenCode: add this under \"mcp\" in your opencode.json:");
    println!(
        "  \"constellation\": {{ \"type\": \"local\", \"command\": [{executable:?}, \"serve\"], \"enabled\": true }}",
    );
}

/// The OpenCode removal, best-effort and the inverse of [`install_opencode`].
fn uninstall_opencode() {
    match deregister_opencode() {
        Ok(Some(path)) => println!("OpenCode: removed constellation from {}", path.display()),
        Ok(None) => println!("OpenCode: constellation was not registered; nothing to remove"),
        Err(error) => {
            eprintln!("OpenCode: could not update the config automatically: {error}");

            println!("OpenCode: remove the \"constellation\" entry under \"mcp\" by hand");
        }
    }
}

/// The removal of the `constellation` entry from OpenCode's global config,
/// preserving other settings. Returns the config path when an entry was removed,
/// `None` when there was nothing to remove (no config file, or no entry).
fn deregister_opencode() -> Result<Option<PathBuf>> {
    let path = opencode_config_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let mut config = read_config(&path)?;

    let removed = config
        .as_object_mut()
        .and_then(|root| root.get_mut("mcp"))
        .and_then(|servers| servers.as_object_mut())
        .is_some_and(|servers| servers.remove("constellation").is_some());

    if !removed {
        return Ok(None);
    }

    write_config(&path, &config)?;

    Ok(Some(path))
}

/// OpenCode's global `opencode.json`. constellation writes to `opencode.json`
/// specifically: it is strict JSON that round-trips without losing comments (unlike
/// `opencode.jsonc`), and OpenCode merges it with its other global files anyway.
fn opencode_config_path() -> Result<PathBuf> {
    Ok(opencode_config_dir()?.join("opencode.json"))
}

/// OpenCode's global config directory, matching its `xdg-basedir` resolution:
/// `$XDG_CONFIG_HOME/opencode` when that variable is set, else
/// `<home>/.config/opencode` (the fallback on every OS, Windows included).
fn opencode_config_dir() -> Result<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok().filter(|value| !value.is_empty());
    let home = dirs::home_dir().context("could not determine the home directory")?;

    Ok(resolve_opencode_dir(xdg.as_deref(), &home))
}

/// The OpenCode config directory from an `XDG_CONFIG_HOME` value and the home
/// directory, separated from the environment so it can be tested directly.
fn resolve_opencode_dir(xdg_config_home: Option<&str>, home: &Path) -> PathBuf {
    match xdg_config_home {
        Some(xdg) => PathBuf::from(xdg).join("opencode"),
        None => home.join(".config").join("opencode"),
    }
}

/// The JSON config read from a file, returning an empty object when it does not exist.
fn read_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }

    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// A JSON config file written to disk, creating parent directories as needed.
fn write_config(path: &Path, config: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let text = serde_json::to_string_pretty(config)?;

    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        claude_command, opencode_config_path, read_config, resolve_opencode_dir, server_command,
        write_config,
    };

    use std::path::Path;

    use serde_json::json;

    #[test]
    fn resolve_opencode_dir_prefers_xdg_then_falls_back_to_home() {
        let home = Path::new("/home/dev");

        assert_eq!(
            resolve_opencode_dir(Some("/custom/cfg"), home),
            Path::new("/custom/cfg").join("opencode"),
            "XDG_CONFIG_HOME wins when set, matching OpenCode's xdg-basedir resolution",
        );
        assert_eq!(
            resolve_opencode_dir(None, home),
            home.join(".config").join("opencode"),
            "the home .config directory is the fallback on every OS",
        );
    }

    #[test]
    fn claude_command_forwards_its_arguments() {
        let command = claude_command(&["mcp", "remove", "--scope", "user", "constellation"]);

        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert!(
            arguments.ends_with(&[
                "mcp".to_string(),
                "remove".to_string(),
                "--scope".to_string(),
                "user".to_string(),
                "constellation".to_string(),
            ]),
            "the passed arguments are forwarded in order, got {arguments:?}",
        );

        let program = command.get_program().to_string_lossy().into_owned();

        #[cfg(windows)]
        assert_eq!(program, "cmd", "the npm shim is launched through cmd on Windows");
        #[cfg(not(windows))]
        assert_eq!(program, "claude", "the claude entry point is launched directly off Windows");
    }

    #[test]
    #[should_panic(expected = "needs arguments")]
    fn claude_command_rejects_empty_arguments() {
        let _ = claude_command(&[]);
    }

    #[test]
    fn server_command_is_never_empty() {
        assert!(!server_command().is_empty(), "the registered server command always resolves to a value");
    }

    #[test]
    fn opencode_config_path_points_at_the_global_config_file() {
        let path = opencode_config_path().unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("opencode.json"),
            "the config file is opencode.json",
        );
        assert_eq!(
            path.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str()),
            Some("opencode"),
            "it lives under the opencode config directory",
        );
    }

    #[test]
    fn read_config_treats_a_missing_file_as_an_empty_object() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent.json");

        assert_eq!(read_config(&missing).unwrap(), json!({}), "an absent config reads as an empty object");
    }

    #[test]
    fn write_config_then_read_config_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("opencode.json");

        let config = json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": { "constellation": { "type": "local", "enabled": true } }
        });

        write_config(&path, &config).unwrap();

        assert_eq!(read_config(&path).unwrap(), config, "the written config reads back identically");
    }
}
