//! Repositories: the write paths for traceability data.
//!
//! ## One hash chain per unit, spanning both evidence tables
//!
//! `unit_event` and `measurement` share a single per-unit `chain_seq`. Keeping
//! one chain across both is deliberately stronger than two parallel chains:
//! deleting a measurement would break the events that follow it, so evidence
//! cannot be removed from one table and left consistent in the other.
//!
//! Appends take a row lock on the unit, which serialises writers for that unit
//! and makes a duplicate `chain_seq` impossible even if two stations somehow
//! hold the same part at once.

use crate::error::{Result, StoreError};
use crate::store::TenantTx;
use chrono::{DateTime, Utc};
use sqlx::Row;
use trace_core::clock::Stamps;
use trace_core::dcp::{DataType, Value, ValueSource, Verdict};
use trace_core::hash::{self, ChainLink};
use trace_core::id::{EventId, MarkId, MeasurementId, RowId, UnitUid};
use trace_core::unit::{EventKind, MarkOutcome, UnitEvent, UnitState};

/// A unit as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    /// Surrogate key.
    pub id: RowId,
    /// Public ULID identity.
    pub uid: String,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Plant of birth.
    pub plant_id: RowId,
    /// Product revision.
    pub product_revision_id: RowId,
    /// Job card.
    pub job_card_id: RowId,
    /// Human-readable serial printed beside the code.
    pub serial: String,
    /// Lifecycle state.
    pub state: String,
    /// Current operation, if any.
    pub current_operation: Option<i32>,
    /// Position within the job card.
    pub index_in_job: i64,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Input for birthing a unit.
#[derive(Debug, Clone)]
pub struct NewUnit {
    /// Public identity, allocated locally so a station can birth a unit with
    /// no network and no server.
    pub uid: UnitUid,
    /// Plant.
    pub plant_id: RowId,
    /// Product revision.
    pub product_revision_id: RowId,
    /// Job card.
    pub job_card_id: RowId,
    /// Human-readable serial.
    pub serial: String,
    /// Position within the job card, for sampling rules.
    pub index_in_job: i64,
}

/// Input for an append-only unit event.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Unit the event belongs to.
    pub unit_id: RowId,
    /// Plant.
    pub plant_id: RowId,
    /// What happened.
    pub kind: EventKind,
    /// Operation in play.
    pub operation_seq: Option<i32>,
    /// Station.
    pub station_id: Option<RowId>,
    /// Operator.
    pub operator_id: Option<RowId>,
    /// Free-form detail, e.g. the failing gate.
    pub detail: Option<String>,
    /// Device and server timestamps.
    pub stamps: Stamps,
}

/// Input for an append-only measurement.
#[derive(Debug, Clone)]
pub struct NewMeasurement {
    /// Unit measured.
    pub unit_id: RowId,
    /// Plant.
    pub plant_id: RowId,
    /// Data collection point.
    pub dcp_id: RowId,
    /// Operation.
    pub operation_seq: i32,
    /// Station.
    pub station_id: Option<RowId>,
    /// Operator.
    pub operator_id: Option<RowId>,
    /// Device, if not manual.
    pub device_id: Option<RowId>,
    /// Parsed value.
    pub value: Value,
    /// Verdict against limits.
    pub verdict: Verdict,
    /// Device or manual.
    pub source: ValueSource,
    /// Exact bytes the device returned.
    pub raw_payload: Option<String>,
    /// Device and server timestamps.
    pub stamps: Stamps,
    /// Measurement this corrects, if any.
    pub supersedes: Option<MeasurementId>,
    /// Why it was corrected.
    pub supersede_reason: Option<String>,
}

fn state_name(s: UnitState) -> &'static str {
    s.name()
}

fn kind_name(k: EventKind) -> String {
    // Serialises as the SCREAMING_SNAKE_CASE discriminant used in the schema.
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{k:?}"))
}

impl TenantTx<'_> {
    // ---------------------------------------------------------------- units

    /// Birth a unit.
    ///
    /// # Errors
    /// Fails if the UID was previously retired by a scrap, which the database
    /// enforces with a trigger.
    pub async fn insert_unit(&mut self, new: &NewUnit) -> Result<RowId> {
        let tenant = self.tenant_id;
        let row = sqlx::query(
            "INSERT INTO trace.unit
                 (uid, tenant_id, plant_id, product_revision_id, job_card_id,
                  serial, state, index_in_job)
             VALUES ($1, $2, $3, $4, $5, $6, 'BORN', $7)
             RETURNING id",
        )
        .bind(new.uid.to_string())
        .bind(tenant)
        .bind(new.plant_id)
        .bind(new.product_revision_id)
        .bind(new.job_card_id)
        .bind(&new.serial)
        .bind(new.index_in_job)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    /// Look a unit up by its public ULID.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on query failure.
    pub async fn unit_by_uid(&mut self, uid: &str) -> Result<Option<UnitRow>> {
        let row = sqlx::query(
            "SELECT id, uid, tenant_id, plant_id, product_revision_id, job_card_id,
                    serial, state, current_operation, index_in_job, created_at
               FROM trace.unit WHERE uid = $1",
        )
        .bind(uid)
        .fetch_optional(&mut *self.tx)
        .await?;

        Ok(row.map(|r| UnitRow {
            id: r.get("id"),
            uid: r.get("uid"),
            tenant_id: r.get("tenant_id"),
            plant_id: r.get("plant_id"),
            product_revision_id: r.get("product_revision_id"),
            job_card_id: r.get("job_card_id"),
            serial: r.get("serial"),
            state: r.get("state"),
            current_operation: r.get("current_operation"),
            index_in_job: r.get("index_in_job"),
            created_at: r.get("created_at"),
        }))
    }

    /// Move a unit to a new state.
    ///
    /// Scrapping retires the UID forever, which a database trigger records and
    /// enforces.
    ///
    /// # Errors
    /// Fails if the unit is already scrapped, since that state is terminal.
    pub async fn set_unit_state(
        &mut self,
        unit_id: RowId,
        state: UnitState,
        current_operation: Option<i32>,
    ) -> Result<()> {
        sqlx::query("UPDATE trace.unit SET state = $2, current_operation = $3 WHERE id = $1")
            .bind(unit_id)
            .bind(state_name(state))
            .bind(current_operation)
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    // ----------------------------------------------------------- hash chain

    /// Lock the unit row, serialising appends for that unit.
    async fn lock_unit(&mut self, unit_id: RowId) -> Result<()> {
        let found = sqlx::query("SELECT id FROM trace.unit WHERE id = $1 FOR UPDATE")
            .bind(unit_id)
            .fetch_optional(&mut *self.tx)
            .await?;
        if found.is_none() {
            return Err(StoreError::NotFound {
                entity: "unit",
                key: unit_id.to_string(),
            });
        }
        Ok(())
    }

    /// The tail of a unit's hash chain across both evidence tables.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on query failure.
    pub async fn chain_tail(&mut self, unit_id: RowId) -> Result<Option<ChainLink>> {
        let row = sqlx::query(
            "SELECT chain_seq, prev_hash, row_hash FROM (
                 SELECT chain_seq, prev_hash, row_hash
                   FROM trace.unit_event WHERE tenant_id = $1 AND unit_id = $2
                 UNION ALL
                 SELECT chain_seq, prev_hash, row_hash
                   FROM trace.measurement WHERE tenant_id = $1 AND unit_id = $2
             ) c ORDER BY chain_seq DESC LIMIT 1",
        )
        .bind(self.tenant_id)
        .bind(unit_id)
        .fetch_optional(&mut *self.tx)
        .await?;

        Ok(row.map(|r| ChainLink {
            seq: r.get::<i64, _>("chain_seq") as u64,
            prev_hash: r.get("prev_hash"),
            row_hash: r.get("row_hash"),
        }))
    }

    // --------------------------------------------------------------- events

    /// Append a unit event, extending the unit's hash chain.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if the unit does not exist, or
    /// [`StoreError::Database`] on failure.
    pub async fn append_event(&mut self, new: &NewEvent) -> Result<ChainLink> {
        self.lock_unit(new.unit_id).await?;
        let tail = self.chain_tail(new.unit_id).await?;

        // Build the canonical payload with the domain type itself, so the
        // hashed representation can never drift from trace-core's definition.
        let id = EventId::generate();
        let probe = UnitEvent {
            id,
            unit_id: new.unit_id,
            kind: new.kind,
            operation_seq: new.operation_seq.map(|v| v as u32),
            station_id: new.station_id,
            operator_id: new.operator_id,
            stamps: new.stamps,
            detail: new.detail.clone(),
            chain_seq: 0,
            prev_hash: String::new(),
            row_hash: String::new(),
        };
        let link = hash::append(tail.as_ref(), &probe.canonical_payload());

        sqlx::query(
            "INSERT INTO trace.unit_event
                 (id, tenant_id, plant_id, unit_id, kind, operation_seq, station_id,
                  operator_id, detail, recorded_at, received_at, clock_skewed,
                  chain_seq, prev_hash, row_hash)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
        )
        .bind(id.to_string())
        .bind(self.tenant_id)
        .bind(new.plant_id)
        .bind(new.unit_id)
        .bind(kind_name(new.kind))
        .bind(new.operation_seq)
        .bind(new.station_id)
        .bind(new.operator_id)
        .bind(&new.detail)
        .bind(new.stamps.recorded_at)
        .bind(new.stamps.received_at)
        .bind(new.stamps.is_skewed_default())
        .bind(link.seq as i64)
        .bind(&link.prev_hash)
        .bind(&link.row_hash)
        .execute(&mut *self.tx)
        .await?;

        Ok(link)
    }

    // --------------------------------------------------------- measurements

    /// Append a measurement, extending the unit's hash chain.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if the unit does not exist, or
    /// [`StoreError::Database`] on failure.
    pub async fn append_measurement(&mut self, new: &NewMeasurement) -> Result<ChainLink> {
        self.lock_unit(new.unit_id).await?;
        let tail = self.chain_tail(new.unit_id).await?;

        let id = MeasurementId::generate();
        let probe = trace_core::unit::Measurement {
            id,
            unit_id: new.unit_id,
            dcp_id: new.dcp_id,
            operation_seq: new.operation_seq as u32,
            value: new.value.clone(),
            verdict: new.verdict,
            source: new.source,
            device_id: new.device_id,
            raw_payload: new.raw_payload.clone(),
            operator_id: new.operator_id,
            stamps: new.stamps,
            chain_seq: 0,
            prev_hash: String::new(),
            row_hash: String::new(),
            supersedes: new.supersedes,
            supersede_reason: new.supersede_reason.clone(),
        };
        let link = hash::append(tail.as_ref(), &probe.canonical_payload());

        let (datatype, num, text, boolean) = match &new.value {
            Value::Numeric(n) => ("NUMERIC", Some(*n), None, None),
            Value::Text(s) => ("TEXT", None, Some(s.clone()), None),
            Value::Boolean(b) => ("BOOLEAN", None, None, Some(*b)),
            Value::Barcode(s) => ("BARCODE", None, Some(s.clone()), None),
        };

        sqlx::query(
            "INSERT INTO trace.measurement
                 (id, tenant_id, plant_id, unit_id, dcp_id, operation_seq, station_id,
                  operator_id, device_id, datatype, num_value, text_value, bool_value,
                  verdict, source, raw_payload, recorded_at, received_at, clock_skewed,
                  chain_seq, prev_hash, row_hash, supersedes, supersede_reason)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,
                     $19,$20,$21,$22,$23,$24)",
        )
        .bind(id.to_string())
        .bind(self.tenant_id)
        .bind(new.plant_id)
        .bind(new.unit_id)
        .bind(new.dcp_id)
        .bind(new.operation_seq)
        .bind(new.station_id)
        .bind(new.operator_id)
        .bind(new.device_id)
        .bind(datatype)
        .bind(num)
        .bind(text)
        .bind(boolean)
        .bind(if new.verdict.is_pass() {
            "PASS"
        } else {
            "FAIL"
        })
        .bind(match new.source {
            ValueSource::Device => "DEVICE",
            ValueSource::Manual => "MANUAL",
        })
        .bind(&new.raw_payload)
        .bind(new.stamps.recorded_at)
        .bind(new.stamps.received_at)
        .bind(new.stamps.is_skewed_default())
        .bind(link.seq as i64)
        .bind(&link.prev_hash)
        .bind(&link.row_hash)
        .bind(new.supersedes.map(|v| v.to_string()))
        .bind(&new.supersede_reason)
        .execute(&mut *self.tx)
        .await?;

        Ok(link)
    }

    // ------------------------------------------------------------ genealogy

    /// Record that a serialised child unit was consumed into a parent.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on failure.
    pub async fn link_child_unit(
        &mut self,
        parent_unit_id: RowId,
        child_unit_id: RowId,
        bom_line_id: Option<RowId>,
        plant_id: RowId,
        operation_seq: i32,
        stamps: Stamps,
    ) -> Result<RowId> {
        let row = sqlx::query(
            "INSERT INTO trace.genealogy_link
                 (tenant_id, plant_id, parent_unit_id, bom_line_id, child_unit_id,
                  operation_seq, recorded_at, received_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
        )
        .bind(self.tenant_id)
        .bind(plant_id)
        .bind(parent_unit_id)
        .bind(bom_line_id)
        .bind(child_unit_id)
        .bind(operation_seq)
        .bind(stamps.recorded_at)
        .bind(stamps.received_at)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    /// Record that a quantity from a tracked lot was consumed into a unit.
    ///
    /// This is the row the recall query walks, so `lot_no` is a real indexed
    /// column rather than a field inside a JSON blob.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_lot(
        &mut self,
        parent_unit_id: RowId,
        lot_no: &str,
        qty: f64,
        bom_line_id: Option<RowId>,
        plant_id: RowId,
        operation_seq: i32,
        stamps: Stamps,
    ) -> Result<RowId> {
        let row = sqlx::query(
            "INSERT INTO trace.genealogy_link
                 (tenant_id, plant_id, parent_unit_id, bom_line_id, lot_no, qty,
                  operation_seq, recorded_at, received_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
        )
        .bind(self.tenant_id)
        .bind(plant_id)
        .bind(parent_unit_id)
        .bind(bom_line_id)
        .bind(lot_no)
        .bind(qty)
        .bind(operation_seq)
        .bind(stamps.recorded_at)
        .bind(stamps.received_at)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    // ----------------------------------------------------------- mark record

    /// Record a label print or mark attempt.
    ///
    /// A reprint requires a reason and an authoriser; the database refuses one
    /// without both, because uncontrolled duplicate labels in the field are a
    /// genuine traceability failure.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_mark(
        &mut self,
        unit_id: RowId,
        plant_id: RowId,
        template_version_id: Option<RowId>,
        device_id: Option<RowId>,
        operation_seq: Option<i32>,
        attempt: i32,
        outcome: MarkOutcome,
        read_back: Option<&str>,
        grade: Option<char>,
        trace_base_url: &str,
        is_reprint: bool,
        reason: Option<&str>,
        authorised_by: Option<RowId>,
        payload_sent: Option<&str>,
        stamps: Stamps,
    ) -> Result<MarkId> {
        let id = MarkId::generate();
        let outcome_name = match outcome {
            MarkOutcome::Verified => "VERIFIED",
            MarkOutcome::VerifyFailed => "VERIFY_FAILED",
            MarkOutcome::DeviceFault => "DEVICE_FAULT",
            MarkOutcome::AppliedUnverified => "APPLIED_UNVERIFIED",
        };
        sqlx::query(
            "INSERT INTO trace.mark_record
                 (id, tenant_id, plant_id, unit_id, template_version_id, device_id,
                  operation_seq, attempt, outcome, read_back, grade, trace_base_url,
                  is_reprint, reason, authorised_by, payload_sent, recorded_at, received_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)",
        )
        .bind(id.to_string())
        .bind(self.tenant_id)
        .bind(plant_id)
        .bind(unit_id)
        .bind(template_version_id)
        .bind(device_id)
        .bind(operation_seq)
        .bind(attempt)
        .bind(outcome_name)
        .bind(read_back)
        .bind(grade.map(|c| c.to_string()))
        .bind(trace_base_url)
        .bind(is_reprint)
        .bind(reason)
        .bind(authorised_by)
        .bind(payload_sent)
        .bind(stamps.recorded_at)
        .bind(stamps.received_at)
        .execute(&mut *self.tx)
        .await?;
        Ok(id)
    }

    // --------------------------------------------------------------- outbox

    /// Enqueue an event for the cloud phase.
    ///
    /// Nothing drains this yet. The table, the ULIDs and the idempotency keys
    /// exist from day one so enabling cloud sync later is a config change.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on failure.
    pub async fn enqueue_outbox(
        &mut self,
        plant_id: RowId,
        topic: &str,
        payload: &serde_json::Value,
    ) -> Result<String> {
        let id = trace_core::PublicId::generate().to_string();
        sqlx::query(
            "INSERT INTO trace.outbox (id, tenant_id, plant_id, topic, payload)
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(&id)
        .bind(self.tenant_id)
        .bind(plant_id)
        .bind(topic)
        .bind(payload)
        .execute(&mut *self.tx)
        .await?;
        Ok(id)
    }

    /// Count unpublished outbox rows.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] on failure.
    pub async fn outbox_pending(&mut self) -> Result<i64> {
        let row = sqlx::query(
            "SELECT count(*) AS n FROM trace.outbox
              WHERE tenant_id = $1 AND published_at IS NULL",
        )
        .bind(self.tenant_id)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(row.get::<i64, _>("n"))
    }
}

/// Parse a stored datatype/value pair back into a domain [`Value`].
///
/// # Errors
/// Returns [`StoreError::Decode`] when the stored columns do not agree with
/// the stored datatype, which would mean schema/code drift.
pub fn value_from_columns(
    datatype: &str,
    num: Option<f64>,
    text: Option<String>,
    boolean: Option<bool>,
) -> Result<Value> {
    let dt = match datatype {
        "NUMERIC" => DataType::Numeric,
        "TEXT" => DataType::Text,
        "BOOLEAN" => DataType::Boolean,
        "BARCODE" => DataType::Barcode,
        other => {
            return Err(StoreError::Decode {
                entity: "measurement",
                field: "datatype",
                detail: format!("unknown datatype {other}"),
            });
        }
    };
    let missing = |field: &'static str| StoreError::Decode {
        entity: "measurement",
        field,
        detail: format!("null column for datatype {datatype}"),
    };
    Ok(match dt {
        DataType::Numeric => Value::Numeric(num.ok_or_else(|| missing("num_value"))?),
        DataType::Text => Value::Text(text.ok_or_else(|| missing("text_value"))?),
        DataType::Boolean => Value::Boolean(boolean.ok_or_else(|| missing("bool_value"))?),
        DataType::Barcode => Value::Barcode(text.ok_or_else(|| missing("text_value"))?),
    })
}
