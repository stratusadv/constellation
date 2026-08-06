<p align="center"><img src="assets/logo/constellation-banner-dark.png" width="640" alt="constellation"></p>

# constellation

A cross-project knowledge graph of Django codebases, served to a coding agent
over MCP.

constellation parses Python and Django into symbols, imports, calls, and the
structure a text search cannot give: models and their fields, URL routes, views,
template inheritance and render targets. It links those per-repo graphs to each
other, so a call that leaves your code for `django-spire` and comes back is one
lookup rather than a hunt across two checkouts.

Indexed: Python 3.11 and 3.13, Django 5 and 6, JavaScript and Alpine.js.

## Install

Windows:

    winget install stratusadv.constellation

Linux (x86_64):

    curl -fsSL https://raw.githubusercontent.com/stratusadv/constellation/main/assets/install.sh | sh

The script fetches the latest release, verifies its checksum, puts the binary in
`~/.local/bin`, appends that directory to your shell's startup file, and runs
`constellation install`. Set `CONSTELLATION_INSTALL_DIR` to install somewhere
else. It is [assets/install.sh](assets/install.sh), short enough to read before
piping it to a shell.

Either installer registers the MCP server with Claude Code, Codex, and OpenCode;
Grok Build discovers a configured server on its own. Then index each repository:

    constellation init

Your agent picks up the graph the next time it starts. While `serve` runs it
watches your files and re-indexes and re-links on every change, so the graph
stays current as you work.

Updating is the same command again (`winget upgrade stratusadv.constellation` on
Windows). The Linux binary links against glibc 2.35, so Ubuntu 22.04+, Debian
12+, Fedora 36+ and anything newer run it; an older or musl-based distribution
builds from source.

## Commands

| Command | Purpose |
|---|---|
| `constellation init [--no-hooks]` | Create and index `.constellation/index.db` in this repo, with a starter config and a Claude Code search hook ([docs](docs/hooks.md)) |
| `constellation sync [db]` | Re-index every project from disk and re-link, in one shot |
| `constellation link <db> <repo>...` | Index several repos into one shared graph and link them |
| `constellation serve [db] [--supervise]` | Serve the graph over MCP (stdio) and watch for changes; registered by `install` |
| `constellation history [db] [--symbols]` | Ingest git history so the graph can be read over time |
| `constellation flows [db] [--project id] [--depth n] [--include-tests]` | Trace and rank every Django execution flow |
| `constellation install [--no-hooks]` / `uninstall` | Register or unregister the MCP server and this repo's search hook |

`serve` and `sync` find the database by walking up from the working directory, or
from `CONSTELLATION_DB` when it is set. `sync` is the manual one-shot for when
`serve` is not running; after that, the watcher is enough.

`history` and `flows` populate derived data the query tools degrade gracefully
without: a tool that needs one and does not have it names the command to run
rather than answering as though the data were empty.

## What the agent asks it

Project overview, symbol search, source exploration, callers and callees, change
impact, a model's effective schema, routes, flows, and cross-project links, all
sub-millisecond. Four are worth knowing by name:

- `explore` takes a question or a bag of identifiers and returns the source
  behind it, grouped by file and line-numbered. Usually the only call needed.
- `changed` ranks the symbols your diff touched by review risk, highest first,
  with the two or three reasons behind each score: missing tests, a
  security-sensitive name, a critical flow, fan-in, cross-repo callers, churn.
- `flows` lists every execution path Django can dispatch, ranked by criticality,
  with no symbol named first; `affected_flows` narrows that to the user-facing
  flows your working tree touches.
- `winnow` composes what would otherwise be four calls and a manual
  intersection: models with a foreign key to `Order`, changed in the last thirty
  days, with no covering tests.

Symbol lines carry a working-tree marker (`[M]` modified, `[A]` added, `[D]`
deleted, `[?]` untracked, nothing at all for a clean file), taken from a snapshot
a background worker refreshes rather than from `git status` on the query path. A
listing that can truncate takes a `cursor=` to page into the tail.

## Companions

Most requests cross into the shared packages a project installs. On `init`,
constellation finds them in the virtual environment, indexes each as its own
project, and links the imports across the boundary, so the agent follows a call
into the library and back. `init` writes a starter
`.constellation/config.toml` you can edit:

```toml
[companions]
enabled = true
packages = ["django-spire", "django-glue", "robit", "dandy"]
# venv = ".venv"
```

A local working copy wins over the installed one, because it is what actually
runs: a path pin under `[tool.uv.sources]`, or `PYTHONPATH_APPEND` set in
`development.env` / `.env`, is indexed in place of the `.venv` version.

Other refs of a library can sit alongside it, which is how to compare old and new
while refactoring one:

```toml
[companions]
versions = { django-spire = "refactor/next", django-glue = "v1.2.0" }
```

Each is checked out from the package's own repository into
`.constellation/sources/` and indexed as its own project
(`django-spire@refactor/next`), rooted exactly like the `.venv` copy so only the
suffix differs. These are reference-only: your code still resolves to the
installed version, and you query the extras to compare.

Reading history over time (`history`, `symbol_history`, `as_of`) needs a git
repository, and a wheel in `.venv` has none. Name the repository and
constellation fetches the history at the tag matching the installed version, no
local clone and no version drift:

```toml
[companions.repositories]
django-spire = "https://github.com/your-org/django-spire"
```

That clone is cached under `.constellation/sources/`, so the network is touched
only on the first run or when the installed version changes. No matching tag
means that library's history is skipped rather than shown at the wrong version.

## Build from source

The Rust toolchain is the only requirement; build automation lives in the
workspace as a Rust binary, so there is nothing else to install:

    cargo xtask install
    ~/.local/bin/constellation install

The first line builds the release binary into `~/.local/bin`, overridable with
`CONSTELLATION_INSTALL_DIR`. The second registers that copy rather than the one
in `target/`, which keeps the file your agents launch separate from the file
`cargo build` rewrites underneath them.

`install` registers `serve --supervise`, and that is what makes
`cargo xtask install` the whole edit-test loop. A client owns the process it
spawns and reads the tool list once, so a new binary would normally mean
reconnecting by hand. Under `--supervise` the process the client holds is a
proxy: when the installed path changes it starts the replacement, hands it the
initialize exchange plus anything the retired worker never answered, and
announces the new tool list. A build that cannot run is refused and the running
worker keeps serving. Nobody reconnects.

To see what an agent would see without involving a client at all:

    cargo xtask probe explore '{"query":"Order order_number"}'

Point `CONSTELLATION_DB` at a real project's graph, since this checkout has no
Django index of its own.

The remaining tasks are `cargo xtask build` (the release build alone) and
`cargo xtask tidy` (the style ratchets CI enforces).

## Eval

The benches under `crates/*/benches` measure how fast constellation is;
`crates/eval` measures whether it answers *better*:

    cargo run -p constellation-eval -- --config eval/configs/<repo>.toml

Seven benchmarks over a version-controlled goldset: search quality through both
plain search and `explore`'s ranking, multi-hop questions, impact accuracy in two
ground-truth modes, token efficiency, a scripted grep-and-read loop as the
baseline to beat, flow completeness, and index shape.

Every report ends with a **Limits** section stating what the run did not measure,
and that is the part to read before quoting a number: the goldset is authored by
the same people who wrote the ranking, token counts are a four-bytes-per-token
approximation, and one of the impact modes is circular by construction and says
so in its own column.

## License

MIT. See [LICENSE](LICENSE).
