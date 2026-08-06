//! Reading edges: callers and callees, the structural relations the
//! synthesis passes need, and the cross-project links.

use constellation_graph::{Edge, EdgeKind, Node, NodeId, ProjectId, relation_field_target};
use rusqlite::params;

use crate::error::{StoreError, charge};
use crate::limits::{BULK_PARAMS_MAX, PREALLOC_ROWS_MAX, ROWS_LOADED_MAX};
use crate::mapping::{node_from_row, node_row, node_row_at};
use crate::rows::{IncomingRef, LinkEdge, OutgoingRef};
use crate::sql::{CALLEES_SQL, CALLERS_SQL, NODE_COLUMNS_PREFIXED, node_columns, placeholder_list};
use crate::store::Store;
use crate::write::insert_edges;

impl Store {
    /// The edges as (source id, target id) pairs, for building the in-memory
    /// adjacency that structural ranking walks.
    pub fn all_edges(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached("SELECT source, target FROM edges")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "edge load")?;

            edges.push(row?);
        }

        Ok(edges)
    }

    /// The edges as `(source, target, kind)`, the directed, kinded form the
    /// explore cache needs to trace a call path between named symbols (`all_edges`
    /// drops the kind for the undirected random-walk adjacency).
    pub fn all_edges_kinded(&self) -> Result<Vec<(String, String, EdgeKind)>, StoreError> {
        let mut statement = self.connection.prepare_cached("SELECT source, target, kind FROM edges")?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut edges: Vec<(String, String, EdgeKind)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "edge load")?;

            let (source, target, kind) = row?;

            if let Some(edge_kind) = EdgeKind::from_str_label(&kind) {
                edges.push((source, target, edge_kind));
            }
        }

        Ok(edges)
    }

    /// The replacement of a project's synthesized edges (those tagged with a `synthesis:`
    /// provenance) with `edges`, atomically. The synthesis pass re-derives them
    /// from scratch each index, so the prior set is cleared first (scoped to
    /// this project's nodes) to avoid duplicates. Returns the number written.
    pub fn replace_synthesized_edges(
        &self,
        project: &ProjectId,
        prefix: &str,
        edges: &[Edge],
    ) -> Result<u32, StoreError> {
        assert!(prefix.starts_with("synthesis:"), "synthesized provenance is namespaced under synthesis:");

        let pattern = format!("{prefix}%");
        let transaction = self.connection.unchecked_transaction()?;

        // Scope the clear to this provenance prefix so independent synthesis
        // passes (events, reverse relations) do not clobber one another's edges.
        transaction.execute(
            "DELETE FROM edges WHERE provenance LIKE ?2
             AND source IN (SELECT id FROM nodes WHERE project_id = ?1)",
            params![project.as_str(), pattern],
        )?;

        insert_edges(&transaction, edges)?;
        transaction.commit()?;

        Ok(u32::try_from(edges.len()).unwrap_or(u32::MAX))
    }

    /// The genuine forward `relates_to` relations whose source is in `project`, as
    /// `(source_id, target_id)`. Excludes the reverse relations a prior synthesis
    /// pass added (provenance `synthesis:reverse%`), so re-deriving reverses from
    /// this set is idempotent: it never feeds its own output back in.
    pub fn relation_edges(&self, project: &ProjectId) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT e.source, e.target FROM edges e
             JOIN nodes s ON e.source = s.id
             WHERE e.kind = 'relates_to' AND s.project_id = ?1
               AND (e.provenance IS NULL OR e.provenance NOT LIKE 'synthesis:reverse%')",
        )?;

        let rows = statement.query_map(params![project.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut relations: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "relation load")?;

            relations.push(row?);
        }

        Ok(relations)
    }

    /// The resolved `extends` edges whose source is in `project`, as
    /// `(subclass_id, base_id)`, the class hierarchy the override synthesis
    /// walks. A base resolved to a third-party `External` node is included; the
    /// override pass simply finds no method to bind under it. `None` spans every
    /// project, the whole-constellation hierarchy an inherited-method walk needs
    /// once cross-project bases have been unified.
    pub fn extends_edges(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let scope = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare_cached(
            "SELECT e.source, e.target FROM edges e
             JOIN nodes s ON e.source = s.id
             WHERE e.kind = 'extends' AND (?1 IS NULL OR s.project_id = ?1)",
        )?;

        let rows = statement.query_map(params![scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "extends load")?;

            edges.push(row?);
        }

        Ok(edges)
    }

    /// The resolved `returns` edges whose source is in `project`, as
    /// `(callable_id, returned_node_id)`: the annotated return type of every
    /// function and method. Backs typing a local from the call that produced it
    /// (`demo = Demo.start(...)`), which needs the callee's annotation and so
    /// cannot be read from the calling file. `None` spans every project, since a
    /// factory often lives across a boundary from its caller.
    pub fn returns_edges(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let scope = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare_cached(
            "SELECT e.source, e.target FROM edges e
             JOIN nodes s ON e.source = s.id
             WHERE e.kind = 'returns' AND (?1 IS NULL OR s.project_id = ?1)",
        )?;

        let rows = statement.query_map(params![scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut edges: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "returns load")?;

            edges.push(row?);
        }

        Ok(edges)
    }

    /// The `(id, name)` of every method node in `project`, the symbols the
    /// override synthesis matches by name against each class's ancestors. `None`
    /// spans every project, so a walk can land on a base class a companion
    /// package defines.
    pub fn class_methods(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let scope = project.map(|project| project.as_str().to_string());

        let mut statement = self.connection.prepare_cached(
            "SELECT id, name FROM nodes WHERE (?1 IS NULL OR project_id = ?1) AND kind = 'method'",
        )?;

        let rows = statement.query_map(params![scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut methods: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "method load")?;

            methods.push(row?);
        }

        Ok(methods)
    }

    /// The `(id, related_model)` of every relation field across the
    /// constellation, the map that types a call made through a model field
    /// (`self.locations.lots()`). A field's signature is its whole declaration, so
    /// the related model is read out of it by
    /// [`constellation_graph::relation_field_target`]; a field declaring a column
    /// rather than a relation names none and is left out.
    pub fn field_relations(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, signature FROM nodes WHERE kind = 'field' AND signature IS NOT NULL",
        )?;

        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;

        let mut fields: Vec<(String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "field load")?;

            let (id, signature) = row?;

            if let Some(related) = relation_field_target(&signature) {
                fields.push((id, related.to_string()));
            }
        }

        Ok(fields)
    }

    /// The `(id, name, file_path)` of every callable node across the
    /// constellation, the pool a receiver-typed call is matched against once its
    /// receiver names a module.
    pub fn callable_identities(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, name, file_path FROM nodes
             WHERE kind IN ('method', 'function', 'view') AND language = 'python'",
        )?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut callables: Vec<(String, String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "callable load")?;

            callables.push(row?);
        }

        Ok(callables)
    }

    /// The `(id, qualified_name, name)` of every class and model node across the
    /// constellation, the lookup that turns a reference's candidate class (a
    /// qualified name, or a Django manager named by convention) into the node id
    /// an inheritance walk starts from.
    pub fn class_identities(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, qualified_name, name FROM nodes WHERE kind IN ('class', 'model')",
        )?;

        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;

        let mut classes: Vec<(String, String, String)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "class load")?;

            classes.push(row?);
        }

        Ok(classes)
    }

    /// The number of cross-project edges (those the linker tagged with a `link:`
    /// provenance).
    pub fn count_links(&self) -> Result<u32, StoreError> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM edges WHERE provenance LIKE 'link:%'",
            [],
            |row| row.get(0),
        )?;

        assert!(total >= 0, "link count must be non-negative");

        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The exact number of cross-project links per directed repo pair, under the
    /// same `project` filter [`Self::link_edges`] applies.
    ///
    /// A renderer cannot count these off a fetched page: a page is bounded by its
    /// limit, so grouping it yields "how many of this pair I happened to fetch",
    /// which rises with the limit and reads as a total. These are counted in the
    /// database over the whole filtered set, so a pair's number and their sum are
    /// both the truth at any limit.
    pub fn link_pair_counts(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<(String, String, u32)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT s.project_id, t.project_id, COUNT(*)
             FROM edges e
             JOIN nodes s ON e.source = s.id
             JOIN nodes t ON e.target = t.id
             WHERE e.provenance LIKE 'link:%'
               AND (?1 IS NULL OR s.project_id = ?1 OR t.project_id = ?1)
             GROUP BY s.project_id, t.project_id",
        )?;

        let rows = statement.query_map(params![project], |row| {
            let source: String = row.get(0)?;
            let target: String = row.get(1)?;
            let total: i64 = row.get(2)?;

            Ok((source, target, u32::try_from(total).unwrap_or(u32::MAX)))
        })?;

        let mut counts: Vec<(String, String, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "pair count load")?;

            counts.push(row?);
        }

        Ok(counts)
    }

    /// The cross-project link edges, both endpoints hydrated, newest first by
    /// edge id. These are the constellation's connective tissue: an import in one
    /// repo resolved to a definition in another, the links no single-repo index holds.
    /// Bounded by `limit`.
    pub fn link_edges(&self, project: Option<&str>, limit: u32) -> Result<Vec<LinkEdge>, StoreError> {
        let source_columns = node_columns("s");
        let target_columns = node_columns("t");

        let sql = format!(
            "SELECT {source_columns}, e.kind, e.provenance, {target_columns}
             FROM edges e
             JOIN nodes s ON e.source = s.id
             JOIN nodes t ON e.target = t.id
             WHERE e.provenance LIKE 'link:%'
               AND (?2 IS NULL OR s.project_id = ?2 OR t.project_id = ?2)
             ORDER BY e.id DESC LIMIT ?1",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![limit, project], |row| {
            let source = node_row_at(row, 0)?;
            let kind = row.get::<_, String>(20)?;
            let provenance = row.get::<_, Option<String>>(21)?;
            let target = node_row_at(row, 22)?;

            Ok((source, kind, provenance, target))
        })?;

        let mut links: Vec<LinkEdge> = Vec::with_capacity((limit as usize).min(PREALLOC_ROWS_MAX));
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "link load")?;

            let (source, kind, provenance, target) = row?;

            if let (Some(source), Some(target), Some(kind)) =
                (node_from_row(source), node_from_row(target), EdgeKind::from_str_label(&kind))
            {
                links.push(LinkEdge { source, target, kind, provenance: provenance.unwrap_or_default() });
            }
        }

        Ok(links)
    }

    /// The number of outgoing edges from `source` that an execution flow would
    /// follow. Zero means the node reaches nothing a flow could trace: for a
    /// route, an `include()` mount prefix or a view the resolver never resolved.
    pub fn flow_edge_count(&self, source: &NodeId) -> Result<u32, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT COUNT(*) FROM edges WHERE source = ?1 AND kind IN
                 ('calls', 'routes_to', 'renders', 'resolves', 'handles',
                  'receives', 'instantiates', 'extends_template', 'includes_template')",
        )?;

        let count: i64 = statement.query_row(params![source.as_str()], |row| row.get(0))?;

        assert!(count >= 0, "a count is never negative");

        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// The nodes (and edge kinds) that reference the target node.
    pub fn callers(&self, target: &NodeId) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        self.edges_join(&CALLERS_SQL, target)
    }

    /// The nodes (and edge kinds) the source node references.
    pub fn callees(&self, source: &NodeId) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        self.edges_join(&CALLEES_SQL, source)
    }

    /// The target ids this node relates to through a *reverse* relation: the
    /// back-edges the reverse-relation synthesis pass adds (provenance
    /// `synthesis:reverse%`), the models that declare a ForeignKey/M2M *to* this
    /// one, reachable here as Django's reverse accessor. Lets `model` label a
    /// relation's direction (a forward FK/M2M this model declares vs. a reverse
    /// accessor onto it), which `callees` alone cannot tell apart, both being
    /// outgoing `relates_to` edges.
    pub fn reverse_relation_targets(&self, source: &NodeId) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT target FROM edges
             WHERE source = ?1 AND kind = 'relates_to' AND provenance LIKE 'synthesis:reverse%'",
        )?;

        let rows = statement.query_map(params![source.as_str()], |row| row.get::<_, String>(0))?;

        let mut targets: Vec<String> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "reverse-relation load")?;

            targets.push(row?);
        }

        Ok(targets)
    }

    /// The [`Store::callers`] result plus the 1-based source line of each
    /// reference edge (the call site), so a caller listing can quote the line of
    /// source where the reference happens. A 0 line means none was recorded.
    pub fn callers_located(
        &self,
        target: &NodeId,
    ) -> Result<Vec<(EdgeKind, Node, u32)>, StoreError> {
        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, e.kind, e.line FROM edges e JOIN nodes n ON e.source = n.id
             WHERE e.target = ?1",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![target.as_str()], |row| {
            Ok((node_row(row)?, row.get::<_, String>(20)?, row.get::<_, Option<u32>>(21)?))
        })?;

        let mut located: Vec<(EdgeKind, Node, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "caller load")?;

            let (raw, kind, line) = row?;

            if let (Some(node), Some(edge_kind)) =
                (node_from_row(raw), EdgeKind::from_str_label(&kind))
            {
                located.push((edge_kind, node, line.unwrap_or(0)));
            }
        }

        Ok(located)
    }

    /// The incoming references to the given nodes, flattened into one bulk read
    /// so a scoring pass over many candidates issues one query per chunk instead
    /// of one per candidate. Containment edges are included; the caller decides
    /// what to keep, so the coverage and fan-in predicates stay defined in one
    /// place rather than duplicated into SQL.
    pub fn incoming_refs(&self, node_ids: &[String]) -> Result<Vec<IncomingRef>, StoreError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut refs: Vec<IncomingRef> = Vec::new();
        let mut count: u32 = 0;

        for chunk in node_ids.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 1);

            let sql = format!(
                "SELECT e.target, e.source, e.kind, n.project_id, n.file_path, n.name
                 FROM edges e JOIN nodes n ON e.source = n.id
                 WHERE e.target IN ({placeholders})",
            );

            let mut statement = self.connection.prepare_cached(&sql)?;

            let rows = statement.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;

            for row in rows {
                charge(&mut count, ROWS_LOADED_MAX, "bulk incoming ref load")?;

                let (target_id, source_id, kind, source_project_id, source_file_path, source_name) =
                    row?;

                if let Some(kind) = EdgeKind::from_str_label(&kind) {
                    refs.push(IncomingRef {
                        kind,
                        source_file_path,
                        source_id,
                        source_name,
                        source_project_id,
                        target_id,
                    });
                }
            }
        }

        Ok(refs)
    }

    /// The outgoing references from the given nodes, the mirror of
    /// [`Store::incoming_refs`], so a filter over what a symbol calls, extends,
    /// relates to, or renders reads one query per chunk rather than one per
    /// candidate.
    pub fn outgoing_refs(&self, node_ids: &[String]) -> Result<Vec<OutgoingRef>, StoreError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut refs: Vec<OutgoingRef> = Vec::new();
        let mut count: u32 = 0;

        for chunk in node_ids.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 1);

            let sql = format!(
                "SELECT e.source, e.target, e.kind, n.name
                 FROM edges e JOIN nodes n ON e.target = n.id
                 WHERE e.source IN ({placeholders})",
            );

            let mut statement = self.connection.prepare_cached(&sql)?;

            let rows = statement.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;

            for row in rows {
                charge(&mut count, ROWS_LOADED_MAX, "bulk outgoing ref load")?;

                let (source_id, target_id, kind, target_name) = row?;

                if let Some(kind) = EdgeKind::from_str_label(&kind) {
                    refs.push(OutgoingRef { kind, source_id, target_id, target_name });
                }
            }
        }

        Ok(refs)
    }

    fn edges_join(
        &self,
        sql: &str,
        node_id: &NodeId,
    ) -> Result<Vec<(EdgeKind, Node)>, StoreError> {
        let mut statement = self.connection.prepare_cached(sql)?;

        let rows = statement.query_map(params![node_id.as_str()], |row| {
            Ok((node_row(row)?, row.get::<_, String>(20)?))
        })?;

        let mut edges: Vec<(EdgeKind, Node)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, ROWS_LOADED_MAX, "edge join")?;

            let (raw, kind) = row?;

            if let (Some(node), Some(edge_kind)) = (node_from_row(raw), EdgeKind::from_str_label(&kind)) {
                edges.push((edge_kind, node));
            }
        }

        Ok(edges)
    }
}
