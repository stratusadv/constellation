use constellation_graph::{Language, NodeId, NodeKind};

use crate::context::ResolutionContext;
use crate::framework::FrameworkResolver;
use crate::refs::{ResolvedBy, ResolvedRef, UnresolvedRef};

/// The only language Django applies to here.
const PYTHON: &[Language] = &[Language::Python];

/// The directory fragments where each Django symbol conventionally lives, used to
/// disambiguate same-named symbols across apps.
const MODEL_DIRECTORIES: &[&str] = &["models"];
const VIEW_DIRECTORIES: &[&str] = &["views", "/api/"];
const FORM_DIRECTORIES: &[&str] = &["forms"];

/// The Django resolver that binds name-convention references to the conventional app directories.
///
/// Detects a Django project and resolves model, view, and form references by
/// directory convention. Route extraction is handled structurally by the Python
/// extractor, so this resolver carries no extraction of its own.
pub struct DjangoResolver;

impl FrameworkResolver for DjangoResolver {
    fn name(&self) -> &str {
        "django"
    }

    fn languages(&self) -> &[Language] {
        assert!(!PYTHON.is_empty(), "django supports at least one language");

        PYTHON
    }

    fn detect(&self, context: &dyn ResolutionContext) -> bool {
        for marker in ["requirements.txt", "pyproject.toml", "setup.py", "Pipfile"] {
            if let Some(contents) = context.read_file(marker)
                && contents.to_lowercase().contains("django")
            {
                return true;
            }
        }

        context.file_exists("manage.py")
    }

    fn resolve(
        &self,
        reference: &UnresolvedRef,
        context: &dyn ResolutionContext,
    ) -> Option<ResolvedRef> {
        assert!(!reference.reference_name.is_empty(), "reference_name must not be empty");
        assert!(reference.language == Language::Python, "django resolves python references");

        let name = reference.reference_name.as_str();

        let target = if name.ends_with("View") || name.ends_with("ViewSet") {
            by_name_in_directories(context, name, &[NodeKind::Class, NodeKind::Function, NodeKind::View], VIEW_DIRECTORIES)
        } else if name.ends_with("Form") {
            by_name_in_directories(context, name, &[NodeKind::Class], FORM_DIRECTORIES)
        } else if name.ends_with("Model") || is_pascal_word(name) {
            by_name_in_directories(context, name, &[NodeKind::Class, NodeKind::Model], MODEL_DIRECTORIES)
        } else {
            None
        };

        target.map(|id| ResolvedRef::new(reference, id, 0.8, ResolvedBy::Framework))
    }
}

/// The node of an accepted kind a name resolves to, preferring the conventional
/// directories. A sole candidate wins outright; among several, only a
/// directory-preferred one resolves, otherwise the match stays ambiguous.
fn by_name_in_directories(
    context: &dyn ResolutionContext,
    name: &str,
    kinds: &[NodeKind],
    directories: &[&str],
) -> Option<NodeId> {
    assert!(!name.is_empty(), "name must not be empty");
    assert!(!kinds.is_empty(), "at least one acceptable node kind");

    let mut candidates = context.nodes_by_name(name);
    candidates.retain(|node| kinds.contains(&node.kind));

    if candidates.is_empty() {
        return None;
    }

    if candidates.len() == 1 {
        return Some(candidates.swap_remove(0).id.clone());
    }

    candidates
        .into_iter()
        .find(|node| directories.iter().any(|directory| node.file_path.contains(directory)))
        .map(|node| node.id.clone())
}

/// Whether `text` is a single capitalized word (`Article`), Django's
/// convention for a model class name.
pub fn is_pascal_word(text: &str) -> bool {
    match text.chars().next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => return false,
    }

    text.len() > 1 && text[1..].chars().all(|character| character.is_ascii_lowercase())
}
