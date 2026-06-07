# constellation

A cross-project knowledge graph of Django codebases, served to an LLM coding agent over MCP. constellation parses Python and Django into a graph of symbols, imports, calls, and Django structure such as models, URL routes, and templates. It links those graphs across separate repositories and answers an agent's questions about how your projects connect.

## Why it exists

An agent exploring a codebase without an index spends most of its budget on grep and file reads, and it typically sees one repository at a time. Most of our projects use shared packages (`django-spire`, `django-glue`, `robit`), and a single request crosses those boundaries. A per-repo index cannot show those edges, and grep cannot follow them across repositories at all.

`constellation` builds the graph once and links it across repositories, so an agent can trace a flow that leaves one repo and lands in another, and answer in a single query what would otherwise be a grep-and-read hunt across several checkouts.

## Install

    winget install stratusadv.constellation

This installs the binary, adds it to your PATH, and registers the MCP server with OpenCode and Claude Code. Open a new terminal, then index each repository:

    constellation init

Your agent picks up the graph the next time it starts. You can re-run `constellation init` after large changes, or use `constellation watch` to re-index automatically.

Update:

    winget upgrade stratusadv.constellation

## What it indexes

- Python 3.11 and 3.13: symbols, imports, calls
- Django 5 and 6: models and fields, URL routes, views, template inheritance and render targets
- JavaScript and Alpine.js, including Alpine `x-data` handlers
- Links across repositories

## Commands

| Command | Purpose |
|---|---|
| `constellation init` | Create and index `.constellation/index.db` in the current repo |
| `constellation watch <repo>` | Index, then re-index on file changes |
| `constellation link <db> <repo>...` | Index several repos into one shared graph and link them |
| `constellation serve [db]` | Serve the graph over MCP (stdio); registered automatically by install |
| `constellation install` / `uninstall` | Register or unregister the MCP server with your agents |

`serve` finds the database by walking up from the working directory, or from the `CONSTELLATION_DB` environment variable.

## How your agent uses it

Once a repo is indexed, the agent queries the graph instead of grepping: project overview, symbol search, source exploration, callers and callees, change impact, a model's effective schema, routes, and cross-project links, all sub-millisecond.

## Build from source

    cargo build --release
    .\target\release\constellation.exe install

This requires the Rust toolchain.

## License

MIT. See [LICENSE](LICENSE).
