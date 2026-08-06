//! `constellation_node`, `constellation_model`, `constellation_callers`,
//! `constellation_callees`, and `constellation_at`: one symbol, in detail.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, Node, NodeKind, ProjectId,
};
use constellation_store::{Store, StoreError};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::limits::{
    AT_RESULTS_MAX, CALL_SITE_SNIPPET_CHARS_MAX, MODEL_MRO_DEPTH_MAX, MODEL_NODES_MAX,
    NODE_CALLERS_INLINE_MAX, NODE_DETAIL_MAX,
};
use crate::rank::{cross_project_rank, edge_rank, listing_order, listing_rank};
use crate::render::{dedup_related, edge_kind_breakdown, load_source, node_line, summarize_kinds};
use crate::symbols::{
    field_signature, is_dispatch_method_name, python_import_line, symbol_role, targetable_name,
};
use crate::tools::impact::project_roots;
use crate::tools::search::seed_nodes;

/// A symbol in detail, rendered: location, role, signature, import line, docstring,
/// deduped caller and callee counts, the dark-caller trust line, and its top callers
/// inline. An overloaded name renders the leading definitions and says how to narrow,
/// rather than picking one.
#[doc(hidden)]
pub fn node_text(store: &Store, symbol: &str) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let shown = nodes.len().min(NODE_DETAIL_MAX);
    let unambiguous = nodes.len() == 1;
    let mut out = String::new();

    for node in &nodes[..shown] {
        node_detail(&mut out, store, node, unambiguous)?;
    }

    if nodes.len() > shown {
        let _ = writeln!(out,
            "(+{} more: {})",
            nodes.len() - shown,
            summarize_kinds(&nodes[shown..]),
        );
    }

    if nodes.len() > 1 {
        let shown_narrow = nodes.len().min(6);
        let narrow: Vec<&str> = nodes.iter().take(shown_narrow).map(targetable_name).collect();
        let suffix = if nodes.len() > shown_narrow { ", …" } else { "" };

        let _ = writeln!(out,
            "{} matches: narrow with one of: {}{suffix}",
            nodes.len(),
            narrow.join(", "),
        );

        // The dark-caller count is keyed by name, so it is identical across these
        // same-named overloads: print it once here rather than on every row. It
        // cannot be attributed to a single overload (that is what "unresolved" means).
        let dark = store.count_unresolved_named(&nodes[0].name)?;

        if dark > 0 {
            if is_dispatch_method_name(&nodes[0].name) {
                let _ = writeln!(out,
                    "note: {:?} is a common method name; {dark} unbound dynamic-dispatch call(s) \
                     workspace-wide share it, not callers of any one of these overloads",
                    nodes[0].name,
                );
            } else {
                let _ = writeln!(out,
                    "dark callers (name-global): {dark} unresolved reference(s) name {:?} \
                     (dynamic dispatch or missing import); not attributable to a single overload",
                    nodes[0].name,
                );
            }
        }
    }

    Ok(out)
}

/// The detail block for one symbol, rendered into `out`: location, signature, docstring,
/// deduped caller/callee counts, the dark-caller trust line, and (for an
/// `unambiguous` symbol) its top callers inline. Extracted from [`node_text`]
/// so each stays one logical unit under the line bound.
fn node_detail(
    out: &mut String,
    store: &Store,
    node: &Node,
    unambiguous: bool,
) -> Result<(), StoreError> {
    assert!(!node.name.is_empty(), "node name must not be empty");

    let _ = writeln!(out, "{}", node_line(node));

    if let Some(role) = symbol_role(node) {
        let _ = writeln!(out, "  role: {role}");
    }

    if let Some(signature) = &node.signature {
        let _ = writeln!(out, "  signature: {signature}");
    }

    if let Some(import) = python_import_line(node) {
        let _ = writeln!(out, "  import: {import}");
    }

    if let Some(docstring) = &node.docstring {
        let _ = writeln!(out, "  doc: {}", docstring.lines().next().unwrap_or(""));
    }

    let mut callers = store.callers(&node.id)?;
    callers.retain(|(kind, _)| *kind != EdgeKind::Contains);
    let callers = dedup_related(callers);

    let mut callees = store.callees(&node.id)?;
    callees.retain(|(kind, _)| *kind != EdgeKind::Contains);
    let callees = dedup_related(callees);

    // The unresolved count belongs on the counts line, not only in the note below
    // it: a bare `callers: 0` on a symbol reached by 500 dynamic call sites reads
    // as dead code, and that is the number a reader acts on.
    let dark = store.count_unresolved_named(&node.name)?;

    let dark_suffix = if dark > 0 {
        format!(" resolved (+{dark} unresolved by name)")
    } else {
        String::new()
    };

    let _ = writeln!(out,
        "  callers: {}{}{dark_suffix}  callees: {}{}",
        callers.len(),
        edge_kind_breakdown(&callers),
        callees.len(),
        edge_kind_breakdown(&callees),
    );

    // Dark-caller trust signal: references that named this symbol but never bound
    // to an edge (dynamic dispatch, a missing import). A non-zero count means the
    // resolved caller count above understates real usage. Shown inline only for an
    // unambiguous symbol; for an overloaded name the count is name-global (the
    // same for every overload), so node_text prints it once after the listing
    // rather than repeating an identical line on every row.
    if unambiguous && dark > 0 {
        if is_dispatch_method_name(&node.name) {
            let _ = writeln!(out,
                "  note: {:?} is a common method name; {dark} unbound dynamic-dispatch call(s) \
                 workspace-wide share it, not necessarily callers of this symbol",
                node.name,
            );
        } else {
            let _ = writeln!(out,
                "  dark callers: {dark} unresolved reference(s) name {:?} (dynamic dispatch or \
                 missing import); resolved caller count understates usage",
                node.name,
            );
        }
    }

    // For an unambiguous symbol, list its top callers inline (strongest relations
    // first, then non-test source) so the common "who uses this" question needs no
    // follow-up callers call. Skipped for an overloaded name to keep the
    // multi-match summary compact.
    if unambiguous && !callers.is_empty() {
        let home = node.project_id.as_str();
        let mut ranked = callers.clone();
        ranked.sort_by(|(left_kind, left, _), (right_kind, right, _)| {
            let rank = |kind: &EdgeKind, node: &Node| {
                (edge_rank(*kind), cross_project_rank(node, home), listing_rank(node))
            };

            rank(left_kind, left)
                .cmp(&rank(right_kind, right))
                .then_with(|| listing_order(left, right))
        });

        out.push_str("  called by:\n");

        for (kind, other, count) in ranked.iter().take(NODE_CALLERS_INLINE_MAX) {
            let times = if *count > 1 { format!(" ×{count}") } else { String::new() };

            let _ = writeln!(out, "    [{}{}] {}", kind.as_str(), times, node_line(other));
        }

        if ranked.len() > NODE_CALLERS_INLINE_MAX {
            let _ = writeln!(out,
                "    (+{} more; use `callers` for the rest)",
                ranked.len() - NODE_CALLERS_INLINE_MAX,
            );
        }
    }

    Ok(())
}

/// A Django model's effective schema, assembled: own fields, fields inherited up the
/// base-class chain, the bases, and relations to other models. Walks Extends edges
/// upward (bounded), gathering each base's Contains-Field members so an abstract
/// base's or mixin's columns appear on the concrete model. A subclass field shadows
/// a base field of the same name. Relations are deduped across the whole chain.
#[doc(hidden)]
pub fn model_text(store: &Store, symbol: &str) -> Result<String, StoreError> {
    let seeds = seed_nodes(store, symbol)?;

    let models: Vec<Node> =
        seeds.into_iter().filter(|node| matches!(node.kind, NodeKind::Model | NodeKind::Class)).collect();

    if models.is_empty() {
        return Ok(format!("no model or class named {symbol:?}"));
    }

    // A class that declares no fields and inherits none has no schema to assemble.
    // Rendering the empty sections anyway answers "this model has no fields", which
    // for a form, a service, or a queryset is not true but is indistinguishable from
    // a real answer; say what the symbol is instead and name the tool that reads it.
    let all_plain = models.iter().all(|node| node.kind == NodeKind::Class);

    if all_plain && !any_model_schema(store, &models)? {
        let roles: Vec<String> = models
            .iter()
            .map(|node| format!("{} ({})", node.name, symbol_role(node).unwrap_or("class")))
            .collect();

        // No `next:` line of its own. The router appends the hint, and two of them
        // in one response is the rule this server's hints are built against: a
        // reader given two follow-ups has been given none.
        return Ok(format!(
            "{} declares no model fields: not a Django model.\n\
             Read it with `node` for its signature, or `callers` for what uses it.\n",
            roles.join(", "),
        ));
    }

    let mut out = String::new();

    for model in &models {
        let _ = writeln!(out, "{}", node_line(model));

        let mut visited: FxHashSet<String> = FxHashSet::default();
        visited.insert(model.id.as_str().to_string());

        let mut frontier: Vec<(Node, u32)> = vec![(model.clone(), 0)];
        let mut bases: Vec<Node> = Vec::new();
        let mut own_fields: Vec<Node> = Vec::new();
        let mut inherited_fields: Vec<(Node, String)> = Vec::new();
        let mut relations: Vec<(Node, RelationDir)> = Vec::new();
        let mut relation_ids: FxHashSet<String> = FxHashSet::default();
        let mut walked: usize = 0;

        // An inheritance chain wider than [`MODEL_NODES_MAX`] stops the walk and
        // says so, rather than failing the whole response: a schema assembled from
        // the first cap-many nodes is a partial answer, and a partial answer the
        // reader can see is bounded beats none.
        let mut walk_truncated = false;

        while let Some((node, depth)) = frontier.pop() {
            if walked >= MODEL_NODES_MAX {
                walk_truncated = true;
                break;
            }

            walked += 1;

            let reverse_targets: FxHashSet<String> =
                store.reverse_relation_targets(&node.id)?.into_iter().collect();

            for (kind, other) in store.callees(&node.id)? {
                match kind {
                    EdgeKind::Contains if other.kind == NodeKind::Field => {
                        if depth == 0 {
                            own_fields.push(other);
                        } else {
                            inherited_fields.push((other, node.name.clone()));
                        }
                    }
                    EdgeKind::RelatesTo => {
                        if relation_ids.insert(other.id.as_str().to_string()) {
                            let direction = if reverse_targets.contains(other.id.as_str()) {
                                RelationDir::Reverse
                            } else {
                                RelationDir::Forward
                            };

                            relations.push((other, direction));
                        }
                    }
                    EdgeKind::Extends
                        if depth < MODEL_MRO_DEPTH_MAX
                            && visited.insert(other.id.as_str().to_string()) =>
                    {
                        bases.push(other.clone());
                        frontier.push((other, depth + 1));
                    }
                    _ => {}
                }
            }
        }

        // Declaration order, because the reader is comparing this against the
        // model file. The walk yields fields in edge order, which came out as
        // reverse source order and reads as though it means something.
        own_fields.sort_by_key(|field| field.span.start_line);

        inherited_fields.sort_by(|(left, left_base), (right, right_base)| {
            left_base.cmp(right_base).then(left.span.start_line.cmp(&right.span.start_line))
        });

        render_model_sections(&mut out, &bases, &own_fields, &inherited_fields, &relations);

        if walk_truncated {
            let _ = writeln!(out,
                "  (walk stopped at {MODEL_NODES_MAX} nodes; the schema above is partial)",
            );
        }
    }

    Ok(out)
}

/// Whether any of `models` reaches a model field, its own or one a base declares.
/// One level of bases is enough to tell a fieldless helper class from a model
/// whose fields all live in a mixin, without paying for the full MRO walk twice.
fn any_model_schema(store: &Store, models: &[Node]) -> Result<bool, StoreError> {
    for model in models {
        for (kind, other) in store.callees(&model.id)? {
            match kind {
                EdgeKind::Contains if other.kind == NodeKind::Field => return Ok(true),
                EdgeKind::Extends => {
                    let inherited = store
                        .callees(&other.id)?
                        .into_iter()
                        .any(|(kind, node)| kind == EdgeKind::Contains && node.kind == NodeKind::Field);

                    if inherited {
                        return Ok(true);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(false)
}

/// The direction of a model relation: outward (a ForeignKey/M2M this model declares)
/// or back (the reverse accessor a model that targets this one creates, the
/// synthesized reverse-relation edge). `model` labels each so a reader tells
/// `inventory.brand` (this model's own FK) from the reverse side a related model
/// exposes, a direction the undifferentiated `relates_to` edge set, where both are
/// outgoing edges, otherwise hides.
#[derive(Clone, Copy)]
enum RelationDir {
    Forward,
    Reverse,
}

impl RelationDir {
    fn arrow(self) -> &'static str {
        match self {
            RelationDir::Forward => "->",
            RelationDir::Reverse => "<-",
        }
    }
}

/// The assembled sections of one model: bases, own then inherited fields (a
/// base field shadowed by an own field of the same name is dropped), and deduped
/// relations, each tagged with its direction.
fn render_model_sections(
    out: &mut String,
    bases: &[Node],
    own_fields: &[Node],
    inherited_fields: &[(Node, String)],
    relations: &[(Node, RelationDir)],
) {
    if bases.is_empty() {
        out.push_str("  bases: (none)\n");
    } else {
        let names: Vec<&str> = bases.iter().map(|base| base.name.as_str()).collect();

        let _ = writeln!(out, "  bases: {}", names.join(", "));
    }

    let own_names: FxHashSet<&str> = own_fields.iter().map(|field| field.name.as_str()).collect();
    let field_total = own_fields.len()
        + inherited_fields.iter().filter(|(field, _)| !own_names.contains(field.name.as_str())).count();

    let _ = writeln!(out, "  fields ({field_total}):");

    for field in own_fields {
        let _ = writeln!(out, "    [own] {}{}", field.name, field_signature(field));
    }

    let mut seen_inherited: FxHashSet<&str> = FxHashSet::default();

    for (field, base) in inherited_fields {
        if own_names.contains(field.name.as_str()) || !seen_inherited.insert(field.name.as_str()) {
            continue;
        }

        let _ = writeln!(out, "    [{base}] {}{}", field.name, field_signature(field));
    }

    if !relations.is_empty() {
        let _ = writeln!(out,
            "  relations ({}): [->] forward FK/M2M this model declares, [<-] reverse (a model that points here):",
            relations.len(),
        );

        for (related, direction) in relations {
            let _ = writeln!(out,
                "    [{}] {} ({}:{})",
                direction.arrow(),
                related.name,
                related.file_path,
                related.span.start_line,
            );
        }
    }
}

/// The references a symbol makes, rendered: its outgoing edges deduped per target and
/// ordered by edge strength then locality, capped by `limit`. The mirror of
/// [`callers_text`], and the "what does this depend on" half of a change's context.
#[doc(hidden)]
pub fn callees_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let mut out = String::new();

    for node in &nodes {
        let mut related = store.callees(&node.id)?;

        related.retain(|(kind, _)| *kind != EdgeKind::Contains);

        let home = node.project_id.as_str();
        let mut deduped = dedup_related(related);
        deduped.sort_by(|(left_kind, left, _), (right_kind, right, _)| {
            let rank = |kind: &EdgeKind, node: &Node| {
                (edge_rank(*kind), cross_project_rank(node, home), listing_rank(node))
            };

            rank(left_kind, left)
                .cmp(&rank(right_kind, right))
                .then_with(|| listing_order(left, right))
        });

        let _ = writeln!(out, "{}", node_line(node));

        if deduped.is_empty() {
            out.push_str("  (none)\n");
        }

        for (kind, other, count) in deduped.iter().take(limit as usize) {
            let times = if *count > 1 { format!(" ×{count}") } else { String::new() };

            let _ = writeln!(out, "  [{}{}] {}", kind.as_str(), times, node_line(other));
        }

        let bound: Vec<&str> = deduped.iter().map(|(_, other, _)| other.name.as_str()).collect();

        append_unresolved_callees(store, node, &bound, limit, &mut out)?;
    }

    Ok(out)
}

/// The unproven, name-matched callee names appended after a definition's precise
/// callees: the calls in its body the resolver could not bind (a `self.obj.services.x()`
/// descriptor hop, an untyped receiver). Labeled so the precise list stays trustworthy.
///
/// A name already listed as a resolved callee is dropped. Resolution is per call
/// site, not per name, so a body calling `error_json_response` twice can bind one
/// site and leave the other pending, and printing the name in both lists reads as
/// two different callees rather than one the reader has already been shown.
fn append_unresolved_callees(
    store: &Store,
    node: &Node,
    bound: &[&str],
    limit: u32,
    out: &mut String,
) -> Result<(), StoreError> {
    let unresolved = store.unresolved_callees_of(&node.id, limit)?;

    let mut pending: Vec<&(String, u32)> =
        unresolved.iter().filter(|(name, _)| !bound.contains(&name.as_str())).collect();

    pending.truncate(limit as usize);

    if pending.is_empty() {
        return Ok(());
    }

    out.push_str("  unresolved callees (name match, receiver type unproven):\n");

    for (name, line) in pending {
        let _ = writeln!(out, "    {name}  ({}:{line})", node.file_path);
    }

    Ok(())
}

/// The innermost symbol(s) at a file:line (the enclosing
/// function/method/class for a traceback frame or grep hit).
#[doc(hidden)]
pub fn at_text(store: &Store, file: &str, line: u32) -> Result<String, StoreError> {
    if file.is_empty() {
        return Ok("a file path is required".to_string());
    }

    if line == 0 {
        return Ok("line is 1-based".to_string());
    }

    let nodes = store.nodes_at(file, line)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol spans {file}:{line}"));
    }

    let mut out = format!("{file}:{line} (innermost first):\n");

    for node in nodes.iter().take(AT_RESULTS_MAX) {
        out.push_str(&node_line(node));
        out.push('\n');
    }

    Ok(out)
}

/// The callers of a symbol rendered with the source line of each call site (the "how is
/// this used" view, not just "who uses it"). Listed per call site (no dedup), so a
/// caller that references twice shows both lines; ordered by edge then locality,
/// capped by `limit`.
#[doc(hidden)]
pub fn callers_text(store: &Store, symbol: &str, limit: u32) -> Result<String, StoreError> {
    let nodes = seed_nodes(store, symbol)?;

    if nodes.is_empty() {
        return Ok(format!("no symbol named {symbol:?}"));
    }

    let roots = project_roots(store)?;
    let mut sources = SourceCache::default();
    let mut out = String::new();

    for node in &nodes {
        let home = node.project_id.as_str();
        let mut callers = store.callers_located(&node.id)?;
        callers.retain(|(kind, _, _)| *kind != EdgeKind::Contains);
        callers.sort_by(|(left_kind, left, _), (right_kind, right, _)| {
            let rank = |kind: &EdgeKind, node: &Node| {
                (edge_rank(*kind), cross_project_rank(node, home), listing_rank(node))
            };

            rank(left_kind, left)
                .cmp(&rank(right_kind, right))
                .then_with(|| listing_order(left, right))
        });

        let _ = writeln!(out, "{}", node_line(node));

        if callers.is_empty() {
            out.push_str("  (none)\n");
        }

        for (kind, caller, line) in callers.iter().take(limit as usize) {
            let _ = writeln!(out, "  [{}] {}", kind.as_str(), node_line(caller));

            if *line >= 1
                && let Some(snippet) = call_site_line(&mut sources, &roots, caller, *line)
            {
                let _ = writeln!(out, "      {}:{line}  {snippet}", caller.file_path);
            }
        }
    }

    // Whether the argument narrowed to a subset of the definitions sharing this
    // name. The unresolved bucket below is keyed by simple name, so under a
    // narrowed query it holds sites that dispatch on the *other* overloads too,
    // and presenting those as this one's callers is the misattribution `node`
    // already refuses to make.
    let narrowed = nodes.len() < seed_nodes(store, &nodes[0].name)?.len();

    append_unresolved_callers(store, &nodes, &roots, &mut sources, limit, narrowed, &mut out)?;

    Ok(out)
}

/// The unproven, name-matched caller sites appended after the precise callers: the
/// `Model.services.x()` / untyped-receiver / overloaded-or-base service calls the
/// resolver dropped rather than bind to a guessed definition. Surfaced under a clear
/// label so the precise edges stay trustworthy while a reader still sees the recall a
/// text search would. Matched by each seed's simple name; deduped across overloads.
fn append_unresolved_callers(
    store: &Store,
    nodes: &[Node],
    roots: &FxHashMap<String, String>,
    sources: &mut SourceCache,
    limit: u32,
    narrowed: bool,
    out: &mut String,
) -> Result<(), StoreError> {
    assert!(!nodes.is_empty(), "at least one seed node to match unresolved callers against");

    let mut names: Vec<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let mut home_projects: Vec<&ProjectId> = nodes.iter().map(|node| &node.project_id).collect();
    home_projects.sort_unstable_by_key(|project| project.as_str());
    home_projects.dedup_by_key(|project| project.as_str());

    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut sites: Vec<(Node, u32)> = Vec::new();

    // The symbol's own repositories are asked first and separately. A single
    // constellation-wide query orders by project id before truncating, so a name
    // dispatched widely in an alphabetically earlier repository fills the window
    // and the local call site, the one a reader is actually looking for, is the row
    // that gets cut.
    for name in &names {
        for project in &home_projects {
            for (caller, line) in store.unresolved_callers_of_in(project, name, limit)? {
                if seen.insert(format!("{}:{line}", caller.id.as_str())) {
                    sites.push((caller, line));
                }
            }
        }
    }

    let mut foreign: Vec<(Node, u32)> = Vec::new();

    if sites.is_empty() {
        for name in &names {
            for (caller, line) in store.unresolved_callers_of(name, limit)? {
                if seen.insert(format!("{}:{line}", caller.id.as_str())) {
                    foreign.push((caller, line));
                }
            }
        }
    }

    // A repository that merely reuses the name is a collision, not dispatch on
    // this symbol: cross-project sites are shown only when the symbol's own
    // repositories offer nothing, and are labelled when they are.
    let cross_project = sites.is_empty();

    if cross_project {
        sites = foreign;
    }

    if sites.is_empty() {
        return Ok(());
    }

    if cross_project {
        out.push_str(
            "  unresolved (name match in another repository, receiver type unproven; may be an \
             unrelated symbol of the same name):\n",
        );
    } else if narrowed {
        let _ = writeln!(out,
            "  unresolved (name-global on {:?}, receiver type unproven; other definitions share \
             this name, so these are NOT attributable to the overload above):",
            nodes[0].name,
        );
    } else {
        out.push_str(
            "  unresolved (name match, receiver type unproven, e.g. a Model.services.x() call):\n",
        );
    }

    for (caller, line) in sites.iter().take(limit as usize) {
        let _ = writeln!(out, "  [calls?] {}", node_line(caller));

        if let Some(snippet) = call_site_line(sources, roots, caller, *line) {
            let _ = writeln!(out, "      {}:{line}  {snippet}", caller.file_path);
        }
    }

    Ok(())
}

/// The caller files read while rendering one response, so a file holding several
/// call sites is read from disk once rather than once per row.
///
/// A caller listing is dominated by rows that share a file (a service called
/// from twenty places in one views module), and every miss is a whole-file read.
/// Scoped to one response, so it never outlives the request that filled it and
/// cannot serve a stale file to the next one. A failed read is cached as `None`,
/// so an unreadable file is not retried per row either.
#[derive(Default)]
struct SourceCache {
    by_path: FxHashMap<String, Option<String>>,
}

impl SourceCache {
    /// A node's source file, read on the first request for it and reused after.
    fn get(&mut self, roots: &FxHashMap<String, String>, node: &Node) -> Option<&str> {
        let key = format!("{}\u{0}{}", node.project_id.as_str(), node.file_path);

        self.by_path
            .entry(key)
            .or_insert_with(|| load_source(roots, node))
            .as_deref()
    }
}

/// The trimmed source of one line in a caller's file, capped, for a call-site
/// snippet. `None` when the file is unreadable or the line is blank.
fn call_site_line(
    sources: &mut SourceCache,
    roots: &FxHashMap<String, String>,
    node: &Node,
    line: u32,
) -> Option<String> {
    assert!(line >= 1, "a call site line is 1-based");

    let source = sources.get(roots, node)?;
    let text = source.lines().nth((line - 1) as usize)?.trim();

    if text.is_empty() {
        return None;
    }

    Some(text.chars().take(CALL_SITE_SNIPPET_CHARS_MAX).collect())
}
