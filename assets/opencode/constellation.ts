// constellation-plugin v1
//
// The OpenCode port of the Claude Code `PreToolUse` hook documented in
// docs/hooks.md. It shells out to the same `constellation hook pre-tool-use`
// subcommand with the same stdin payload, so the pattern extraction, the
// latency budget, and the output bound all stay in Rust and there is exactly
// one implementation of them.
//
// The four hard constraints from docs/hooks.md hold here too, and two of them
// are enforced differently because OpenCode runs a plugin inside its own
// process rather than as a separate command:
//
//   1. Never block. Every failure path returns without touching `output`.
//      A Claude Code hook is a child process, so a crash is contained by the
//      operating system. A plugin is not, so the blanket catch below is the
//      containment, and it must stay wrapped around every fallible step
//      including the JSON parse and the mutation itself.
//   2. Budget the latency. HOOK_TIMEOUT_MS is the outer guard, matching the
//      `timeout` on the registered Claude Code entry. The binary budgets
//      itself to 150ms and stays silent past it.
//   3. Bound the output. The binary truncates to 2000 bytes; this repeats the
//      cap so a drifting binary version cannot flood a session's context.
//   4. Opt in, and be removable. CONSTELLATION_HOOK=0 disables it for a
//      session without deleting the file mid-run.
//
// The one deliberate divergence from Claude Code: this runs on
// `tool.execute.after`, not before. OpenCode's `tool.execute.before` can only
// rewrite arguments, and has no channel for injecting context. The context is
// therefore appended to the tool result rather than shown ahead of the call.
// The search still runs. Appended rather than prepended so a `read` result
// cannot be mistaken for file content.

import type { Plugin } from "@opencode-ai/plugin";

/// The `constellation` executable, resolved from PATH.
const BINARY = "constellation";

/// The milliseconds to wait before abandoning the hook. The binary budgets
/// itself far tighter; this is only the outer guard, mirroring HOOK_TIMEOUT_SECS
/// in crates/cli/src/bootstrap.rs.
const HOOK_TIMEOUT_MS = 5000;

/// The ceiling on injected text, mirroring HOOK_CONTEXT_BYTES_MAX in
/// crates/cli/src/hook.rs. This text lands on every matching tool call, so a
/// verbose hook is worse than no hook.
const HOOK_CONTEXT_BYTES_MAX = 2000;

/// The consecutive failures after which the hook disables itself for the rest
/// of the session. A missing binary, a broken build, or a hung index stops
/// costing a spawn per search instead of degrading every one of them.
const HOOK_FAILURES_MAX = 3;

/// The shortest pattern worth looking up, mirroring PATTERN_CHARS_MIN in
/// crates/cli/src/hook.rs. Checked here as well so a one-character grep never
/// pays for a process spawn.
const PATTERN_CHARS_MIN = 3;

/// The levels walked up from the working directory while looking for an index.
const INDEX_SEARCH_DEPTH_MAX = 32;

/// The tool call, reduced to the Claude Code `tool_input` the binary parses.
/// `null` when the tool is not one the hook enriches, or carries no usable
/// pattern.
type PatternSource = {
    claudeName: string;
    toolInput: (args: any) => Record<string, unknown> | null;
};

/// The OpenCode tool ids the hook enriches, mapped onto the Claude Code tool
/// names and field names `extract_pattern` in crates/cli/src/hook.rs expects.
/// OpenCode names its tools in lower case and its read tool takes `filePath`,
/// so the shim is a rename, not a reimplementation.
const PATTERN_SOURCES: Record<string, PatternSource> = {
    grep: {
        claudeName: "Grep",
        toolInput: (args) => (args?.pattern ? { pattern: args.pattern } : null),
    },
    glob: {
        claudeName: "Glob",
        toolInput: (args) => (args?.pattern ? { pattern: args.pattern } : null),
    },
    read: {
        claudeName: "Read",
        toolInput: (args) => {
            const path = args?.filePath ?? args?.file_path ?? args?.path;

            return path ? { file_path: path } : null;
        },
    },
    bash: {
        claudeName: "Bash",
        toolInput: (args) => (args?.command ? { command: args.command } : null),
    },
};

let consecutiveFailures = 0;

/// Whether the hook is switched off for this session, by the environment or by
/// the circuit breaker having tripped.
function isDisabled(): boolean {
    if (consecutiveFailures >= HOOK_FAILURES_MAX) return true;

    const setting = process.env.CONSTELLATION_HOOK;

    return setting === "0" || setting === "false";
}

/// Whether an index is reachable from a directory, by the same discovery the
/// binary performs: the CONSTELLATION_DB override, else the nearest
/// `.constellation/index.db` at or above the directory. Checked before
/// spawning so an unindexed project never pays for a process that can only
/// exit silently.
async function hasIndex(directory: string): Promise<boolean> {
    if (process.env.CONSTELLATION_DB) return true;

    let current = directory;

    for (let level = 0; level < INDEX_SEARCH_DEPTH_MAX; level += 1) {
        if (await Bun.file(`${current}/.constellation/index.db`).exists()) return true;

        const parent = current.replace(/[\\/][^\\/]*$/, "");
        if (!parent || parent === current) return false;

        current = parent;
    }

    return false;
}

/// The graph context for one tool call, or `null` when the binary produced
/// none. Anything at all going wrong counts as a failure against the circuit
/// breaker and yields `null`, never a throw.
async function fetchContext(payload: string): Promise<string | null> {
    try {
        const process_ = Bun.spawn([BINARY, "hook", "pre-tool-use"], {
            stdin: new TextEncoder().encode(payload),
            stdout: "pipe",
            stderr: "ignore",
            signal: AbortSignal.timeout(HOOK_TIMEOUT_MS),
        });

        const stdout = (await new Response(process_.stdout).text()).trim();

        consecutiveFailures = 0;

        if (!stdout) return null;

        const context = JSON.parse(stdout)?.hookSpecificOutput?.additionalContext;
        if (typeof context !== "string" || !context) return null;

        return context.slice(0, HOOK_CONTEXT_BYTES_MAX);
    } catch {
        consecutiveFailures += 1;

        return null;
    }
}

export const ConstellationHook: Plugin = async ({ directory }) => {
    return {
        "tool.execute.after": async (input, output) => {
            try {
                if (isDisabled()) return;

                const source = PATTERN_SOURCES[input.tool];
                if (!source) return;

                const toolInput = source.toolInput(input.args);
                if (!toolInput) return;

                const pattern = Object.values(toolInput)[0];
                if (typeof pattern !== "string" || pattern.length < PATTERN_CHARS_MIN) return;

                if (!(await hasIndex(directory))) return;

                const payload = JSON.stringify({
                    hook_event_name: "PreToolUse",
                    tool_name: source.claudeName,
                    tool_input: toolInput,
                    cwd: directory,
                });

                const context = await fetchContext(payload);
                if (!context) return;

                output.output = `${output.output}\n\n${context}`;
            } catch {
                // Constraint 1, and the containment this port needs that the
                // Claude Code hook gets from process isolation. A plugin throw
                // propagates into OpenCode's own tool handling.
            }
        },
    };
};
