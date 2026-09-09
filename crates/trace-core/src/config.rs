//! Configuration entities: the hierarchy, products, BOMs and station topology.
//!
//! Everything here is data loaded from the database. The charter rule is that
//! if you are about to hardcode a process step, a limit, a label field, a
//! station name or a product family, it belongs in one of these structures
//! instead.
//!
//! The `tenant / plant / line / station / user` hierarchy deliberately uses the
//! same names, semantics and natural keys as ElectronIx MES, so a future merge
//! is a foreign-key join rather than a data reconciliation project.

use crate::id::{Code, RowId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A customer. Present from migration 001 even though v1 ships one factory,
/// because retrofitting tenancy into millions of measurement rows is not
/// something we are going to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    /// Surrogate key.
    pub id: RowId,
    /// Natural key, aligned with MES.
    pub code: Code,
    /// Display name.
    pub name: String,
}

/// A factory site. One edge box serves one plant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plant {
    /// Surrogate key.
    pub id: RowId,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Natural key, unique within the tenant.
    pub code: Code,
    /// Display name.
    pub name: String,
    /// Base URL printed into every QR on this plant's labels.
    ///
    /// Per-plant configuration, never a constant: on-premise this is the edge
    /// box's LAN name, e.g. `http://trace.plant.local`.
    pub trace_base_url: String,
}

/// A production line within a plant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// Surrogate key.
    pub id: RowId,
    /// Owning plant.
    pub plant_id: RowId,
    /// Natural key, unique within the plant.
    pub code: Code,
    /// Display name.
    pub name: String,
}

/// A physical operator terminal.
///
/// A station is assigned a **set** of operations. It is never bound to exactly
/// one, because real lines vary: a small line may run an entire route on a
/// single terminal, and a busy line may duplicate one operation across several
/// stations for capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Station {
    /// Surrogate key.
    pub id: RowId,
    /// Owning line.
    pub line_id: RowId,
    /// Natural key, unique within the line.
    pub code: Code,
    /// Display name.
    pub name: String,
    /// Operation templates this station is equipped to perform.
    pub operations: BTreeSet<RowId>,
    /// Retired stations stop accepting new work but their historical events
    /// must still resolve, so they are never deleted.
    pub active: bool,
}

impl Station {
    /// Whether this station is equipped for a given operation template.
    #[must_use]
    pub fn can_perform(&self, operation_def_id: RowId) -> bool {
        self.operations.contains(&operation_def_id)
    }
}

/// A product model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Product {
    /// Surrogate key.
    pub id: RowId,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Natural key: the model number, aligned with MES.
    pub model_no: Code,
    /// Display name.
    pub name: String,
}

/// How identity is applied to a part.
///
/// Per **product revision**, not per plant: the first site marks aluminium
/// castings and plastics on the same line, and those do not mark the same way.
/// "Do not laser it, apply a label at operation 1 instead" is a legitimate
/// engineering answer, not a workaround.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarkMethod {
    /// Laser direct part marking. Currently deferred — see
    /// `docs/LASER-DPM-DEFERRED.md`.
    LaserDpm,
    /// Printed label applied to the part.
    Label,
    /// Both a laser mark and a label.
    Both,
    /// No physical mark; identity is carried on a travelling document.
    None,
}

/// Substrate being marked. Aluminium and plastic are genuinely different
/// problems and must not collapse into one setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Material {
    /// Aluminium casting or machined aluminium.
    Aluminium,
    /// Polymer housing or moulding.
    Plastic,
    /// Steel or stainless.
    Steel,
    /// Printed circuit board assembly.
    Pcb,
    /// Anything else; treat conservatively.
    Other,
}

/// An engineering revision of a product. The unit of configuration: routes,
/// BOMs and mark strategy all hang off a revision, so a change is auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductRevision {
    /// Surrogate key.
    pub id: RowId,
    /// Owning product.
    pub product_id: RowId,
    /// Engineering revision, e.g. `A`, `B`, `02`.
    pub revision: Code,
    /// Substrate, which drives what marking is realistic.
    pub material: Material,
    /// How identity is applied for this revision.
    pub mark_method: MarkMethod,
    /// Minimum acceptable mark grade (ISO/IEC 29158 letter, `A`..`F`) when a
    /// grading-capable reader is present. `None` means the bar is a successful
    /// read-back. Legitimately differs between the aluminium and plastic parts.
    pub min_mark_grade: Option<char>,
    /// Whether this revision may still be started.
    pub active: bool,
}

/// How a BOM line is tracked through production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrackingMode {
    /// One row per physical unit, unique UID. Scanned individually.
    Serialised,
    /// Tracked by lot identifier plus quantity consumed: consumables,
    /// adhesives, grease, fasteners, PCBs by reel.
    LotTracked,
    /// Consumed but not traced.
    NonTracked,
}

/// A revision-controlled bill of materials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bom {
    /// Surrogate key.
    pub id: RowId,
    /// Product revision this BOM belongs to.
    pub product_revision_id: RowId,
    /// BOM revision, independent of the engineering revision.
    pub revision: Code,
    /// Lines making up the assembly.
    pub lines: Vec<BomLine>,
}

impl Bom {
    /// Find the line a scanned part number belongs to.
    #[must_use]
    pub fn line_for_part(&self, part_no: &str) -> Option<&BomLine> {
        self.lines.iter().find(|l| l.part_no.as_str() == part_no)
    }
}

/// One component position in a BOM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BomLine {
    /// Surrogate key.
    pub id: RowId,
    /// Position reference, e.g. `10`, `R12`.
    pub position: String,
    /// Component part number. Natural key for component verification.
    pub part_no: Code,
    /// Quantity required per parent unit.
    pub qty: f64,
    /// How this component is traced.
    pub tracking: TrackingMode,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn station_with(ops: &[RowId]) -> Station {
        Station {
            id: 1,
            line_id: 1,
            code: Code::new("ST-01").unwrap(),
            name: "Station 1".into(),
            operations: ops.iter().copied().collect(),
            active: true,
        }
    }

    #[test]
    fn one_station_can_serve_the_whole_route() {
        // A small line runs every operation on a single terminal.
        let s = station_with(&[1, 2, 3, 4, 5]);
        for op in 1..=5 {
            assert!(s.can_perform(op), "station must serve operation {op}");
        }
    }

    #[test]
    fn station_rejects_operations_it_is_not_equipped_for() {
        let s = station_with(&[1, 2]);
        assert!(!s.can_perform(9));
    }

    #[test]
    fn one_operation_can_live_on_many_stations() {
        // Parallel capacity: whichever station is free takes the unit.
        let a = station_with(&[7]);
        let b = station_with(&[7]);
        assert!(a.can_perform(7) && b.can_perform(7));
    }

    #[test]
    fn bom_lookup_by_part_number() {
        let bom = Bom {
            id: 1,
            product_revision_id: 1,
            revision: Code::new("A").unwrap(),
            lines: vec![BomLine {
                id: 1,
                position: "10".into(),
                part_no: Code::new("SCR-M6-20").unwrap(),
                qty: 4.0,
                tracking: TrackingMode::LotTracked,
            }],
        };
        assert!(bom.line_for_part("SCR-M6-20").is_some());
        assert!(bom.line_for_part("WRONG-PART").is_none());
    }

    #[test]
    fn mark_method_serialises_as_stable_screaming_snake() {
        // The database stores these as text; the spelling is a contract.
        let j = serde_json::to_string(&MarkMethod::LaserDpm).unwrap();
        assert_eq!(j, "\"LASER_DPM\"");
        assert_eq!(
            serde_json::to_string(&TrackingMode::LotTracked).unwrap(),
            "\"LOT_TRACKED\""
        );
    }
}

/// An operator, authenticated at the station by badge scan or PIN.
///
/// Login is deliberately local: a station must be able to authenticate an
/// operator with the network, the server and the internet all down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operator {
    /// Surrogate key.
    pub id: RowId,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Natural key, aligned with MES.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// Skill certifications currently held.
    pub certifications: BTreeSet<RowId>,
    /// Whether the account may log in.
    pub active: bool,
}

impl Operator {
    /// Whether this operator holds a given certification.
    #[must_use]
    pub fn is_certified_for(&self, skill_id: RowId) -> bool {
        self.certifications.contains(&skill_id)
    }
}
