//! Precomputed Django execution flows: an entry point, the path it
//! reaches, and which nodes take part in it.

use constellation_graph::{Node, NodeId, ProjectId};
use rusqlite::{Connection, params};
use rustc_hash::FxHashMap;

use crate::error::{StoreError, charge};
use crate::limits::{
    BULK_PARAMS_MAX, FLOW_PARTICIPATION_ROWS_MAX, PREALLOC_ROWS_MAX, ROWS_LOADED_MAX,
    ROWS_PER_FILE_MAX,
};
use crate::mapping::{count, node_from_row, node_row};
use crate::rows::{FlowMember, FlowRecord, FlowRow, FlowSort};
use crate::sql::{FLOW_COLUMNS, NODE_COLUMNS_PREFIXED, flow_columns, placeholder_list};
use crate::store::Store;
use crate::time::now_ms;

/// A [`FlowRow`] read off a result row carrying [`FLOW_COLUMNS`] in order.
fn flow_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowRow> {
    Ok(FlowRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        entry_node_id: row.get(3)?,
        entry_kind: row.get(4)?,
        depth_max: u32::try_from(row.get::<_, i64>(5)?).unwrap_or(u32::MAX),
        node_count: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(u32::MAX),
        file_count: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(u32::MAX),
        app_count: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(u32::MAX),
        project_count: u32::try_from(row.get::<_, i64>(9)?).unwrap_or(u32::MAX),
        criticality: row.get(10)?,
        truncated: row.get::<_, i64>(11)? != 0,
    })
}

/// The flow rows materialized from a query, bounded by the limit the query
/// already applied.
fn collect_flows<I>(rows: I, limit: u32) -> Result<Vec<FlowRow>, StoreError>
where
    I: Iterator<Item = rusqlite::Result<FlowRow>>,
{
    let mut flows: Vec<FlowRow> = Vec::with_capacity((limit as usize).min(PREALLOC_ROWS_MAX));
    let mut count: u32 = 0;

    for row in rows {
        charge(&mut count, limit, "flow load")?;

        flows.push(row?);
    }

    Ok(flows)
}

/// The flow rows and their membership rows written on the given connection or
/// transaction. The caller owns the transaction boundary, so the delete that
/// precedes a wholesale replace and these inserts commit together.
fn insert_flows(
    connection: &Connection,
    project: &ProjectId,
    flows: &[FlowRecord],
    computed_at: i64,
) -> Result<u32, StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO flow
             (project_id, name, entry_node_id, entry_kind, depth_max, node_count,
              file_count, app_count, project_count, criticality, truncated, reach_json,
              computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;

    let mut stored: u32 = 0;

    for flow in flows {
        assert!(stored < u32::MAX, "flow count must not overflow");
        assert!(!flow.name.is_empty(), "a flow name must not be empty");

        statement.execute(
            params![
                project.as_str(),
                flow.name,
                flow.entry_node_id,
                flow.entry_kind,
                flow.depth_max,
                u32::try_from(flow.members.len()).unwrap_or(u32::MAX),
                flow.file_count,
                flow.app_count,
                flow.project_count,
                flow.criticality,
                i64::from(flow.truncated),
                reach_json(&flow.members),
                computed_at,
            ],
        )?;

        let flow_id = connection.last_insert_rowid();

        insert_flow_members(connection, flow_id, &flow.members)?;

        stored += 1;
    }

    Ok(stored)
}

/// A flow's membership rows written against its freshly inserted id.
fn insert_flow_members(
    connection: &Connection,
    flow_id: i64,
    members: &[FlowMember],
) -> Result<(), StoreError> {
    let mut statement = connection.prepare_cached(
        "INSERT OR REPLACE INTO flow_membership (flow_id, node_id, depth) VALUES (?1, ?2, ?3)",
    )?;

    let mut written: u32 = 0;

    for member in members {
        charge(&mut written, ROWS_PER_FILE_MAX, "flow membership insert")?;

        statement.execute(params![flow_id, member.node_id, member.depth])?;
    }

    Ok(())
}

/// The denormalized reach set as a compact JSON array of `[node id, depth]`
/// pairs, kept alongside the relational membership rows so one read can recover
/// a whole flow without a join.
fn reach_json(members: &[FlowMember]) -> String {
    let pairs: Vec<serde_json::Value> = members
        .iter()
        .map(|member| serde_json::json!([member.node_id, member.depth]))
        .collect();

    serde_json::Value::Array(pairs).to_string()
}

impl Store {
    /// The precomputed flows of one project, replacing any previously stored for
    /// it. The delete and the inserts share one transaction, so a crash cannot
    /// orphan `flow_membership` rows against a half-written `flow` table.
    /// Returns the number of flows stored.
    pub fn replace_flows(
        &self,
        project: &ProjectId,
        flows: &[FlowRecord],
    ) -> Result<u32, StoreError> {
        assert!(!project.as_str().is_empty(), "project id must not be empty");

        let transaction = self.connection.unchecked_transaction()?;

        transaction.execute("DELETE FROM flow WHERE project_id = ?1", params![project.as_str()])?;

        let stored = insert_flows(&transaction, project, flows, now_ms())?;

        transaction.commit()?;

        assert!(stored as usize <= flows.len(), "no flow is stored twice");

        Ok(stored)
    }

    /// The flows named by `stale_ids` deleted and `flows` inserted in their
    /// place, in one transaction. Returns the number of flows stored.
    ///
    /// The incremental counterpart to [`Store::replace_flows`], and atomic for
    /// the same reason it is: an interrupted retrace must not leave the project
    /// missing the flows it deleted but never got to rewrite. Two transactions
    /// would make a crash in the gap look exactly like an entry point that
    /// genuinely stopped reaching anything, which is a wrong answer rather than
    /// an absent one.
    pub fn replace_flow_subset(
        &self,
        project: &ProjectId,
        stale_ids: &[i64],
        flows: &[FlowRecord],
    ) -> Result<u32, StoreError> {
        assert!(!project.as_str().is_empty(), "project id must not be empty");

        let transaction = self.connection.unchecked_transaction()?;
        let mut removed: u32 = 0;

        for chunk in stale_ids.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 1);
            let sql = format!("DELETE FROM flow WHERE id IN ({placeholders})");

            let affected = transaction.execute(&sql, rusqlite::params_from_iter(chunk))?;

            removed = removed.saturating_add(u32::try_from(affected).unwrap_or(u32::MAX));
        }

        let stored = insert_flows(&transaction, project, flows, now_ms())?;

        transaction.commit()?;

        assert!(removed as usize <= stale_ids.len(), "no flow is deleted twice");
        assert!(stored as usize <= flows.len(), "no flow is stored twice");

        Ok(stored)
    }

    /// The flows of `project` whose reach set touches any of `file_paths`: the
    /// flows a change to those files could have altered, and therefore the ones
    /// an incremental retrace must recompute.
    pub fn flows_touching_files(
        &self,
        project: &ProjectId,
        file_paths: &[String],
    ) -> Result<Vec<FlowRow>, StoreError> {
        if file_paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut found: FxHashMap<i64, FlowRow> = FxHashMap::default();
        let mut count: u32 = 0;

        for chunk in file_paths.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 2);

            let sql = format!(
                "SELECT DISTINCT {} FROM flow f
                 JOIN flow_membership m ON m.flow_id = f.id
                 JOIN nodes n ON n.id = m.node_id
                 WHERE f.project_id = ?1 AND n.file_path IN ({placeholders})",
                flow_columns("f"),
            );

            let mut values: Vec<&str> = Vec::with_capacity(chunk.len() + 1);
            values.push(project.as_str());
            values.extend(chunk.iter().map(String::as_str));

            let mut statement = self.connection.prepare_cached(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(values), flow_row)?;

            for row in rows {
                charge(&mut count, ROWS_LOADED_MAX, "flow load")?;

                let flow = row?;

                found.insert(flow.id, flow);
            }
        }

        Ok(found.into_values().collect())
    }

    /// The flows of one project (or of every project when `project` is `None`),
    /// ordered by `sort` and bounded by `limit`.
    pub fn flows(
        &self,
        project: Option<&ProjectId>,
        sort: FlowSort,
        limit: u32,
    ) -> Result<Vec<FlowRow>, StoreError> {
        assert!(limit > 0, "a flow listing limit is positive");

        let project_filter = project.map(|project| project.as_str().to_string());

        let sql = format!(
            "SELECT {FLOW_COLUMNS} FROM flow
             WHERE (?1 IS NULL OR project_id = ?1)
             ORDER BY {} LIMIT ?2",
            sort.order_by(),
        );

        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement.query_map(params![project_filter, limit], flow_row)?;

        collect_flows(rows, limit)
    }

    /// The flows whose reach set contains any of `node_ids`, highest criticality
    /// first. The query behind "which user-facing flows does my diff touch".
    pub fn flows_for_nodes(
        &self,
        node_ids: &[String],
        limit: u32,
    ) -> Result<Vec<FlowRow>, StoreError> {
        assert!(limit > 0, "a flow listing limit is positive");

        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut found: FxHashMap<i64, FlowRow> = FxHashMap::default();
        let mut count: u32 = 0;

        for chunk in node_ids.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 1);

            let sql = format!(
                "SELECT DISTINCT {} FROM flow f
                 JOIN flow_membership m ON m.flow_id = f.id
                 WHERE m.node_id IN ({placeholders})",
                flow_columns("f"),
            );

            let mut statement = self.connection.prepare_cached(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(chunk), flow_row)?;

            for row in rows {
                charge(&mut count, ROWS_LOADED_MAX, "flow load")?;

                let flow = row?;

                found.insert(flow.id, flow);
            }
        }

        let mut flows: Vec<FlowRow> = found.into_values().collect();

        flows.sort_by(|left, right| {
            right.criticality.total_cmp(&left.criticality).then(left.name.cmp(&right.name))
        });

        flows.truncate(limit as usize);

        Ok(flows)
    }

    /// The reach-set members of one flow, as `(node, depth)` pairs ordered by
    /// depth then location, bounded by `limit`.
    pub fn flow_members(&self, flow_id: i64, limit: u32) -> Result<Vec<(Node, u32)>, StoreError> {
        assert!(limit > 0, "a member listing limit is positive");

        let sql = format!(
            "SELECT {NODE_COLUMNS_PREFIXED}, m.depth FROM flow_membership m
             JOIN nodes n ON n.id = m.node_id
             WHERE m.flow_id = ?1
             ORDER BY m.depth, n.file_path, n.start_line
             LIMIT ?2",
        );

        let mut statement = self.connection.prepare_cached(&sql)?;

        let rows = statement.query_map(params![flow_id, limit], |row| {
            Ok((node_row(row)?, row.get::<_, i64>(20)?))
        })?;

        let mut members: Vec<(Node, u32)> = Vec::new();
        let mut count: u32 = 0;

        for row in rows {
            charge(&mut count, limit, "member load")?;

            let (raw, depth) = row?;

            if let Some(node) = node_from_row(raw) {
                members.push((node, u32::try_from(depth).unwrap_or(u32::MAX)));
            }
        }

        Ok(members)
    }

    /// The summed criticality of the flows one node participates in, with the
    /// name of the most critical among them. `(0.0, None)` when the node is in
    /// no flow, or when no flows have been computed at all.
    pub fn flow_participation(&self, node: &NodeId) -> Result<(f64, Option<String>), StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT f.criticality, f.name FROM flow_membership m
             JOIN flow f ON f.id = m.flow_id
             WHERE m.node_id = ?1
             ORDER BY f.criticality DESC, f.name
             LIMIT ?2",
        )?;

        let rows = statement.query_map(params![node.as_str(), FLOW_PARTICIPATION_ROWS_MAX], |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut total = 0.0;
        let mut top: Option<String> = None;
        let mut count: u32 = 0;

        for row in rows {
            let (criticality, name) = row?;

            charge(&mut count, FLOW_PARTICIPATION_ROWS_MAX, "participation")?;

            total += criticality;

            if top.is_none() {
                top = Some(name);
            }
        }

        assert!(total >= 0.0, "summed criticality is non-negative");

        Ok((total, top))
    }

    /// The number of flows computed for `project`, zero until
    /// [`Store::replace_flows`] has run for it.
    pub fn count_flows(&self, project: &ProjectId) -> Result<u32, StoreError> {
        count(&self.connection, "SELECT COUNT(*) FROM flow WHERE project_id = ?1", project)
    }

    /// The flows each of the given nodes participates in, as
    /// `(node id, flow name, criticality)` triples. One bulk read behind a
    /// flow-membership filter and a criticality ranking.
    pub fn flow_membership_for(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String, f64)>, StoreError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut memberships: Vec<(String, String, f64)> = Vec::new();
        let mut count: u32 = 0;

        for chunk in node_ids.chunks(BULK_PARAMS_MAX) {
            let placeholders = placeholder_list(chunk.len(), 1);

            let sql = format!(
                "SELECT m.node_id, f.name, f.criticality
                 FROM flow_membership m JOIN flow f ON f.id = m.flow_id
                 WHERE m.node_id IN ({placeholders})",
            );

            let mut statement = self.connection.prepare_cached(&sql)?;

            let rows = statement.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
            })?;

            for row in rows {
                charge(&mut count, ROWS_LOADED_MAX, "membership load")?;

                memberships.push(row?);
            }
        }

        Ok(memberships)
    }
}
