use crate::edge::EdgeKind;
use crate::paths::{is_management_command_path, is_migration_path, is_test_path};

/// The method and class names Django (and django_spire) invoke with no static
/// call site: lifecycle hooks, protocol methods, and app configuration entry
/// points. One definition serves two opposite readers. Dead-code detection uses
/// it to suppress false positives, since a symbol here has no caller by design;
/// flow detection uses it as the framework entry-point set, which is the same
/// list read the other way round.
pub const FRAMEWORK_HOOK_NAMES: &[&str] = &[
    "base_breadcrumb",
    "breadcrumbs",
    "clean",
    "delete",
    "get_absolute_url",
    "handle",
    "Meta",
    "ready",
    "save",
];

/// The class-name suffixes Django resolves by convention rather than by import:
/// an `AppConfig`, a migration class, an admin registration.
pub const FRAMEWORK_CLASS_SUFFIXES: &[&str] = &["Admin", "Config", "Migration"];

/// Whether a symbol name is one the framework calls without a static call site.
pub fn is_framework_hook_name(name: &str) -> bool {
    FRAMEWORK_HOOK_NAMES.contains(&name)
}

/// Whether a symbol name is a dunder (`__init__`, `__str__`), which the language
/// itself dispatches rather than any caller in the codebase.
pub fn is_dunder_name(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// Whether a class name ends in one of the [`FRAMEWORK_CLASS_SUFFIXES`].
pub fn has_framework_class_suffix(name: &str) -> bool {
    FRAMEWORK_CLASS_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Whether a symbol is reached by the framework rather than by a static call, so
/// its lack of callers proves nothing. `true` covers tests, migrations,
/// management commands, package initializers and admin registration modules,
/// dunder methods, the lifecycle hook names, and the conventional class
/// suffixes.
pub fn is_framework_reached(name: &str, file_path: &str) -> bool {
    let path = file_path.replace('\\', "/");

    if is_test_path(&path) || is_migration_path(&path) || is_management_command_path(&path) {
        return true;
    }

    if path.ends_with("__init__.py") || path.ends_with("admin.py") {
        return true;
    }

    is_dunder_name(name) || is_framework_hook_name(name) || has_framework_class_suffix(name)
}

/// Whether a reference covers its target as a test: a `Tests` edge (a `TestCase`
/// bound to it by the `XTestCase -> X` naming convention), or any non-structural
/// reference from a file under a test path, which is a test exercising the symbol
/// however it reaches it, including the instantiation that is the common Django
/// model-test pattern.
pub fn is_covering_ref(kind: EdgeKind, caller_path: &str) -> bool {
    kind == EdgeKind::Tests || (kind != EdgeKind::Contains && is_test_path(caller_path))
}

/// The Django model field constructors that declare a relation to another model,
/// naming it in their first argument.
pub const RELATION_FIELDS: &[&str] =
    &["ForeignKey", "ManyToManyField", "OneToOneField", "GenericRelation"];

/// The related model a field's declaration names
/// (`ForeignKey(Location, related_name='lots')` -> `Location`), or `None` when the
/// field declares a column rather than a relation.
///
/// A field node's signature is its declaration, which both a reader and the
/// receiver-typing pass consume: the reader wants the column's type and arguments,
/// the typing pass wants only the related model. This is the one parser of that
/// format, so the two cannot drift into disagreeing about it.
pub fn relation_field_target(signature: &str) -> Option<&str> {
    let (constructor, arguments) = signature.split_once('(')?;

    if !RELATION_FIELDS.contains(&constructor) {
        return None;
    }

    let first = arguments.split(',').next()?.trim_end_matches(')').trim();

    if first.is_empty() || first.contains('=') {
        return None;
    }

    Some(first)
}

#[cfg(test)]
mod tests {
    use crate::edge::EdgeKind;

    use super::{
        has_framework_class_suffix, is_covering_ref, is_dunder_name, is_framework_hook_name,
        is_framework_reached,
    };

    #[test]
    fn coverage_needs_a_tests_edge_or_a_reference_from_a_test_file() {
        assert!(is_covering_ref(EdgeKind::Tests, "app/services.py"), "a Tests edge always covers");

        assert!(
            is_covering_ref(EdgeKind::Instantiates, "app/tests/test_models.py"),
            "a Django model test instantiates rather than calls",
        );

        assert!(
            !is_covering_ref(EdgeKind::Contains, "app/tests/test_models.py"),
            "containment is structural, never coverage",
        );

        assert!(
            !is_covering_ref(EdgeKind::Calls, "app/services.py"),
            "a call from source is not coverage",
        );
    }

    #[test]
    fn lifecycle_hooks_are_framework_reached() {
        for name in ["save", "clean", "delete", "ready", "handle", "Meta", "get_absolute_url"] {
            assert!(is_framework_hook_name(name), "{name:?} is a framework hook");
        }

        assert!(!is_framework_hook_name("recalculate_totals"), "ordinary code is not a hook");
    }

    #[test]
    fn dunders_need_both_affixes_and_a_name_between_them() {
        assert!(is_dunder_name("__init__"));
        assert!(is_dunder_name("__str__"));
        assert!(!is_dunder_name("__"), "the bare affix is not a dunder");
        assert!(!is_dunder_name("_private"), "a single underscore is not a dunder");
    }

    #[test]
    fn conventional_class_suffixes_are_recognized() {
        assert!(has_framework_class_suffix("OrdersConfig"));
        assert!(has_framework_class_suffix("Migration"));
        assert!(has_framework_class_suffix("OrderAdmin"));
        assert!(!has_framework_class_suffix("OrderService"));
    }

    #[test]
    fn framework_reach_covers_paths_and_names_together() {
        assert!(is_framework_reached("run", "app/tests/test_run.py"), "a test is framework-reached");
        assert!(is_framework_reached("Migration", "app/migrations/0001.py"));
        assert!(is_framework_reached("handle", "app/management/commands/sync.py"));
        assert!(is_framework_reached("anything", "app/__init__.py"));
        assert!(is_framework_reached("save", "app/models.py"), "a lifecycle hook anywhere");

        assert!(
            !is_framework_reached("recalculate_totals", "app/services.py"),
            "ordinary code is a real dead-code candidate",
        );
    }
}
