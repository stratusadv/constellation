//! Risk-ranked `changed`, against a real git working tree.
//!
//! `changed_text` shells out to `git diff`, so these need a repository rather
//! than a synthetic diff: a test that fed it a canned hunk list would not
//! exercise the half most likely to break.

use std::path::Path;
use std::process::Command;

use constellation_graph::{
    Edge, EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_mcp::changed_text;
use constellation_mcp::cursor::Page;
use constellation_store::{FileIndex, Store};

/// The fixture module, committed and then edited so every symbol in it lands in
/// the diff.
const MODELS_SOURCE: &str = "\
class Order:
    def recalculate_totals(self):
        return 1

    def verify_password(self):
        return True

    def format_label(self):
        return 'label'
";

/// The same module with every method body changed.
const MODELS_EDITED: &str = "\
class Order:
    def recalculate_totals(self):
        return 2

    def verify_password(self):
        return False

    def format_label(self):
        return 'other'
";

/// A node spanning the given 1-based line range.
fn node(kind: NodeKind, name: &str, start: u32, end: u32) -> Node {
    Node::new(
        NodeId::from_raw(format!("shop::orders/models.py::Order.{name}")),
        ProjectId::new("shop"),
        kind,
        NodeIdentity {
            name: name.to_string(),
            qualified_name: format!("orders/models.py::Order.{name}"),
            file_path: "orders/models.py".to_string(),
            language: Language::Python,
        },
        Span::new(start, end, 0, 0),
        0,
    )
}

/// A git invocation, returning whether it succeeded.
fn git(root: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A temporary git repository holding the fixture module, with a store whose
/// graph describes it. Returns `None` when git is unavailable.
fn repository() -> Option<(tempfile::TempDir, Store, ProjectId)> {
    if !Command::new("git").arg("--version").output().is_ok_and(|out| out.status.success()) {
        return None;
    }

    let directory = tempfile::tempdir().expect("a temporary directory");
    let root = directory.path().to_path_buf();

    std::fs::create_dir_all(root.join("orders")).expect("the app directory");
    std::fs::write(root.join("orders/models.py"), MODELS_SOURCE).expect("the fixture module");

    if !git(&root, &["init", "--initial-branch=main"])
        || !git(&root, &["config", "user.email", "tests@example.invalid"])
        || !git(&root, &["config", "user.name", "constellation tests"])
        || !git(&root, &["add", "-A"])
        || !git(&root, &["commit", "-m", "fixture", "--no-gpg-sign"])
    {
        return None;
    }

    let store = Store::open_in_memory().expect("an in-memory store");
    let project = ProjectId::new("shop");

    store
        .upsert_project(&project, "shop", &root.to_string_lossy())
        .expect("the project row");

    // Three methods, deliberately differing only in what the risk score can see:
    // one has a caller, one has a security-sensitive name, one has neither.
    let recalculate = node(NodeKind::Method, "recalculate_totals", 2, 3);
    let verify = node(NodeKind::Method, "verify_password", 5, 6);
    let format = node(NodeKind::Method, "format_label", 8, 9);

    let caller = Node::new(
        NodeId::from_raw("shop::orders/views.py::checkout_view".to_string()),
        ProjectId::new("shop"),
        NodeKind::View,
        NodeIdentity {
            name: "checkout_view".to_string(),
            qualified_name: "orders/views.py::checkout_view".to_string(),
            file_path: "orders/views.py".to_string(),
            language: Language::Python,
        },
        Span::new(1, 4, 0, 0),
        0,
    );

    let nodes = vec![recalculate.clone(), verify.clone(), format.clone(), caller.clone()];
    let edges = vec![Edge::new(caller.id.clone(), recalculate.id.clone(), EdgeKind::Calls)];

    let file = FileIndex {
        path: "orders/models.py",
        content_hash: "h",
        language: Language::Python,
        size_bytes: 1,
        modified_at_ms: 0,
        source: "",
    };

    store.persist_file(&project, &file, &nodes, &edges, &[], &[], &[]).expect("persisting");

    Some((directory, store, project))
}

/// The `changed` response for the current working tree.
fn changed(store: &Store, page: &Page, generation: u64) -> String {
    changed_text(store, None, 25, page, generation).expect("scoring the diff")
}

#[test]
fn a_clean_tree_reports_no_changed_symbols() {
    let Some((_directory, store, _project)) = repository() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    let text = changed(&store, &Page::default(), 0);

    assert!(text.contains("no changed symbols"), "an unedited tree says so plainly: {text}");
}

#[test]
fn changed_symbols_come_back_risk_ranked_with_their_reasons() {
    let Some((directory, store, _project)) = repository() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    std::fs::write(directory.path().join("orders/models.py"), MODELS_EDITED)
        .expect("editing the module");

    let text = changed(&store, &Page::default(), 0);

    assert!(text.contains("by review risk"), "the header states the ordering: {text}");
    assert!(text.contains("risk 0."), "every row carries a score: {text}");
    assert!(text.contains("no tests"), "and the reasons behind it: {text}");

    // The security-sensitive name must outrank the neutral one, all else equal.
    let verify_at = text.find("verify_password").expect("the sensitive method is listed");
    let format_at = text.find("format_label").expect("the neutral method is listed");

    assert!(
        verify_at < format_at,
        "a security-adjacent name ranks above a neutral one: {text}",
    );
}

#[test]
fn a_missing_derived_pass_is_named_rather_than_scored_as_zero() {
    let Some((directory, store, _project)) = repository() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    std::fs::write(directory.path().join("orders/models.py"), MODELS_EDITED)
        .expect("editing the module");

    let text = changed(&store, &Page::default(), 0);

    assert!(
        text.contains("scored without"),
        "the absent factors are named rather than silently treated as zero: {text}",
    );
    assert!(text.contains("constellation history"), "with the command that supplies churn: {text}");
    assert!(text.contains("constellation flows"), "and the one that supplies flows: {text}");
    assert!(text.contains("renormalized"), "and the fact the rest were rescaled: {text}");
}

#[test]
fn the_ordering_is_stable_across_runs() {
    let Some((directory, store, _project)) = repository() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    std::fs::write(directory.path().join("orders/models.py"), MODELS_EDITED)
        .expect("editing the module");

    assert_eq!(
        changed(&store, &Page::default(), 0),
        changed(&store, &Page::default(), 0),
        "two runs over one working tree emit byte-identical output",
    );
}

#[test]
fn a_truncated_page_offers_a_cursor_into_the_tail() {
    let Some((directory, store, _project)) = repository() else {
        eprintln!("changed: git is not on PATH; skipping");

        return;
    };

    std::fs::write(directory.path().join("orders/models.py"), MODELS_EDITED)
        .expect("editing the module");

    let first = changed_text(&store, None, 1, &Page::default(), 4).expect("scoring the diff");

    assert!(first.contains("next: cursor=1.4"), "the tail is reachable: {first}");

    let page = constellation_mcp::cursor::resolve(Some("1.4"), 4);
    let second = changed_text(&store, None, 1, &page, 4).expect("scoring the diff");

    assert!(second.contains("risk 0."), "the second page holds the next symbol: {second}");

    let first_line = first.lines().find(|line| line.contains("risk 0.")).unwrap_or_default();
    let second_line = second.lines().find(|line| line.contains("risk 0.")).unwrap_or_default();

    assert_ne!(first_line, second_line, "and it is a different symbol from the first page");
}
