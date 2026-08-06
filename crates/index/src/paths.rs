//! Reading meaning out of a path or a qualified name.
//!
//! Python's module system is a naming convention over directories, so much of
//! what the indexer must know (which app a file belongs to, which module an
//! import names) is a string operation. Each is done once, here.


use rustc_hash::{FxHashMap, FxHashSet};

use crate::limits::NAMESPACE_DEPTH_MAX;

/// The project's application namespace: the app_name of its root urlconf, taken as
/// the uniquely shallowest (fewest dotted segments) module that declares an app_name.
/// `None` when the shallowest depth is shared by several modules, so a project whose
/// root urlconf declares no app_name never prepends an arbitrary app's namespace to
/// every reverse.
pub(crate) fn project_root_app_name(
    app_name_by_module: &FxHashMap<String, String>,
) -> Option<String> {
    let depth_min = app_name_by_module.keys().map(|module| module.split('.').count()).min()?;

    let mut shallowest = app_name_by_module
        .iter()
        .filter(|(module, _)| module.split('.').count() == depth_min);

    let (_, app_name) = shallowest.next()?;

    if shallowest.next().is_some() {
        return None;
    }

    Some(app_name.clone())
}

/// The indexed url module an include's dotted module string names, matched as a
/// suffix so a stripped package root (`django_spire.ai.urls` indexed as `ai.urls`)
/// still resolves. An exact match wins; otherwise a unique dot-boundary suffix
/// match in either direction; an ambiguous suffix returns `None` rather than bind
/// the wrong parent.
pub(crate) fn resolve_include_module(
    module_string: &str,
    url_modules: &FxHashSet<String>,
) -> Option<String> {
    if url_modules.contains(module_string) {
        return Some(module_string.to_string());
    }

    let mut matched: Option<&String> = None;

    for candidate in url_modules {
        let is_suffix = ends_with_dotted(module_string, candidate)
            || ends_with_dotted(candidate, module_string);

        if is_suffix {
            if matched.is_some() {
                return None;
            }

            matched = Some(candidate);
        }
    }

    matched.cloned()
}

/// Whether `text` ends with `suffix` at a dot boundary, the test
/// [`resolve_include_module`] runs against every indexed url module. Spelled
/// against the raw strings because the direct reading, `ends_with(&format!(".{suffix}"))`,
/// allocates once per candidate on a set that grows with the constellation.
fn ends_with_dotted(text: &str, suffix: &str) -> bool {
    let Some(head) = text.strip_suffix(suffix) else {
        return false;
    };

    head.as_bytes().last() == Some(&b'.')
}

/// The dotted module path of a Python file: `app/partner/urls/page_urls.py` ->
/// `app.partner.urls.page_urls`, and a package `__init__.py` to the package
/// itself (`app/partner/urls/__init__.py` -> `app.partner.urls`). Matches the
/// module strings Django `include('app.partner.urls')` calls carry.
#[doc(hidden)]
pub fn module_of(file_path: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let without_extension = normalized.strip_suffix(".py").unwrap_or(&normalized);
    let module = without_extension.strip_suffix("/__init__").unwrap_or(without_extension);

    module.replace('/', ".")
}

/// The URL pattern carried in a route node's qualified name
/// (`…::route::<pattern>` -> `<pattern>`), the fragment its own urls.py declares.
#[doc(hidden)]
pub fn route_pattern(qualified_name: &str) -> &str {
    qualified_name.split("route::").nth(1).unwrap_or(qualified_name)
}

/// The mounted URL prefix of `module`, walking the mount map child -> parent and
/// joining each include's own URL fragment root-first.
///
/// A route's pattern is only the fragment its own `urls.py` declares (`create/`),
/// which is not a URL anyone can request: the path a request actually takes is that
/// fragment under every `path('...', include(...))` it is mounted below. Django
/// scatters those prefixes across a chain of url modules; this reassembles them, so
/// the URL map can print `production/line/schedule/entry/create/` instead of
/// `create/`. Empty for a root urlconf's own routes. Bounded by
/// [`NAMESPACE_DEPTH_MAX`]; a visited set breaks any cyclic include.
#[doc(hidden)]
pub fn url_prefix_chain(module: &str, mounts: &FxHashMap<String, (String, String)>) -> String {
    let mut fragments: Vec<String> = Vec::new();
    let mut visited: FxHashSet<String> = FxHashSet::default();
    let mut current = module.to_string();
    let mut depth: u32 = 0;

    while let Some((fragment, parent)) = mounts.get(&current) {
        depth += 1;

        assert!(depth <= NAMESPACE_DEPTH_MAX, "url prefix walk exceeded {NAMESPACE_DEPTH_MAX} levels");

        if !visited.insert(current.clone()) {
            break;
        }

        if !fragment.is_empty() {
            fragments.push(fragment.clone());
        }

        current = parent.clone();
    }

    fragments.reverse();

    fragments.join("")
}

/// The namespace chain from the root urlconf down to `module`, walking the include
/// map child -> parent. Each include hop contributes its namespace when it has one
/// (a `namespace=` kwarg or the child's app_name); a hop with neither still chains
/// upward but adds no level. The topmost module reached contributes its own
/// app_name, the project-wide application namespace (django-spire's root
/// `app_name='django_spire'`) that no include carries. Returned root-first
/// (`["django_spire", "auth", "user", "page"]`), or `None` when nothing on the path
/// carries a namespace. Bounded by [`NAMESPACE_DEPTH_MAX`]; a visited set breaks any
/// cyclic include.
#[doc(hidden)]
pub fn namespace_chain(
    module: &str,
    includes: &FxHashMap<String, (Option<String>, String)>,
    app_name_by_module: &FxHashMap<String, String>,
) -> Option<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();
    let mut visited: FxHashSet<String> = FxHashSet::default();
    let mut current = module.to_string();
    let mut depth: u32 = 0;

    while let Some((namespace, parent)) = includes.get(&current) {
        depth += 1;

        assert!(depth <= NAMESPACE_DEPTH_MAX, "namespace walk exceeded {NAMESPACE_DEPTH_MAX} levels");

        if !visited.insert(current.clone()) {
            break;
        }

        if let Some(level) = namespace {
            chain.push(level.clone());
        }

        current = parent.clone();
    }

    if let Some(root_namespace) = app_name_by_module.get(&current) {
        chain.push(root_namespace.clone());
    }

    if chain.is_empty() {
        return None;
    }

    chain.reverse();

    Some(chain)
}

/// The project segment of a node id (`blog::app.py::X` -> `blog`): everything
/// before the first `::` separator, or the whole string if absent.
pub(crate) fn project_prefix(node_id: &str) -> &str {
    node_id.split("::").next().unwrap_or(node_id)
}

/// The installed package name a project's root path ends in, the import-package
/// key a companion is linked by: `.../site-packages/django_spire` -> `django_spire`.
/// `None` when the path has no final segment.
pub(crate) fn package_root_name(root_path: &str) -> Option<&str> {
    root_path.rsplit(['/', '\\']).find(|segment| !segment.is_empty())
}

/// The simple class name a class node id ends in
/// (`blog::models.py::Article` -> `Article`).
pub(crate) fn class_name_of(class_id: &str) -> &str {
    let tail = class_id.rsplit("::").next().unwrap_or(class_id);

    tail.split('.').next().unwrap_or(tail)
}

/// Whether a file path's stem (its module name) equals `stem`, so a receiver that
/// names a module can be matched to the file that defines it.
pub(crate) fn file_stem_is(file_path: &str, stem: &str) -> bool {
    let name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);

    name.strip_suffix(".py").is_some_and(|module| module == stem)
}

/// The project id that canonically owns a template name: its leading namespace
/// segment with underscores normalized to hyphens (`django_spire/page/full_page
/// .html` -> `django-spire`). A bare name (`base.html`) maps to itself, matching
/// no project, so it stays ambiguous rather than binding to an arbitrary copy.
#[doc(hidden)]
pub fn template_owner(name: &str) -> String {
    name.split('/').next().unwrap_or_default().replace('_', "-")
}
