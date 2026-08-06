//! The advertised tool names, guarded.
//!
//! `#[tool]` defaults a tool's name to its method name, and the methods in
//! `server::router` stay `constellation_`-prefixed because the rest of the
//! crate refers to them that way. Every client namespaces an MCP tool by its
//! server, so a defaulted name reaches the agent stuttered
//! (`constellation_constellation_overview`), which agents then "correct" to a
//! name that does not exist. A new tool that forgets its explicit
//! `#[tool(name = "...")]` reintroduces that silently; these tests fail instead.

use std::path::PathBuf;

use constellation_mcp::ConstellationServer;


#[test]
fn no_tool_name_carries_the_server_prefix() {
    for tool in ConstellationServer::tool_router().list_all() {
        assert!(
            !tool.name.contains("constellation"),
            "tool {:?} is advertised with the server name inside it; give it an explicit \
             #[tool(name = \"...\")] that drops the `constellation_` prefix its method keeps",
            tool.name,
        );
    }
}

#[test]
fn tool_names_are_unique() {
    let mut names: Vec<String> = ConstellationServer::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();

    let advertised = names.len();

    names.sort();
    names.dedup();

    assert_eq!(advertised, names.len(), "two tools are advertised under the same name");
}

#[test]
fn every_tool_is_described() {
    for tool in ConstellationServer::tool_router().list_all() {
        let description = tool.description.as_deref().unwrap_or("");

        assert!(
            !description.is_empty(),
            "tool {:?} is advertised with no description; an agent picks tools by description",
            tool.name,
        );
    }
}

/// The directory [`crate::snapshot`] writes into.
fn snapshot_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/it/snapshots")
}

/// The check that every tool an agent can call has its rendered output pinned.
///
/// A tool without a snapshot is a rendering nobody is watching: it can be
/// reordered, truncated, or emptied by a change to `rank` or `render` and no
/// test says so, because the tests that exist assert the lines someone thought
/// to assert. This is the check that a new tool arrives with its output pinned
/// rather than three months later.
///
/// Matched by prefix, because one tool can need more than one snapshot: `path`
/// renders a chain, a cross-project chain, and a "no path" message, and each is
/// a different rendering worth pinning separately.
#[test]
fn every_tool_has_a_snapshot() {
    let directory = snapshot_directory();

    let pinned: Vec<String> = std::fs::read_dir(&directory)
        .expect("the snapshot directory exists")
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .filter_map(|name| Some(name.strip_prefix("it__snapshot__")?.to_string()))
        .collect();

    assert!(!pinned.is_empty(), "no snapshots found in {}", directory.display());

    let mut unpinned: Vec<String> = Vec::new();

    for tool in ConstellationServer::tool_router().list_all() {
        if !pinned.iter().any(|name| name.starts_with(tool.name.as_ref())) {
            unpinned.push(tool.name.to_string());
        }
    }

    unpinned.sort();

    assert!(
        unpinned.is_empty(),
        "these tools render text no snapshot pins: {unpinned:?}; add one to \
         tests/it/snapshot.rs so a change to what they hand an agent has to be read",
    );
}
