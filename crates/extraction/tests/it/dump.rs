//! The canonical text an [`ExtractionOutput`] snapshots as.
//!
//! Extraction is pure and per-file, so a whole run is comparable as one value:
//! the same source yields the same nodes, edges, references, imports, and
//! events every time. Rendering that value as text turns "did this refactor
//! change what we extract" into a line diff, which is the question the snapshot
//! suite exists to answer and the reason a parser refactor can be reviewed at
//! all.
//!
//! Three rules keep a snapshot worth reading. Every section is sorted by a key
//! that does not depend on emission order, so a change that reorders the tree
//! walk without changing what it finds produces no diff and a change that finds
//! something new produces a small one. An unset optional field prints nothing,
//! so a snapshot's length tracks what was extracted rather than what could have
//! been. And the two prefixes every id in a single-file dump shares (the
//! project, then the file) are stated once in the header and stripped from the
//! lines, because an id column that is nine tenths identical hides the tenth
//! that is not.

use constellation_extraction::ExtractionOutput;
use constellation_graph::{Edge, Language, Node, NodeId, ProjectId};
use constellation_resolution::{EventRecord, EventRole, ImportMapping, UnresolvedRef};

/// The width a node kind pads to, one past the longest [`NodeKind`] label
/// (`parameter`) so a kind is always followed by a space.
///
/// [`NodeKind`]: constellation_graph::NodeKind
const NODE_KIND_COLUMNS: usize = 11;

/// The width an edge or reference kind pads to, one past the longest
/// [`EdgeKind`] label (`derived_collection`).
///
/// [`EdgeKind`]: constellation_graph::EdgeKind
const EDGE_KIND_COLUMNS: usize = 20;

/// The width a node or binding name pads to.
const NAME_COLUMNS: usize = 24;

/// The width a stripped node id pads to.
const ID_COLUMNS: usize = 32;

/// The width a continuation line's label pads to.
const LABEL_COLUMNS: usize = 12;

/// A padded column, always followed by at least one space.
///
/// Padding alone is not enough: a value wider than its column would otherwise
/// run straight into the next one, and `derived_collectionorders/detail.html`
/// reads as a single token rather than as two fields.
fn column(value: &str, width: usize) -> String {
    assert!(width >= 2, "a column holds a value and its separator");

    format!("{value:<inner$} ", inner = width - 1)
}

/// The prefixes one dump strips and the language its file implies, worked out
/// once rather than reformatted for every id on every line.
struct Scope {
    project: String,
    file: String,
    language: Option<Language>,
}

impl Scope {
    /// The scope for one file extracted under one project.
    fn new(project: &ProjectId, file_path: &str) -> Self {
        assert!(!file_path.is_empty(), "a dump names the file it extracted");

        let extension = file_path.rsplit_once('.').map(|(_, extension)| extension);

        Self {
            project: format!("{}::", project.as_str()),
            file: format!("{file_path}::"),
            language: extension.and_then(Language::from_extension),
        }
    }

    /// A node id with the project and file prefixes removed.
    ///
    /// A prefix that does not match is left in place, so an id belonging to
    /// another project or another file stays visibly foreign rather than being
    /// quietly shortened into something that reads as local.
    fn short(&self, raw: &str) -> String {
        let without_project = raw.strip_prefix(&self.project).unwrap_or(raw);

        without_project.strip_prefix(&self.file).unwrap_or(without_project).to_string()
    }

    /// A node id with the project and file prefixes removed.
    fn short_id(&self, id: &NodeId) -> String {
        self.short(id.as_str())
    }
}

/// The whole output of one extraction as sorted, line-oriented text.
///
/// `project` and `file_path` must be the pair `output` was extracted under:
/// they are the prefixes the id columns are shortened against, and naming a
/// different file would print ids that no longer match the graph.
pub fn dump(project: &ProjectId, file_path: &str, output: &ExtractionOutput) -> String {
    let scope = Scope::new(project, file_path);

    let mut text = format!("file    {file_path}\nproject {}\n\n", project.as_str());

    text.push_str(&nodes_section(&scope, &output.nodes));
    text.push_str(&edges_section(&scope, &output.edges));
    text.push_str(&refs_section(&scope, &output.unresolved_refs));
    text.push_str(&imports_section(&output.import_mappings));
    text.push_str(&events_section(&scope, &output.events));

    assert!(text.contains("nodes ("), "a dump always carries its section headers");
    assert!(text.ends_with('\n'), "a dump ends on a line boundary");

    text
}

/// A `4:0-11:23` rendering of a node's 1-based span.
fn span_of(node: &Node) -> String {
    format!(
        "{}:{}-{}:{}",
        node.span.start_line, node.span.start_column, node.span.end_line, node.span.end_column,
    )
}

/// The set flags on a node as a space-separated list, empty when none are set.
fn flags_of(node: &Node) -> String {
    let mut flags: Vec<&str> = Vec::new();

    if node.is_abstract {
        flags.push("abstract");
    }

    if node.is_async {
        flags.push("async");
    }

    if node.is_exported {
        flags.push("exported");
    }

    if node.is_static {
        flags.push("static");
    }

    flags.join(" ")
}

/// A continuation line under an entry, or an empty string when `value` is
/// blank, so an unset attribute costs no line.
fn attribute(label: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    format!("    {}{}\n", column(label, LABEL_COLUMNS), value.replace('\n', "\\n"))
}

/// The nodes, sorted by where each starts so the section reads in source order
/// without depending on the order the walk emitted it in.
fn nodes_section(scope: &Scope, nodes: &[Node]) -> String {
    let mut sorted: Vec<&Node> = nodes.iter().collect();

    sorted.sort_by(|left, right| {
        (left.span.start_line, left.span.start_column, left.kind.as_str(), &left.qualified_name)
            .cmp(&(
                right.span.start_line,
                right.span.start_column,
                right.kind.as_str(),
                &right.qualified_name,
            ))
    });

    let mut text = format!("nodes ({})\n", sorted.len());

    for node in sorted {
        text.push_str(&format!(
            "  {}{}{}{}\n",
            column(node.kind.as_str(), NODE_KIND_COLUMNS),
            column(&node.name, NAME_COLUMNS),
            column(&scope.short_id(&node.id), ID_COLUMNS),
            span_of(node),
        ));

        // Only when it contradicts the extension, which is the case worth
        // seeing: a stylesheet rule or a script symbol lifted out of a template.
        if scope.language != Some(node.language) {
            text.push_str(&attribute("language", node.language.as_str()));
        }

        text.push_str(&attribute("signature", node.signature.as_deref().unwrap_or("")));
        text.push_str(&attribute("docstring", node.docstring.as_deref().unwrap_or("")));
        text.push_str(&attribute("decorators", &node.decorators.join(", ")));
        text.push_str(&attribute("flags", &flags_of(node)));

        let visibility = node.visibility.map(|value| value.as_str()).unwrap_or("");

        text.push_str(&attribute("visibility", visibility));
    }

    text
}

/// The edges, grouped under their source so the section reads as an adjacency
/// list rather than as the order the extractor happened to append them in.
fn edges_section(scope: &Scope, edges: &[Edge]) -> String {
    let mut sorted: Vec<&Edge> = edges.iter().collect();

    sorted.sort_by(|left, right| {
        (&left.source, left.kind.as_str(), &left.target, left.line, left.column).cmp(&(
            &right.source,
            right.kind.as_str(),
            &right.target,
            right.line,
            right.column,
        ))
    });

    let mut text = format!("\nedges ({})\n", sorted.len());

    for edge in sorted {
        let at = match (edge.line, edge.column) {
            (Some(line), Some(column)) => format!("  @{line}:{column}"),
            _ => String::new(),
        };

        let provenance = match &edge.provenance {
            Some(label) => format!("  [{label}]"),
            None => String::new(),
        };

        text.push_str(&format!(
            "  {}{}-> {}{at}{provenance}\n",
            column(edge.kind.as_str(), EDGE_KIND_COLUMNS),
            column(&scope.short_id(&edge.source), ID_COLUMNS),
            scope.short_id(&edge.target),
        ));
    }

    text
}

/// The references awaiting resolution, sorted by where each was observed.
///
/// This is the section a resolution change moves and an extraction change must
/// not: a reference is what extraction hands to the next stage, so its name,
/// kind, and candidate list are the parser's real contract with the pipeline.
fn refs_section(scope: &Scope, references: &[UnresolvedRef]) -> String {
    let mut sorted: Vec<&UnresolvedRef> = references.iter().collect();

    sorted.sort_by(|left, right| {
        (left.line, left.column, left.reference_kind.as_str(), &left.reference_name).cmp(&(
            right.line,
            right.column,
            right.reference_kind.as_str(),
            &right.reference_name,
        ))
    });

    let mut text = format!("\nrefs ({})\n", sorted.len());

    for reference in sorted {
        text.push_str(&format!(
            "  {}{}-> {:?}  @{}:{}\n",
            column(reference.reference_kind.as_str(), EDGE_KIND_COLUMNS),
            column(&scope.short_id(&reference.from_node_id), ID_COLUMNS),
            reference.reference_name,
            reference.line,
            reference.column,
        ));

        text.push_str(&attribute("candidates", &reference.candidates.join(", ")));
    }

    text
}

/// The import bindings, sorted by the module imported from and then by the
/// local name it was bound to.
fn imports_section(mappings: &[ImportMapping]) -> String {
    let mut sorted: Vec<&ImportMapping> = mappings.iter().collect();

    sorted.sort_by(|left, right| {
        (&left.source, &left.local_name, &left.exported_name).cmp(&(
            &right.source,
            &right.local_name,
            &right.exported_name,
        ))
    });

    let mut text = format!("\nimports ({})\n", sorted.len());

    for mapping in sorted {
        let mut shape: Vec<&str> = Vec::new();

        if mapping.is_default {
            shape.push("default");
        }

        if mapping.is_namespace {
            shape.push("namespace");
        }

        let resolved = match &mapping.resolved_path {
            Some(path) => format!("  -> {path}"),
            None => String::new(),
        };

        text.push_str(&format!(
            "  {}{}from {}{resolved}\n",
            column(&mapping.local_name, NAME_COLUMNS),
            column(&mapping.exported_name, NAME_COLUMNS),
            mapping.source,
        ));

        text.push_str(&attribute("shape", &shape.join(" ")));
    }

    text
}

/// The lowercase label for an event role. `EventRole` carries no `as_str` of
/// its own, and a comparator must not allocate a `Debug` string per comparison.
fn role_label(role: EventRole) -> &'static str {
    match role {
        EventRole::Dispatch => "dispatch",
        EventRole::Listen => "listen",
    }
}

/// The event observations, sorted by channel name so a dispatch and the
/// listeners that answer it land next to each other.
fn events_section(scope: &Scope, events: &[EventRecord]) -> String {
    let mut sorted: Vec<&EventRecord> = events.iter().collect();

    sorted.sort_by(|left, right| {
        (&left.event, role_label(left.role), &left.symbol, left.line, left.column).cmp(&(
            &right.event,
            role_label(right.role),
            &right.symbol,
            right.line,
            right.column,
        ))
    });

    let mut text = format!("\nevents ({})\n", sorted.len());

    for event in sorted {
        text.push_str(&format!(
            "  {}{}{}  @{}:{}\n",
            column(role_label(event.role), NODE_KIND_COLUMNS),
            column(&event.event, NAME_COLUMNS),
            scope.short(&event.symbol),
            event.line,
            event.column,
        ));
    }

    text
}
