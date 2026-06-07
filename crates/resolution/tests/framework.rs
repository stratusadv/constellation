use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use constellation_graph::{
    EdgeKind, Language, Node, NodeId, NodeIdentity, NodeKind, ProjectId, Span,
};
use constellation_resolution::{
    DjangoResolver, FrameworkResolver, ImportMapping, ResolutionContext, ResolvedBy, ResolvedRef,
    UnresolvedRef, is_pascal_word,
};

struct FakeContext {
    files: Vec<(String, Option<String>)>,
    nodes: Vec<Arc<Node>>,
    root: PathBuf,
}

impl FakeContext {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            nodes: Vec::new(),
            root: PathBuf::from("/tmp/blog"),
        }
    }

    fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.push((path.to_string(), Some(contents.to_string())));
        self
    }

    fn with_node(mut self, name: &str, file: &str, kind: NodeKind) -> Self {
        self.nodes.push(node(name, file, kind));
        self
    }
}

impl ResolutionContext for FakeContext {
    fn nodes_by_name(&self, name: &str) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.name == name).cloned().collect()
    }

    fn nodes_by_lower_name(&self, lower_name: &str) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.name.to_lowercase() == lower_name).cloned().collect()
    }

    fn nodes_by_qualified_name(&self, qualified_name: &str) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.qualified_name == qualified_name).cloned().collect()
    }

    fn nodes_by_kind(&self, kind: NodeKind) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.kind == kind).cloned().collect()
    }

    fn nodes_in_file(&self, file_path: &str) -> Vec<Arc<Node>> {
        self.nodes.iter().filter(|node| node.file_path == file_path).cloned().collect()
    }

    fn file_exists(&self, file_path: &str) -> bool {
        self.files.iter().any(|(path, _)| path == file_path)
    }

    fn read_file(&self, file_path: &str) -> Option<String> {
        self.files.iter().find(|(path, _)| path == file_path).and_then(|(_, contents)| contents.clone())
    }

    fn all_files(&self) -> Vec<String> {
        self.files.iter().map(|(path, _)| path.clone()).collect()
    }

    fn project_root(&self) -> &Path {
        self.root.as_path()
    }

    fn import_mappings(&self, _file_path: &str, _language: Language) -> Vec<ImportMapping> {
        Vec::new()
    }
}

fn node(name: &str, file: &str, kind: NodeKind) -> Arc<Node> {
    let project = ProjectId::new("blog");
    let id = NodeId::new(&project, &format!("{file}::{name}"));

    let identity = NodeIdentity {
        name: name.to_string(),
        qualified_name: format!("{file}::{name}"),
        file_path: file.to_string(),
        language: Language::Python,
    };

    Arc::new(Node::new(id, project, kind, identity, Span::new(1, 1, 0, 0), 0))
}

fn reference(name: &str) -> UnresolvedRef {
    let from = NodeId::new(&ProjectId::new("blog"), "urls.py::urlpatterns");

    UnresolvedRef::new(from, name, EdgeKind::RoutesTo, 1, 0, "blog/urls.py", Language::Python)
}

#[test]
fn detect_reads_django_from_a_requirements_marker() {
    let context = FakeContext::new().with_file("requirements.txt", "Django==5.0\npsycopg2\n");

    assert!(DjangoResolver.detect(&context), "a requirements file naming Django marks the project");
}

#[test]
fn detect_finds_django_by_manage_py() {
    let context = FakeContext::new().with_file("manage.py", "#!/usr/bin/env python\n");

    assert!(DjangoResolver.detect(&context), "a manage.py marks a Django project even with no dependency marker");
}

#[test]
fn detect_is_false_without_any_marker() {
    let context = FakeContext::new().with_file("README.md", "a plain project\n");

    assert!(!DjangoResolver.detect(&context), "no dependency mention and no manage.py means not Django");
}

#[test]
fn resolve_prefers_the_view_directory_among_ambiguous_candidates() {
    let context = FakeContext::new()
        .with_node("ArticleView", "blog/api/article.py", NodeKind::Class)
        .with_node("ArticleView", "blog/core/article.py", NodeKind::Class);

    let resolved = DjangoResolver
        .resolve(&reference("ArticleView"), &context)
        .expect("the api-directory candidate disambiguates the view");

    assert_eq!(resolved.confidence, 0.8, "a framework resolution carries the convention confidence");
    assert_eq!(resolved.resolved_by, ResolvedBy::Framework, "the strategy is recorded as framework");

    assert_eq!(
        resolved.target_node_id,
        node("ArticleView", "blog/api/article.py", NodeKind::Class).id,
        "the candidate under /api/ wins",
    );
}

#[test]
fn resolve_binds_a_sole_candidate_regardless_of_directory() {
    let context = FakeContext::new().with_node("ContactForm", "blog/anywhere/contact.py", NodeKind::Class);

    let resolved = DjangoResolver.resolve(&reference("ContactForm"), &context);

    assert!(resolved.is_some(), "a single Form candidate resolves even outside a forms/ directory");
}

#[test]
fn resolve_treats_a_pascal_word_as_a_model() {
    let context = FakeContext::new().with_node("Article", "blog/models/article.py", NodeKind::Model);

    let resolved = DjangoResolver.resolve(&reference("Article"), &context);

    assert!(resolved.is_some(), "a bare PascalCase name resolves as a model by convention");
}

#[test]
fn resolve_is_none_when_ambiguous_with_no_directory_preference() {
    let context = FakeContext::new()
        .with_node("Article", "blog/alpha/article.py", NodeKind::Model)
        .with_node("Article", "blog/beta/article.py", NodeKind::Model);

    assert!(
        DjangoResolver.resolve(&reference("Article"), &context).is_none(),
        "two models in non-conventional directories stay ambiguous",
    );
}

#[test]
fn resolve_ignores_a_name_that_matches_no_convention() {
    let context = FakeContext::new().with_node("helper", "blog/utils.py", NodeKind::Function);

    assert!(
        DjangoResolver.resolve(&reference("helper"), &context).is_none(),
        "a lowercase non-suffixed name is not a model, view, or form reference",
    );
}

#[test]
fn resolved_by_labels_are_distinct_and_kebab_case() {
    let strategies = [
        ResolvedBy::ExactMatch,
        ResolvedBy::QualifiedName,
        ResolvedBy::Import,
        ResolvedBy::InstanceMethod,
        ResolvedBy::Framework,
        ResolvedBy::FilePath,
        ResolvedBy::Fuzzy,
    ];

    let labels: HashSet<&str> = strategies.iter().map(|strategy| strategy.as_str()).collect();

    assert_eq!(labels.len(), strategies.len(), "every strategy has a distinct label");
    assert_eq!(ResolvedBy::QualifiedName.as_str(), "qualified-name", "multiword labels are kebab-case");
    assert_eq!(ResolvedBy::InstanceMethod.as_str(), "instance-method", "multiword labels are kebab-case");
}

#[test]
fn unresolved_ref_starts_with_no_candidates() {
    let reference = reference("ArticleView");

    assert!(reference.candidates.is_empty(), "a fresh reference carries no dispatch candidates");
    assert_eq!(reference.reference_kind, EdgeKind::RoutesTo, "the reference kind is preserved");
}

#[test]
#[should_panic(expected = "reference_name must not be empty")]
fn unresolved_ref_rejects_an_empty_name() {
    let from = NodeId::from_raw("blog::urls.py::urlpatterns");

    let _ = UnresolvedRef::new(from, "", EdgeKind::Calls, 1, 0, "blog/urls.py", Language::Python);
}

#[test]
#[should_panic(expected = "1-based")]
fn unresolved_ref_rejects_a_zero_line() {
    let from = NodeId::from_raw("blog::urls.py::urlpatterns");

    let _ = UnresolvedRef::new(from, "Article", EdgeKind::Calls, 0, 0, "blog/urls.py", Language::Python);
}

#[test]
fn resolved_ref_copies_the_origin_fields_from_the_reference() {
    let reference = reference("Article");
    let target = NodeId::from_raw("blog::models/article.py::Article");

    let resolved = ResolvedRef::new(&reference, target.clone(), 0.8, ResolvedBy::Framework);

    assert_eq!(resolved.from_node_id, reference.from_node_id, "the origin node carries over");
    assert_eq!(resolved.reference_kind, EdgeKind::RoutesTo, "the edge kind carries over");
    assert_eq!(resolved.target_node_id, target, "the bound target is stored");
}

#[test]
#[should_panic(expected = "must not exceed one")]
fn resolved_ref_rejects_confidence_above_one() {
    let reference = reference("Article");
    let target = NodeId::from_raw("blog::models/article.py::Article");

    let _ = ResolvedRef::new(&reference, target, 1.5, ResolvedBy::ExactMatch);
}

#[test]
fn pascal_word_is_one_capitalized_then_lowercase_word() {
    assert!(is_pascal_word("Article"), "a leading capital then lowercase is a model name");
    assert!(is_pascal_word("Ab"), "two letters, capital then lowercase, qualify");
}

#[test]
fn non_pascal_words_are_rejected() {
    assert!(!is_pascal_word("article"), "a lowercase first letter is not pascal");
    assert!(!is_pascal_word("ARTICLE"), "an all-caps acronym is not a single pascal word");
    assert!(!is_pascal_word("AbCdef"), "an inner capital breaks the single-word convention");
    assert!(!is_pascal_word("A"), "a single letter is too short to disambiguate");
    assert!(!is_pascal_word("Ar7icle"), "a digit is not a lowercase letter");
    assert!(!is_pascal_word(""), "the empty string is not a word");
}
