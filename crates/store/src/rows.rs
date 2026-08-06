//! The values the store hands back.
//!
//! These are the crate's public vocabulary, so they are plain data with public
//! fields and no behavior beyond the odd label conversion. Anything that reads
//! a `rusqlite::Row` belongs in [`crate::mapping`] instead.

use constellation_graph::{EdgeKind, Language, Node, ProjectId};


/// A project row: its id, display name, filesystem root, the epoch-ms timestamp
/// of its last index, and whether it is reference-only.
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub root_path: String,
    pub indexed_at: i64,
    /// Whether this project is withheld from cross-project link targets: a
    /// reference-only version copy, queryable but never linked into.
    pub reference_only: bool,
}

/// A commit from a project's git history: its hash, author name, committer
/// timestamp (epoch seconds), subject line, and the files it touched.
pub struct CommitRecord {
    pub commit_hash: String,
    pub author: String,
    pub committed_at: i64,
    pub summary: String,
    pub files: Vec<CommitFile>,
}

/// A file a commit touched, with its line churn (both zero for a binary file
/// or a pure rename, which git reports as `-`).
pub struct CommitFile {
    pub file_path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// A row of a history timeline: a commit that touched the queried path, with
/// its churn aggregated over only the files matching that path.
pub struct HistoryHit {
    pub project_id: String,
    pub commit_hash: String,
    pub author: String,
    pub committed_at: i64,
    pub summary: String,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// The kind of change a symbol underwent between two consecutive revisions of its
/// file, as recorded in `git_symbol_revision`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolChange {
    Added,
    Modified,
    Removed,
}

impl SymbolChange {
    /// The lowercase label stored for this change.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolChange::Added => "added",
            SymbolChange::Modified => "modified",
            SymbolChange::Removed => "removed",
        }
    }
}

/// A symbol-level change to record: a trackable symbol added, modified, or
/// removed in a commit, identified within its file by qualified name.
pub struct SymbolRevision {
    pub commit_hash: String,
    pub file_path: String,
    pub qualified_name: String,
    pub name: String,
    pub kind: String,
    pub change: SymbolChange,
    pub signature: Option<String>,
}

/// A commit that touched a file, used to drive symbol diffing in commit order.
pub struct FileTouch {
    pub file_path: String,
    pub commit_hash: String,
}

/// A row of a symbol's change history: when it changed (commit, time, subject),
/// how (`added`/`modified`/`removed`), and its kind and signature at that point.
pub struct SymbolHistoryHit {
    pub project_id: String,
    pub commit_hash: String,
    pub committed_at: i64,
    pub qualified_name: String,
    pub kind: String,
    pub change: String,
    pub signature: Option<String>,
    pub summary: String,
}

/// A symbol alive at a reconstructed point in time: its file, qualified name,
/// kind, and the signature in effect then.
pub struct AsOfSymbol {
    pub project_id: String,
    pub file_path: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: Option<String>,
}

/// A file row for the `files` listing: its path, language, symbol (node) count,
/// and size in bytes.
pub struct FileRow {
    pub path: String,
    pub language: String,
    pub node_count: i64,
    pub size_bytes: i64,
}

/// A cross-project link edge with both endpoints hydrated: an import in
/// `source`'s repo resolved to the `target` definition in another repo, tagged
/// with the linker's `link:<from>-><to>` provenance.
pub struct LinkEdge {
    pub source: Node,
    pub target: Node,
    pub kind: EdgeKind,
    pub provenance: String,
}

/// A member of a flow's reach set: a node and the breadth-first depth it was
/// first reached at.
pub struct FlowMember {
    pub depth: u32,
    pub node_id: String,
}

/// A computed flow to persist: its entry point, the aggregate shape of its
/// reach set, its criticality, and the members themselves.
pub struct FlowRecord {
    pub app_count: u32,
    pub criticality: f64,
    pub depth_max: u32,
    pub entry_kind: String,
    pub entry_node_id: String,
    pub file_count: u32,
    pub members: Vec<FlowMember>,
    pub name: String,
    pub project_count: u32,
    /// Whether the reach set hit its node cap and was cut short, so a reader
    /// knows the counts are a floor rather than the whole picture.
    pub truncated: bool,
}

/// A stored flow read back, without its member list (fetch that with
/// [`Store::flow_members`] when it is actually needed).
pub struct FlowRow {
    pub app_count: u32,
    pub criticality: f64,
    pub depth_max: u32,
    pub entry_kind: String,
    pub entry_node_id: String,
    pub file_count: u32,
    pub id: i64,
    pub name: String,
    pub node_count: u32,
    pub project_count: u32,
    pub project_id: String,
    pub truncated: bool,
}

/// The order a flow listing comes back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowSort {
    Criticality,
    Name,
    Size,
}

impl FlowSort {
    /// The sort parsed from its lowercase label, or `None` if unknown.
    pub fn from_str_label(label: &str) -> Option<FlowSort> {
        let sort = match label {
            "criticality" => FlowSort::Criticality,
            "name" => FlowSort::Name,
            "size" => FlowSort::Size,
            _ => return None,
        };

        Some(sort)
    }

    /// The `ORDER BY` fragment for this sort. A fixed set of static strings, so
    /// interpolating it into SQL carries no injection risk.
    pub(crate) fn order_by(self) -> &'static str {
        match self {
            FlowSort::Criticality => "criticality DESC, name",
            FlowSort::Name => "name, criticality DESC",
            FlowSort::Size => "node_count DESC, name",
        }
    }
}

/// A route handler that named a view and never bound to one: the reference as
/// the URL file wrote it, and where to go read it.
#[derive(Clone, Debug)]
pub struct UnresolvedRoute {
    /// The handler qualified by its receiver module where the URL file used
    /// one, so `json_views.bulk_update_view` reads back as it was written.
    pub reference: String,
    pub file_path: String,
    pub line: u32,
}

/// An incoming reference to a candidate node, flattened for bulk scoring: the
/// edge kind plus the source's identity, project, and file.
pub struct IncomingRef {
    pub kind: EdgeKind,
    pub source_file_path: String,
    pub source_id: String,
    pub source_name: String,
    pub source_project_id: String,
    pub target_id: String,
}

/// An outgoing reference from a candidate node, the mirror of [`IncomingRef`]:
/// the edge kind plus the target's identity and name.
pub struct OutgoingRef {
    pub kind: EdgeKind,
    pub source_id: String,
    pub target_id: String,
    pub target_name: String,
}

/// The metadata for one indexed file, written alongside its extracted graph.
pub struct FileIndex<'a> {
    pub path: &'a str,
    pub content_hash: &'a str,
    pub language: Language,
    pub size_bytes: u64,
    pub modified_at_ms: i64,
    /// The file's full source, full-text indexed so `explore` can rank from
    /// body content. Empty when content indexing is not wanted (e.g. tests).
    pub source: &'a str,
}
