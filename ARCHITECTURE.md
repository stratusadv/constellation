# Architecture

This document is the map. It names the layers, states the invariants that hold
across them, and says where a given change belongs. It deliberately avoids line
numbers and mostly avoids function names, because those rot; it names crates,
modules, and boundaries, which do not.

## Bird's eye view

constellation turns a directory of Django source into a queryable graph, links
the graphs of separate repositories together, and serves the result to a coding
agent over MCP.

```
    source tree  ->  extract  ->  resolve  ->  link  ->  store  ->  serve
                     (per file)   (per      (across    (SQLite)   (MCP over
                                   project)  projects)             stdio)
```

Every stage is a crate, and the arrows are the only direction dependencies
point. A file is parsed once, in isolation, into nodes and *unresolved*
references. Nothing at parse time knows what another file contains. Turning a
reference into an edge is a separate, later stage that has the whole project in
hand, and turning a reference into an edge that crosses a repository boundary is
a later stage still.

That split is the central design decision. It is what makes incremental
re-indexing possible (a changed file re-extracts alone) and what keeps the
parsers free of global state.

## The cut

A system can be sliced by feature, by layer of abstraction, or by pipeline
stage. constellation is sliced by **pipeline stage**, and the slicing is
intentional rather than accidental.

The consequence to keep in mind: a single user-visible feature usually touches
several crates, because a feature is a vertical line through a horizontal
stack. Django template inheritance, for example, is a lexer in `extraction`, a
resolver rule in `resolution`, a table in `store`, a synthesis pass in `index`,
and a renderer in `mcp`. That is the cost of this cut. What it buys is that
each crate has one job, and a change to how templates are *parsed* cannot
accidentally change how they are *served*.

## Crates

`crates/` is flat: no nesting, no sub-workspaces. Every crate is named
`constellation-<layer>` and the directory drops the prefix. Crates are listed
in `Cargo.toml` leaf first, so the member list doubles as a reading order.

| Crate | Depends on | Owns |
|---|---|---|
| `graph` | nothing | The vocabulary: `Node`, `Edge`, `NodeId`, `ProjectId`, `Language`, and the path and name predicates every other layer shares. |
| `resolution` | `graph` | Turning an `UnresolvedRef` into a `ResolvedRef`. The `ResolutionContext` and `FrameworkResolver` traits, and the Django resolver. |
| `extraction` | `graph`, `resolution` | tree-sitter parsers. One `Extractor` per language, plus the hand-written Django template lexer and parser. |
| `store` | `graph`, `resolution` | SQLite: the schema, and the `Store` handle with its batched per-file write path and every query the server runs. |
| `linking` | `graph` | Matching an import in one project against a definition in another. |
| `index` | all of the above | Orchestration. The walk, parallel extraction, persistence, the resolution pass, the synthesis passes, cross-project linking, the file watcher, git history, and execution flows. |
| `mcp` | `graph`, `index`, `store` | The `rmcp` server: the tool surface, ranking, and the rendering of results as text an agent reads. |
| `cli` | `graph`, `index`, `mcp`, `store` | Argument dispatch, agent registration, the `PreToolUse` hook, progress output. The `constellation` binary. |
| `eval` | `graph`, `index`, `mcp`, `store` | The retrieval-quality harness. Not published. Measures the server, so it sits beside `cli` rather than inside `mcp`. |
| `xtask` | nothing | Build automation: `build`, `install`, `tidy`. Not published. |

The dependency graph is acyclic and every edge in the table points strictly
down the list. Adding an edge that points up is the one refactor this codebase
does not accept.

### Inside a crate

The crates share one shape, so knowing your way around one is knowing your way
around all of them.

- **`lib.rs` is a facade.** It carries the crate doc, the `mod` declarations,
  and the `pub use` list that is the crate's API. Nothing else. A crate root
  that grows logic is the first sign a module is missing.
- **`limits.rs` holds every bound.** `store`, `index`, and `mcp` each have one.
  Scattered through the code the caps read as arbitrary numbers; gathered in one
  file they read as a budget, which is what they are, and the "everything is
  bounded" invariant becomes something you can check by reading one screen.
- **A module is a job, not a bag.** `store` splits by query family
  (`query/nodes.rs`, `query/git.rs`), `index` by pipeline stage (`walk`,
  `extract`, `resolve`, `synthesize/`, `link`), `mcp` by tool family
  (`tools/search.rs`, `tools/changed.rs`). In each case the name answers "what
  would I be changing", which is the question a reader actually arrives with.
- **Shared judgment lives above the modules that share it.** What counts as a
  definition, how a row maps to a `Node`, how a date parses: decided once, in
  its own module, never re-decided per caller.

Two mechanical rules keep this from eroding. `cargo xtask tidy` fails when a
module passes a thousand lines more often than it did yesterday, and the
workspace denies warnings, so an item nothing uses cannot quietly survive a
move.

### What the boundaries buy

- **`graph` depends on nothing.** It is the shared vocabulary, so it must be
  reachable from everywhere without dragging anything along. Nothing that
  touches a file, a database, or a socket may be defined here.
- **`extraction` cannot see the store.** An `Extractor` takes a project id, a
  path, and a `&str` of source, and returns an `ExtractionOutput`. No I/O, no
  database handle, no knowledge of other files. This is what lets extraction run
  across files in parallel over a shared `&dyn Extractor`, and what makes the
  parsers trivially testable: source in, structure out.
- **`resolution` cannot parse.** It matches names against a
  `ResolutionContext`. Handing it a tree-sitter node would collapse the split
  the whole design rests on.
- **Only `index` and above know SQLite exists.** Only `mcp` and above know MCP
  exists. `cli` is a dispatcher over `index` and `mcp`: a subcommand reads
  arguments, calls one function in a lower layer, and prints.

  `serve --supervise` is the one exception, and it is a deliberate one. The
  supervisor is a stdio proxy that keeps a client's session alive across a
  rebuild by replacing the worker underneath it, so it must parse enough MCP
  framing to replay a handshake and re-address in-flight requests. That is real
  logic, it lives in `cli/src/commands/supervise.rs`, and it is tested through
  the binary in `cli/tests/supervise.rs` because the binary *is* the unit: the
  thing under test is one process supervising another. It stays in `cli` rather
  than moving to `mcp` because it is about process lifecycle, not about the
  graph, and `mcp` should not grow a reason to spawn anything.

## Data flow

### Indexing: `constellation init <dir>`

1. **Walk.** `index` walks the tree with the `ignore` crate, honoring
   `.gitignore` and a hard skip list. Files above a size cap and minified files
   are skipped rather than parsed, so one pathological file cannot dominate a
   run.
2. **Extract.** Files are handed to the matching `Extractor` in parallel
   batches sized against the host's thread count. Each returns nodes, the edges
   already known at parse time (containment, inheritance, imports), unresolved
   references, import mappings, and framework events.
3. **Persist.** Each file's output is written in one batched, idempotent
   transaction. Re-indexing a file replaces exactly that file's rows.
4. **Resolve.** With every node in the project known, each unresolved reference
   is matched against a `ResolutionContext` and, when it resolves, becomes an
   edge. Framework resolvers handle what static parsing cannot see on its own:
   URL routing, template rendering, ORM dynamic dispatch.
5. **Synthesize.** Passes that derive edges from the resolved graph rather than
   from source: reverse relations, method overrides, template members,
   references to third-party bases.
6. **Link.** `link_constellation` runs across every indexed project and adds
   the edges that cross repository boundaries.

Steps 4 through 6 read the whole project (or the whole constellation) and are
therefore the expensive half. Step 3 is the throughput bottleneck, being
serial against SQLite.

### Serving: `constellation serve`

The agent speaks MCP over stdio. A tool call reaches `ConstellationServer`,
which runs one or more `Store` queries, ranks the result, and renders it as
text. Query work is synchronous and blocking, dispatched off the async runtime,
because it is SQLite reads rather than I/O worth awaiting.

A background watcher re-indexes changed files while the server runs, so the
graph a long session queries stays current without a restart.

## Invariants

These hold everywhere. A change that breaks one is a change to the
architecture, not a bug fix.

- **A `NodeId` is `{project}::{qualified_name}`.** A `ProjectId` may never
  contain the separator, so the project prefix is always recoverable. One
  database holds every project's graph without collision.
- **A cross-project edge is an ordinary edge.** It is only distinguished by its
  endpoints carrying different project prefixes. There is no separate table and
  no separate type, and query code should not special-case one.
- **Extraction is pure and per-file.** Same source, same output, in any order,
  with no reference to any other file.
- **A resolved reference is archived, not discarded.** Turning a reference into
  an edge moves it to `resolved_refs` rather than deleting it, because the edge
  is not permanent: re-indexing the file an edge points *into* deletes that
  file's nodes and the cascade takes the inbound edges with them, and the files
  that wrote those edges have not changed and so are never re-extracted.
  Clearing a file therefore requeues the archived references that pointed into
  it, and the resolution pass rebuilds what the cascade took. Without it a
  file's dependents lose their edges into it permanently, one file at a time,
  every time it is touched.
- **The schema is rebuilt when it cannot be extended.** Every statement in
  `schema.sql` is `CREATE ... IF NOT EXISTS`, so adding a table, an index, or a
  nullable column re-applies onto an existing database and keeps its rows;
  `SCHEMA_VERSION` records the shape and `SCHEMA_VERSION_MIN` records the oldest
  one this build can read, and raising the latter drops and recreates rather
  than migrating in place. Raise it only for a change SQLite cannot express
  against an existing database. The index is a derived artifact and re-deriving
  it is cheaper than being wrong about it.
- **The watcher converges to a cold index.** A store that has been watched
  through arbitrary edits must end up holding exactly what indexing the final
  tree from scratch would hold. This is the oracle the watcher test suite
  checks, and it is the reason those tests compare file sets and counts rather
  than row dumps.
- **Derived data degrades, it does not lie.** Git history and execution flows are
  optional. A tool that wants one and does not have it says which command to run;
  it never renders missing data as empty data.
- **Everything is bounded.** Every loop over external input has a stated
  maximum, named `*_MAX` and asserted against. A file cap, a node-per-file cap,
  a reference cap, a traversal cap. Unbounded work on untrusted input is not
  acceptable anywhere in this tree.

## Where do I add X

| Change | Goes in |
|---|---|
| Capture more Python syntax | `crates/extraction/src/python.rs` |
| Capture more template syntax | `crates/extraction/src/django/`, then `template.rs` |
| A new node or edge kind | `crates/graph/src/node.rs` or `edge.rs`, then whoever emits it, then the store round-trip |
| A Django pattern static parsing misses | `crates/resolution/src/frameworks.rs` |
| A new stored table or query | `crates/store/src/schema.sql` and `store.rs` |
| A new derived pass over the resolved graph | `crates/index/src/` as its own module |
| A new MCP tool | `crates/mcp/src/`, one module per tool family |
| A new CLI subcommand | `crates/cli/src/main.rs`, one function per command |
| A new retrieval benchmark | `crates/eval/src/benchmarks/` |
| A fixture pinning extractor or tool output | the `snapshot` module of that crate's `tests/it/` |
| A new build or check task | `xtask/src/` |

If a change does not fit a row above, it is probably crossing a boundary, and
the question to answer first is which layer owns it.

## Testing

- **Unit tests** live inline in a `#[cfg(test)] mod` beside the code, and are
  for pure functions whose contract is local: a parser of a date string, a
  ranking comparator, a path predicate.
- **Integration tests** live in exactly one binary per crate, at
  `tests/it/main.rs`, with one module per area. Cargo compiles and links every
  file directly under `tests/` as a separate binary, so N files is N links and N
  copies of every shared fixture; one binary with N modules is one link.
- **Snapshot tests** pin whole outputs rather than named facts, and exist on the
  two surfaces where the output *is* the product: `extraction`, where a run is a
  pure function of one file's source, and `mcp`, where the rendered text is what
  an agent actually reads. Both live in a `snapshot` module of their crate's
  suite. They answer the question an assertion cannot, which is not "is this
  line right" but "did anything move", and they are what makes a refactor of a
  3,000-line parser or of the ranking layer reviewable rather than hopeful.

  An assertion and a snapshot are not alternatives. An assertion states why a
  value is what it is and survives being reread a year later; a snapshot states
  only that the value has not changed. A behaviour worth naming gets an
  assertion next to the code that produces it; the surrounding hundred that
  nobody would think to name get a snapshot. Adding a snapshot is not a reason
  to delete an assertion.

  The rule that keeps them honest: **a snapshot is updated only after its diff
  has been read.** `cargo test` writes a `.snap.new` beside each miss and
  `cargo insta review` walks the diffs one at a time. `INSTA_UPDATE=always`
  rewrites them unread, which is correct exactly once, when a fixture is first
  added. CI pins `INSTA_UPDATE=no` so a run can never rewrite its own
  expectations.

  Anything volatile is normalized before it reaches a snapshot, not tolerated
  in one. The mcp suite masks temporary roots and elapsed times in one place
  (`fixture::Fixture::render`), because a snapshot that flakes is worse than no
  snapshot: it teaches whoever hits it to accept diffs without reading them,
  which is the only failure mode that makes the whole suite worthless.
- **The convergence oracle** in the index test suite is shared fixture code, not
  a helper: the watcher tests are all one invariant stressed different ways.
- **The eval harness** (`crates/eval`) is the quality ratchet. CI indexes a
  fixture project, runs the harness, and fails if retrieval quality drops below
  a recorded floor.
- **`cargo xtask tidy`** is the style ratchet, and the only formatting gate. It
  counts known style debt and fails if a count moves in either direction, so debt
  can only be paid down and progress is recorded when it happens.
- **rustfmt is not used**, and CI installs no rustfmt component. The project's
  whitespace rule separates a function's logical steps with blank lines, which
  rustfmt neither inserts nor reliably preserves, so running it would erase the
  rule it appears to enforce. Formatting is hand-applied and gated by `tidy`
  alone.

## Non-goals

Named here because they are the questions that recur.

- Languages outside the target stack. Python, Django templates, JavaScript, and
  CSS, and nothing else.
- Frameworks outside Django. No Flask, FastAPI, Rails, or Spring route
  detection.
- Mobile or cross-platform bridging of any kind.
- npm or pip distribution.
- In-place schema migration. See the invariant above.
