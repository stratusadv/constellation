//! The constellation summary every indexing command prints.
//!
//! One shape, printed by `init`, the bare-path index, `sync`, and `link`
//! alike, so the output never depends on which command produced it. Columns are
//! sized to the rows rather than fixed, which is why the widths are computed
//! here instead of being baked into a format string.

use std::path::Path;

use anyhow::Result;
use constellation_store::Store;

use crate::{NAME, VERSION};

/// A row of the index summary: a project, its file and node totals, and a short
/// tag for where its source lives.
pub(crate) struct SummaryRow {
    pub(crate) label: String,
    pub(crate) files: u32,
    pub(crate) nodes: u32,
    pub(crate) source: &'static str,
}

/// A short source tag for a project's root, relative to the workspace: empty for the
/// workspace itself, `.venv` for an installed copy, `ref` for a version checkout, and
/// `local` for a working copy that overrides the install.
pub(crate) fn project_source(
    root: &Path,
    workspace_root: &Path,
    reference_only: bool,
) -> &'static str {
    if root == workspace_root {
        return "";
    }

    if root
        .components()
        .any(|part| part.as_os_str() == "site-packages")
    {
        return ".venv";
    }

    if reference_only {
        return "ref";
    }

    "local"
}

/// The number of decimal digits in `value`, at least one, for column alignment.
pub(crate) fn digits(value: u32) -> usize {
    value.to_string().len()
}

/// The compact constellation summary: a version header, one aligned row per
/// project (name, file and node totals, source tag), and the cross-project link
/// total. Columns are sized to the rows, so it stays readable on a narrow terminal.
fn print_constellation_summary(rows: &[SummaryRow], links: u32) {
    let name_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let files_width = rows.iter().map(|row| digits(row.files)).max().unwrap_or(1);
    let nodes_width = rows.iter().map(|row| digits(row.nodes)).max().unwrap_or(1);

    println!("{NAME} {VERSION}");

    for row in rows {
        let label = &row.label;
        let files = row.files;
        let nodes = row.nodes;
        let source = row.source;

        let line = format!(
            "  {label:<name_width$}  {files:>files_width$} files  {nodes:>nodes_width$} nodes  {source}",
        );

        println!("{}", line.trim_end());
    }

    if links > 0 {
        println!("  {links} cross-project links");
    }
}

/// The summary built from every project already in `store`, used after a sync or a
/// link. The workspace root is inferred from the database location
/// (`<workspace>/.constellation/index.db`) to tag the workspace row distinctly.
pub(crate) fn print_store_summary(store: &Store, database: &Path) -> Result<()> {
    let workspace_root = database.parent().and_then(Path::parent).unwrap_or(database);

    print_summary_all_projects(store, workspace_root)
}

/// The projects in the store summarized (files, nodes, source tag) with the
/// cross-project link total, relative to `workspace_root`. Shared by `init`/index and
/// `sync` so both always list the whole constellation, whether each project was
/// freshly indexed this run or already present (a re-index skips companions that
/// already exist, but they still belong in the summary).
pub(crate) fn print_summary_all_projects(store: &Store, workspace_root: &Path) -> Result<()> {
    let mut rows: Vec<SummaryRow> = Vec::new();

    for project in store.all_projects()? {
        rows.push(SummaryRow {
            label: project.id.as_str().to_string(),
            files: store.count_files(&project.id)?,
            nodes: store.count_nodes(&project.id)?,
            source: project_source(
                Path::new(&project.root_path),
                workspace_root,
                project.reference_only,
            ),
        });
    }

    print_constellation_summary(&rows, store.count_links()?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{digits, project_source};

    use std::path::Path;

    #[test]
    fn project_source_tags_each_origin() {
        let workspace = Path::new("/code/workspace");

        assert_eq!(
            project_source(workspace, workspace, false),
            "",
            "the workspace itself has no tag"
        );
        assert_eq!(
            project_source(
                Path::new("/code/workspace/.venv/Lib/site-packages/robit"),
                workspace,
                false
            ),
            ".venv",
            "a site-packages path is the installed copy",
        );
        assert_eq!(
            project_source(
                Path::new("/code/.constellation/sources/x/pkg"),
                workspace,
                true
            ),
            "ref",
            "a reference-only checkout is a version ref",
        );
        assert_eq!(
            project_source(
                Path::new("/code/django-spire/django_spire"),
                workspace,
                false
            ),
            "local",
            "a working copy outside the venv is a local override",
        );
    }

    #[test]
    fn digits_counts_decimal_places() {
        assert_eq!(digits(0), 1, "zero is one digit");
        assert_eq!(digits(7), 1);
        assert_eq!(digits(9477), 4);
    }
}
