use std::fmt;

/// The separator between a project prefix and a qualified name inside a
/// [`NodeId`]. A [`ProjectId`] may never contain it, so the prefix is always
/// recoverable by splitting on the first occurrence.
const ID_SEPARATOR: &str = "::";

/// A stable identifier for one project (repository) within the constellation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectId(String);

impl ProjectId {
    /// A project id built from a non-empty, separator-free name.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();

        assert!(!value.is_empty(), "project id must not be empty");

        assert!(
            !value.contains(ID_SEPARATOR),
            "project id must not contain the '{ID_SEPARATOR}' separator",
        );

        Self(value)
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        debug_assert!(!self.0.is_empty(), "project id is never empty");

        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A globally unique node identifier of the form `{project}::{qualified_name}`.
///
/// The project prefix keeps identifiers distinct across the whole
/// constellation, so a single store can hold every project's graph without
/// collision, and a single edge can cross a project boundary unambiguously.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(String);

impl NodeId {
    /// A node id built by prefixing a qualified name with its project.
    pub fn new(project: &ProjectId, qualified_name: &str) -> Self {
        assert!(!qualified_name.is_empty(), "qualified_name must not be empty");

        assert!(
            !qualified_name.starts_with(ID_SEPARATOR),
            "qualified_name must not begin with the id separator",
        );

        // One exact-size allocation instead of format!'s grow-and-realloc; this
        // runs once per node, so the saved reallocations add up across a project.
        let project = project.as_str();
        let mut id = String::with_capacity(project.len() + ID_SEPARATOR.len() + qualified_name.len());
        id.push_str(project);
        id.push_str(ID_SEPARATOR);
        id.push_str(qualified_name);

        Self(id)
    }

    /// A node id wrapped around an already-prefixed raw id string.
    pub fn from_raw(value: impl Into<String>) -> Self {
        let value = value.into();

        assert!(!value.is_empty(), "node id must not be empty");

        assert!(
            value.contains(ID_SEPARATOR),
            "raw node id must carry a project prefix",
        );

        Self(value)
    }

    /// The project prefix this node belongs to, everything before the first
    /// separator. Two nodes with differing prefixes live in different projects.
    pub fn project_prefix(&self) -> &str {
        let prefix = self
            .0
            .split_once(ID_SEPARATOR)
            .map_or(self.0.as_str(), |(prefix, _)| prefix);

        assert!(!prefix.is_empty(), "node id must carry a project prefix");

        prefix
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        debug_assert!(!self.0.is_empty(), "node id is never empty");

        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
