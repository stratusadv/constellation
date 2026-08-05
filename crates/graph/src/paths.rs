//! The path predicates every layer shares.
//!
//! What counts as a test file, a generated file, a migration: decided once,
//! here, because `index` skips them during the walk, `linking` refuses them as
//! cross-project link targets, and `mcp` sinks them below hand-written source.
//! Three layers asking the same question three different ways is how they come
//! to disagree, which is what happened before this module owned all of it.
//!
//! Every predicate is allocation-free. They run per node inside listing sorts
//! and per file inside the walk, so a lowercased copy of the path per call
//! would be an allocation in two of the hottest loops in the tree. Matching is
//! ASCII case-insensitive, which every name these compare against is.

/// The separators a path may carry. Stored paths use forward slashes, but a
/// path taken straight off a Windows filesystem reaches these with backslashes.
const SEPARATORS: [char; 2] = ['/', '\\'];

/// The directory names holding machine-generated, vendored, or collected output
/// rather than hand-written source.
const GENERATED_DIRECTORIES: [&str; 5] =
    ["migrations", "node_modules", "static_files", "staticfiles", "vendor"];

/// The file-name suffixes of a minified or bundled asset: vendor code with no
/// readable structure left in it.
const MINIFIED_SUFFIXES: [&str; 3] = [".bundle.js", ".min.css", ".min.js"];

/// The file-name suffixes of a generated source file that is not minified.
const GENERATED_SUFFIXES: [&str; 1] = ["_pb2.py"];

/// Whether `text` begins with `prefix`, ASCII case-insensitive, without allocating.
fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Whether `text` ends with `suffix`, ASCII case-insensitive, without allocating.
fn ends_with_ignore_ascii_case(text: &str, suffix: &str) -> bool {
    text.len() >= suffix.len()
        && text.as_bytes()[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// The final segment of a path: its file name, or the whole path when it holds
/// no separator.
pub fn base_name(path: &str) -> &str {
    let base = path.rsplit(SEPARATORS).next().unwrap_or(path);

    debug_assert!(base.len() <= path.len(), "a base name is no longer than its path");

    base
}

/// The segments of a path except the last. The last is the file name, so these
/// are the directory names a "does this live under X/" question is really about.
fn directories(path: &str) -> impl Iterator<Item = &str> {
    let cut = path.rfind(SEPARATORS).unwrap_or(0);

    path[..cut].split(SEPARATORS).filter(|segment| !segment.is_empty())
}

/// Whether any directory in `path` is named one of `names`.
fn under_directory(path: &str, names: &[&str]) -> bool {
    directories(path)
        .any(|segment| names.iter().any(|name| segment.eq_ignore_ascii_case(name)))
}

/// The leading path segment of a file path, which in a Django layout names the
/// app a symbol belongs to. Crossing an app boundary is the coupling signal that
/// matters; crossing a file inside one app is not.
pub fn app_segment(path: &str) -> &str {
    let segment = path.split(SEPARATORS).next().unwrap_or(path);

    debug_assert!(segment.len() <= path.len(), "a segment is no longer than its path");

    segment
}

/// Whether a path is a test file, by the layout and naming conventions Django
/// and pytest projects use. Listing tools sink these below hand-written source,
/// and a test never seeds an execution flow.
pub fn is_test_path(path: &str) -> bool {
    if under_directory(path, &["test", "tests"]) {
        return true;
    }

    let base = base_name(path);

    starts_with_ignore_ascii_case(base, "test_")
        || starts_with_ignore_ascii_case(base, "conftest")
        || ends_with_ignore_ascii_case(base, "_test.py")
        || ends_with_ignore_ascii_case(base, ".test.js")
        || ends_with_ignore_ascii_case(base, ".spec.js")
}

/// Whether a path is a minified or bundled asset (`*.min.js`, `*.min.css`,
/// `*.bundle.js`).
///
/// The walk excludes these from the index entirely rather than sinking them:
/// they parse into thousands of mangled one-letter "symbols" that pollute
/// search, files, and counts, and never help an agent. [`is_generated_path`] is
/// the wider question, and answers true for everything this does.
pub fn is_minified_path(path: &str) -> bool {
    let base = base_name(path);

    MINIFIED_SUFFIXES.iter().any(|suffix| ends_with_ignore_ascii_case(base, suffix))
}

/// Whether a path is machine-generated or minified: Django migrations, vendored
/// or collected static assets, minified and bundled JavaScript or CSS, generated
/// protobuf stubs, packages under `node_modules`. Such files are real graph nodes
/// but rarely what an agent wants to read first, so the listing tools sink them
/// below source and the linker refuses them as cross-project targets.
pub fn is_generated_path(path: &str) -> bool {
    if is_minified_path(path) {
        return true;
    }

    let base = base_name(path);

    if GENERATED_SUFFIXES.iter().any(|suffix| ends_with_ignore_ascii_case(base, suffix)) {
        return true;
    }

    under_directory(path, &GENERATED_DIRECTORIES)
}

/// Whether a path is a Django migration, which is generated, never called
/// directly, and never a meaningful execution-flow entry point.
pub fn is_migration_path(path: &str) -> bool {
    under_directory(path, &["migrations"])
}

/// Whether a path holds a Django management command, whose `Command.handle` the
/// framework invokes with no static call site.
pub fn is_management_command_path(path: &str) -> bool {
    let mut previous: Option<&str> = None;

    for segment in directories(path) {
        let nested_in_management =
            previous.is_some_and(|name| name.eq_ignore_ascii_case("management"));

        if nested_in_management && segment.eq_ignore_ascii_case("commands") {
            return true;
        }

        previous = Some(segment);
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{
        app_segment, base_name, is_generated_path, is_management_command_path, is_migration_path,
        is_minified_path, is_test_path,
    };

    #[test]
    fn app_segment_takes_the_leading_directory() {
        assert_eq!(app_segment("orders/models.py"), "orders");
        assert_eq!(app_segment("orders\\models.py"), "orders", "windows separators too");
        assert_eq!(app_segment("manage.py"), "manage.py", "a root file is its own segment");
    }

    #[test]
    fn base_name_takes_the_trailing_segment() {
        assert_eq!(base_name("orders/models.py"), "models.py");
        assert_eq!(base_name("orders\\models.py"), "models.py", "windows separators too");
        assert_eq!(base_name("manage.py"), "manage.py", "a bare name is its own base");
    }

    #[test]
    fn test_paths_cover_the_layouts_we_index() {
        for path in [
            "app/tests/test_views.py",
            "tests/test_views.py",
            "app/test/helpers.py",
            "app/views_test.py",
            "app/conftest.py",
            "static/app.test.js",
            "static/app.spec.js",
        ] {
            assert!(is_test_path(path), "{path:?} is a test path");
        }

        for path in ["app/views.py", "app/latest/models.py", "app/contest.py"] {
            assert!(!is_test_path(path), "{path:?} is hand-written source");
        }
    }

    #[test]
    fn a_file_named_for_a_test_directory_is_not_one() {
        assert!(!is_test_path("app/tests.py"), "a module named tests.py holds tests");
        assert!(is_test_path("app/tests/models.py"), "a directory named tests does");
    }

    #[test]
    fn generated_paths_cover_migrations_vendor_and_bundles() {
        for path in [
            "app/migrations/0001_initial.py",
            "migrations/0002_auto.py",
            "static/vendor/chart.js",
            "staticfiles/app.css",
            "static_files/app.css",
            "node_modules/left-pad/index.js",
            "static/js/app.min.js",
            "static/js/app.bundle.js",
            "static/css/app.min.css",
            "proto/service_pb2.py",
        ] {
            assert!(is_generated_path(path), "{path:?} is generated");
        }

        assert!(!is_generated_path("app/models.py"), "source is not generated");
    }

    #[test]
    fn every_minified_path_is_also_a_generated_one() {
        for path in ["static/js/app.min.js", "static/js/a.bundle.js", "static/css/a.min.css"] {
            assert!(is_minified_path(path), "{path:?} is minified");
            assert!(is_generated_path(path), "{path:?} is therefore generated");
        }

        assert!(!is_minified_path("app/migrations/0001_initial.py"), "a migration is readable");
        assert!(!is_minified_path("app/models.py"), "source is not minified");
    }

    #[test]
    fn path_predicates_ignore_ascii_case_and_separator_style() {
        assert!(is_generated_path("static\\JS\\App.Min.JS"), "case and backslashes both");
        assert!(is_test_path("APP\\TESTS\\views.py"), "case and backslashes both");
        assert!(is_migration_path("App\\Migrations\\0001_initial.py"));
    }

    #[test]
    fn migration_and_command_paths_are_recognized_at_any_depth() {
        assert!(is_migration_path("app/migrations/0001_initial.py"));
        assert!(is_migration_path("migrations/0001_initial.py"));
        assert!(!is_migration_path("app/migration_notes.py"));

        assert!(is_management_command_path("app/management/commands/sync.py"));
        assert!(is_management_command_path("management/commands/sync.py"));
        assert!(!is_management_command_path("app/management/mixins.py"));
        assert!(
            !is_management_command_path("app/commands/sync.py"),
            "commands must sit directly under management",
        );
    }
}
