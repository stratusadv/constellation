//! The edges *into* a file, across a re-index of that file.
//!
//! Re-indexing a file clears its rows, and clearing its nodes cascades to every
//! edge that touches them. Half of those edges belong to other files: the urls
//! module that routes to a view, the caller of a helper. Those files did not
//! change, so the run has no reason to re-extract them, and the references that
//! produced their edges were consumed the first time they resolved. Nothing
//! would rebuild the edge.
//!
//! The symptom that named this suite: every route in one urls module resolving
//! to `(unresolved)` while a byte-identical module in a sibling app resolved
//! fine, the difference being that the first app's views module had been edited
//! since. So these tests always edit the *target* and assert against the
//! *source*, which is the direction the bug hid in.

use constellation_graph::{EdgeKind, NodeKind, ProjectId};
use constellation_index::index_project;
use constellation_store::Store;

/// The apps the route fixture builds, each importing the module alias
/// `json_views`. Three, because one shared alias is what made the original
/// failure look like an alias collision: resolution must scope
/// `json_views.<attr>` by the importing file's own import table, and keep doing
/// so after any one app's views module is rewritten.
const APPS: [&str; 3] = ["inventory", "production", "sales"];

/// A views module for `app`, with one uniquely-named view and one whose name
/// every app repeats, so a bind to the wrong app is visible as a wrong file
/// rather than hidden behind a unique name.
fn views_source(app: &str, extra: &str) -> String {
    format!(
        "from django.shortcuts import render\n\n\n\
         def {app}_detail_view(request):\n\
         \x20   return render(request, '{app}/detail.html')\n\n\n\
         def bulk_update_view(request):\n\
         \x20   return render(request, '{app}/bulk.html')\n{extra}",
    )
}

/// A urls module for `app` in the exact shape the bug report pinned: a module
/// alias imported from the app's views package, then `alias.attr` handlers.
fn urls_source(app: &str) -> String {
    format!(
        "from django.urls import path\n\n\
         from app.{app}.views import json_views\n\n\n\
         app_name = 'json'\n\n\
         urlpatterns = [\n\
         \x20   path('detail/', json_views.{app}_detail_view, name='detail'),\n\
         \x20   path('bulk-update/', json_views.bulk_update_view, name='bulk_update'),\n\
         ]\n",
    )
}

/// Every app's routes and the view file each one bound to, in [`APPS`] order.
fn resolved_per_app(store: &Store, project: &ProjectId) -> Vec<Vec<String>> {
    APPS.iter().map(|app| resolved_views(store, project, app)).collect()
}

/// The file path each route in `app`'s urls module resolved its view to,
/// ordered by route line so two runs compare directly. Empty entries are
/// impossible by construction: a route with no `RoutesTo` edge contributes the
/// string that names it unresolved, so a lost edge fails as a diff rather than
/// as a shorter list.
fn resolved_views(store: &Store, project: &ProjectId, app: &str) -> Vec<String> {
    let urls_path = format!("app/{app}/urls/json_urls.py");

    let mut routes: Vec<_> = store
        .all_nodes(Some(project))
        .expect("reading nodes")
        .into_iter()
        .filter(|node| node.kind == NodeKind::Route && node.file_path == urls_path)
        .collect();

    routes.sort_by_key(|route| route.span.start_line);

    routes
        .iter()
        .map(|route| {
            let view = store
                .callees(&route.id)
                .expect("reading callees")
                .into_iter()
                .find(|(kind, _)| *kind == EdgeKind::RoutesTo);

            match view {
                Some((_, node)) => format!("{} -> {}", route.name, node.file_path),
                None => format!("{} -> (unresolved)", route.name),
            }
        })
        .collect()
}

/// The three-app fixture written into a temporary directory.
fn write_project(root: &std::path::Path) {
    for app in APPS {
        let views = root.join(format!("app/{app}/views"));
        let urls = root.join(format!("app/{app}/urls"));

        std::fs::create_dir_all(&views).expect("the views directory");
        std::fs::create_dir_all(&urls).expect("the urls directory");

        let views_module = views.join("json_views.py");

        std::fs::write(views_module, views_source(app, "")).expect("the views module");
        std::fs::write(urls.join("json_urls.py"), urls_source(app)).expect("the urls module");
    }
}

#[test]
fn editing_a_views_module_keeps_every_route_bound_to_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();

    write_project(root);

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("portal");

    index_project(&store, &project, "portal", root).unwrap();

    let before = resolved_per_app(&store, &project);

    for (app, resolved) in APPS.iter().zip(&before) {
        assert_eq!(
            resolved,
            &[
                format!("detail -> app/{app}/views/json_views.py"),
                format!("bulk_update -> app/{app}/views/json_views.py"),
            ],
            "every route binds to its own app's views module on a cold index",
        );
    }

    // One app's views module rewritten, and nothing else. Its content hash
    // moves, so the next run re-extracts exactly this file and leaves the two
    // sibling apps and all three urls modules untouched. That is the whole
    // setup: the edges at risk are the ones written by a file the run will not
    // look at.
    std::fs::write(
        root.join("app/inventory/views/json_views.py"),
        views_source("inventory", "\n\ndef added_view(request):\n    return None\n"),
    )
    .unwrap();

    index_project(&store, &project, "portal", root).unwrap();

    let after = resolved_per_app(&store, &project);

    assert_eq!(
        after, before,
        "re-indexing a views module must not drop the routes that point into it",
    );
}

#[test]
fn editing_a_module_keeps_the_calls_into_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();

    std::fs::write(root.join("helpers.py"), "def compute_total(order):\n    return 1\n").unwrap();

    std::fs::write(
        root.join("service.py"),
        "from helpers import compute_total\n\n\n\
         def summarize(order):\n\
         \x20   return compute_total(order)\n",
    )
    .unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = ProjectId::new("portal");

    index_project(&store, &project, "portal", root).unwrap();

    let caller = store
        .all_nodes(Some(&project))
        .unwrap()
        .into_iter()
        .find(|node| node.name == "summarize")
        .expect("the calling function");

    let calls_before = store
        .callees(&caller.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, node)| *kind == EdgeKind::Calls && node.name == "compute_total")
        .count();

    assert_eq!(calls_before, 1, "the cold index binds the call");

    std::fs::write(root.join("helpers.py"), "def compute_total(order):\n    return 2\n").unwrap();

    index_project(&store, &project, "portal", root).unwrap();

    let calls_after = store
        .callees(&caller.id)
        .unwrap()
        .into_iter()
        .filter(|(kind, node)| *kind == EdgeKind::Calls && node.name == "compute_total")
        .count();

    assert_eq!(calls_after, 1, "editing the callee must not drop its caller's edge");
}
