//! `constellation_impact`, `constellation_tests`,
//! `constellation_subclasses`, and `constellation_orphans`: what a change
//! reaches, and what nothing reaches.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, Node, NodeId, NodeKind, is_covering_ref, is_test_path,
};
use constellation_store::{Store, StoreError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::limits::{
    IMPACT_DEPTH_MAX, IMPACT_LEVEL_LINES_MAX, IMPACT_LINES_MAX, IMPACT_NODES_MAX,
    ORPHAN_SCAN_MAX, SUBCLASS_HOPS_MAX, TESTS_SYMBOLS_MAX,
};
use crate::rank::{edge_rank, listing_order, listing_rank};
use crate::render::{dedup_related, node_line, summarize_kinds};
use crate::symbols::{is_coverage_checkable, is_orphan_candidate, targetable_name};
use crate::tools::history::find_project;
use crate::tools::search::seed_nodes;
use crate::cursor;

/// The tests covering a symbol: `TestCase` classes the extractor bound to it by the
/// `XTestCase -> X` convention (a `Tests` edge), plus any reference to it from a test
/// module (a call, or the instantiation Django model tests use). `(no covering tests)`
/// when none. The signal an LLM needs before editing: what to run, and whether guarded.
#[doc(hidden)]
pub fn tests_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut out = String::new();

    for node in nodes.iter().take(TESTS_SYMBOLS_MAX) {
        let mut covering = store.callers(&node.id)?;

        covering.retain(|(kind, caller)| is_covering_ref(*kind, &caller.file_path));

        let covering = dedup_related(covering);

        let _ = writeln!(out, "{}", node_line(node));

        if covering.is_empty() {
            append_uncovered_note(store, node, &mut out)?;
        }

        for (kind, test, _count) in covering.iter().take(limit as usize) {
            let _ = writeln!(out, "  [{}] {}", kind.as_str(), node_line(test));
        }
    }

    if nodes.len() > TESTS_SYMBOLS_MAX {
        let withheld = nodes.len() - TESTS_SYMBOLS_MAX;

        let _ = writeln!(out,
            "({withheld} more definition(s) named {symbol:?} not shown; \
             pass Owner.member to target one)",
        );
    }

    Ok(out)
}

/// The honest "nothing covers this" line for a symbol with no covering reference.
///
/// A member (a method, a property, a nested `Meta`) is only ever reached by an
/// attribute read or a descriptor call, neither of which leaves an edge, so an empty
/// caller set is under-detection rather than evidence. Saying `(no covering tests)`
/// flatly there is the most dangerous output this server can produce: a reader edits
/// or deletes guarded code on the strength of it. So a member says what it does not
/// know, and points at the owner whose coverage is measurable.
fn append_uncovered_note(store: &Store, node: &Node, out: &mut String) -> Result<(), StoreError> {
    if is_coverage_checkable(node) {
        out.push_str("  (no covering tests)\n");

        return Ok(());
    }

    let Some(owner) = owner_node(store, node)? else {
        out.push_str("  (no covering tests)\n");

        return Ok(());
    };

    let mut covering = store.callers(&owner.id)?;
    covering.retain(|(kind, caller)| is_covering_ref(*kind, &caller.file_path));

    if covering.is_empty() {
        let _ = writeln!(out,
            "  (no covering tests, and none on its owner {} either)",
            owner.name,
        );

        return Ok(());
    }

    let covering = dedup_related(covering);

    out.push_str(
        "  (no test binds to this member directly; member-level coverage is under-detected, \
         because a `self.member` read or a `.services.member()` call leaves no edge)\n",
    );

    let _ = writeln!(out,
        "  its owner {} has {} covering test reference(s): `tests {}`",
        owner.name,
        covering.len(),
        targetable_name(&owner),
    );

    Ok(())
}

/// The class or model that declares a member, found by trimming the member off its
/// qualified name (`app/x.py::Order.total` -> `app/x.py::Order`). `None` for a
/// top-level definition, which has no owner to fall back to.
fn owner_node(store: &Store, node: &Node) -> Result<Option<Node>, StoreError> {
    let Some((file, tail)) = node.qualified_name.rsplit_once("::") else {
        return Ok(None);
    };

    let Some((owner, _)) = tail.rsplit_once('.') else {
        return Ok(None);
    };

    let mut owners = store.nodes_qualified(&format!("{file}::{owner}"))?;
    owners.sort_by(listing_order);

    Ok(owners.into_iter().next())
}

/// The transitive subclasses of a base class or mixin: every node reached by following
/// incoming `Extends` edges breadth-first, so a deep mixin tree (`HistoryModelMixin`,
/// `BaseDjangoModelService`) comes back whole, across projects. Bounded by hops and
/// `limit`.
#[doc(hidden)]
pub fn subclasses_text(
    store: &Store,
    symbol: &str,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let seeds = seed_nodes(store, symbol)?;

    if seeds.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut visited: FxHashSet<String> =
        seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut frontier: Vec<NodeId> = seeds.iter().map(|node| node.id.clone()).collect();
    let mut found: Vec<Node> = Vec::new();
    let mut hops: u32 = 0;
    let mut deeper_unwalked = false;

    // Enough for the requested page and one more, so the cursor line can be emitted
    // truthfully without walking a 200-subclass tree to its leaves on every call.
    let ceiling = page.offset.saturating_add(limit as usize).saturating_add(1);

    while !frontier.is_empty() && hops < SUBCLASS_HOPS_MAX {
        if found.len() >= ceiling {
            deeper_unwalked = true;

            break;
        }

        hops += 1;

        let mut next: Vec<NodeId> = Vec::new();

        for id in &frontier {
            for (kind, child) in store.callers(id)? {
                if kind != EdgeKind::Extends {
                    continue;
                }

                if visited.insert(child.id.as_str().to_string()) {
                    next.push(child.id.clone());
                    found.push(child);
                }
            }
        }

        frontier = next;
    }

    let window = cursor::slice(&found, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out,
        "subclasses of {symbol} ({} shown of {} found):",
        window.len(),
        found.len(),
    );

    if window.is_empty() {
        out.push_str("  (none)\n");
    }

    for node in window {
        let _ = writeln!(out, "  {}", node_line(node));
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), found.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    // A breadth-first walk stopped once the page was full has seen the shallow
    // levels only. Saying so keeps "found" from reading as the whole closure.
    if deeper_unwalked {
        out.push_str(
            "(walk stopped once this page filled; deeper levels of the tree were not \
             expanded, so the count is a lower bound)\n",
        );
    }

    Ok(out)
}

/// The candidate dead code in one project: definitions with no incoming edge but
/// structural containment, after dropping framework-reached symbols that legitimately
/// lack a static caller. Scoped to one project; over-fetched then path/name-filtered so
/// `limit` rows of real candidates come back.
#[doc(hidden)]
pub fn orphans_text(
    store: &Store,
    project: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let project_id = match project {
        Some(name) => match find_project(store, name)? {
            Some(id) => id,
            None => return Ok(format!("no project named {name:?}")),
        },
        None => {
            return Ok("pass project=<id or name>: orphans is scoped to one project".to_string());
        }
    };

    // Scan the project, not the page: the framework and dispatch filters reject
    // most rows, and a page-sized fetch answered "the dead code in this project"
    // from the first few dozen files in path order.
    let fetched = store.orphan_definitions(&project_id, ORPHAN_SCAN_MAX)?;
    let scanned = u32::try_from(fetched.len()).unwrap_or(u32::MAX);

    let mut candidates: Vec<Node> = Vec::new();
    let mut dispatched: usize = 0;

    for node in fetched.into_iter().filter(is_orphan_candidate) {
        // A method reached only through a manager/service descriptor (`.objects.by_pk()`,
        // `.services.x()`) has no static caller edge but does have a dark (unresolved)
        // reference by name: it is dispatched dynamically, not dead. Asked of this
        // project only: another repository dispatching on a name as common as
        // `process` or `total` says nothing about this project's definition, and
        // asking constellation-wide silently swallowed most real candidates.
        if store.count_unresolved_named_in(&project_id, &node.name)? == 0 {
            candidates.push(node);
        } else {
            dispatched += 1;
        }
    }

    let window = cursor::slice(&candidates, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out,
        "orphan candidates in {project_id} ({} shown of {}, verify before deleting):",
        window.len(),
        candidates.len(),
    );

    if window.is_empty() {
        out.push_str("  (none)\n");
    }

    for node in window {
        let _ = writeln!(out, "  {}", node_line(node));
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), candidates.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    // What the scan withheld, so a short listing is not read as a clean bill of
    // health: a filtered candidate is a judgement call, not an absence.
    if dispatched > 0 {
        let _ = writeln!(out,
            "({dispatched} definition(s) withheld: their names appear as unbound dynamic calls in \
             this project, so they may be reached at runtime)",
        );
    }

    if scanned >= ORPHAN_SCAN_MAX {
        let _ = writeln!(out,
            "(scan capped at {ORPHAN_SCAN_MAX} uncalled definitions in path order; \
             later files were not examined)",
        );
    }

    Ok(out)
}

/// The blast radius of a change to `symbol`, rendered: the transitive non-test callers
/// walked breadth-first to `depth`, strongest edge and cross-project hops first, paged
/// by cursor. Type and schema associations are counted apart from callers and expanded
/// one hop only, because a neighbour of a neighbour is a fact about the neighbourhood
/// rather than about the symbol under change.
#[doc(hidden)]
pub fn impact_text(
    store: &Store,
    symbol: &str,
    depth: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    assert!(depth <= IMPACT_DEPTH_MAX, "traversal depth is capped");

    let seeds = seed_nodes(store, symbol)?;

    if seeds.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut visited: FxHashSet<String> = seeds.iter().map(|node| node.id.as_str().to_string()).collect();
    let mut frontier: Vec<NodeId> = seeds.iter().map(|node| node.id.clone()).collect();
    let mut out = String::new();

    // Collected rather than streamed, so a cursor can page into the tail of a
    // large blast radius instead of the caller having to lower the depth.
    let mut rows: Vec<String> = Vec::new();

    // The home projects of the seeds: a caller outside all of them is a
    // cross-project hop, surfaced ahead of same-project callers within each tier.
    let seed_projects: FxHashSet<String> =
        seeds.iter().map(|node| node.project_id.as_str().to_string()).collect();

    if seeds.len() > 1 {
        let _ = writeln!(out,
            "{} definitions of {symbol:?} ({}): blast radii merged; narrow with Owner.member.",
            seeds.len(),
            summarize_kinds(&seeds),
        );
    }

    let mut level: u32 = 0;
    let mut printed: usize = 0;
    let mut omitted: usize = 0;
    let mut tests_omitted: usize = 0;
    let mut reached: usize = 0;
    let mut associated: usize = 0;

    // A blast radius larger than [`IMPACT_NODES_MAX`] stops the walk rather than
    // failing it: every other bound in this crate truncates and says so, and a
    // hub symbol in a large constellation is a legitimate query, not a defect.
    let mut walk_truncated = false;

    while level < depth && !frontier.is_empty() && !walk_truncated {
        level += 1;

        let mut callers: Vec<(EdgeKind, Node)> = Vec::new();

        for id in &frontier {
            callers.extend(store.callers(id)?);
        }

        // Drop containment and the file-level `imports` edges that double every
        // hub's caller list: when a file imports the symbol, the symbol inside
        // that file which actually extends/calls it is already a caller, so the
        // File node is redundant noise, never a meaningful blast-radius entry.
        callers.retain(|(kind, caller)| *kind != EdgeKind::Contains && caller.kind != NodeKind::File);
        callers.sort_by(|(left_kind, left), (right_kind, right)| {
            let rank = |kind: &EdgeKind, node: &Node| {
                let cross = u8::from(seed_projects.contains(node.project_id.as_str()));

                (edge_rank(*kind), cross, listing_rank(node))
            };

            rank(left_kind, left)
                .cmp(&rank(right_kind, right))
                .then_with(|| listing_order(left, right))
        });

        let mut next: Vec<NodeId> = Vec::new();
        let mut printed_this_level: usize = 0;

        for (kind, caller) in callers {
            if is_test_path(&caller.file_path) {
                tests_omitted += 1;
                continue;
            }

            if visited.len() >= IMPACT_NODES_MAX {
                walk_truncated = true;
                break;
            }

            if !visited.insert(caller.id.as_str().to_string()) {
                continue;
            }

            if is_reach_edge(kind) {
                next.push(caller.id.clone());
                reached += 1;
            } else {
                associated += 1;

                // One hop out, "what relates to this" names the neighborhood and is
                // worth seeing. Two hops out it is a fact about a neighbor, not
                // about the symbol under change: a service reached through the model
                // that declares it would otherwise report every foreign key pointing
                // at that model as its own blast radius.
                if level > 1 {
                    continue;
                }
            }

            if printed < IMPACT_LINES_MAX && printed_this_level < IMPACT_LEVEL_LINES_MAX {
                rows.push(format!("L{level} [{}] {}", kind.as_str(), node_line(&caller)));
                printed += 1;
                printed_this_level += 1;
            } else {
                omitted += 1;
            }
        }

        frontier = next;
    }

    let window = cursor::slice(&rows, page.offset, IMPACT_LINES_MAX);

    for row in window {
        out.push_str(row);
        out.push('\n');
    }

    if let Some(next) = cursor::next_line(page.offset, window.len(), rows.len(), generation) {
        out.push_str(&next);
        out.push('\n');
    }

    if printed == 0 && omitted == 0 {
        out.push_str("no non-test transitive callers\n");
    }

    if omitted > 0 {
        let _ = writeln!(out,
            "(+{omitted} more transitive callers; inspect a specific caller, or lower depth)",
        );
    }

    if tests_omitted > 0 {
        let _ = writeln!(out, "({tests_omitted} test caller(s) omitted)");
    }

    if walk_truncated {
        let _ = writeln!(out,
            "(walk stopped at {IMPACT_NODES_MAX} nodes; narrow the seed with Owner.member)",
        );
    }

    let mut header = String::new();

    if let Some(note) = &page.note {
        header.push_str(note);
        header.push('\n');
    }

    // Counted apart, because they answer different questions. A caller runs this
    // symbol's code and a change can break it; a model with a foreign key to the
    // model that declares it merely sits nearby. Reporting one total conflates
    // "83 things depend on this" with "this lives in a busy neighborhood".
    let _ = write!(header, "impact of {symbol} (depth {depth}): {reached} non-test caller(s)");

    if associated > 0 {
        let _ = write!(header,
            ", plus {associated} type/schema association(s) (first hop only, not expanded)",
        );
    }

    header.push('\n');

    Ok(format!("{header}{out}"))
}

/// Whether an incoming edge means this symbol's behavior is reached through the
/// caller, the relation a blast radius keeps expanding along.
///
/// A schema relation (`relates_to`), a type mention (`type_of`, `returns`), or a
/// template/attribute association is a fact about the neighborhood, not a path a
/// change propagates down. Expanding through them turns "what calls this service"
/// into "everything that touches the model that declares it": a service is reached
/// from its model by one `instantiates` edge (`services = XService()`), and from
/// there every foreign key, queryset return type, and annotation in the app follows.
fn is_reach_edge(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls
            | EdgeKind::Decorates
            | EdgeKind::Extends
            | EdgeKind::ExtendsTemplate
            | EdgeKind::Handles
            | EdgeKind::Imports
            | EdgeKind::IncludesTemplate
            | EdgeKind::Instantiates
            | EdgeKind::Overrides
            | EdgeKind::OverridesTemplate
            | EdgeKind::Receives
            | EdgeKind::References
            | EdgeKind::Renders
            | EdgeKind::Resolves
            | EdgeKind::RoutesTo
    )
}

/// A map from every project's id to its filesystem root, so explore can read source
/// files after releasing the store lock.
pub(crate) fn project_roots(store: &Store) -> Result<FxHashMap<String, String>, StoreError> {
    let projects = store.all_projects()?;

    let mut roots: FxHashMap<String, String> =
        FxHashMap::with_capacity_and_hasher(projects.len(), Default::default());

    for row in projects {
        roots.insert(row.id.as_str().to_string(), row.root_path);
    }

    Ok(roots)
}
