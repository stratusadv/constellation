//! The lifecycle of an unresolved reference: loaded for resolution,
//! committed as an edge, and deleted once satisfied.
//!
//! Also the queries over what stayed unresolved, which is how the
//! server reports a call it can see but cannot bind.

use constellation_graph::{Edge, EdgeKind, Language, Node, NodeId, ProjectId};
use constellation_resolution::{QUERYSET_BUILTINS, UnresolvedRef};
use rusqlite::{Connection, params};
use rustc_hash::FxHashMap;

use crate::error::{StoreError, charge};
use crate::limits::{ROWS_LOADED_MAX, ROWS_PER_FILE_MAX, UNRESOLVED_SOURCE_ROWS_MAX};
use crate::mapping::{column_u32, line_u32, node_from_row, node_row};
use crate::rows::UnresolvedRoute;
use crate::sql::NODE_COLUMNS_PREFIXED;
use crate::store::Store;
use crate::time::now_ms;
use crate::write::{insert_edges, insert_nodes};

/// A raw `unresolved_refs` row, before parsing.
struct RefRow {
    id: i64,
    from_node_id: String,
    reference_name: String,
    reference_kind: String,
    line: i64,
    column: i64,
    file_path: String,
    language: String,
    candidates: Option<String>,
}

fn reference_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        id: row.get(0)?,
        from_node_id: row.get(1)?,
        reference_name: row.get(2)?,
        reference_kind: row.get(3)?,
        line: row.get(4)?,
        column: row.get(5)?,
        file_path: row.get(6)?,
        language: row.get(7)?,
        candidates: row.get(8)?,
    })
}

/// An [`UnresolvedRef`] and its row id rebuilt, returning `None` for a row that
/// no longer satisfies the type's invariants.
fn reference_from_row(raw: RefRow) -> Option<(i64, UnresolvedRef)> {
    let kind = EdgeKind::from_str_label(&raw.reference_kind)?;
    let language = Language::from_str_label(&raw.language)?;

    if raw.reference_name.is_empty() || raw.file_path.is_empty() || raw.line < 1 {
        return None;
    }

    let mut reference = UnresolvedRef::new(
        NodeId::from_raw(raw.from_node_id),
        raw.reference_name,
        kind,
        line_u32(raw.line),
        column_u32(raw.column),
        raw.file_path,
        language,
    );

    reference.candidates = raw
        .candidates
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    Some((raw.id, reference))
}

/// The handler as the URL file spelled it. A route reference carries its
/// receiver module as the first candidate (`json_views` of
/// `json_views.bulk_update_view`), which is the half that says *which* app's
/// views module was meant, so a reader can tell a missing view from a view that
/// resolution declined to bind.
fn qualify_handler(name: &str, candidates: Option<&str>) -> String {
    assert!(!name.is_empty(), "a route reference names a handler");

    let receiver = candidates
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .and_then(|candidates| candidates.into_iter().next())
        .filter(|receiver| !receiver.is_empty());

    match receiver {
        Some(receiver) => format!("{receiver}.{name}"),
        None => name.to_string(),
    }
}

/// The archived references pointing into `path` moved back into the pending
/// table, on the given connection or transaction, returning how many moved.
///
/// This is the counterpart of the `ON DELETE CASCADE` that is about to run.
/// Deleting a file's nodes takes every edge that touches them, and the inbound
/// half of those edges belongs to files this run has no reason to re-extract.
/// Their references were archived when they first resolved, so putting them
/// back is what lets resolution rebuild an edge it did not lose by choice.
///
/// The insert names its columns and reads them from `resolved_refs` in the same
/// order, so the two tables' shared shape is stated once rather than trusted.
pub(crate) fn requeue_refs_into_file(
    connection: &Connection,
    project: &ProjectId,
    path: &str,
) -> Result<u32, StoreError> {
    assert!(!path.is_empty(), "file path must not be empty");

    let requeued = connection.execute(
        "INSERT INTO unresolved_refs
             (project_id, from_node_id, reference_name, reference_kind,
              line, column, file_path, language, candidates)
         SELECT r.project_id, r.from_node_id, r.reference_name, r.reference_kind,
                r.line, r.column, r.file_path, r.language, r.candidates
         FROM resolved_refs r
         JOIN nodes n ON n.id = r.target_node_id
         WHERE n.project_id = ?1 AND n.file_path = ?2",
        params![project.as_str(), path],
    )?;

    connection.execute(
        "DELETE FROM resolved_refs
         WHERE target_node_id IN
             (SELECT id FROM nodes WHERE project_id = ?1 AND file_path = ?2)",
        params![project.as_str(), path],
    )?;

    Ok(u32::try_from(requeued).unwrap_or(u32::MAX))
}

impl Store {
    /// The deletion of a project's still-pending references of one edge kind, returning
    /// how many were removed. Backs the styles gate: a `styles` reference that
    /// matched no indexed selector can never resolve (the project's CSS is fully
    /// known by the time resolution finishes), so persisting it is dead weight.
    pub fn delete_unresolved_kind(
        &self,
        project: &ProjectId,
        kind: EdgeKind,
    ) -> Result<u32, StoreError> {
        let removed = self.connection.execute(
            "DELETE FROM unresolved_refs WHERE project_id = ?1 AND reference_kind = ?2",
            params![project.as_str(), kind.as_str()],
        )?;

        Ok(u32::try_from(removed).unwrap_or(u32::MAX))
    }

    /// The replacement of a project's route reverse-name index: every prior row for
    /// the project is cleared, then the freshly computed `(reverse_name, route_id)`
    /// pairs are written, so each index re-derives the project's reverse names from
    /// scratch. Read across projects by [`Store::route_reverse_names`] for
    /// cross-project `{% url %}`/reverse resolution.
    pub fn replace_route_reverse_names(
        &self,
        project: &ProjectId,
        names: &[(String, String)],
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM route_reverse_name WHERE project_id = ?1",
            params![project.as_str()],
        )?;

        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO route_reverse_name (project_id, reverse_name, route_id) VALUES (?1, ?2, ?3)",
            )?;

            let mut written: u32 = 0;

            for (reverse_name, route_id) in names {
                charge(&mut written, ROWS_PER_FILE_MAX, "reverse-name insert")?;

                insert.execute(params![project.as_str(), reverse_name, route_id])?;
            }
        }

        transaction.commit()?;

        Ok(u32::try_from(names.len()).unwrap_or(u32::MAX))
    }

    /// The route nodes a namespaced reverse name (`production:line:schedule:page:detail`)
    /// resolves to. This is the name every other tool *prints* for a route, and the
    /// only form a reader has to hand after reading the URL map, so it must also be a
    /// name they can pass back in. Empty when the name reverses to nothing.
    pub fn nodes_by_reverse_name(&self, reverse_name: &str) -> Result<Vec<Node>, StoreError> {
        assert!(!reverse_name.is_empty(), "reverse name must not be empty");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}
             FROM route_reverse_name r
             JOIN nodes n ON n.id = r.route_id
             WHERE r.reverse_name = ?1",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params![reverse_name], node_row)?;

        let mut nodes: Vec<Node> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "reverse-name load")?;

            if let Some(node) = node_from_row(row?) {
                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// The project's route URL paths, replaced wholesale (the same replace-per-project
    /// shape as [`Self::replace_route_reverse_names`], so a re-index never leaves a
    /// stale path behind a moved include).
    pub fn replace_route_url_paths(
        &self,
        project: &ProjectId,
        paths: &[(String, String)],
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;

        transaction
            .execute("DELETE FROM route_url_path WHERE project_id = ?1", params![project.as_str()])?;

        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO route_url_path (project_id, route_id, url_path) VALUES (?1, ?2, ?3)",
            )?;

            let mut written: u32 = 0;

            for (route_id, url_path) in paths {
                charge(&mut written, ROWS_PER_FILE_MAX, "url-path insert")?;

                insert.execute(params![project.as_str(), route_id, url_path])?;
            }
        }

        transaction.commit()?;

        Ok(u32::try_from(paths.len()).unwrap_or(u32::MAX))
    }

    /// The assembled URL path of every route, as `(route_id, url_path)`. Empty on a
    /// database indexed before the table existed, which a caller must treat as
    /// "unknown" and fall back to the declared fragment, never as "no prefix".
    pub fn route_url_paths(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement =
            self.connection.prepare_cached("SELECT route_id, url_path FROM route_url_path")?;

        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;

        let mut paths: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "url-path load")?;

            paths.push(row?);
        }

        Ok(paths)
    }

    /// The route reverse names across all projects, as `(project_id, reverse_name,
    /// route_id)`, for the cross-project linker to resolve a namespaced reverse into
    /// the route another project defines.
    pub fn route_reverse_names(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT project_id, reverse_name, route_id FROM route_reverse_name")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut names: Vec<(String, String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "reverse-name index load")?;

            names.push(row?);
        }

        Ok(names)
    }

    /// The replacement of the project's synthesized External symbol nodes and the edges into
    /// them. External nodes (kind `external`) are cleared first (deleting a node
    /// cascades to its edges) then re-created, so each index re-derives the
    /// library-boundary layer from scratch (like [`Store::replace_synthesized_edges`]).
    pub fn replace_external(
        &self,
        project: &ProjectId,
        nodes: &[Node],
        edges: &[Edge],
    ) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute(
            "DELETE FROM nodes WHERE project_id = ?1 AND kind = 'external'",
            params![project.as_str()],
        )?;

        insert_nodes(&transaction, nodes, now_ms())?;
        insert_edges(&transaction, edges)?;

        transaction.commit()?;

        Ok(u32::try_from(edges.len()).unwrap_or(u32::MAX))
    }

    /// The pending references, each paired with its row id so a resolved
    /// reference can be deleted by id once its edge is written. Scoped to one
    /// project, or (with `None`) across every project.
    pub fn load_unresolved(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<(i64, UnresolvedRef)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, from_node_id, reference_name, reference_kind, line, column, file_path, language, candidates
             FROM unresolved_refs WHERE (?1 IS NULL OR project_id = ?1)",
        )?;

        let rows = statement.query_map(params![project.map(ProjectId::as_str)], reference_row)?;

        let mut references: Vec<(i64, UnresolvedRef)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "reference load")?;

            if let Some(pair) = reference_from_row(row?) {
                references.push(pair);
            }
        }

        Ok(references)
    }

    /// The atomic write of each resolved edge, moving the reference that
    /// produced it out of the pending table and into `resolved_refs`. The two
    /// arrays move in lockstep: `resolved[i]` is the row id of the reference
    /// that produced `edges[i]`.
    ///
    /// The reference is archived rather than discarded because the edge it
    /// produced is not permanent: re-indexing the file the edge points *into*
    /// deletes that file's nodes, and the cascade takes every inbound edge with
    /// them. The archived row is what [`Store::requeue_refs_into_file`] puts
    /// back so the next resolution pass rebuilds the edge.
    pub fn commit_resolved(&self, resolved: &[(i64, Edge)]) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut written: u32 = 0;

        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO edges (source, target, kind, line, column, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;

            let mut archive = transaction.prepare_cached(
                "INSERT INTO resolved_refs
                     (project_id, from_node_id, target_node_id, reference_name, reference_kind,
                      line, column, file_path, language, candidates)
                 SELECT project_id, from_node_id, ?2, reference_name, reference_kind,
                        line, column, file_path, language, candidates
                 FROM unresolved_refs WHERE id = ?1",
            )?;

            let mut delete =
                transaction.prepare_cached("DELETE FROM unresolved_refs WHERE id = ?1")?;

            for (reference_id, edge) in resolved {
                charge(&mut written, ROWS_LOADED_MAX, "resolved commit")?;

                insert.execute(params![
                    edge.source.as_str(),
                    edge.target.as_str(),
                    edge.kind.as_str(),
                    edge.line,
                    edge.column,
                    edge.provenance,
                ])?;

                archive.execute(params![reference_id, edge.target.as_str()])?;
                delete.execute(params![reference_id])?;
            }
        }

        transaction.commit()?;

        Ok(written)
    }

    /// The references archived in `resolved_refs` whose target lives in `path`,
    /// moved back into the pending table so the next resolution pass rebuilds
    /// the edges that clearing the file is about to destroy. Returns how many
    /// were requeued.
    ///
    /// Call it *before* the file's nodes are deleted: the archived rows are
    /// found by joining through those nodes, so once they are gone the set is
    /// unrecoverable.
    pub fn requeue_refs_into_file(
        &self,
        project: &ProjectId,
        path: &str,
    ) -> Result<u32, StoreError> {
        requeue_refs_into_file(&self.connection, project, path)
    }

    /// The route references that never became an edge, keyed by the id of the
    /// route that emitted them: the handler each one names, qualified by the
    /// receiver module when the URL file reached the view through one
    /// (`json_views.bulk_update_view`), and the file and line it was written on.
    ///
    /// A route with no `RoutesTo` edge is otherwise indistinguishable from a
    /// route whose view was never named, and both render as `(unresolved)`,
    /// which says nothing a reader can act on. This is what turns that into the
    /// symbol that failed to bind and the line to go read.
    pub fn unresolved_routes_in(
        &self,
        project: &ProjectId,
    ) -> Result<FxHashMap<String, UnresolvedRoute>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT from_node_id, reference_name, candidates, file_path, line
             FROM unresolved_refs
             WHERE project_id = ?1 AND reference_kind = ?2",
        )?;

        let rows = statement.query_map(
            params![project.as_str(), EdgeKind::RoutesTo.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;

        let mut routes: FxHashMap<String, UnresolvedRoute> = FxHashMap::default();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "unresolved route load")?;

            let (route_id, name, candidates, file_path, line) = row?;

            routes.insert(
                route_id,
                UnresolvedRoute {
                    reference: qualify_handler(&name, candidates.as_deref()),
                    file_path,
                    line: line_u32(line),
                },
            );
        }

        Ok(routes)
    }

    /// The deletion of every pending reference that now has a satisfying edge.
    /// External synthesis and the synthesis passes write their edges in bulk
    /// (`replace_external`, `replace_synthesized_edges`) rather than through
    /// [`Store::commit_resolved`], so the reference rows they satisfy are never
    /// deleted by id and linger as false pending rows. Run once after every
    /// resolution, linking, and synthesis pass has emitted its edges, this clears
    /// those now-resolved rows so the pending table holds only references that bind
    /// to nothing. Returns the number of rows removed.
    ///
    /// Two clauses, both safe against deleting a genuinely-unresolved reference:
    /// - A location match (same source, kind, line, column): the precise key, so a
    ///   reference is never dropped because a same-named sibling on its line
    ///   resolved (`f(g(x))`).
    /// - A name match to a template or external target (same source and kind, target
    ///   node named the reference). External synthesis deduplicates its edges by
    ///   (source, target, kind), so an `{% include %}` repeated down a card, or a
    ///   library symbol called on many lines, yields one edge at the first line and
    ///   leaves the later locations unmatched by the first clause. Template names are
    ///   globally namespaced and an external node is named for the very symbol the
    ///   reference imports/calls, so a name match to one of those kinds is the same
    ///   resolution, not a collision.
    pub fn delete_satisfied_unresolved(&self) -> Result<u32, StoreError> {
        let by_location = self.connection.execute(
            "DELETE FROM unresolved_refs
             WHERE EXISTS (
                 SELECT 1 FROM edges e
                 WHERE e.source = unresolved_refs.from_node_id
                   AND e.kind = unresolved_refs.reference_kind
                   AND e.line = unresolved_refs.line
                   AND e.column = unresolved_refs.column
             )",
            [],
        )?;

        let by_name = self.connection.execute(
            "DELETE FROM unresolved_refs
             WHERE EXISTS (
                 SELECT 1 FROM edges e
                 JOIN nodes n ON n.id = e.target
                 WHERE e.source = unresolved_refs.from_node_id
                   AND e.kind = unresolved_refs.reference_kind
                   AND n.name = unresolved_refs.reference_name
                   AND n.kind IN ('template', 'external')
             )",
            [],
        )?;

        Ok(u32::try_from(by_location + by_name).unwrap_or(u32::MAX))
    }

    /// The collapse of each external stub into the real cross-project definition it
    /// shadows: redirect every edge pointing at the stub to the definition, then
    /// delete the stub. Retargets *before* deleting so the `ON DELETE CASCADE` on
    /// `edges.target` cannot drop the just-redirected edges. After this a model
    /// that "extends an external mixin" extends the real, indexed class across the
    /// project boundary, and `node`/`search` show one definition, not a definition
    /// plus per-project import stubs. Returns the number of stubs unified.
    pub fn unify_externals(&self, redirects: &[(NodeId, NodeId)]) -> Result<u32, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut unified: u32 = 0;

        {
            let mut retarget =
                transaction.prepare_cached("UPDATE edges SET target = ?1 WHERE target = ?2")?;
            let mut delete = transaction.prepare_cached("DELETE FROM nodes WHERE id = ?1")?;

            for (stub, definition) in redirects {
                charge(&mut unified, ROWS_LOADED_MAX, "unify")?;

                retarget.execute(params![definition.as_str(), stub.as_str()])?;
                delete.execute(params![stub.as_str()])?;
            }
        }

        transaction.commit()?;

        Ok(unified)
    }

    /// The number of still-unresolved references naming `symbol`, across all projects.
    /// These are call sites that named the symbol but never bound to an edge:
    /// dynamic dispatch (a chained queryset, `get_queryset`, a template or admin
    /// reference) or a missing import. A non-zero count means a symbol's real
    /// callers are likely undercounted by the resolved edges alone: a trust signal
    /// the agent needs before treating a low caller count as "safe to change".
    pub fn count_unresolved_named(&self, symbol: &str) -> Result<u32, StoreError> {
        assert!(!symbol.is_empty(), "symbol must not be empty");

        // The template member-access pipeline (`accesses_member` for a
        // `{{ var.attr }}`, `context_type` for a view's `var -> model` binding)
        // leaves its references pending by design: a synthesis pass consumes
        // them, so they are not unresolved callers and must not inflate the
        // dark-caller trust signal for a member or model name.
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM unresolved_refs
             WHERE reference_name = ?1
               AND reference_kind NOT IN
                   ('accesses_member', 'context_type', 'loop_binding', 'reverse_accessor',
                    'derived_collection')",
            params![symbol],
            |row| row.get(0),
        )?;

        assert!(total >= 0, "unresolved count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The same count restricted to one project. A name that some other
    /// repository dispatches on says nothing about whether this project's
    /// definition of it is reachable, so a dead-code scan must ask about its own
    /// project rather than the whole constellation.
    pub fn count_unresolved_named_in(
        &self,
        project: &ProjectId,
        symbol: &str,
    ) -> Result<u32, StoreError> {
        assert!(!symbol.is_empty(), "symbol must not be empty");

        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM unresolved_refs
             WHERE reference_name = ?1
               AND project_id = ?2
               AND reference_kind NOT IN
                   ('accesses_member', 'context_type', 'loop_binding', 'reverse_accessor',
                    'derived_collection')",
            params![symbol, project.as_str()],
            |row| row.get(0),
        )?;

        assert!(total >= 0, "unresolved count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The unbound call sites naming `name`: `Calls` references the resolver could
    /// not tie to a definition (an overloaded or base service method reached through
    /// a `Model.services.x()` descriptor, a builtin like `save_model_obj`, or any
    /// untyped receiver), joined to the enclosing node they sit in, with the call
    /// line. Dropped from the precise edge set to avoid a false edge; surfaced here by
    /// name so a caller listing can show them as unproven, the recall a text search
    /// gives without inventing an edge. Bounded by `limit`.
    pub fn unresolved_callers_of(&self, name: &str, limit: u32) -> Result<Vec<(Node, u32)>, StoreError> {
        assert!(!name.is_empty(), "name must not be empty");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, u.line
             FROM unresolved_refs u
             JOIN nodes n ON n.id = u.from_node_id
             WHERE u.reference_name = ?1 AND u.reference_kind = 'calls'
             ORDER BY n.project_id, n.file_path, u.line
             LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![name, limit], |row| {
            let raw = node_row(row)?;
            let line: i64 = row.get(20)?;

            Ok((raw, line))
        })?;

        let mut out: Vec<(Node, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, limit, "unresolved caller load")?;

            let (raw, line) = row?;

            if let Some(node) = node_from_row(raw) {
                out.push((node, line.max(1) as u32));
            }
        }

        Ok(out)
    }

    /// The same unbound call sites restricted to one project. [`Self::unresolved_callers_of`]
    /// orders by project id and then truncates, so a symbol whose name is dispatched
    /// widely in an alphabetically earlier repository (`django-spire` before
    /// `shop-portal`) fills the whole window and the symbol's own
    /// repository never appears: the caller listing then labels every site
    /// "name match in another repository" while the real local call site is the one
    /// row that got cut. A caller asks each home project first, so the sites that
    /// belong to the symbol are never the ones the limit drops.
    pub fn unresolved_callers_of_in(
        &self,
        project: &ProjectId,
        name: &str,
        limit: u32,
    ) -> Result<Vec<(Node, u32)>, StoreError> {
        assert!(!name.is_empty(), "name must not be empty");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, u.line
             FROM unresolved_refs u
             JOIN nodes n ON n.id = u.from_node_id
             WHERE u.reference_name = ?1 AND u.reference_kind = 'calls' AND n.project_id = ?2
             ORDER BY n.file_path, u.line
             LIMIT ?3",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![name, project.as_str(), limit], |row| {
            let raw = node_row(row)?;
            let line: i64 = row.get(20)?;

            Ok((raw, line))
        })?;

        let mut out: Vec<(Node, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, limit, "unresolved caller load")?;

            let (raw, line) = row?;

            if let Some(node) = node_from_row(raw) {
                out.push((node, line.max(1) as u32));
            }
        }

        Ok(out)
    }

    /// The unbound callee names a definition invokes: `Calls` references originating
    /// in `from_id` that the resolver could not tie to a target, each with its call
    /// line. The callee counterpart of [`unresolved_callers_of`]; disjoint from the
    /// resolved callees, since a bound call leaves this table. The Django
    /// QuerySet/Manager builtins (`all`, `filter`, `select_related`, ...) are excluded:
    /// dynamically dispatched with no project definition to bind to, they are incidental
    /// noise in this view, not a dropped project call. Excluded in SQL so `limit` still
    /// yields real callees. Bounded by `limit`.
    pub fn unresolved_callees_of(
        &self,
        from_id: &NodeId,
        limit: u32,
    ) -> Result<Vec<(String, u32)>, StoreError> {
        // The builtin names are compile-time constant identifiers (no quotes or
        // separators), so formatting them into the `NOT IN` list cannot inject.
        let exclusions =
            QUERYSET_BUILTINS.iter().map(|name| format!("'{name}'")).collect::<Vec<_>>().join(", ");

        let sql = format!(
            "SELECT reference_name, line FROM unresolved_refs
             WHERE from_node_id = ?1 AND reference_kind = 'calls'
               AND reference_name NOT IN ({exclusions})
             ORDER BY line LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![from_id.as_str(), limit], |row| {
            let name: String = row.get(0)?;
            let line: i64 = row.get(1)?;

            Ok((name, line))
        })?;

        let mut out: Vec<(String, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, limit, "unresolved callee load")?;

            let (name, line) = row?;

            out.push((name, line.max(1) as u32));
        }

        Ok(out)
    }

    /// The count of unresolved references emitted by each node, keyed by the
    /// emitting node's id. One aggregate read per project rather than a query per
    /// candidate, so a flow-criticality pass can charge each reach set for the
    /// dynamic dispatch inside it.
    pub fn unresolved_counts_by_source(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<FxHashMap<String, u32>, StoreError> {
        let project_filter = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare_cached(
            "SELECT from_node_id, COUNT(*) FROM unresolved_refs
             WHERE (?1 IS NULL OR project_id = ?1)
             GROUP BY from_node_id
             LIMIT ?2",
        )?;

        let rows = statement.query_map(params![project_filter, UNRESOLVED_SOURCE_ROWS_MAX], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut counts: FxHashMap<String, u32> = FxHashMap::default();
        let mut loaded: u32 = 0;

        for row in rows {
            charge(&mut loaded, UNRESOLVED_SOURCE_ROWS_MAX, "unresolved load")?;

            let (source, count) = row?;

            counts.insert(source, u32::try_from(count).unwrap_or(u32::MAX));
        }

        Ok(counts)
    }
}
