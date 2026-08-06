//! `constellation_overview`, `constellation_files`,
//! `constellation_routes`, and `constellation_links`: the shape of a
//! project, and of the constellation around it.

use std::fmt::Write;

use constellation_graph::{
    EdgeKind, NodeKind,
};
use constellation_index::route_pattern;
use constellation_store::{FileRow, LinkEdge, ProjectRow, Store, StoreError};
use rustc_hash::FxHashMap;

use crate::limits::{
    LINKS_FETCH_MAX, OVERVIEW_PACKAGES_MAX, ROUTES_PER_PROJECT_MAX, ROUTES_UNBOUND_NAMED_MAX,
};
use crate::rank::path_penalty;
use crate::cursor;

/// The symbols the indexed files hold, the denominator every package breakdown
/// below a header is built from.
///
/// Deliberately not a project-wide node count: that also counts nodes no indexed
/// file owns (external stubs stand at a `<external>/...` path), so a header taken
/// from it never matched the rows beneath it, and the breakdown read as though it
/// were concealing symbols. Header and rows now come from one source.
fn file_symbol_total(files: &[FileRow]) -> i64 {
    files.iter().map(|file| file.node_count).sum()
}

/// The files aggregated by their first `depth` path directory segments (root files
/// under `(root)`), returning `(directory, file count, symbol count)` sorted by
/// symbol count descending then name. `depth` 1 is the top-level package; a
/// deeper value breaks a project down by sub-directory.
fn aggregate_by_depth(files: &[FileRow], depth: usize) -> Vec<(String, usize, i64)> {
    assert!(depth >= 1, "aggregation depth is at least one");

    let mut totals: FxHashMap<String, (usize, i64)> = FxHashMap::default();

    for file in files {
        let segments: Vec<&str> = file.path.split('/').collect();

        let key = if segments.len() <= 1 {
            "(root)".to_string()
        } else {
            let take = depth.min(segments.len() - 1);
            segments[..take.max(1)].join("/")
        };

        let entry = totals.entry(key).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += file.node_count;
    }

    let mut directories: Vec<(String, usize, i64)> =
        totals.into_iter().map(|(name, (count, symbols))| (name, count, symbols)).collect();

    directories.sort_by(|left, right| right.2.cmp(&left.2).then(left.0.cmp(&right.0)));

    directories
}

/// The constellation's file layout. With no `filter`, each project is
/// summarized by top-level package (file + symbol counts, a layout map, not a
/// file dump). With a project `filter`, that project's files are listed (capped).
/// Aggregating by default keeps a 2,000-file repo from blowing the response budget.
#[doc(hidden)]
pub fn files_text(
    store: &Store,
    filter: Option<&str>,
    pattern: Option<&str>,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    const DIRECTORIES_MAX: usize = 80;
    const FILES_MATCH_MAX: usize = 100;

    let projects = store.all_projects()?;
    let needle = pattern.map(str::to_lowercase);

    let mut out = String::new();
    let mut shown: u32 = 0;

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    for project in projects {
        if let Some(filter) = filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        let files = store.files_for(&project.id)?;
        let symbol_total = file_symbol_total(&files);

        if let Some(needle) = &needle {
            let mut matched: Vec<&FileRow> =
                files.iter().filter(|file| file.path.to_lowercase().contains(needle)).collect();

            matched.sort_by(|left, right| {
                path_penalty(&left.path).cmp(&path_penalty(&right.path)).then(left.path.cmp(&right.path))
            });

            // The match count, not the project's totals: a filtered listing headed
            // "3147 files, 19931 symbols" invites the reader to believe the thirteen
            // rows below it are the whole project.
            let _ = writeln!(out,
                "[{}] {} ({} files matching {needle:?}, of {} in the project)",
                project.id,
                project.name,
                matched.len(),
                files.len(),
            );

            let window = cursor::slice(&matched, page.offset, FILES_MATCH_MAX);

            for file in window {
                let _ = writeln!(out, "  {} ({} symbols)", file.path, file.node_count);
            }

            if let Some(next) =
                cursor::next_line(page.offset, window.len(), matched.len(), generation)
            {
                let _ = writeln!(out, "  {next}");
            }

            if matched.is_empty() {
                let _ = writeln!(out, "  (no files matching {pattern:?})", pattern = needle);
            }
        } else {
            let _ = writeln!(out,
                "[{}] {} ({} files, {symbol_total} symbols)",
                project.id,
                project.name,
                files.len(),
            );

            // Depth 2 even with no filter: a Django repo nests everything under one
            // top package (`app/`), so depth 1 collapses the whole project to a single
            // useless line. Depth 2 surfaces the per-domain breakdown (`app/inventory`,
            // `app/procurement`), still bounded by DIRECTORIES_MAX.
            let directories = aggregate_by_depth(&files, 2);

            for (name, file_count, symbol_count) in directories.iter().take(DIRECTORIES_MAX) {
                let _ = writeln!(out, "  {name}/ ({file_count} files, {symbol_count} symbols)");
            }

            if directories.len() > DIRECTORIES_MAX {
                let _ = writeln!(out, "  (+{} more directories)", directories.len() - DIRECTORIES_MAX);
            }

            if filter.is_none() {
                out.push_str("  (pass project=<id> to focus a single project, or pattern=<text> to list files)\n");
            }
        }

        out.push('\n');
        shown += 1;
    }

    if shown == 0 {
        return Ok(match filter {
            Some(filter) => format!("no project matches {filter:?}"),
            None => "no projects indexed".to_string(),
        });
    }

    Ok(out)
}

/// A one-call orientation digest. Per project: file and symbol counts, the
/// Django surface (models/views/routes/templates), the dominant packages, and
/// the constellation-wide cross-project link total. Built from cheap aggregate
/// queries (counts and a GROUP BY), never a full node load.
#[doc(hidden)]
pub fn overview_text(store: &Store, filter: Option<&str>) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let links = store.count_links()?;

    let mut out = String::new();
    let mut shown: u32 = 0;

    for project in projects {
        if let Some(filter) = filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        overview_project(&mut out, store, &project)?;
        shown += 1;
    }

    if shown == 0 {
        return Ok(match filter {
            Some(filter) => format!("no project matches {filter:?}"),
            None => "no projects indexed".to_string(),
        });
    }

    let _ = writeln!(out,
        "cross-project links: {links}{}",
        if links > 0 { " (`links` to list)" } else { "" },
    );

    Ok(out)
}

/// The overview block for one project, rendered into `out`: file/symbol counts, the
/// Django surface, the code surface, and the largest packages. Extracted from
/// [`overview_text`] so the per-project body stays one unit under the line bound.
fn overview_project(out: &mut String, store: &Store, project: &ProjectRow) -> Result<(), StoreError> {
    let files = store.files_for(&project.id)?;
    let counts = store.kind_counts(&project.id)?;

    let lookup: FxHashMap<NodeKind, u32> = counts.iter().copied().collect();
    let symbol_total = file_symbol_total(&files);

    let _ = writeln!(out,
        "[{}] {} ({} files, {symbol_total} symbols)",
        project.id,
        project.name,
        files.len(),
    );

    let django = kind_summary(
        &lookup,
        &[
            ("models", NodeKind::Model),
            ("views", NodeKind::View),
            ("routes", NodeKind::Route),
            ("templates", NodeKind::Template),
        ],
    );

    if !django.is_empty() {
        let _ = writeln!(out, "  django: {django}");
    }

    let code = kind_summary(
        &lookup,
        &[
            ("classes", NodeKind::Class),
            ("functions", NodeKind::Function),
            ("methods", NodeKind::Method),
        ],
    );

    if !code.is_empty() {
        let _ = writeln!(out, "  code: {code}");
    }

    let packages = aggregate_by_depth(&files, 2);

    if !packages.is_empty() {
        let listed: Vec<String> = packages
            .iter()
            .take(OVERVIEW_PACKAGES_MAX)
            .map(|(name, file_count, symbol_count)| format!("{name}/ ({file_count}f {symbol_count}s)"))
            .collect();

        let _ = writeln!(out, "  packages: {}", listed.join(", "));
    }

    out.push('\n');

    Ok(())
}

/// A " 12 models, 3 views" summary of selected kinds present in `lookup`, in the
/// given order, dropping any with a zero count. Empty when none are present.
fn kind_summary(lookup: &FxHashMap<NodeKind, u32>, kinds: &[(&str, NodeKind)]) -> String {
    kinds
        .iter()
        .filter_map(|(label, kind)| {
            let count = lookup.get(kind).copied().unwrap_or(0);

            (count > 0).then(|| format!("{count} {label}"))
        })
        .collect::<Vec<String>>()
        .join(", ")
}

/// The footer naming the route handlers that stayed unbound, as
/// `file:line handler`, appended after the table when there are any.
///
/// A per-row `(unresolved: x)` says what failed; this says where to go read it,
/// which is the half a reader needs to tell a typo from a view that resolution
/// declined to bind. Bounded by [`ROUTES_UNBOUND_NAMED_MAX`], past which only
/// the count is reported: a truncated list that does not say it was truncated is
/// the one shape worse than a count.
fn write_unbound_note(out: &mut String, unbound: &[String]) {
    if unbound.is_empty() {
        return;
    }

    let named = unbound.len().min(ROUTES_UNBOUND_NAMED_MAX);

    let _ = writeln!(out, "  ({} route handler(s) unresolved:", unbound.len());

    for entry in &unbound[..named] {
        let _ = writeln!(out, "     {entry}");
    }

    if unbound.len() > named {
        let _ = writeln!(out, "     ... and {} more", unbound.len() - named);
    }

    let _ = writeln!(out, "   run `constellation index` if the views module changed since)");
}

/// The URL map: every route's pattern to its view to the template the view
/// renders, grouped by project. The app's external surface as one table, the
/// orientation a pile of `urls.py` files cannot give at a glance. `filter`
/// restricts to one project; recommended for a large constellation.
#[doc(hidden)]
pub fn routes_text(
    store: &Store,
    project_filter: Option<&str>,
    pattern_filter: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    let projects = store.all_projects()?;
    let needle = pattern_filter.map(str::to_lowercase);

    // The namespaced reverse name (`harvest:load:detail`) each route answers to.
    // A route's pattern is only the fragment its own urls.py declares, so the table
    // cannot show where a route is mounted; the reverse name carries the whole
    // include chain and is what `reverse()` and `{% url %}` take, which is what a
    // reader leaves this table to write.
    let reverse_names: FxHashMap<String, String> = store
        .route_reverse_names()?
        .into_iter()
        .map(|(_, reverse_name, route_id)| (route_id, reverse_name))
        .collect();

    // The mounted path assembled at index time. Absent on a database indexed before
    // the table existed, which falls back to the declared fragment rather than
    // printing a path that is missing its prefix and looks requestable.
    let url_paths: FxHashMap<String, String> = store.route_url_paths()?.into_iter().collect();

    let mut out = String::new();
    let mut shown_projects: u32 = 0;
    let mut route_total: usize = 0;
    let mut consumed: usize = 0;
    let mut shown_rows: usize = 0;

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    for project in projects {
        if let Some(filter) = project_filter
            && project.id.as_str() != filter
            && project.name != filter
        {
            continue;
        }

        let mut routes = store.nodes_kind_in(&project.id, NodeKind::Route)?;

        if routes.is_empty() {
            continue;
        }

        // The handler each unbound route named. A route with no view otherwise
        // renders as a bare `(unresolved)`, which reads as "constellation has
        // nothing to say" when what it actually has is the symbol that failed to
        // bind and the line it is written on.
        let unresolved = store.unresolved_routes_in(&project.id)?;

        routes.sort_by(|left, right| {
            left.file_path.cmp(&right.file_path).then(left.span.start_line.cmp(&right.span.start_line))
        });

        // Resolve each route's view and template up front, then keep only the
        // rows the pattern filter matches (against pattern, view, template, or
        // the full route name), so a single-route question returns that route,
        // not the whole 572-row map.
        let mut rows: Vec<(String, String, String, String)> = Vec::new();
        let mut mounts_skipped: usize = 0;
        let mut unbound: Vec<String> = Vec::new();

        for route in &routes {
            let view = store
                .callees(&route.id)?
                .into_iter()
                .find(|(kind, _)| *kind == EdgeKind::RoutesTo)
                .map(|(_, node)| node);

            // An `include()` prefix is a Route node with a bare-URL name and no view
            // of its own. It is a mount point, not an endpoint: printing it as
            // `page/ -> (unresolved) -> (no template)` puts rows in the URL map that
            // no request can ever reach, and reads as a broken route rather than a
            // structural one. Its prefix is not lost, it is folded into the paths of
            // the routes mounted under it.
            if view.is_none() && route.name.contains('/') {
                mounts_skipped += 1;

                continue;
            }

            let template = match &view {
                Some(view) => store
                    .callees(&view.id)?
                    .into_iter()
                    .find(|(kind, _)| *kind == EdgeKind::Renders)
                    .map(|(_, node)| node.name),
                None => None,
            };

            let fragment = route_pattern(&route.qualified_name);
            let pattern = url_paths.get(route.id.as_str()).cloned().unwrap_or_else(|| fragment.to_string());

            let pending = match &view {
                Some(_) => None,
                None => unresolved.get(route.id.as_str()),
            };

            let view_name = match (&view, pending) {
                (Some(node), _) => node.name.clone(),
                (None, Some(pending)) => format!("(unresolved: {})", pending.reference),
                (None, None) => "(unresolved)".to_string(),
            };

            let template_name = template.as_deref().unwrap_or("(no template)").to_string();

            let reverse_name = reverse_names
                .get(route.id.as_str())
                .cloned()
                .unwrap_or_else(|| "(no name)".to_string());

            if let Some(needle) = &needle {
                let haystack = format!(
                    "{pattern} {view_name} {template_name} {reverse_name} {}",
                    route.qualified_name,
                )
                .to_lowercase();

                if !haystack.contains(needle) {
                    continue;
                }
            }

            // Recorded only for a row that survived the filter, so the footer
            // names the routes this table actually printed.
            if let Some(pending) = pending {
                let (path, line) = (&pending.file_path, pending.line);

                unbound.push(format!("{path}:{line} {}", pending.reference));
            }

            rows.push((reverse_name, pattern, view_name, template_name));
        }

        if rows.is_empty() {
            continue;
        }

        route_total += rows.len();

        // The page offset is global across projects, so a page boundary may fall
        // inside one project's table without losing or repeating a row.
        let already = consumed;
        consumed += rows.len();

        let local_offset = page.offset.saturating_sub(already);
        let remaining = (limit as usize).saturating_sub(shown_rows);
        let window = cursor::slice(&rows, local_offset, remaining.min(ROUTES_PER_PROJECT_MAX));

        if window.is_empty() {
            continue;
        }

        shown_projects += 1;
        shown_rows += window.len();

        let matching = match &needle {
            Some(needle) => format!(" matching {needle:?}"),
            None => String::new(),
        };

        let _ = writeln!(out, "[{}] {} ({} routes{matching})", project.id, project.name, rows.len());

        for (reverse_name, pattern, view_name, template_name) in window {
            let _ = writeln!(out,
                "  {reverse_name}\n    {pattern}  →  {view_name}  →  {template_name}",
            );
        }

        if mounts_skipped > 0 {
            let _ = writeln!(out,
                "  ({mounts_skipped} include() prefix row(s) folded into the paths above)",
            );
        }

        write_unbound_note(&mut out, &unbound);

        out.push('\n');
    }

    if let Some(next) = cursor::next_line(page.offset, shown_rows, route_total, generation) {
        out.push_str(&next);
        out.push('\n');
    }

    if shown_projects == 0 {
        return Ok(match (project_filter, pattern_filter) {
            (_, Some(pattern)) => format!("no routes matching {pattern:?}"),
            (Some(filter), None) => format!("no routes for {filter:?}"),
            (None, None) => "no routes indexed".to_string(),
        });
    }

    Ok(out)
}

/// The cross-project links grouped by repo pair: an import in one repo
/// resolved to a definition in another. Pairs are ordered by link count descending;
/// each edge prints its source and target endpoints. A `filter` restricts to links
/// touching that project on either end. Bounded by `limit`.
#[doc(hidden)]
pub fn links_text(
    store: &Store,
    filter: Option<&str>,
    limit: u32,
    page: &cursor::Page,
    generation: u64,
) -> Result<String, StoreError> {
    if store.count_links()? == 0 {
        return Ok("no cross-project links (index a second repo with `constellation link`)".to_string());
    }

    // Counted in the database under the same filter, not off the fetched page: a
    // denominator taken from the unfiltered total, or a pair count taken from a
    // bounded page, disagree with each other and with the rows below them.
    let pair_counts = store.link_pair_counts(filter)?;
    let total: u32 = pair_counts.iter().map(|(_, _, count)| *count).sum();

    let pair_totals: FxHashMap<(&str, &str), u32> = pair_counts
        .iter()
        .map(|(source, target, count)| ((source.as_str(), target.as_str()), *count))
        .collect();

    let wanted = u32::try_from(page.offset.saturating_add(limit as usize)).unwrap_or(u32::MAX);
    let fetch = wanted.saturating_mul(2).clamp(limit, LINKS_FETCH_MAX).max(limit);
    let links = store.link_edges(filter, fetch)?;

    if links.is_empty() {
        return Ok(match filter {
            Some(filter) => format!("no cross-project links touching {filter:?}"),
            None => "no cross-project links".to_string(),
        });
    }

    // Group by directed repo pair, preserving first-seen order, so the output
    // reads pair by pair rather than interleaving every repo combination.
    // Pair keys borrow the project ids out of `links`, which outlives the grouping.
    let mut pair_order: Vec<(&str, &str)> = Vec::new();
    let mut by_pair: FxHashMap<(&str, &str), Vec<&LinkEdge>> = FxHashMap::default();

    for link in &links {
        let pair = (link.source.project_id.as_str(), link.target.project_id.as_str());

        match by_pair.get_mut(&pair) {
            Some(group) => group.push(link),
            None => {
                pair_order.push(pair);
                by_pair.insert(pair, vec![link]);
            }
        }
    }

    pair_order.sort_by_key(|pair| std::cmp::Reverse(by_pair.get(pair).map_or(0, Vec::len)));

    // Flatten the grouped edges into one ordered sequence before paging, so a
    // page boundary can fall inside a pair without losing or repeating a row.
    let ordered: Vec<(&(&str, &str), &&LinkEdge)> = pair_order
        .iter()
        .flat_map(|pair| {
            by_pair.get(pair).expect("ordered pair has a group").iter().map(move |link| (pair, link))
        })
        .collect();

    let window = cursor::slice(&ordered, page.offset, limit as usize);

    let mut out = String::new();

    if let Some(note) = &page.note {
        out.push_str(note);
        out.push('\n');
    }

    let _ = writeln!(out, "cross-project links: {} shown of {total}", window.len());

    let mut current_pair: Option<(&str, &str)> = None;

    for (pair, link) in window {
        if current_pair != Some(**pair) {
            let count = pair_totals.get(*pair).copied().unwrap_or_default();

            let _ = writeln!(out, "\n{} -> {}: {count}", pair.0, pair.1);
            current_pair = Some(**pair);
        }

        let _ = writeln!(out,
            "  [{}] {} ({}:{}) -> {} ({}:{})",
            link.kind.as_str(),
            link.source.name,
            link.source.file_path,
            link.source.span.start_line,
            link.target.name,
            link.target.file_path,
            link.target.span.start_line,
        );
    }

    // Paged against the filtered total, not the fetched buffer. `ordered` holds
    // only what this call fetched, so counting against it printed a second, smaller
    // denominator under a header already stating the real one.
    let remaining = total.max(u32::try_from(ordered.len()).unwrap_or(u32::MAX)) as usize;

    if let Some(next) = cursor::next_line(page.offset, window.len(), remaining, generation) {
        out.push_str(&next);
        out.push('\n');
    }

    Ok(out)
}
