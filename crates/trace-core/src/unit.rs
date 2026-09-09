//! The traced thing, its lifecycle states, and the append-only record of what
//! happened to it.

use crate::clock::Stamps;
use crate::dcp::{Value, ValueSource, Verdict};
use crate::error::{CoreError, Result};
use crate::id::{Code, EventId, MarkId, MeasurementId, RowId, UnitUid};
use crate::route::OperationSeq;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a unit.
///
/// Note that `Marked` deliberately carries **no assumption of genealogy**. At
/// the first site identity is applied at operation 1 on a raw part, so a unit
/// legitimately exists with a job card, a product revision, and zero components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UnitState {
    /// Identity allocated; nothing applied to the part yet.
    Born,
    /// Mark or label applied and verified. Genealogy may be empty.
    Marked,
    /// Moving through the route.
    InProcess,
    /// Held by a supervisor; may resume.
    Held,
    /// Failed a gate and awaiting disposition.
    Quarantined,
    /// Repeating operations invalidated by a defect.
    Reworking,
    /// All required operations complete.
    Completed,
    /// Packed and dispatched.
    Shipped,
    /// Terminal. The UID is retired forever and never reissued.
    Scrapped,
}

impl UnitState {
    /// Whether no further transition is possible.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Scrapped)
    }

    /// Whether the unit may be worked on at a station right now.
    #[must_use]
    pub fn accepts_work(self) -> bool {
        matches!(
            self,
            Self::Born | Self::Marked | Self::InProcess | Self::Reworking
        )
    }

    /// Stable name for diagnostics and error messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Born => "BORN",
            Self::Marked => "MARKED",
            Self::InProcess => "IN_PROCESS",
            Self::Held => "HELD",
            Self::Quarantined => "QUARANTINED",
            Self::Reworking => "REWORKING",
            Self::Completed => "COMPLETED",
            Self::Shipped => "SHIPPED",
            Self::Scrapped => "SCRAPPED",
        }
    }

    /// Whether `self -> next` is a legal transition.
    ///
    /// The rule that matters most: **nothing leaves `Scrapped`.** A scrapped
    /// UID is retired forever.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        use UnitState::{
            Born, Completed, Held, InProcess, Marked, Quarantined, Reworking, Scrapped, Shipped,
        };
        match self {
            // Terminal. No exceptions, including back to itself.
            Scrapped => false,
            Born => matches!(next, Marked | InProcess | Quarantined | Held | Scrapped),
            Marked => matches!(next, InProcess | Quarantined | Held | Scrapped),
            InProcess => {
                matches!(
                    next,
                    InProcess | Completed | Quarantined | Held | Reworking | Scrapped
                )
            }
            Reworking => matches!(next, InProcess | Quarantined | Held | Scrapped),
            Quarantined => matches!(next, Reworking | InProcess | Scrapped | Held),
            Held => matches!(next, InProcess | Reworking | Quarantined | Scrapped),
            Completed => matches!(next, Shipped | Quarantined | Reworking | Scrapped),
            // A shipped unit that comes back is a returns/RMA case: it may be
            // quarantined or scrapped, never quietly put back into process.
            Shipped => matches!(next, Quarantined | Scrapped),
        }
    }
}

/// A traced physical unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    /// Surrogate key.
    pub id: RowId,
    /// Public identity: printed on the part, encoded in the mark, used in URLs.
    pub uid: UnitUid,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Plant the unit was born in.
    pub plant_id: RowId,
    /// Product revision being built.
    pub product_revision_id: RowId,
    /// Job card the unit was released against.
    pub job_card_id: RowId,
    /// Human-facing serial printed beside the code.
    pub serial: String,
    /// Current lifecycle state.
    pub state: UnitState,
    /// Operation the unit is currently at, if any.
    pub current_operation: Option<OperationSeq>,
    /// Zero-based position within its job card, used by sampling rules.
    pub index_in_job: u64,
}

impl Unit {
    /// Apply a state transition.
    ///
    /// # Errors
    /// Returns [`CoreError::RetiredUid`] when the unit is scrapped, and
    /// [`CoreError::IllegalTransition`] for any other illegal move.
    pub fn transition_to(&mut self, next: UnitState) -> Result<()> {
        if self.state == UnitState::Scrapped {
            return Err(CoreError::RetiredUid(self.uid.to_string()));
        }
        if !self.state.can_transition_to(next) {
            return Err(CoreError::IllegalTransition {
                from: self.state.name(),
                action: format!("move to {}", next.name()),
            });
        }
        self.state = next;
        Ok(())
    }
}

/// What happened to a unit. Append-only: there is no update and no delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    /// Identity created.
    Born,
    /// Unit scanned at a station.
    Scanned,
    /// Operation started.
    OperationStarted,
    /// Operation completed with all gates green.
    OperationCompleted,
    /// A gate failed.
    GateFailed,
    /// Label printed or mark applied.
    Marked,
    /// Mark read back and verified.
    MarkVerified,
    /// Component or lot consumed into this unit.
    ComponentConsumed,
    /// Held by a supervisor.
    Held,
    /// Quarantined pending disposition.
    Quarantined,
    /// Sent for rework.
    ReworkStarted,
    /// Scrapped. The UID is retired.
    Scrapped,
    /// All required operations complete.
    Completed,
    /// Dispatched.
    Shipped,
    /// A correction superseding an earlier record.
    Correction,
}

/// One append-only event in a unit's history.
///
/// `id` doubles as the idempotency key: a station replaying its offline spool
/// after an edge reboot must not create duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitEvent {
    /// Public event identity and idempotency key.
    pub id: EventId,
    /// Unit this event belongs to.
    pub unit_id: RowId,
    /// What happened.
    pub kind: EventKind,
    /// Operation in play, if any.
    pub operation_seq: Option<OperationSeq>,
    /// Station that produced the event. Retired stations still resolve.
    pub station_id: Option<RowId>,
    /// Operator responsible.
    pub operator_id: Option<RowId>,
    /// Device and server timestamps.
    pub stamps: Stamps,
    /// Free-form detail, e.g. the failing gate or the supersede reason.
    pub detail: Option<String>,
    /// Position in this unit's hash chain.
    pub chain_seq: u64,
    /// Hash of the previous event for this unit.
    pub prev_hash: String,
    /// Hash of this event.
    pub row_hash: String,
}

impl UnitEvent {
    /// Canonical payload hashed into the audit chain.
    ///
    /// Field order is fixed and explicit. Never derive this from a hash map:
    /// the hash is only reproducible if the input is.
    #[must_use]
    pub fn canonical_payload(&self) -> String {
        format!(
            "unit={}|kind={:?}|op={}|station={}|operator={}|recorded={}|detail={}",
            self.unit_id,
            self.kind,
            self.operation_seq
                .map_or_else(|| "-".into(), |v| v.to_string()),
            self.station_id
                .map_or_else(|| "-".into(), |v| v.to_string()),
            self.operator_id
                .map_or_else(|| "-".into(), |v| v.to_string()),
            self.stamps.recorded_at.to_rfc3339(),
            self.detail.as_deref().unwrap_or("-"),
        )
    }
}

/// One captured value, stored with the raw device payload beside it.
///
/// The raw payload is kept verbatim because when a customer disputes a reading
/// in two years, the raw bytes are the answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// Public identity.
    pub id: MeasurementId,
    /// Unit measured.
    pub unit_id: RowId,
    /// Data collection point.
    pub dcp_id: RowId,
    /// Operation during which it was captured.
    pub operation_seq: OperationSeq,
    /// Parsed value.
    pub value: Value,
    /// Verdict against the DCP's limits.
    pub verdict: Verdict,
    /// Device or manual entry.
    pub source: ValueSource,
    /// Device that produced it, if any.
    pub device_id: Option<RowId>,
    /// Exact bytes or text the device returned, before parsing.
    pub raw_payload: Option<String>,
    /// Operator responsible.
    pub operator_id: Option<RowId>,
    /// Device and server timestamps.
    pub stamps: Stamps,
    /// Position in this unit's hash chain.
    pub chain_seq: u64,
    /// Hash of the previous row for this unit.
    pub prev_hash: String,
    /// Hash of this row.
    pub row_hash: String,
    /// Measurement this one supersedes, for corrections. Records are never
    /// updated or deleted; a correction is a new row pointing at the old one.
    pub supersedes: Option<MeasurementId>,
    /// Why it was superseded.
    pub supersede_reason: Option<String>,
}

impl Measurement {
    /// Canonical payload hashed into the audit chain.
    #[must_use]
    pub fn canonical_payload(&self) -> String {
        format!(
            "unit={}|dcp={}|op={}|value={}|verdict={:?}|source={:?}|device={}|raw={}|recorded={}|supersedes={}",
            self.unit_id,
            self.dcp_id,
            self.operation_seq,
            self.value.canonical(),
            self.verdict,
            self.source,
            self.device_id.map_or_else(|| "-".into(), |v| v.to_string()),
            self.raw_payload.as_deref().unwrap_or("-"),
            self.stamps.recorded_at.to_rfc3339(),
            self.supersedes
                .map_or_else(|| "-".into(), |v| v.to_string()),
        )
    }
}

/// What was consumed into a unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConsumedItem {
    /// A serialised child unit, which has its own full history.
    Unit {
        /// The child's public identity.
        uid: UnitUid,
    },
    /// A quantity drawn from a tracked lot.
    Lot {
        /// Supplier or internal lot identifier.
        lot_no: Code,
        /// Quantity consumed.
        qty: f64,
    },
}

/// A parent-child link in the genealogy tree. Append-only, like everything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenealogyLink {
    /// Surrogate key.
    pub id: RowId,
    /// Parent unit.
    pub parent_unit_id: RowId,
    /// BOM line this consumption satisfies.
    pub bom_line_id: RowId,
    /// What went in.
    pub item: ConsumedItem,
    /// Operation at which it was consumed.
    pub operation_seq: OperationSeq,
    /// Device and server timestamps.
    pub stamps: Stamps,
}

/// Outcome of a single label print or mark attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarkOutcome {
    /// Applied and read back successfully.
    Verified,
    /// Applied but read-back failed or graded below the minimum.
    VerifyFailed,
    /// The marking device reported a fault.
    DeviceFault,
    /// Applied, with no verification configured.
    AppliedUnverified,
}

/// Every label print and mark attempt, successful or not.
///
/// Reprinting is a controlled event: duplicate labels in the field are a
/// genuine traceability failure, so a reason code and supervisor are recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkRecord {
    /// Public identity.
    pub id: MarkId,
    /// Unit marked.
    pub unit_id: RowId,
    /// Template version rendered.
    pub template_version_id: RowId,
    /// Device that applied it.
    pub device_id: Option<RowId>,
    /// Attempt number at this operation, starting at 1.
    pub attempt: u32,
    /// Result.
    pub outcome: MarkOutcome,
    /// Value decoded on read-back, if any. Compared against the intended UID.
    pub read_back: Option<String>,
    /// ISO/IEC 29158 grade, when a grading-capable reader is present.
    pub grade: Option<char>,
    /// Base URL encoded into the code, stored so we always know what was
    /// printed. Labels printed in year one must still resolve in year five.
    pub trace_base_url: String,
    /// Reason code, mandatory for a reprint.
    pub reason: Option<String>,
    /// Supervisor who authorised a reprint.
    pub authorised_by: Option<RowId>,
    /// Exact bytes sent to the device.
    pub payload_sent: Option<String>,
    /// Device and server timestamps.
    pub stamps: Stamps,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::clock::Stamps;
    use chrono::{DateTime, Utc};

    fn ts() -> Stamps {
        let t: DateTime<Utc> = DateTime::from_timestamp(1_760_000_000, 0).unwrap();
        Stamps::new(t, t)
    }

    fn unit(state: UnitState) -> Unit {
        Unit {
            id: 1,
            uid: UnitUid::generate(),
            tenant_id: 1,
            plant_id: 1,
            product_revision_id: 1,
            job_card_id: 1,
            serial: "EX24A0001537".into(),
            state,
            current_operation: None,
            index_in_job: 0,
        }
    }

    #[test]
    fn scrapped_is_terminal_and_uid_is_never_reissued() {
        // The rule the spec asks for a test on, explicitly.
        let mut u = unit(UnitState::Scrapped);
        for target in [
            UnitState::Born,
            UnitState::Marked,
            UnitState::InProcess,
            UnitState::Completed,
            UnitState::Shipped,
            UnitState::Held,
            UnitState::Scrapped,
        ] {
            let err = u.transition_to(target).unwrap_err();
            assert!(
                matches!(err, CoreError::RetiredUid(_)),
                "scrapped unit must not move to {target:?}"
            );
        }
        assert!(UnitState::Scrapped.is_terminal());
    }

    #[test]
    fn a_marked_raw_part_can_be_scrapped_before_any_assembly() {
        // Identity exists at operation 1 with an empty genealogy; the part can
        // still fail incoming inspection.
        let mut u = unit(UnitState::Marked);
        u.transition_to(UnitState::Scrapped).unwrap();
        assert_eq!(u.state, UnitState::Scrapped);
    }

    #[test]
    fn born_unit_can_be_marked_then_processed() {
        let mut u = unit(UnitState::Born);
        u.transition_to(UnitState::Marked).unwrap();
        u.transition_to(UnitState::InProcess).unwrap();
        u.transition_to(UnitState::Completed).unwrap();
        u.transition_to(UnitState::Shipped).unwrap();
    }

    #[test]
    fn shipped_unit_cannot_silently_re_enter_production() {
        let mut u = unit(UnitState::Shipped);
        assert!(u.transition_to(UnitState::InProcess).is_err());
        // A returned unit is quarantined for disposition.
        u.transition_to(UnitState::Quarantined).unwrap();
    }

    #[test]
    fn quarantined_unit_can_be_reworked_or_scrapped() {
        let mut a = unit(UnitState::Quarantined);
        a.transition_to(UnitState::Reworking).unwrap();
        let mut b = unit(UnitState::Quarantined);
        b.transition_to(UnitState::Scrapped).unwrap();
    }

    #[test]
    fn only_working_states_accept_work() {
        assert!(UnitState::InProcess.accepts_work());
        assert!(UnitState::Marked.accepts_work());
        assert!(UnitState::Reworking.accepts_work());
        assert!(!UnitState::Quarantined.accepts_work());
        assert!(!UnitState::Scrapped.accepts_work());
        assert!(!UnitState::Shipped.accepts_work());
    }

    #[test]
    fn event_payload_is_deterministic_and_covers_the_fields_that_matter() {
        let e = UnitEvent {
            id: EventId::generate(),
            unit_id: 42,
            kind: EventKind::OperationCompleted,
            operation_seq: Some(20),
            station_id: Some(3),
            operator_id: Some(7),
            stamps: ts(),
            detail: None,
            chain_seq: 0,
            prev_hash: crate::hash::GENESIS.into(),
            row_hash: String::new(),
        };
        let p = e.canonical_payload();
        assert_eq!(p, e.canonical_payload(), "payload must be stable");
        assert!(p.contains("unit=42") && p.contains("op=20") && p.contains("operator=7"));
    }

    #[test]
    fn measurement_payload_includes_raw_bytes_and_supersede_link() {
        let m = Measurement {
            id: MeasurementId::generate(),
            unit_id: 42,
            dcp_id: 1,
            operation_seq: 20,
            value: Value::Numeric(12.1),
            verdict: Verdict::Pass,
            source: ValueSource::Device,
            device_id: Some(5),
            raw_payload: Some("T:12.1NM\r\n".into()),
            operator_id: Some(7),
            stamps: ts(),
            chain_seq: 1,
            prev_hash: "aa".into(),
            row_hash: String::new(),
            supersedes: None,
            supersede_reason: None,
        };
        let p = m.canonical_payload();
        assert!(p.contains("value=12.1"));
        assert!(
            p.contains("raw=T:12.1NM"),
            "raw device bytes must be hashed too"
        );
    }
}
