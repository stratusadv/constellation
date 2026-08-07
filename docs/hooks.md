# The agent hook contract

`constellation install --hooks` registers a `PreToolUse` hook with Claude Code
that injects graph context into a file-search tool call. This document is the
contract: what the hook reads, what it writes, and the four rules it will not
break.

Hooks are opt-in. A plain `constellation install` registers the MCP server and
the bundled skills, and prints a line saying hooks were not installed.

The same install writes an OpenCode plugin that calls the same subcommand with
the same payload. [The OpenCode plugin](#the-opencode-plugin) records what
differs there; everything above that section describes both.

## Why

An instruction file can only ask an agent to prefer the graph over grep.
Instructions are read once, at the top of a long session, and compete with
everything after them. A hook fires on the call itself.

When the agent is about to run `Grep`, `Glob`, `Read`, or a `Bash` command that
invokes `rg` or `grep`, the hook takes the pattern, looks it up in the graph, and
hands back what a text search cannot give: where the symbol is defined, how many
things reference it, whether a test covers it, and which execution flow it sits
in.

## The four hard constraints

These are the difference between a useful hook and one that gets uninstalled on
day two. They are enforced in `crates/cli/src/hook.rs`, not merely intended.

### 1. Always exit 0

Any error, missing index, malformed input, or blown deadline emits nothing and
succeeds. The hook must never block or fail a tool call. Every fallible step
inside it degrades to "no output" rather than propagating.

### 2. Budget the latency

`HOOK_BUDGET_MS = 150`, checked between steps against a deadline taken at entry.
Opening SQLite and running a bounded search is single-digit milliseconds on a
warm page cache. When it is not (a cold open on a large index, a loaded
machine), the deadline fires and the hook stays silent rather than making every
search wait for it.

Claude Code's own `timeout` is set to 5 seconds in the registered entry. That is
the outer guard, not the budget: the hook is expected to finish thirty times
faster than that or produce nothing.

### 3. Bound the output

`HOOK_CONTEXT_BYTES_MAX = 2000`, truncated on a UTF-8 boundary. This text is
injected into *every* matching tool call. A verbose hook is worse than no hook,
because it costs context on every search whether or not the search needed help.

At most `HOOK_SYMBOLS_MAX = 4` symbols are reported, and only definitions
(class, function, method, model, route, view) rather than imports or local
variables that happen to share the name.

### 4. Opt in, and be removable

Installed only by `constellation install --hooks`. `constellation uninstall`
removes it. Both operations merge into `~/.claude/settings.json` rather than
rewriting it, and identify constellation's own entries by the `hook
pre-tool-use` subcommand in the command string, so a hook another tool
registered is never touched.

## Input

The hook reads one JSON object on stdin, the standard `PreToolUse` payload:

```json
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Grep",
  "tool_input": { "pattern": "Order", "path": "app/" },
  "cwd": "/path/to/the/project"
}
```

Stdin is read to at most 1 MiB. `cwd` locates the database by walking up for a
`.constellation/index.db`, or the `CONSTELLATION_DB` environment variable when
set. A payload that is not valid JSON, or names a tool the hook does not enrich,
produces no output.

## Pattern extraction

| Tool | Field read |
|---|---|
| `Grep` | `tool_input.pattern` |
| `Glob` | `tool_input.pattern` |
| `Read` | the basename of `tool_input.file_path` |
| `Bash` | the search pattern inside `tool_input.command` |

A pattern shorter than three characters is ignored: one or two characters match
half the codebase, so the lookup would be noise.

### The `Bash` case

Extracting a pattern from a shell command line is the fiddly part, and getting
it wrong is the classic bug in this kind of hook. `rg -t py Foo` must yield
`Foo`, not `py`.

The extractor finds the `rg`, `grep`, `egrep`, `fgrep`, or `ripgrep` token
(after stripping any directory prefix and a `.exe` suffix), then walks forward:

- `-e PATTERN`, `--regexp PATTERN`, and `--regexp=PATTERN` name the pattern
  explicitly and win outright, because with `-e` present the first bare token is
  a path.
- A flag in the value-taking list (`-t`, `--type`, `-g`, `--glob`, `-A`, `-B`,
  `-C`, `--max-count`, and the rest) skips itself *and the next token*.
- Any other `-`-prefixed token skips itself.
- The first remaining bare token is the pattern, with one layer of quotes
  stripped.

Scanning is bounded at 512 tokens. The table of cases is tested in
`crates/cli/src/hook.rs`; add a row there before changing the flag list.

## Output

On success, one JSON object on stdout:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "additionalContext": "constellation already indexes \"Order\". Prefer constellation_explore over reading these by hand:\n  model Order (shop/models.py:14) - 37 references, in \"checkout/\" flow (criticality 0.81)\n"
  }
}
```

On anything else, nothing at all, and exit 0.

## What is deliberately *not* installed

**No `PostToolUse` hook on `Write` or `Edit`.** `constellation serve` already
watches every indexed root and re-indexes after each debounced burst. A
post-write re-index would duplicate that work and race it for the same SQLite
writer, which is a worse outcome than the freshness it buys. The install prints
a line saying so, so the absence reads as a decision rather than an oversight.

## The OpenCode plugin

OpenCode has no hooks. It has plugins: TypeScript modules loaded from
`.opencode/plugins/` in a project or the global config directory, exporting
named lifecycle functions that run inside OpenCode's own process.

`install --hooks` writes `assets/opencode/constellation.ts` into each indexed
project's `.opencode/plugins/`, project-scoped for the same reason the Claude
Code hook is: a plugin in the global config directory loads into every session
on the machine, including projects constellation has never indexed.

The plugin does not reimplement anything. It renames OpenCode's tool call into
the payload above and shells out to the same `hook pre-tool-use` subcommand, so
pattern extraction, the latency budget, and the output bound stay in Rust with
one implementation and one test table.

| OpenCode | Claude Code |
|---|---|
| `grep`, arg `pattern` | `Grep`, `tool_input.pattern` |
| `glob`, arg `pattern` | `Glob`, `tool_input.pattern` |
| `read`, arg `filePath` | `Read`, `tool_input.file_path` |
| `bash`, arg `command` | `Bash`, `tool_input.command` |

### What differs, and why

**It runs after the tool, not before.** OpenCode's `tool.execute.before` can
only rewrite a call's arguments; it has no channel for injecting context. The
plugin therefore uses `tool.execute.after` and appends the graph context to the
tool result. The search runs either way. Appended rather than prepended so a
`read` result cannot have the context mistaken for the first line of the file.

**The blanket catch is load-bearing.** A Claude Code hook is a child process, so
the operating system contains a crash. A plugin is not: a throw propagates into
OpenCode's own tool handling. The `catch` in the handler is that containment,
and it must stay wrapped around every fallible step including the JSON parse and
the mutation itself. Narrowing it breaks constraint 1.

**It fails safe twice more.** `CONSTELLATION_HOOK=0` disables it for a session
without deleting the file, and three consecutive failures trip a circuit breaker
that stops it for the rest of the session, so a missing or broken binary costs
one spawn rather than one per search.

### Installation and removal

The file is written whole, not merged. A plugin of that name whose first line
does not carry the `// constellation-plugin v` marker was written by someone
else and is reported and left alone; one that does carry it is upgraded in
place, and identical content is not rewritten at all so OpenCode's file watcher
is not woken for nothing. `constellation uninstall` prints the path to delete,
matching how it treats the project hook.

## Manual installation

If the automatic merge cannot proceed (an unparsable settings file, a read-only
home directory), `install --hooks` prints the entry to add by hand under
`hooks.PreToolUse` in `~/.claude/settings.json`:

```json
{
  "matcher": "Grep|Glob|Read|Bash",
  "hooks": [
    {
      "type": "command",
      "command": "/path/to/constellation hook pre-tool-use",
      "timeout": 5
    }
  ]
}
```

## Testing it by hand

```
echo '{"tool_name":"Grep","tool_input":{"pattern":"Order"},"cwd":"."}' \
  | constellation hook pre-tool-use
```

From inside an indexed project this prints the JSON above. From anywhere else it
prints nothing and exits 0, which is the correct behaviour rather than a
failure.
