use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::commands::supervise::SUPERVISE_FLAG;

/// The tool names the `PreToolUse` hook enriches, as the regular expression
/// Claude Code matches a tool call against.
const HOOK_MATCHER: &str = "Grep|Glob|Read|Bash";

/// The seconds Claude Code waits for the hook before abandoning it. The hook
/// budgets itself far tighter; this is only the outer guard.
const HOOK_TIMEOUT_SECS: u64 = 5;

/// The OpenCode plugin source, embedded so an installed binary writes it without
/// needing the repository it was built from.
const OPENCODE_PLUGIN: &str = include_str!("../../../assets/opencode/constellation.ts");

/// The first-line marker identifying a plugin file constellation wrote. Matched
/// without its version, so an install upgrades a file an older version wrote
/// while still leaving a hand-written plugin of the same name alone.
const OPENCODE_PLUGIN_MARKER: &str = "// constellation-plugin v";

/// The registration of `constellation serve` as an MCP server with every agent
/// that needs it (Claude Code, Codex, and OpenCode; Grok Build discovers a
/// configured server on its own). The registered command takes no database
/// argument (`serve` discovers the project's `.constellation/index.db` from the
/// working directory), so one registration covers every project.
///
/// Registered with `--supervise`, so the client's process outlives the worker it
/// proxies and a `cargo xtask install` reaches a running session without anyone
/// reconnecting. The flag costs a session nothing when constellation is not being
/// worked on: the supervisor is a pipe until the binary underneath it changes.
///
/// Deliberately nothing per-tool at user scope. Claude Code matches a hook on
/// the tool name alone and OpenCode loads a global plugin for every session, so
/// either registered for the whole machine would fire in every project there,
/// including the ones constellation has never indexed, where it can only spawn
/// and exit. [`install_project_hook`] and [`install_project_plugin`] register
/// them per project instead, so a project without a `.constellation/` directory
/// never pays for one. What every client gets regardless is the MCP server's own
/// `instructions`, which carry the same "ask the graph before you grep" message
/// over the protocol they all speak rather than through one client's settings.
///
/// Both are registered for the directory this ran inside, when that directory
/// holds an index, and skipped by `--no-hooks`. `init` is what normally writes
/// them; this covers the project indexed before the binary knew how, which
/// otherwise needs a re-index to gain them.
pub fn install(rest: &[String]) -> Result<()> {
    let hooks = !rest.iter().any(|argument| argument == "--no-hooks");

    install_claude_code();
    install_codex();
    install_opencode();

    println!("Grok Build: discovers constellation automatically; no registration needed");

    install_hooks_here(hooks);

    println!("Then, in each project: `constellation init`");

    Ok(())
}

/// The Claude Code hook and the OpenCode plugin registered for the working
/// directory's own project, unless `--no-hooks` was passed or the directory sits
/// in no indexed project.
fn install_hooks_here(hooks: bool) {
    if !hooks {
        println!("  hook     skipped (--no-hooks)");

        return;
    }

    let Some(root) = indexed_root_here() else {
        return;
    };

    install_project_hook(&root);
    install_project_plugin(&root);
}

/// The root of the indexed project the working directory sits in, or `None` when
/// it sits in none. The root holds the `.constellation/` directory, so it is the
/// database's grandparent.
fn indexed_root_here() -> Option<PathBuf> {
    let database = crate::workspace::discover_database_optional().ok()??;

    database.parent().and_then(Path::parent).map(Path::to_path_buf)
}

/// The removal of `constellation` from every supported agent, the inverse of
/// `install`. Each project's `.constellation/` index is left untouched; only the
/// agent registrations are undone.
///
/// The user-scope hook is removed too. Nothing writes one any more, but a
/// machine that ran an earlier `install` still carries it, and leaving it behind
/// would keep taxing every non-constellation project on that machine.
pub fn uninstall() -> Result<()> {
    uninstall_claude_code();
    uninstall_codex();
    uninstall_opencode();
    uninstall_hooks();

    println!("Project indexes are kept; delete each `.constellation/` to remove them");
    println!("Project hooks: delete `.claude/settings.local.json` in each indexed project");
    println!("Project plugins: delete `.opencode/plugins/constellation.ts` in each project");

    Ok(())
}

/// The `PreToolUse` hook registered for one project, in its own
/// `.claude/settings.local.json`.
///
/// Project scope rather than user scope is the whole point: Claude Code matches
/// a hook on the tool name alone, so a user-scope registration runs on every
/// `Grep`, `Read`, and `Bash` in every project on the machine, and in one
/// constellation has not indexed it can only start up, find no database, and
/// exit. Writing it beside the index means the two appear and disappear
/// together.
///
/// `settings.local.json` rather than `settings.json` because the entry names an
/// absolute path to this binary, which is correct for this machine and wrong for
/// a teammate's; the local file is also the one Claude Code keeps out of version
/// control, so a repository shared with people using other clients gains nothing
/// it has to ignore.
pub fn install_project_hook(root: &Path) {
    match register_project_hook(root) {
        Ok(path) => {
            println!("  hook     registered in {}", path.display());

            // Deliberately no PostToolUse hook on Write or Edit: `serve` already
            // watches every indexed root and re-indexes after each debounced
            // burst, so a post-write re-index would duplicate that work and race
            // it for the same SQLite writer.
        }
        Err(error) => {
            eprintln!("  hook     not registered: {error}");

            print_hooks_manual(root);
        }
    }
}

/// The OpenCode plugin written into one project's `.opencode/plugins/`, the
/// counterpart to [`install_project_hook`] for the other client.
///
/// Project scope for the same reason the hook uses it: OpenCode loads a plugin
/// from its global config directory into every session on the machine, including
/// the ones constellation has never indexed. Writing it beside the index means
/// the two appear and disappear together.
///
/// The plugin runs on `tool.execute.after` rather than before, because OpenCode's
/// `tool.execute.before` can only rewrite a call's arguments and has no channel
/// for injecting context. The search therefore still runs and the graph context
/// is appended to its result. docs/hooks.md records the divergence.
///
/// The file is written whole rather than merged, so a plugin of the same name
/// that constellation did not write is reported and left alone.
pub fn install_project_plugin(root: &Path) {
    match register_project_plugin(root) {
        Ok(Some(path)) => println!("  plugin   written to {}", path.display()),
        Ok(None) => {
            let path = project_plugin_path(root);

            println!("  plugin   kept {}: not written by constellation", path.display());
        }
        Err(error) => {
            eprintln!("  plugin   not written: {error}");

            print_plugin_manual(root);
        }
    }
}

/// The plugin file written into one project, returning its path. Returns `None`
/// when a file is already there that constellation did not write, which is left
/// untouched rather than overwritten.
fn register_project_plugin(root: &Path) -> Result<Option<PathBuf>> {
    let path = project_plugin_path(root);

    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        if !is_constellation_plugin(&existing) {
            return Ok(None);
        }

        // Rewriting identical bytes would touch the file's modification time and
        // wake OpenCode's own watcher for nothing.
        if existing == OPENCODE_PLUGIN {
            return Ok(Some(path));
        }
    }

    write_plugin(&path, OPENCODE_PLUGIN)?;

    Ok(Some(path))
}

/// Whether a plugin file is one constellation wrote, identified by the marker on
/// its first line.
fn is_constellation_plugin(source: &str) -> bool {
    source
        .lines()
        .next()
        .is_some_and(|line| line.starts_with(OPENCODE_PLUGIN_MARKER))
}

/// A project's OpenCode plugin file. OpenCode loads both `.opencode/plugin/` and
/// `.opencode/plugins/`; the plural is the one its documentation names.
fn project_plugin_path(root: &Path) -> PathBuf {
    root.join(".opencode").join("plugins").join("constellation.ts")
}

/// The manual instruction printed when the plugin file cannot be written.
fn print_plugin_manual(root: &Path) {
    println!(
        "  plugin   write the OpenCode plugin to {} by hand (see docs/hooks.md)",
        project_plugin_path(root).display(),
    );
}

/// A plugin file written to disk, creating parent directories as needed.
fn write_plugin(path: &Path, source: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    std::fs::write(path, source).with_context(|| format!("writing {}", path.display()))
}

/// The hook entry merged into one project's `.claude/settings.local.json`,
/// preserving every other setting and any hook another tool registered. Returns
/// the settings path.
fn register_project_hook(root: &Path) -> Result<PathBuf> {
    let path = project_settings_path(root);

    merge_hook_entry(&path)?;

    Ok(path)
}

/// The hook entry merged into the settings file at `path`.
fn merge_hook_entry(path: &Path) -> Result<()> {
    let mut settings = read_config(path)?;

    let root = settings
        .as_object_mut()
        .context("claude settings root is not a JSON object")?;

    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("claude `hooks` is not a JSON object")?;

    let events = hooks
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("claude `hooks.PreToolUse` is not a JSON array")?;

    events.retain(|entry| !is_constellation_hook(entry));

    events.push(json!({
        "matcher": HOOK_MATCHER,
        "hooks": [{
            "type": "command",
            "command": hook_command(),
            "timeout": HOOK_TIMEOUT_SECS,
        }],
    }));

    write_config(path, &settings)?;

    Ok(())
}

/// The legacy user-scope hook removed, leaving every other hook in place. Only
/// `uninstall` calls this; nothing registers a user-scope hook any more.
fn uninstall_hooks() {
    match deregister_hooks() {
        Ok(Some(path)) => println!("Hooks: removed the legacy user-scope hook from {}", path.display()),
        Ok(None) => println!("Hooks: no user-scope hook registered; nothing to remove"),
        Err(error) => {
            eprintln!("Hooks: could not update the settings automatically: {error}");

            println!("Hooks: remove the constellation entry under \"hooks\" by hand");
        }
    }
}

/// The removal itself, returning the settings path when an entry was removed and
/// `None` when there was nothing to remove.
fn deregister_hooks() -> Result<Option<PathBuf>> {
    let path = claude_settings_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let mut settings = read_config(&path)?;

    let Some(events) = settings
        .as_object_mut()
        .and_then(|root| root.get_mut("hooks"))
        .and_then(|hooks| hooks.get_mut("PreToolUse"))
        .and_then(|events| events.as_array_mut())
    else {
        return Ok(None);
    };

    let before = events.len();

    events.retain(|entry| !is_constellation_hook(entry));

    if events.len() == before {
        return Ok(None);
    }

    write_config(&path, &settings)?;

    Ok(Some(path))
}

/// Whether a `PreToolUse` entry is one constellation registered, identified by
/// the `hook pre-tool-use` subcommand in its command string. Matching on the
/// subcommand rather than the whole path survives a moved or reinstalled binary.
fn is_constellation_hook(entry: &Value) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };

    hooks.iter().any(|hook| {
        hook.get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains("hook pre-tool-use"))
    })
}

/// The Claude Code user-scope settings file, read only to remove a hook an earlier
/// version installed there.
fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;

    Ok(home.join(".claude").join("settings.json"))
}

/// A project's Claude Code settings file. The `.local` variant deliberately:
/// the entry names an absolute path to this binary, and Claude Code keeps this
/// file out of version control.
fn project_settings_path(root: &Path) -> PathBuf {
    root.join(".claude").join("settings.local.json")
}

/// The manual hook entry, printed when automatic registration cannot proceed.
fn print_hooks_manual(root: &Path) {
    println!(
        "  hook     add this under \"hooks\".\"PreToolUse\" in {}:",
        project_settings_path(root).display(),
    );
    println!(
        "  {{ \"matcher\": {HOOK_MATCHER:?}, \"hooks\": [{{ \"type\": \"command\", \
         \"command\": {:?} }}] }}",
        hook_command(),
    );
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

/// The full `hook pre-tool-use` command line as it is written into a settings
/// file, with the binary path made safe for a shell.
///
/// Claude Code runs a hook command through a shell, and on Windows that shell is
/// commonly bash, which reads the backslashes of a native path as escapes and
/// collapses `C:\Users\...` to `C:Users...`. Forward slashes survive both shells
/// and are accepted by the Windows path APIs; the quotes cover an install
/// directory containing spaces.
fn hook_command() -> String {
    let executable = server_command().replace('\\', "/");

    format!("\"{executable}\" hook pre-tool-use")
}

/// The registration of Claude Code via its own CLI (user scope), so its config stays valid.
///
/// `claude mcp add` refuses a name it already holds, which would make an install
/// that changes the served command a no-op that reports success. The existing
/// entry is removed first so a reinstall updates rather than declines; removing
/// one that is not there is not an error worth reporting.
fn install_claude_code() {
    let executable = server_command();

    let _ = agent_command("claude", &["mcp", "remove", "--scope", "user", "constellation"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let status = agent_command(
        "claude",
        &[
            "mcp",
            "add",
            "--scope",
            "user",
            "constellation",
            "--",
            executable.as_str(),
            "serve",
            SUPERVISE_FLAG,
        ],
    )
    .status();

    match status {
        Ok(status) if status.success() => {
            println!("Claude Code: registered constellation (user scope)");
        }
        _ => {
            println!(
                "Claude Code: add manually -> claude mcp add --scope user \
                 constellation -- {executable} serve {SUPERVISE_FLAG}",
            );
        }
    }
}

/// The removal of the Claude Code registration via its own CLI (user scope).
fn uninstall_claude_code() {
    let status = agent_command(
        "claude",
        &["mcp", "remove", "--scope", "user", "constellation"],
    )
    .status();

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

/// The registration of Codex via its own CLI, which writes the global
/// `~/.codex/config.toml`, so one registration covers every project. Best-effort:
/// a missing or failing `codex` falls back to printed manual instructions, so it
/// never aborts the rest of `install`.
fn install_codex() {
    let executable = server_command();

    let status = agent_command(
        "codex",
        &[
            "mcp",
            "add",
            "constellation",
            "--",
            executable.as_str(),
            "serve",
            SUPERVISE_FLAG,
        ],
    )
    .status();

    match status {
        Ok(status) if status.success() => {
            println!("Codex: registered constellation");
        }
        _ => {
            println!(
                "Codex: add manually -> \
                 codex mcp add constellation -- {executable} serve {SUPERVISE_FLAG}",
            );
        }
    }
}

/// The removal of the Codex registration via its own CLI, the inverse of
/// [`install_codex`].
fn uninstall_codex() {
    let status = agent_command("codex", &["mcp", "remove", "constellation"]).status();

    match status {
        Ok(status) if status.success() => {
            println!("Codex: removed constellation");
        }
        _ => {
            println!("Codex: remove manually -> codex mcp remove constellation");
        }
    }
}

/// A CLI invocation of an agent's own entry point (`claude`, `codex`). On Windows
/// such an entry point is often a shim (`claude.cmd` from npm) or a portable
/// `.exe` that `Command::new` cannot reliably launch by bare name, so it is run
/// through `cmd /c`, which resolves either; on other platforms it is invoked
/// directly.
fn agent_command(program: &str, arguments: &[&str]) -> Command {
    assert!(!program.is_empty(), "an agent invocation needs a program");
    assert!(!arguments.is_empty(), "an agent invocation needs arguments");

    #[cfg(windows)]
    let mut command = {
        let mut shim = Command::new("cmd");
        shim.arg("/c").arg(program);
        shim
    };

    #[cfg(not(windows))]
    let mut command = Command::new(program);

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
            "command": [server_command(), "serve", SUPERVISE_FLAG],
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
        "  \"constellation\": {{ \"type\": \"local\", \"command\": \
         [{executable:?}, \"serve\", \"{SUPERVISE_FLAG}\"], \"enabled\": true }}",
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

/// The OpenCode global `opencode.json`. constellation writes to `opencode.json`
/// specifically: it is strict JSON that round-trips without losing comments (unlike
/// `opencode.jsonc`), and OpenCode merges it with its other global files anyway.
fn opencode_config_path() -> Result<PathBuf> {
    Ok(opencode_config_dir()?.join("opencode.json"))
}

/// The OpenCode global config directory, matching its `xdg-basedir` resolution:
/// `$XDG_CONFIG_HOME/opencode` when that variable is set, else
/// `<home>/.config/opencode` (the fallback on every OS, Windows included).
fn opencode_config_dir() -> Result<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty());
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

    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

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
        OPENCODE_PLUGIN, agent_command, is_constellation_plugin, opencode_config_path,
        project_plugin_path, read_config, register_project_plugin, resolve_opencode_dir,
        server_command, write_config,
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
    fn agent_command_forwards_its_arguments() {
        let command = agent_command(
            "claude",
            &["mcp", "remove", "--scope", "user", "constellation"],
        );

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
        assert_eq!(
            program, "cmd",
            "an agent entry point is launched through cmd on Windows"
        );
        #[cfg(not(windows))]
        assert_eq!(
            program, "claude",
            "the agent entry point is launched directly off Windows"
        );
    }

    #[test]
    fn agent_command_uses_the_named_program() {
        let command = agent_command("codex", &["mcp", "remove", "constellation"]);

        let program = command.get_program().to_string_lossy().into_owned();

        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        #[cfg(windows)]
        {
            assert_eq!(
                program, "cmd",
                "a portable exe is launched through cmd on Windows"
            );
            assert!(
                arguments.starts_with(&["/c".to_string(), "codex".to_string()]),
                "cmd runs the named program in /c mode, got {arguments:?}",
            );
        }

        #[cfg(not(windows))]
        {
            assert_eq!(
                program, "codex",
                "the named program is launched directly off Windows"
            );
            assert!(
                arguments.starts_with(&["mcp".to_string()]),
                "the arguments are forwarded as-is, got {arguments:?}",
            );
        }
    }

    #[test]
    #[should_panic(expected = "needs arguments")]
    fn agent_command_rejects_empty_arguments() {
        let _ = agent_command("claude", &[]);
    }

    #[test]
    #[should_panic(expected = "needs a program")]
    fn agent_command_rejects_an_empty_program() {
        let _ = agent_command("", &["mcp"]);
    }

    #[test]
    fn server_command_is_never_empty() {
        assert!(
            !server_command().is_empty(),
            "the registered server command always resolves to a value"
        );
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
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("opencode"),
            "it lives under the opencode config directory",
        );
    }

    #[test]
    fn the_embedded_plugin_carries_its_own_marker() {
        assert!(
            is_constellation_plugin(OPENCODE_PLUGIN),
            "the embedded plugin is recognised as constellation's own, so an install upgrades it",
        );
    }

    #[test]
    fn a_plugin_without_the_marker_is_not_mistaken_for_constellations() {
        assert!(
            !is_constellation_plugin("export const MyPlugin = async () => ({})\n"),
            "a plugin constellation did not write is left alone rather than overwritten",
        );
    }

    #[test]
    fn project_plugin_path_sits_inside_the_project() {
        let path = project_plugin_path(Path::new("/repo"));

        assert_eq!(
            path,
            Path::new("/repo")
                .join(".opencode")
                .join("plugins")
                .join("constellation.ts"),
            "the plugin is written into the project, not the global config directory",
        );
    }

    #[test]
    fn registering_the_plugin_writes_it_and_then_leaves_a_foreign_file_alone() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        let written = register_project_plugin(root).unwrap().unwrap();

        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            OPENCODE_PLUGIN,
            "a first install writes the embedded plugin"
        );

        let foreign = "export const MyPlugin = async () => ({})\n";
        std::fs::write(&written, foreign).unwrap();

        assert_eq!(
            register_project_plugin(root).unwrap(),
            None,
            "a plugin constellation did not write is reported rather than replaced"
        );
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            foreign,
            "and its contents survive the install"
        );
    }

    #[test]
    fn read_config_treats_a_missing_file_as_an_empty_object() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent.json");

        assert_eq!(
            read_config(&missing).unwrap(),
            json!({}),
            "an absent config reads as an empty object"
        );
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

        assert_eq!(
            read_config(&path).unwrap(),
            config,
            "the written config reads back identically"
        );
    }
}
