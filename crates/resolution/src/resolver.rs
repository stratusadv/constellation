use std::sync::Arc;

use constellation_graph::{Edge, EdgeKind, Language, Node, NodeKind};

use crate::context::ResolutionContext;
use crate::refs::{ResolvedBy, ResolvedRef, UnresolvedRef};

/// The sentinel candidate the extractor tags onto a `Model.objects.method()` call so
/// resolution routes it through queryset-method dispatch instead of the generic
/// import-scoped path: the queryset class is rarely imported by the caller, so
/// the generic path would (correctly, for that path) drop it.
pub const QUERYSET_DISPATCH: &str = "\u{1}queryset-dispatch";

/// The prefix every dispatch sentinel below carries, the marker that tells a
/// routing candidate apart from a real class or model name in the same list.
pub const SENTINEL_PREFIX: char = '\u{1}';

/// The sentinel candidate the extractor tags onto a `super().method()` call, with
/// the enclosing class's qualified name following it. Python defines `super()` as
/// a lookup that *skips* the calling class, so this must never bind to the class's
/// own method: it is resolved by the inherited-method pass, which walks the base
/// chain and refuses anything but a single ancestor definition.
pub const SUPER_DISPATCH: &str = "\u{1}super-dispatch";

/// The sentinel candidate the extractor tags onto a call whose receiver is a bare
/// name it cannot type on its own (`portal_views.template_view()`,
/// `AssetTypeChoices.to_glue_choices()`). The receiver's dotted text follows it.
/// Typing the receiver needs the file's import bindings and the whole
/// constellation's classes, so the inherited/receiver-typed pass resolves it;
/// generic name resolution deliberately drops these, because the name is reached
/// through an object rather than an import of the name itself.
pub const RECEIVER_ROOT: &str = "\u{1}receiver-root";

/// The sentinel candidate the extractor tags onto a call whose receiver is a
/// type-annotated parameter (`def view(self, order: Order): order.x()`).
/// The annotated type name follows it in the candidate list, so resolution can
/// bind the method to that exact class (typed-receiver dispatch, the annotated
/// analogue of `self.x()` instance-method resolution).
pub const TYPED_RECEIVER: &str = "\u{1}typed-receiver";

/// The sentinel candidate the extractor tags onto an `obj.services.method()` or
/// `obj.services.<sub>.method()` call, this codebase's dominant service-dispatch
/// convention (`order.services.processor.recalculate_totals()`). The service class
/// is reached through a model attribute, not an import, so the import-scoped path
/// drops it; this routes it to service-method dispatch instead.
///
/// The receiving model's name follows it in the candidate list when the receiver
/// is written as a class (`Order.services.x()`), letting dispatch pick that
/// model's service among the many that define the same method name.
pub const SERVICE_DISPATCH: &str = "\u{1}service-dispatch";

/// The sentinel candidate on a `ContextType` reference whose variable is a
/// *collection* of the model: a queryset (`Model.objects.filter(...)`) or a
/// `get_list_or_404`, not a single instance. The template member synthesis types
/// only the variable's `{% for %}` loop elements as the model, never a direct
/// `{{ var.attr }}` on the collection itself, so a queryset's own methods are not
/// mistaken for model members.
pub const COLLECTION_CONTEXT: &str = "\u{1}collection-context";

/// The base service method names dispatched on every model through the external base
/// service (`obj.services.save_model_obj()`). The base defines them, so binding by
/// a sole local override would false-attribute every model's call to whichever app
/// service happens to override it. Dropped from dispatch.
const SERVICE_BUILTINS: &[&str] = &["save_model_obj", "save_model_objs"];

/// The Django QuerySet/Manager builtin method names, dispatched dynamically by
/// Django with no project-local definition to bind to. Filtered out so a
/// builtin like `order_by` never binds to some app's custom queryset that
/// overrides it (the C-1 false-edge class).
pub const QUERYSET_BUILTINS: &[&str] = &[
    "aggregate", "all", "annotate", "bulk_create", "bulk_update", "contains", "count", "create",
    "defer", "delete", "difference", "distinct", "earliest", "exclude", "exists", "filter",
    "first", "get", "get_or_create", "in_bulk", "intersection", "iterator", "last", "latest",
    "none", "only", "order_by", "prefetch_related", "raw", "reverse", "select_related", "union",
    "update", "update_or_create", "using", "values", "values_list",
];

/// A fail-fast bound on directory depth compared between two paths, far past any
/// real source tree, so the comparison loop is provably finite.
const PATH_SEGMENTS_MAX: u32 = 64;

/// A fail-fast bound on the package initializers examined while chasing one
/// re-export, far above the number of `__init__.py` files that share a name.
const REEXPORT_INITS_MAX: usize = 64;

/// A fail-fast bound on how many package `__init__.py` hops a single re-export
/// chase follows. Bounds the loop so a circular re-export terminates, far above
/// any real package-nesting depth.
const REEXPORT_HOPS_MAX: u32 = 8;

/// The target node one reference resolves to against a single project's graph,
/// or `None` when no confident target exists (an external symbol, or too
/// ambiguous to pick). Import references route through import-specific logic;
/// everything else resolves by name with a kind preference.
pub fn resolve_reference(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "reference_name must not be empty");
    assert!(reference.line >= 1, "reference line is 1-based");

    // The template member-access pipeline resolves itself in a synthesis pass
    // (type-scoped: a `{{ var.attr }}` binds only to the member of the model the
    // rendering view gives `var`), so the generic name resolver must leave these
    // refs pending rather than bind `attr`/the model name to any same-named node.
    if matches!(
        reference.reference_kind,
        EdgeKind::AccessesMember
            | EdgeKind::ContextType
            | EdgeKind::LoopBinding
            | EdgeKind::ReverseAccessor
            | EdgeKind::DerivedCollection
    ) {
        return None;
    }

    let resolved = match reference.reference_kind {
        EdgeKind::Imports => resolve_import(reference, context),
        _ => resolve_by_name(reference, context),
    }?;

    if resolved.target_node_id == reference.from_node_id {
        return None;
    }

    Some(resolved)
}

/// The edge a resolved reference implies, tagged with the strategy that
/// produced it.
pub fn edge_from_resolved(resolved: &ResolvedRef) -> Edge {
    assert!(resolved.confidence >= 0.0, "confidence must be non-negative");
    assert!(resolved.confidence <= 1.0, "confidence must not exceed one");

    assert!(resolved.line >= 1, "resolved reference line is 1-based");

    Edge::new(
        resolved.from_node_id.clone(),
        resolved.target_node_id.clone(),
        resolved.reference_kind,
    )
    .at(resolved.line, resolved.column)
    .with_provenance(format!("resolution:{}", resolved.resolved_by.as_str()))
}

/// The target a non-import reference resolves to: exact name match first, then
/// a lower-case fuzzy match, each constrained to the kinds the reference can
/// plausibly name.
fn resolve_by_name(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "reference_name must not be empty");

    // A `super()` call names a method the enclosing class deliberately skips, so
    // neither instance-method nor generic name resolution may see it: the first
    // would bind the class's own override, the second any same-named method in the
    // project. Only the inherited-method pass, which walks the base chain with the
    // whole constellation loaded, can bind it.
    if reference.candidates.iter().any(|candidate| candidate == SUPER_DISPATCH) {
        return None;
    }

    if reference.reference_kind == EdgeKind::Calls
        && reference.candidates.iter().any(|candidate| candidate == QUERYSET_DISPATCH)
    {
        return resolve_queryset_method(reference, context);
    }

    if reference.reference_kind == EdgeKind::Calls
        && reference.candidates.iter().any(|candidate| candidate == SERVICE_DISPATCH)
    {
        return resolve_service_method(reference, context);
    }

    if reference.reference_kind == EdgeKind::Calls
        && reference.candidates.iter().any(|candidate| candidate == TYPED_RECEIVER)
    {
        return resolve_typed_receiver(reference, context);
    }

    if reference.reference_kind == EdgeKind::Calls
        && let Some(resolved) = resolve_instance_method(reference, context)
    {
        return Some(resolved);
    }

    let preferred = preferred_kinds(reference.reference_kind);
    let target = target_language(reference.reference_kind);

    assert!(!preferred.is_empty(), "preferred_kinds yields at least one kind");

    let mut exact = filter_language(context.nodes_by_name(&reference.reference_name), target);
    scope_target(&mut exact, reference, context);

    if let Some(resolved) = match_candidates(reference, exact, preferred, ResolvedBy::ExactMatch, 0.9, 0.6) {
        return Some(resolved);
    }

    if let Some(resolved) = resolve_via_import(reference, context) {
        return Some(resolved);
    }

    let lowered = reference.reference_name.to_lowercase();
    let mut fuzzy = filter_language(context.nodes_by_lower_name(&lowered), target);
    scope_target(&mut fuzzy, reference, context);

    match_candidates(reference, fuzzy, preferred, ResolvedBy::Fuzzy, 0.5, 0.4)
}

/// The import-scoping applied to a name-keyed reference whose target is
/// collision-prone across files: a `Calls` to a bare method (`qs.order_by()`), a `RoutesTo`
/// whose view is shadowed across nested apps (`page_views.detail_view`), or a
/// `Handles` whose Alpine handler is an `x-data` method scoped to its own
/// template. Other kinds (models, routes-by-string) are unique enough to leave
/// alone.
fn scope_target(
    candidates: &mut Vec<Arc<Node>>,
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) {
    match reference.reference_kind {
        EdgeKind::Calls => scope_to_imports(candidates, reference, context),
        EdgeKind::RoutesTo => scope_route(candidates, reference, context),
        EdgeKind::Handles => scope_handles(candidates, reference),
        _ => {}
    }
}

/// The restriction of an Alpine `Handles` reference to a handler defined in the
/// same template. An inline `x-data` method is scoped to its own component, so two
/// templates that each define a same-named method (`submitOrder()`) must each
/// resolve to their own, not collapse onto whichever the global name lookup
/// returns first. When no candidate shares the template's file, the handler lives
/// in a shared `.js` file (a different file); leave every candidate so the global
/// match still resolves that cross-file handler.
fn scope_handles(candidates: &mut Vec<Arc<Node>>, reference: &UnresolvedRef) {
    if candidates.iter().any(|node| node.file_path == reference.file_path) {
        candidates.retain(|node| node.file_path == reference.file_path);
    }
}

/// The restriction of candidates to those in scope of the referencing file:
/// defined in that file, or imported into it (the free function by name, or a method's
/// owning class). A cross-file reference to a symbol the file never imports is
/// almost always a same-name collision (a Django `qs.order_by()` hitting a
/// local `JQLBuilder.order_by`), and a dropped edge beats a confidently wrong
/// one. Python-exact for free functions (you must import to reach across files).
fn scope_to_imports(
    candidates: &mut Vec<Arc<Node>>,
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) {
    let imports = context.import_mappings(&reference.file_path, reference.language);
    candidates.retain(|node| imported_or_local(node, &reference.file_path, &imports));
}

/// Whether a candidate is defined in the referencing file, or imported into it
/// (its top-level owner appears as an imported name).
fn imported_or_local(
    node: &constellation_graph::Node,
    file_path: &str,
    imports: &[crate::context::ImportMapping],
) -> bool {
    node.file_path == file_path
        || imports.iter().any(|mapping| mapping.exported_name == top_owner(&node.qualified_name))
}

/// The restriction of a route's view candidates by the URL file's import of the
/// handler's receiver module: `page_views.detail_view` in a file with `from a.b.employee.views
/// import page_views` binds inside `a/b/employee/views/page_views.py`, never a
/// sibling app's `page_views.py`. The receiver module is carried as the route
/// reference's first candidate. With no receiver (a bare `view` reference) it
/// falls back to plain import-scoping by the view's own name.
fn scope_route(
    candidates: &mut Vec<Arc<Node>>,
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) {
    let imports = context.import_mappings(&reference.file_path, reference.language);

    if let Some(receiver) = reference.candidates.first()
        && let Some(mapping) = imports.iter().find(|mapping| &mapping.local_name == receiver)
    {
        let module_file = module_file_path(&mapping.source, &mapping.exported_name, &reference.file_path);
        candidates.retain(|node| path_in_module(&node.file_path, &module_file));

        return;
    }

    candidates.retain(|node| imported_or_local(node, &reference.file_path, &imports));
}

/// The relative path of the file a `from source import module` names, where
/// `module` is itself a module (the `page_views` of `page_views.detail_view`).
/// Absolute (`a.b.c`) maps to `a/b/c/<module>.py`; relative (`.x` / `..x`)
/// resolves the dotted prefix against the referencing file's directory.
fn module_file_path(source: &str, module: &str, reference_file: &str) -> String {
    assert!(!module.is_empty(), "module name must not be empty");

    let leaf = format!("{module}.py");

    if !source.starts_with('.') {
        let directory = source.replace('.', "/");

        return format!("{}/{leaf}", directory.trim_matches('/'));
    }

    let dots = source.chars().take_while(|character| *character == '.').count();

    // A relative import's leading-dot count is the number of package levels to
    // climb; bound it so a malformed `source` cannot drive an unbounded climb.
    assert!(
        dots as u32 <= PATH_SEGMENTS_MAX,
        "relative import depth exceeded {PATH_SEGMENTS_MAX}",
    );

    let tail = source.trim_start_matches('.').replace('.', "/");
    let mut directory: String = parent_directory(reference_file).to_string();

    for _ in 1..dots {
        directory = parent_directory(&directory).to_string();
    }

    let base = match (directory.is_empty(), tail.is_empty()) {
        (true, _) => tail,
        (_, true) => directory,
        _ => format!("{directory}/{tail}"),
    };

    format!("{}/{leaf}", base.trim_matches('/'))
}

/// Whether a candidate's project-relative file path names the same module file
/// as `module_file` (an absolute-from-package-root path). Compared as a path
/// suffix in either direction, so an index rooted below the import's top package
/// (file `employee/views/page_views.py` vs import `app...employee.views`) still
/// matches the right module; a sibling app (`employment/...`) does not.
fn path_in_module(candidate: &str, module_file: &str) -> bool {
    let candidate = candidate.replace('\\', "/");

    module_file == candidate
        || module_file.ends_with(&format!("/{candidate}"))
        || candidate.ends_with(&format!("/{module_file}"))
}

/// The top-level owner of a qualified name: the class a method hangs off, or
/// the symbol itself for a free function. `a/b.py::Cls.save` -> `Cls`,
/// `a/b.py::helper` -> `helper`.
fn top_owner(qualified_name: &str) -> &str {
    let tail = qualified_name.rsplit("::").next().unwrap_or(qualified_name);

    tail.split('.').next().unwrap_or(tail)
}

/// The language a reference of this kind must resolve into, when the kind
/// pins one. Keeps a Python route from binding to a same-named JavaScript
/// symbol, and an Alpine handler from binding to a same-named Python view.
fn target_language(kind: EdgeKind) -> Option<Language> {
    match kind {
        EdgeKind::RoutesTo
        | EdgeKind::RelatesTo
        | EdgeKind::Receives
        | EdgeKind::AdminOf
        | EdgeKind::Tests
        | EdgeKind::Reads => Some(Language::Python),
        EdgeKind::Returns | EdgeKind::TypeOf => Some(Language::Python),
        EdgeKind::UsesTag => Some(Language::Python),
        EdgeKind::Handles => Some(Language::JavaScript),
        _ => None,
    }
}

/// The candidates kept in the target language, unless none match; then all are
/// kept, so a missing same-language definition still resolves best-effort.
fn filter_language(
    mut candidates: Vec<Arc<Node>>,
    target: Option<Language>,
) -> Vec<Arc<Node>> {
    let Some(target) = target else {
        return candidates;
    };

    if candidates.iter().any(|node| node.language == target) {
        // Retain in place: no second Vec allocation and no per-node move that
        // `into_iter().filter().collect()` would do.
        candidates.retain(|node| node.language == target);

        assert!(!candidates.is_empty(), "language filter keeps the matched nodes");

        assert!(
            candidates.iter().all(|node| node.language == target),
            "language filter retains only the target language",
        );
    }

    candidates
}

/// The definition inside the same project an import resolves to. External imports
/// (absolute, non-first-party) intentionally return `None` and stay pending
/// for the cross-project linker.
fn resolve_import(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "import reference_name must not be empty");

    if reference.language == Language::JavaScript {
        return resolve_js_import(reference, context);
    }

    let name = reference.reference_name.as_str();
    let module = reference.candidates.first().map_or("", String::as_str);
    let module_stem = module.rsplit('.').next().unwrap_or(module).trim_start_matches('.');

    if !module_stem.is_empty() {
        let mut symbols = context.nodes_by_name(name);
        symbols.retain(|node| is_definition(node.kind) && file_stem(&node.file_path) == module_stem);

        if let Some(node) = symbols.into_iter().next() {
            return Some(ResolvedRef::new(reference, node.id.clone(), 0.9, ResolvedBy::Import));
        }
    }

    if let Some(resolved) = resolve_reexport(reference, module, name, context) {
        return Some(resolved);
    }

    // An absolute first-party submodule import (`from app.asset import models`)
    // names a module file, not a symbol: bind it to the file at the module path the
    // import spells (`app/asset/models.py`), matched as a path suffix so an index
    // rooted below the top package still resolves. A symbol import of the same shape
    // (`from app.asset.models import Inventory`) resolved through the name-and-stem
    // path above; one that names neither a file nor a symbol here is external and
    // stays pending for the cross-project linker.
    if !module.starts_with('.') {
        let module_path = format!("{}/{name}.py", module.replace('.', "/"));

        let mut files = context.nodes_by_kind(NodeKind::File);
        files.retain(|node| path_in_module(&node.file_path, &module_path));

        return files
            .into_iter()
            .next()
            .map(|node| ResolvedRef::new(reference, node.id.clone(), 0.85, ResolvedBy::Import));
    }

    let mut files = context.nodes_by_kind(NodeKind::File);
    files.retain(|node| file_stem(&node.file_path) == name);

    if let Some(node) = files.into_iter().next() {
        return Some(ResolvedRef::new(reference, node.id.clone(), 0.85, ResolvedBy::Import));
    }

    let mut anywhere = context.nodes_by_name(name);
    anywhere.retain(|node| is_definition(node.kind));

    if anywhere.len() == 1 {
        let node = anywhere.remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.8, ResolvedBy::Import));
    }

    None
}

/// The target a JavaScript import resolves to. Only relative specifiers
/// (intra-project) link; the target is the JavaScript file whose name matches the
/// specifier's final path segment. Bare specifiers (`react`) are external and stay pending.
fn resolve_js_import(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(reference.language == Language::JavaScript, "js resolver requires a js reference");
    assert!(!reference.reference_name.is_empty(), "js import reference_name must not be empty");

    match reference.candidates.first() {
        Some(module) => resolve_js_symbol(reference, module, context),
        None => resolve_js_module(reference, &reference.reference_name, context),
    }
}

/// The exported symbol a named/default JS import names, in the imported
/// module's file when possible.
fn resolve_js_symbol(
    reference: &UnresolvedRef,
    module: &str,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!module.is_empty(), "module specifier must not be empty");

    if !module.starts_with('.') {
        return None;
    }

    let stem = js_module_stem(module);
    let mut candidates = context.nodes_by_name(&reference.reference_name);

    candidates.retain(|node| {
        node.language == Language::JavaScript
            && is_definition(node.kind)
            && node.kind != NodeKind::File
    });

    if let Some(node) = candidates.iter().find(|node| file_stem(&node.file_path) == stem) {
        return Some(ResolvedRef::new(reference, node.id.clone(), 0.9, ResolvedBy::Import));
    }

    if candidates.len() == 1 {
        let node = candidates.swap_remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.7, ResolvedBy::Import));
    }

    None
}

/// The module's file a side-effect/namespace JS import resolves to, by path stem.
fn resolve_js_module(
    reference: &UnresolvedRef,
    module: &str,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!module.is_empty(), "module specifier must not be empty");

    if !module.starts_with('.') {
        return None;
    }

    let stem = js_module_stem(module);

    if stem.is_empty() {
        return None;
    }

    let mut files = context.nodes_by_kind(NodeKind::File);
    files.retain(|node| node.language == Language::JavaScript && file_stem(&node.file_path) == stem);

    files
        .into_iter()
        .next()
        .map(|node| ResolvedRef::new(reference, node.id.clone(), 0.85, ResolvedBy::Import))
}

/// The bare module name of a JavaScript specifier: `./utils/card.js` -> `card`.
fn js_module_stem(module: &str) -> &str {
    assert!(!module.is_empty(), "module specifier must not be empty");

    let last = module.rsplit('/').next().unwrap_or(module);

    for extension in [".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx"] {
        if let Some(stem) = last.strip_suffix(extension) {
            return stem;
        }
    }

    assert!(last.len() <= module.len(), "stem is a slice of the module specifier");

    last
}

/// The target picked from name-match candidates. A single preferred-kind hit is
/// high confidence; a sole candidate of any kind is taken at the lower
/// confidence; multiple non-preferred candidates are too ambiguous to resolve.
fn match_candidates(
    reference: &UnresolvedRef,
    candidates: Vec<Arc<Node>>,
    preferred: &[NodeKind],
    resolved_by: ResolvedBy,
    confidence_unique: f32,
    confidence_ambiguous: f32,
) -> Option<ResolvedRef> {
    assert!(confidence_unique <= 1.0, "unique confidence stays within range");
    assert!(confidence_ambiguous <= 1.0, "ambiguous confidence stays within range");

    let mut candidates = candidates;
    candidates.retain(|node| is_definition(node.kind));

    if candidates.is_empty() {
        return None;
    }

    let preferred_count = candidates.iter().filter(|node| preferred.contains(&node.kind)).count();

    let (index, confidence) = if preferred_count >= 1 {
        let index = preferred_by_locality(&candidates, preferred, &reference.file_path)?;
        let confidence = if preferred_count == 1 { confidence_unique } else { confidence_ambiguous };

        (index, confidence)
    } else if candidates.len() == 1
        && allows_loose_fallback(reference.reference_kind)
        && is_resolvable_target(candidates[0].kind)
    {
        (0, confidence_ambiguous)
    } else {
        return None;
    };

    assert!(index < candidates.len(), "selected index stays within candidates");

    let target = candidates.swap_remove(index);

    Some(ResolvedRef::new(reference, target.id.clone(), confidence, resolved_by))
}

/// The target a reference whose name is a local import alias resolves to,
/// rewriting it to the exported name and binding it in the import's source
/// module. Handles `from m import f as g; g()` and `import m as n` (names that
/// have no node of their own). Prefers a definition in the source module's file.
fn resolve_via_import(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "reference_name must not be empty");

    let mappings = context.import_mappings(&reference.file_path, reference.language);
    let mapping = mappings.iter().find(|mapping| mapping.local_name == reference.reference_name)?;

    assert!(!mapping.exported_name.is_empty(), "import mapping carries an exported name");

    let stem = mapping
        .source
        .rsplit(['.', '/'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(&mapping.source);

    let mut candidates = context.nodes_by_name(&mapping.exported_name);
    candidates.retain(|node| is_importable(node.kind));

    if candidates.is_empty() {
        return None;
    }

    if let Some(node) = candidates.iter().find(|node| file_stem(&node.file_path) == stem) {
        return Some(ResolvedRef::new(reference, node.id.clone(), 0.9, ResolvedBy::Import));
    }

    if candidates.len() == 1 {
        let node = candidates.swap_remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.7, ResolvedBy::Import));
    }

    resolve_reexport(reference, &mapping.source, &mapping.exported_name, context)
}

/// The target a custom QuerySet/Manager method dispatched through a model manager
/// resolves to (`Article.objects.by_year()`). The `.objects.` indirection that
/// `objects = XQuerySet.as_manager()` sets up isn't statically followable, so
/// this binds by the method's (rare, custom) name to the sole `*QuerySet` /
/// `*Manager`-owned method of that name. Builtins are excluded above; an
/// ambiguous custom name (two managers define it) stays unresolved rather than
/// guess.
fn resolve_queryset_method(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "queryset method name must not be empty");

    if QUERYSET_BUILTINS.contains(&reference.reference_name.as_str()) {
        return None;
    }

    let mut candidates = context.nodes_by_name(&reference.reference_name);
    candidates.retain(|node| node.kind == NodeKind::Method && owner_is_manager(&node.qualified_name));

    if candidates.len() == 1 {
        let node = candidates.swap_remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.85, ResolvedBy::Framework));
    }

    // Several querysets define the name, so uniqueness cannot pick one. The
    // model the chain started from can: `Order.objects.active()` wants
    // `OrderQuerySet.active`, not `FileQuerySet.active`. Still conservative, and
    // still one edge or none: a model whose name matches two owners resolves to
    // neither.
    let model = reference.candidates.iter().find(|candidate| {
        candidate.as_str() != QUERYSET_DISPATCH && !candidate.is_empty()
    })?;

    candidates.retain(|node| owner_matches_model(&node.qualified_name, model, MANAGER_SUFFIXES));

    if candidates.len() != 1 {
        return None;
    }

    let node = candidates.swap_remove(0);

    Some(ResolvedRef::new(reference, node.id.clone(), 0.8, ResolvedBy::Framework))
}

/// Whether a companion class owner belongs to `model`, by the naming convention
/// that binds them: `Order` owns `OrderQuerySet`, `OrderManager`, and
/// `OrderQuerySetManager` under [`MANAGER_SUFFIXES`], and `OrderService`,
/// `OrderFactoryService`, and the rest under [`SERVICE_SUFFIXES`]. Convention
/// rather than a declared link, because the `objects = OrderQuerySet.as_manager()`
/// and `services = OrderService()` assignments that would prove it are not
/// something the extractor follows today.
///
/// The name past the model prefix must be exactly one of `suffixes`, not merely
/// start with the model: a bare prefix test also matches a *sibling* model's
/// companion, so `Inventory` would own `InventoryRecordQuerySet` and a method
/// only that sibling defines would bind to the wrong class. Requiring the whole
/// remainder keeps one model's companion from standing in for another's.
fn owner_matches_model(qualified_name: &str, model: &str, suffixes: &[&str]) -> bool {
    assert!(!model.is_empty(), "model name must not be empty");
    assert!(!suffixes.is_empty(), "at least one companion suffix");

    let owner = top_owner(qualified_name);

    if owner.len() <= model.len() || !owner.starts_with(model) {
        return false;
    }

    suffixes.contains(&&owner[model.len()..])
}

/// The class-name suffixes Django's convention appends to a model name to form
/// its queryset or manager, the remainder [`owner_matches_model`] accepts. Shared
/// with the inherited-method pass, which builds the same names to find the
/// classes a model dispatches through.
pub const MANAGER_SUFFIXES: &[&str] = &["Manager", "QuerySet", "QuerySetManager"];

/// The class-name suffixes this codebase's convention appends to a model name to
/// form its service classes, the service-dispatch counterpart of
/// [`MANAGER_SUFFIXES`]. A model reaches the plain `Service` directly
/// (`obj.services.x()`) and the rest through a named sub-service
/// (`obj.services.processor.x()`), so both spellings must map back to the model.
pub const SERVICE_SUFFIXES: &[&str] = &[
    "FactoryService",
    "IntelligenceService",
    "ProcessorService",
    "Service",
    "TransformationService",
];

/// Whether a method's top-level owning class names a Django queryset or manager
/// (ends with `QuerySet` or `Manager`).
fn owner_is_manager(qualified_name: &str) -> bool {
    let owner = top_owner(qualified_name);

    owner.ends_with("QuerySet") || owner.ends_with("Manager")
}

/// The target a custom service method dispatched through a model's `services`
/// attribute resolves to (`order.services.processor.recalculate_totals()`). The service object
/// is reached through chained attributes, not statically followable, so this binds
/// the method's (rare, custom) name to the sole `*Service`-owned method of that
/// name, and failing that to the service the receiving model owns. Base methods
/// are excluded; a name that stays ambiguous under both tests resolves to nothing
/// rather than guess, the same false-edge guard as queryset dispatch.
fn resolve_service_method(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "service method name must not be empty");

    if SERVICE_BUILTINS.contains(&reference.reference_name.as_str()) {
        return None;
    }

    let mut candidates = context.nodes_by_name(&reference.reference_name);
    candidates.retain(|node| node.kind == NodeKind::Method && owner_is_service(&node.qualified_name));

    if candidates.len() == 1 {
        let node = candidates.swap_remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.85, ResolvedBy::Framework));
    }

    // Several services define the name, so uniqueness cannot pick one. The model
    // the chain started from can: `Target.services.set_quantity_for_day()` wants
    // `TargetService.set_quantity_for_day`, not the quota or forecast service's
    // method of the same name. Whole families of service methods share a name by
    // design here (one per model), so without this the convention that carries
    // most of the business logic stays entirely unresolved.
    let model = reference.candidates.iter().find(|candidate| {
        candidate.as_str() != SERVICE_DISPATCH && !candidate.is_empty()
    })?;

    candidates.retain(|node| owner_matches_model(&node.qualified_name, model, SERVICE_SUFFIXES));

    if candidates.len() != 1 {
        return None;
    }

    let node = candidates.swap_remove(0);

    Some(ResolvedRef::new(reference, node.id.clone(), 0.8, ResolvedBy::Framework))
}

/// The method of the exact class a call on a type-annotated receiver resolves to
/// (`order.recalculate()` where `order: Order`). Binds only
/// when a single method/function of that name is owned by a class whose name
/// equals the annotated type, so it never guesses across same-named methods on
/// other classes. The annotated type name is carried as the candidate following
/// the [`TYPED_RECEIVER`] sentinel.
fn resolve_typed_receiver(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "typed-receiver method name must not be empty");

    let type_name = reference
        .candidates
        .iter()
        .find(|candidate| candidate.as_str() != TYPED_RECEIVER && !candidate.is_empty())?;

    let mut candidates = context.nodes_by_name(&reference.reference_name);

    candidates.retain(|node| {
        matches!(node.kind, NodeKind::Method | NodeKind::Function)
            && top_owner(&node.qualified_name) == type_name.as_str()
    });

    if candidates.len() == 1 {
        let node = candidates.swap_remove(0);

        return Some(ResolvedRef::new(reference, node.id.clone(), 0.9, ResolvedBy::InstanceMethod));
    }

    None
}

/// Whether a method's top-level owning class is a service (ends with `Service`,
/// covering `Service`, `ProcessorService`, `FactoryService`, `IntelligenceService`).
/// Excludes test classes, which end `ServiceTestCase`, not `Service`.
fn owner_is_service(qualified_name: &str) -> bool {
    top_owner(qualified_name).ends_with("Service")
}

/// The method of the enclosing class a `self.x()` / `cls.x()` call resolves to,
/// carried on the reference's candidates as the class's qualified name. Matches
/// the method by exact qualified name, so it never binds to a same-named method
/// on a different class.
fn resolve_instance_method(
    reference: &UnresolvedRef,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!reference.reference_name.is_empty(), "method name must not be empty");

    // One reusable buffer for the `Class.method` lookup key, cleared per
    // candidate, instead of a fresh `format!` allocation each iteration.
    let mut qualified = String::new();

    for class in &reference.candidates {
        assert!(!class.is_empty(), "candidate class qualified name must not be empty");

        // A dispatch sentinel and the receiver text that trails it are routing
        // data, not a class to look a method up under.
        if class.starts_with(SENTINEL_PREFIX) {
            return None;
        }

        qualified.clear();
        qualified.push_str(class);
        qualified.push('.');
        qualified.push_str(&reference.reference_name);

        let target = context
            .nodes_by_qualified_name(&qualified)
            .into_iter()
            .find(|node| matches!(node.kind, NodeKind::Method | NodeKind::Function));

        if let Some(node) = target {
            return Some(ResolvedRef::new(reference, node.id.clone(), 0.95, ResolvedBy::InstanceMethod));
        }
    }

    None
}

/// The node kinds a reference of the given edge kind can plausibly resolve to.
fn preferred_kinds(kind: EdgeKind) -> &'static [NodeKind] {
    let kinds: &'static [NodeKind] = match kind {
        EdgeKind::Calls => &[NodeKind::Function, NodeKind::Method],
        EdgeKind::Extends => &[NodeKind::Class, NodeKind::Model],
        EdgeKind::Instantiates => &[NodeKind::Class, NodeKind::Model],
        EdgeKind::Decorates => &[NodeKind::Function, NodeKind::Method, NodeKind::Class],
        EdgeKind::Returns | EdgeKind::TypeOf => &[NodeKind::Class, NodeKind::Model],
        EdgeKind::RelatesTo | EdgeKind::Receives | EdgeKind::AdminOf => &[NodeKind::Model, NodeKind::Class],
        EdgeKind::Renders | EdgeKind::ExtendsTemplate | EdgeKind::IncludesTemplate => {
            &[NodeKind::Template]
        }
        EdgeKind::Styles => &[NodeKind::Selector],
        EdgeKind::Resolves => &[NodeKind::Route],
        EdgeKind::Handles => &[NodeKind::Function, NodeKind::Method],
        EdgeKind::RoutesTo => {
            &[NodeKind::View, NodeKind::Function, NodeKind::Method, NodeKind::Class]
        }
        EdgeKind::Tests => &[NodeKind::Class, NodeKind::Model, NodeKind::Function, NodeKind::View],
        EdgeKind::Reads => &[NodeKind::Constant, NodeKind::Variable],
        EdgeKind::UsesTag => &[NodeKind::Function, NodeKind::Method],
        _ => &[
            NodeKind::Class,
            NodeKind::Function,
            NodeKind::Method,
            NodeKind::Model,
            NodeKind::Variable,
            NodeKind::View,
        ],
    };

    assert!(!kinds.is_empty(), "every edge kind names at least one node kind");
    assert!(kinds.len() <= 6, "preferred kinds stay a small set");

    kinds
}

/// Whether an edge kind may bind to a sole candidate of a non-preferred kind.
/// Loose kinds (a call, an instantiation, a decoration) can plausibly name a
/// symbol of an unanticipated kind, so a unique same-name match is worth taking.
/// Strict kinds name exactly one kind family (a model relation hits a model, a
/// render hits a template, a return type hits a class), so a sole off-kind match
/// is a same-name collision (a `'auth.User'` FK landing on a local field named
/// `user`), not a resolution. Dropping it beats emitting a false edge, the one
/// failure mode this tool must never have.
fn allows_loose_fallback(kind: EdgeKind) -> bool {
    !matches!(
        kind,
        EdgeKind::RelatesTo
            | EdgeKind::Receives
            | EdgeKind::Returns
            | EdgeKind::TypeOf
            | EdgeKind::Extends
            | EdgeKind::Instantiates
            | EdgeKind::Renders
            | EdgeKind::ExtendsTemplate
            | EdgeKind::IncludesTemplate
            | EdgeKind::Styles
            | EdgeKind::Resolves
            | EdgeKind::AdminOf
            | EdgeKind::Tests
            | EdgeKind::Reads
            | EdgeKind::UsesTag
    )
}

/// Whether a node kind is a plausible target for the ambiguous sole-candidate
/// fallback. A value or member (field, property, variable, constant, parameter)
/// and an import node are never what a name reference resolves to: a `date(...)`
/// call whose only same-name node is a model field named `date` is a collision,
/// not a call. Definitions, files, and modules are fine (an asset `src`/`href`
/// reference legitimately binds to a File). Gating the fallback on this stops a
/// loose edge (a call, a decoration) from landing on junk.
fn is_resolvable_target(kind: NodeKind) -> bool {
    !matches!(
        kind,
        NodeKind::Field
            | NodeKind::Property
            | NodeKind::Variable
            | NodeKind::Constant
            | NodeKind::Parameter
            | NodeKind::Import
    )
}

/// Whether a node kind can be a module-level importable name. A field, property,
/// or parameter is a class member or a local, never reachable by `from m import
/// x`, so an import alias resolving to one is a name collision (`from datetime
/// import date` colliding with a model's `date` field). Constants and variables
/// can be module-level, so they stay importable; only members, import nodes, and
/// synthesized stubs are excluded.
fn is_importable(kind: NodeKind) -> bool {
    is_definition(kind)
        && !matches!(kind, NodeKind::Field | NodeKind::Property | NodeKind::Parameter)
}

/// Whether a node kind is a definition resolution may bind a reference to.
///
/// An `Import` names something defined elsewhere, and an `External` is a
/// synthesized stub for a symbol no indexed project defines. Neither is a
/// definition, and the stub in particular is *derived*: the synthesis pass
/// clears every external node and re-derives it from whatever stayed pending, so
/// a reference bound to one is bound to the previous run's output. The edge dies
/// with the stub on the next index and the reference that produced it is already
/// spent, which makes a re-index disagree with a cold index over the same tree.
fn is_definition(kind: NodeKind) -> bool {
    !matches!(kind, NodeKind::Import | NodeKind::External)
}

/// A file path's base name without its extension: `app/models.py` -> `models`.
fn file_stem(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);

    assert!(stem.len() <= path.len(), "stem is a slice of the path");

    stem
}

/// The index, among `candidates`, of the preferred-kind node whose file shares
/// the longest leading directory path with `source_path` (the same Django app).
/// So a route in `customer/contact/urls.py` binds to the `table_rows_view` in
/// `customer/contact/views.py`, not a same-named view in another app. A tie and
/// the common no-shared-prefix case keep the first preferred candidate (the
/// prior behaviour), so this only ever breaks ties, never overrides a kind
/// preference.
fn preferred_by_locality(
    candidates: &[Arc<Node>],
    preferred: &[NodeKind],
    source_path: &str,
) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;

    for (index, node) in candidates.iter().enumerate() {
        if !preferred.contains(&node.kind) {
            continue;
        }

        let affinity = shared_directory_depth(source_path, &node.file_path);

        let improves = match best {
            Some((_, best_affinity)) => affinity > best_affinity,
            None => true,
        };

        if improves {
            best = Some((index, affinity));
        }
    }

    best.map(|(index, _)| index)
}

/// The number of leading directory segments two paths share, comparing
/// directory components only (the file name is excluded), so sibling files in
/// one directory share their whole directory depth. `a/b/urls.py` and
/// `a/b/views.py` share two; `a/b/urls.py` and `a/c/views.py` share one.
#[doc(hidden)]
pub fn shared_directory_depth(source: &str, target: &str) -> u32 {
    let source_directory = parent_directory(source);
    let target_directory = parent_directory(target);

    let mut source_segments = source_directory.split(['/', '\\']);
    let mut target_segments = target_directory.split(['/', '\\']);
    let mut shared: u32 = 0;

    loop {
        assert!(shared <= PATH_SEGMENTS_MAX, "shared depth stays within the path bound");

        match (source_segments.next(), target_segments.next()) {
            (Some(source_segment), Some(target_segment))
                if !source_segment.is_empty() && source_segment == target_segment =>
            {
                shared += 1;
            }
            _ => break,
        }
    }

    shared
}

/// The target a name resolves to through a chain of package `__init__.py`
/// re-exports. `from pkg import X` may name a symbol the package re-exports rather than
/// defines (`from .models import X` in `pkg/__init__.py`). When `.models` is
/// itself a package whose `__init__.py` re-exports `X` again (`from .article
/// import X`), the chase descends one level deeper, and so on. An iterative
/// loop bounded by [`REEXPORT_HOPS_MAX`], so the call graph stays acyclic and a
/// circular chain terminates.
fn resolve_reexport(
    reference: &UnresolvedRef,
    source_module: &str,
    exported_name: &str,
    context: &dyn ResolutionContext,
) -> Option<ResolvedRef> {
    assert!(!exported_name.is_empty(), "exported_name must not be empty");

    let mut name = exported_name.to_string();
    let mut initializers = package_initializers(reference, source_module, context);
    let mut hops: u32 = 0;

    while hops < REEXPORT_HOPS_MAX && !initializers.is_empty() {
        hops += 1;

        let (source, exported, init_directory) =
            first_reexport(&initializers, &name, reference.language, context)?;

        let stem = module_stem(&source);

        let mut candidates = context.nodes_by_name(&exported);
        candidates.retain(|node| is_importable(node.kind) && file_stem(&node.file_path) == stem);

        if let Some(node) = candidates.into_iter().next() {
            return Some(ResolvedRef::new(reference, node.id.clone(), 0.8, ResolvedBy::Import));
        }

        initializers = sub_package_initializers(&init_directory, &source, context);
        name = exported;
    }

    None
}

/// The first `__init__.py` among `initializers` that re-exports `name`, with the
/// re-export's source module, its exported name, and the initializer's
/// directory. Backs one hop of [`resolve_reexport`].
fn first_reexport(
    initializers: &[String],
    name: &str,
    language: Language,
    context: &dyn ResolutionContext,
) -> Option<(String, String, String)> {
    for init_path in initializers.iter().take(REEXPORT_INITS_MAX) {
        let mappings = context.import_mappings(init_path, language);

        if let Some(reexport) = mappings.iter().find(|mapping| mapping.local_name == name) {
            return Some((
                reexport.source.clone(),
                reexport.exported_name.clone(),
                parent_directory(init_path).to_string(),
            ));
        }
    }

    None
}

/// The `__init__.py` files of the sub-package a single-dot-relative re-export
/// `source` (`.models`, `.sub.models`) names relative to `init_directory`. Empty when
/// `source` is a parent import (`..x`) or names no package, at which point the
/// chase stops.
fn sub_package_initializers(
    init_directory: &str,
    source: &str,
    context: &dyn ResolutionContext,
) -> Vec<String> {
    let relative = match source.strip_prefix('.') {
        Some(rest) if !rest.starts_with('.') => rest,
        None => source,
        _ => return Vec::new(),
    };

    if relative.is_empty() {
        return Vec::new();
    }

    let sub_path = relative.replace('.', "/");

    let target_directory =
        if init_directory.is_empty() { sub_path } else { format!("{init_directory}/{sub_path}") };

    context
        .all_files()
        .into_iter()
        .filter(|path| is_init_file(path) && parent_directory(path) == target_directory)
        .collect()
}

/// The `__init__.py` files that could carry the re-export for `source_module`,
/// resolved relative to the importing file. A relative (`.`) or empty source
/// points at the importing file's own package; a named source matches any
/// package directory of that name.
fn package_initializers(
    reference: &UnresolvedRef,
    source_module: &str,
    context: &dyn ResolutionContext,
) -> Vec<String> {
    assert!(!reference.file_path.is_empty(), "reference file_path must not be empty");

    let current_package =
        source_module.is_empty() || source_module.chars().all(|character| character == '.');

    if current_package {
        let directory = parent_directory(&reference.file_path);

        let inits: Vec<String> = context
            .all_files()
            .into_iter()
            .filter(|path| is_init_file(path) && parent_directory(path) == directory)
            .collect();

        assert!(inits.iter().all(|path| is_init_file(path)), "only initializers are returned");

        return inits;
    }

    let Some(package) = source_module.rsplit(['.', '/']).find(|segment| !segment.is_empty()) else {
        return Vec::new();
    };

    context
        .all_files()
        .into_iter()
        .filter(|path| is_init_file(path) && directory_basename(path) == package)
        .collect()
}

/// The final segment of a dotted/relative import source: `.models` -> `models`,
/// `a.b.c` -> `c`, a bare `.` -> "".
fn module_stem(source: &str) -> &str {
    let stem = source.rsplit(['.', '/']).find(|segment| !segment.is_empty()).unwrap_or("");

    assert!(stem.len() <= source.len(), "stem is a slice of the source");

    stem
}

/// The directory portion of a path: `app/models.py` -> `app`, `models.py` -> "".
fn parent_directory(path: &str) -> &str {
    let directory = path.rsplit_once(['/', '\\']).map_or("", |(directory, _)| directory);

    assert!(directory.len() <= path.len(), "directory is a prefix slice of the path");

    directory
}

/// The final directory segment of a path's parent: `a/b/__init__.py` -> `b`.
fn directory_basename(path: &str) -> &str {
    let directory = parent_directory(path);
    let base = directory.rsplit(['/', '\\']).next().unwrap_or(directory);

    assert!(base.len() <= path.len(), "basename is a slice of the path");

    base
}

/// Whether a path is a package initializer (`.../__init__.py`).
fn is_init_file(path: &str) -> bool {
    assert!(!path.is_empty(), "path must not be empty");

    path.rsplit(['/', '\\']).next() == Some("__init__.py")
}
