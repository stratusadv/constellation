# constellation

A cross-project knowledge graph of Django codebases, served to an LLM coding agent over MCP. constellation parses Python and Django into a graph of symbols, imports, calls, and Django structure such as models, URL routes, and templates. It links those graphs across separate repositories and answers an agent's questions about how your projects connect.

## Why it exists

An agent exploring a codebase without an index spends most of its budget on grep and file reads, and it typically sees one repository at a time. Most of our projects use shared packages (`django-spire`, `django-glue`, `robit`), and a single request crosses those boundaries. A per-repo index cannot show those edges, and grep cannot follow them across repositories at all.

`constellation` builds the graph once and links it across repositories, so an agent can trace a flow that leaves one repo and lands in another, and answer in a single query what would otherwise be a grep-and-read hunt across several checkouts.

## Install

    winget install stratusadv.constellation

This installs the binary, adds it to your PATH, and registers the MCP server with OpenCode and Claude Code. Open a new terminal, then index each repository:

    constellation init

Your agent picks up the graph the next time it starts. While `serve` is running it watches your files and re-indexes and re-links automatically, so the graph stays current as you work. Run `constellation sync` for a manual one-shot refresh, or `constellation init` again after large changes.

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
| `constellation init` | Create and index `.constellation/index.db` in the current repo, with a starter config |
| `constellation sync [db]` | Re-index every project from disk and re-link, in one shot |
| `constellation link <db> <repo>...` | Index several repos into one shared graph and link them |
| `constellation serve [db]` | Serve the graph over MCP (stdio) and watch for changes; registered automatically by install |
| `constellation install` / `uninstall` | Register or unregister the MCP server with your agents |

`serve` keeps the graph current on its own: while it runs, a background watcher re-indexes and re-links every project on each change, so you rarely need `sync` by hand. Both `serve` and `sync` find the database by walking up from the working directory, or from the `CONSTELLATION_DB` environment variable.

## Companions

Most requests cross into the shared packages a project installs (`django-spire`, `django-glue`, `robit`). On `init`, constellation finds those packages in the project's virtual environment, indexes each as its own project, and links the imports across the boundary, so the agent can follow a call from your code into the library and back.

`init` writes a starter `.constellation/config.toml` you can edit:

```toml
[companions]
enabled = true
packages = ["django-spire", "django-glue", "robit"]
# venv = ".venv"
```

A local working copy wins over the installed copy. If your `pyproject.toml` pins a package to a path under `[tool.uv.sources]`, or a `development.env` / `.env` sets `PYTHONPATH_APPEND` to a directory holding it, that working copy is indexed in place of the `.venv` version, because it is what actually runs.

### Comparing versions

While refactoring a shared library, index other git refs of it alongside the installed one to compare old and new:

```toml
[companions]
versions = ["django-spire@refactor/next", "django-glue@v1.2.0"]
```

Each `"package@ref"` is checked out from the package's own repository (its editable checkout, or the git url pip recorded) into `.constellation/sources/`, and indexed as a separate project (`django-spire@refactor/next`) rooted exactly like the `.venv` copy, so only the version suffix differs. These copies are reference-only: your code still resolves to the installed version, and you query the extra ones to compare. This needs the library installed editable or from git, so a repository exists to take other refs from.

## How your agent uses it

Once a repo is indexed, the agent queries the graph instead of grepping: project overview, symbol search, source exploration, callers and callees, change impact, a model's effective schema, routes, and cross-project links, all sub-millisecond.

## Build from source

    cargo build --release
    .\target\release\constellation.exe install

This requires the Rust toolchain.

## License

MIT. See [LICENSE](LICENSE).
