//! Facts about a node that several tools need to agree on.
//!
//! What counts as a definition, what counts as an orphan, what role a
//! symbol plays in a Django project. Each is a judgment call, so each is
//! made once here rather than re-made per tool.

use constellation_graph::{
    Language, Node, NodeKind, Profile,
    is_framework_reached, is_test_path,
};

/// The method names dispatched dynamically across the whole codebase (Django
/// queryset/manager builtins, model lifecycle hooks, and dict/list/str methods),
/// for which the name-global dark-caller count is workspace-wide dispatch noise,
/// not hidden callers of any one definition. `qs.filter()` / `obj.save()` /
/// `data.get()` appear thousands of times with no statically-bound receiver, so a
/// model method named `save` would otherwise report every `.save()` in the
/// constellation as its dark callers.
///
/// The list shares most of its names with `constellation_resolution::QUERYSET_BUILTINS`
/// without being it: that one bars a name from the resolver's by-name path, this one
/// calls a name's dark-caller count noise. They are kept apart deliberately, and the
/// cost of that is a name added here is not a name added there, so both are worth a
/// look when either changes.
const DISPATCH_METHOD_NAMES: &[&str] = &[
    "add",
    "aggregate",
    "all",
    "annotate",
    "append",
    "bulk_create",
    "bulk_update",
    "clean",
    "clean_fields",
    "count",
    "create",
    "defer",
    "delete",
    "distinct",
    "exclude",
    "exists",
    "extend",
    "filter",
    "first",
    "full_clean",
    "get",
    "get_or_create",
    "items",
    "keys",
    "last",
    "latest",
    "none",
    "only",
    "order_by",
    "pop",
    "prefetch_related",
    "refresh_from_db",
    "remove",
    "save",
    "select_related",
    "setdefault",
    "update",
    "update_or_create",
    "values",
    "values_list",
];

/// Whether a symbol name is a codebase-wide dynamic-dispatch method, so its
/// name-global unresolved count is noise rather than a dark-caller signal.
pub(crate) fn is_dispatch_method_name(name: &str) -> bool {
    DISPATCH_METHOD_NAMES.contains(&name)
}

/// Whether a node kind defines behavior worth seeding structural ranking
/// from: function, method, class, model, view, or route, not file/import/field.
pub(crate) fn is_definition_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Class
            | NodeKind::Model
            | NodeKind::View
            | NodeKind::Route
    )
}

/// Whether `qualified` ends with the dotted `path` at a name boundary:
/// the character just before it is `.` (a nested owner) or `::` (a top-level
/// owner), never mid-identifier. `a/b.py::Sync.save` ends with `Sync.save`,
/// not with `ave`.
#[doc(hidden)]
pub fn qualified_name_ends_with(qualified: &str, path: &str) -> bool {
    assert!(!path.is_empty(), "dotted path must not be empty");

    if qualified == path {
        return true;
    }

    match qualified.strip_suffix(path) {
        Some(head) => head.ends_with('.') || head.ends_with("::"),
        None => false,
    }
}

/// The canonical Python import for a definition, from its file path and the top-level
/// owner of its qualified name: `app/x/models.py::Order.save` -> `from app.x.models
/// import Order`. A package `__init__.py` imports as the package. `None` for non-Python
/// nodes and for kinds that are not importable names. A re-export in an `__init__.py`
/// may offer a shorter path; this defining-module import is always valid, and saves an
/// LLM guessing the dotted path (file path is not the import path).
pub(crate) fn python_import_line(node: &Node) -> Option<String> {
    if node.language != Language::Python {
        return None;
    }

    if matches!(
        node.kind,
        NodeKind::File
            | NodeKind::Import
            | NodeKind::Module
            | NodeKind::Route
            | NodeKind::Template
            | NodeKind::Selector
            | NodeKind::Parameter
            | NodeKind::External
    ) {
        return None;
    }

    let path = node.file_path.replace('\\', "/");
    let module = path.strip_suffix(".py")?;
    let module = module.strip_suffix("/__init__").unwrap_or(module);

    if module.is_empty() {
        return None;
    }

    let dotted = module.replace('/', ".");
    let after = node.qualified_name.rsplit("::").next().unwrap_or(&node.qualified_name);
    let owner = after.split('.').next().unwrap_or(after);

    if owner.is_empty() {
        return None;
    }

    Some(format!("from {dotted} import {owner}"))
}

/// Whether a symbol is worth a "no covering tests" flag: a top-level definition a reader
/// would test directly (a model, class, free function, or view), not a method, property,
/// nested `Meta`, or dunder, which inherit coverage from their owner and only add noise.
pub(crate) fn is_coverage_checkable(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Class | NodeKind::Model | NodeKind::Function | NodeKind::View)
        && node.name != "Meta"
        && !(node.name.starts_with("__") && node.name.ends_with("__"))
}

/// Whether an edgeless definition is a real dead-code candidate, not a framework hook
/// that simply has no static caller: tests, migrations, package initializers, dunder
/// methods, app configs, and a management command's `handle` are excluded. Reads the
/// one shared definition of the framework-reached set, under the hook names the
/// workspace's `profile` makes effective, the same set flow detection uses as its
/// framework entry-point list.
pub(crate) fn is_orphan_candidate(profile: &Profile, node: &Node) -> bool {
    !is_framework_reached(profile, &node.name, &node.file_path)
}

/// A field's type suffix (e.g. " - CharField(max_length=200)") built from its
/// signature, or an empty string when the extractor captured none.
pub(crate) fn field_signature(field: &Node) -> String {
    match &field.signature {
        Some(signature) if !signature.is_empty() => format!(" - {}", signature.replace('\n', " ")),
        _ => String::new(),
    }
}

/// The `Owner.member` tail of a qualified name (`a/b.py::Cls.save` -> `Cls.save`),
/// the form a caller passes to `node`/`callers`/`callees` to disambiguate an
/// overloaded name.
fn qualified_tail(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// The shortest form a caller can pass to target this exact node: the
/// `Owner.member` tail for a method, or the full `file::name` qualified form for
/// a free function (which has no owner to disambiguate by). Both are accepted by
/// [`seed_nodes`].
pub(crate) fn targetable_name(node: &Node) -> &str {
    let tail = qualified_tail(&node.qualified_name);

    if tail.contains('.') {
        tail
    } else {
        node.qualified_name.as_str()
    }
}

/// The architectural layer a symbol belongs to, inferred from this
/// codebase's strict file/name conventions, so an agent can tell a page view from
/// a json endpoint, a service from a queryset, at a glance. `None` for ordinary
/// code that matches no convention.
pub(crate) fn symbol_role(node: &Node) -> Option<&'static str> {
    let path = node.file_path.as_str();
    let name = node.name.as_str();

    match node.kind {
        NodeKind::Route => return Some("route"),
        NodeKind::Template => return Some("template"),
        NodeKind::Selector => return Some("css-selector"),
        NodeKind::Model => return Some("model"),
        _ => {}
    }

    if is_test_path(path) {
        return Some("test");
    }

    // Class-name conventions, most specific first.
    if name.ends_with("QuerySet") || name.ends_with("Manager") {
        return Some("queryset");
    }
    if name.ends_with("Service") {
        return Some("service");
    }
    if name.ends_with("Form") {
        return Some("form");
    }
    if name.ends_with("Serializer") {
        return Some("serializer");
    }
    if name.ends_with("Admin") {
        return Some("admin");
    }
    if name.ends_with("Choices") {
        return Some("choices");
    }

    // File-path conventions for the view sub-layers and the service/data layers.
    let role = if path.contains("json_views") {
        "json-view"
    } else if path.contains("form_views") {
        "form-view"
    } else if path.contains("page_views") {
        "page-view"
    } else if path.contains("template_views") {
        "template-view"
    } else if path.ends_with("/views.py") || path.contains("/views/") {
        "view"
    } else if path.contains("queryset") {
        "queryset"
    } else if path.contains("factories") || path.contains("factory") {
        "factory"
    } else if path.ends_with("/services.py") || path.contains("/services/") {
        "service"
    } else if path.ends_with("/forms.py") {
        "form"
    } else if path.ends_with("/admin.py") {
        "admin"
    } else if path.ends_with("/serializers.py") {
        "serializer"
    } else {
        return None;
    };

    Some(role)
}

#[cfg(test)]
mod dispatch_name_tests {
    use super::is_dispatch_method_name;

    #[test]
    fn common_dispatch_methods_are_recognized() {
        for name in ["get", "save", "filter", "create", "delete", "all", "values", "refresh_from_db"] {
            assert!(is_dispatch_method_name(name), "{name:?} is a codebase-wide dispatch method");
        }
    }

    #[test]
    fn distinctive_names_are_not_dispatch() {
        for name in ["recalculate_totals", "generate_order_number", "Inventory", "Order"] {
            assert!(!is_dispatch_method_name(name), "{name:?} is distinctive, a real dark-caller signal");
        }
    }
}
