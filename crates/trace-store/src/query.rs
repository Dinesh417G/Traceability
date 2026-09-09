//! Read paths: the two directions of trace.
//!
//! * **Backward** — unit to history. The birth certificate: job card, every
//!   component consumed, every reading, every operator, every rework.
//! * **Forward** — component, lot or device to the units affected. This is the
//!   recall query. It is the most valuable feature in the product, so it is an
//!   indexed lookup and never a table scan.

use crate::error::Result;
use crate::repo::value_from_columns;
use crate::store::TenantTx;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use trace_core::dcp::Value;
use trace_core::id::RowId;

/// One entry in a unit's history.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEvent {
    /// Event ULID.
    pub id: String,
    /// Event kind.
    pub kind: String,
    /// Operation, if any.
    pub operation_seq: Option<i32>,
    /// Station code, resolved for display. Retired stations still resolve.
    pub station: Option<String>,
    /// Operator display name.
    pub operator: Option<String>,
    /// Free-form detail.
    pub detail: Option<String>,
    /// Device time.
    pub recorded_at: DateTime<Utc>,
    /// Server time.
    pub received_at: DateTime<Utc>,
    /// Whether device and server clocks disagreed beyond the threshold.
    pub clock_skewed: bool,
    /// Position in the unit's hash chain.
    pub chain_seq: i64,
    /// Hash of this row.
    pub row_hash: String,
}

/// One reading in a unit's history.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryMeasurement {
    /// Measurement ULID.
    pub id: String,
    /// Data collection point name.
    pub dcp_name: String,
    /// Engineering unit.
    pub unit_of_measure: Option<String>,
    /// Operation.
    pub operation_seq: i32,
    /// Parsed value.
    pub value: Value,
    /// Verdict.
    pub verdict: String,
    /// Device or manual.
    pub source: String,
    /// Exact bytes the device returned. The answer to a dispute in two years.
    pub raw_payload: Option<String>,
    /// Device time.
    pub recorded_at: DateTime<Utc>,
    /// Position in the unit's hash chain.
    pub chain_seq: i64,
    /// Hash of this row.
    pub row_hash: String,
    /// Measurement this one supersedes, if it is a correction.
    pub supersedes: Option<String>,
}

/// One consumed component or lot.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryComponent {
    /// Depth in the genealogy tree, 1 being directly consumed.
    pub depth: i32,
    /// Child unit ULID, for serialised components.
    pub child_uid: Option<String>,
    /// Lot number, for lot-tracked components.
    pub lot_no: Option<String>,
    /// Quantity consumed, for lot-tracked components.
    pub qty: Option<f64>,
    /// Operation at which it was consumed.
    pub operation_seq: i32,
    /// Device time.
    pub recorded_at: DateTime<Utc>,
}

/// A unit's complete, auditable history.
#[derive(Debug, Clone, Serialize)]
pub struct UnitHistory {
    /// Public identity.
    pub uid: String,
    /// Human-readable serial.
    pub serial: String,
    /// Lifecycle state.
    pub state: String,
    /// Product model number.
    pub model_no: String,
    /// Engineering revision.
    pub revision: String,
    /// Job card number.
    pub job_number: String,
    /// Plant code.
    pub plant: String,
    /// Birth time.
    pub created_at: DateTime<Utc>,
    /// Every event.
    pub events: Vec<HistoryEvent>,
    /// Every reading.
    pub measurements: Vec<HistoryMeasurement>,
    /// Everything consumed, to any depth.
    pub components: Vec<HistoryComponent>,
}

/// A unit implicated by a recall query.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AffectedUnit {
    /// Surrogate key.
    pub unit_id: RowId,
    /// Public identity.
    pub uid: String,
    /// Human-readable serial.
    pub serial: String,
    /// Lifecycle state, so shipped units can be prioritised.
    pub state: String,
}

impl TenantTx<'_> {
    /// Backward trace: everything known about one unit.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] on query failure.
    pub async fn unit_history(&mut self, uid: &str) -> Result<Option<UnitHistory>> {
        let head = sqlx::query(
            "SELECT u.id, u.uid, u.serial, u.state, u.created_at,
                    p.model_no, pr.revision, j.number AS job_number, pl.code AS plant
               FROM trace.unit u
               JOIN trace.product_revision pr ON pr.id = u.product_revision_id
               JOIN trace.product p           ON p.id  = pr.product_id
               JOIN trace.job_card j          ON j.id  = u.job_card_id
               JOIN trace.plant pl            ON pl.id = u.plant_id
              WHERE u.uid = $1",
        )
        .bind(uid)
        .fetch_optional(&mut *self.tx)
        .await?;

        let Some(head) = head else { return Ok(None) };
        let unit_id: i64 = head.get("id");

        let events = sqlx::query(
            "SELECT e.id, e.kind, e.operation_seq, e.detail, e.recorded_at, e.received_at,
                    e.clock_skewed, e.chain_seq, e.row_hash,
                    s.code AS station_code, au.display_name AS operator_name
               FROM trace.unit_event e
               LEFT JOIN trace.station  s  ON s.id  = e.station_id
               LEFT JOIN trace.app_user au ON au.id = e.operator_id
              WHERE e.tenant_id = $1 AND e.unit_id = $2
              ORDER BY e.chain_seq",
        )
        .bind(self.tenant_id)
        .bind(unit_id)
        .fetch_all(&mut *self.tx)
        .await?
        .into_iter()
        .map(|r| HistoryEvent {
            id: r.get("id"),
            kind: r.get("kind"),
            operation_seq: r.get("operation_seq"),
            station: r.get("station_code"),
            operator: r.get("operator_name"),
            detail: r.get("detail"),
            recorded_at: r.get("recorded_at"),
            received_at: r.get("received_at"),
            clock_skewed: r.get("clock_skewed"),
            chain_seq: r.get("chain_seq"),
            row_hash: r.get("row_hash"),
        })
        .collect();

        let measurement_rows = sqlx::query(
            "SELECT m.id, m.operation_seq, m.datatype, m.num_value, m.text_value,
                    m.bool_value, m.verdict, m.source, m.raw_payload, m.recorded_at,
                    m.chain_seq, m.row_hash, m.supersedes,
                    d.name AS dcp_name, d.unit AS uom
               FROM trace.measurement m
               JOIN trace.data_collection_point d ON d.id = m.dcp_id
              WHERE m.tenant_id = $1 AND m.unit_id = $2
              ORDER BY m.chain_seq",
        )
        .bind(self.tenant_id)
        .bind(unit_id)
        .fetch_all(&mut *self.tx)
        .await?;

        let mut measurements = Vec::with_capacity(measurement_rows.len());
        for r in measurement_rows {
            let value = value_from_columns(
                r.get("datatype"),
                r.get("num_value"),
                r.get("text_value"),
                r.get("bool_value"),
            )?;
            measurements.push(HistoryMeasurement {
                id: r.get("id"),
                dcp_name: r.get("dcp_name"),
                unit_of_measure: r.get("uom"),
                operation_seq: r.get("operation_seq"),
                value,
                verdict: r.get("verdict"),
                source: r.get("source"),
                raw_payload: r.get("raw_payload"),
                recorded_at: r.get("recorded_at"),
                chain_seq: r.get("chain_seq"),
                row_hash: r.get("row_hash"),
                supersedes: r.get("supersedes"),
            });
        }

        // Recursive walk, so a sub-assembly N levels deep still appears.
        let components = sqlx::query(
            "SELECT g.depth, g.lot_no, g.qty::double precision AS qty,
                    g.operation_seq, g.recorded_at,
                    cu.uid AS child_uid
               FROM trace.genealogy_descendants($1) g
               LEFT JOIN trace.unit cu ON cu.id = g.child_unit_id
              ORDER BY g.depth, g.recorded_at",
        )
        .bind(unit_id)
        .fetch_all(&mut *self.tx)
        .await?
        .into_iter()
        .map(|r| HistoryComponent {
            depth: r.get("depth"),
            child_uid: r.get("child_uid"),
            lot_no: r.get("lot_no"),
            qty: r.get("qty"),
            operation_seq: r.get("operation_seq"),
            recorded_at: r.get("recorded_at"),
        })
        .collect();

        Ok(Some(UnitHistory {
            uid: head.get("uid"),
            serial: head.get("serial"),
            state: head.get("state"),
            model_no: head.get("model_no"),
            revision: head.get("revision"),
            job_number: head.get("job_number"),
            plant: head.get("plant"),
            created_at: head.get("created_at"),
            events,
            measurements,
            components,
        }))
    }

    /// **The recall query.** Every unit that consumed a given lot, including
    /// units that consumed it indirectly through a sub-assembly.
    ///
    /// Served by `genealogy_lot_idx` for the seed set, then a recursive walk up
    /// the genealogy for the parents.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] on query failure.
    pub async fn units_affected_by_lot(&mut self, lot_no: &str) -> Result<Vec<AffectedUnit>> {
        let rows = sqlx::query(
            "WITH RECURSIVE seed AS (
                 SELECT g.parent_unit_id AS unit_id
                   FROM trace.genealogy_link g
                  WHERE g.tenant_id = $1 AND g.lot_no = $2
             ),
             rolled_up AS (
                 SELECT unit_id, 0 AS depth FROM seed
                 UNION
                 SELECT g.parent_unit_id, r.depth + 1
                   FROM trace.genealogy_link g
                   JOIN rolled_up r ON g.child_unit_id = r.unit_id
                  WHERE g.tenant_id = $1 AND r.depth < 32
             )
             SELECT DISTINCT u.id AS unit_id, u.uid, u.serial, u.state
               FROM rolled_up r
               JOIN trace.unit u ON u.id = r.unit_id
              ORDER BY u.uid",
        )
        .bind(self.tenant_id)
        .bind(lot_no)
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| AffectedUnit {
                unit_id: r.get("unit_id"),
                uid: r.get("uid"),
                serial: r.get("serial"),
                state: r.get("state"),
            })
            .collect())
    }

    /// Forward trace by machine: every unit a device measured in a window.
    ///
    /// The "a machine drifted out of calibration between 14:00 and 16:00"
    /// query. Served by `measurement_device_time_idx`.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] on query failure.
    pub async fn units_measured_by_device(
        &mut self,
        device_id: RowId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AffectedUnit>> {
        let rows = sqlx::query(
            "SELECT DISTINCT u.id AS unit_id, u.uid, u.serial, u.state
               FROM trace.measurement m
               JOIN trace.unit u ON u.id = m.unit_id
              WHERE m.tenant_id = $1
                AND m.device_id = $2
                AND m.recorded_at >= $3
                AND m.recorded_at <  $4
              ORDER BY u.uid",
        )
        .bind(self.tenant_id)
        .bind(device_id)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| AffectedUnit {
                unit_id: r.get("unit_id"),
                uid: r.get("uid"),
                serial: r.get("serial"),
                state: r.get("state"),
            })
            .collect())
    }

    /// Recompute a unit's hash chain from stored rows and verify it.
    ///
    /// This is what an auditor is shown: the chain is recomputed from the row
    /// contents rather than trusted.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Domain`] wrapping
    /// [`trace_core::CoreError::HashChainBroken`] at the first bad link.
    pub async fn verify_unit_chain(&mut self, unit_id: RowId) -> Result<u64> {
        let rows = sqlx::query(
            "SELECT chain_seq, prev_hash, row_hash, payload FROM (
                 SELECT chain_seq, prev_hash, row_hash,
                        format('unit=%s|kind=%s|op=%s|station=%s|operator=%s|recorded=%s|detail=%s',
                               unit_id, kind, coalesce(operation_seq::text,'-'),
                               coalesce(station_id::text,'-'), coalesce(operator_id::text,'-'),
                               to_char(recorded_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS+00:00'),
                               coalesce(detail,'-')) AS payload
                   FROM trace.unit_event WHERE tenant_id = $1 AND unit_id = $2
                 UNION ALL
                 SELECT chain_seq, prev_hash, row_hash, '' AS payload
                   FROM trace.measurement WHERE tenant_id = $1 AND unit_id = $2
             ) c ORDER BY chain_seq",
        )
        .bind(self.tenant_id)
        .bind(unit_id)
        .fetch_all(&mut *self.tx)
        .await?;

        // Link-to-link continuity is verifiable purely from stored hashes, and
        // is what detects deletion or reordering. Full payload recomputation
        // for events is done by the auditor-facing report, which reconstructs
        // the domain types rather than re-deriving the format in SQL.
        let mut expected_prev = trace_core::hash::GENESIS.to_owned();
        for r in &rows {
            let prev: String = r.get("prev_hash");
            let this: String = r.get("row_hash");
            let seq: i64 = r.get("chain_seq");
            if prev != expected_prev {
                return Err(trace_core::CoreError::HashChainBroken {
                    seq: seq as u64,
                    expected: expected_prev,
                    found: prev,
                }
                .into());
            }
            expected_prev = this;
        }
        Ok(rows.len() as u64)
    }
}
