//! Data collection points: *what must be captured* at an operation.
//!
//! Every limit in the product lives here, as a row, never as a constant in
//! code. A DCP declares a name, a datatype, limits, whether it is mandatory,
//! and where the value comes from.

use crate::error::{CoreError, Result};
use crate::id::RowId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Datatype of a captured value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DataType {
    /// Continuous numeric reading: torque, mass, voltage.
    Numeric,
    /// Free or coded text: a batch code, a firmware version.
    Text,
    /// Pass/fail or present/absent.
    Boolean,
    /// A scanned barcode payload.
    Barcode,
}

impl DataType {
    fn name(self) -> &'static str {
        match self {
            Self::Numeric => "NUMERIC",
            Self::Text => "TEXT",
            Self::Boolean => "BOOLEAN",
            Self::Barcode => "BARCODE",
        }
    }
}

/// Where a value came from.
///
/// Manual entry is **not second class**. The same DCP, the same limits, the
/// same audit fields — only the source differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ValueSource {
    /// Read from a device driver.
    Device,
    /// Typed or selected by an operator on the terminal.
    Manual,
}

/// A captured value, before or after limit evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Value {
    /// Numeric reading.
    Numeric(f64),
    /// Text value.
    Text(String),
    /// Boolean value.
    Boolean(bool),
    /// Barcode payload.
    Barcode(String),
}

impl Value {
    /// The datatype this value satisfies.
    #[must_use]
    pub fn datatype(&self) -> DataType {
        match self {
            Self::Numeric(_) => DataType::Numeric,
            Self::Text(_) => DataType::Text,
            Self::Boolean(_) => DataType::Boolean,
            Self::Barcode(_) => DataType::Barcode,
        }
    }

    /// Numeric payload, if this is a numeric value.
    #[must_use]
    pub fn as_numeric(&self) -> Option<f64> {
        match self {
            Self::Numeric(n) => Some(*n),
            _ => None,
        }
    }

    /// Canonical rendering used for the audit hash chain and for display.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            // `{:?}` on f64 round-trips exactly, which `{}` does not guarantee.
            Self::Numeric(n) => format!("{n:?}"),
            Self::Text(s) | Self::Barcode(s) => s.clone(),
            Self::Boolean(b) => b.to_string(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Acceptance limits for a numeric DCP.
///
/// All bounds are optional so a DCP can be one-sided (for example "at least
/// 10 Nm, no upper bound"). Bounds are **inclusive**.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Inclusive lower bound.
    pub min: Option<f64>,
    /// Inclusive upper bound.
    pub max: Option<f64>,
    /// Target value, for display and capability reporting. Not enforced.
    pub nominal: Option<f64>,
}

impl Limits {
    /// Whether a numeric reading is within bounds.
    #[must_use]
    pub fn accepts(&self, v: f64) -> bool {
        if v.is_nan() {
            // A NaN reading is a broken instrument, never a pass.
            return false;
        }
        self.min.is_none_or(|m| v >= m) && self.max.is_none_or(|m| v <= m)
    }

    /// Whether any bound is set at all.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    /// Whether the limits are self-consistent (`min <= max`).
    #[must_use]
    pub fn is_coherent(&self) -> bool {
        match (self.min, self.max) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        }
    }
}

/// How often a DCP is captured across a batch of units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SampleRule {
    /// Capture on every unit.
    Every,
    /// Capture on the first unit of a job card only (first-off inspection).
    FirstOff,
    /// Capture on every Nth unit.
    EveryNth(u32),
}

impl SampleRule {
    /// Whether the DCP must be captured for the given zero-based position of
    /// the unit within its job card.
    #[must_use]
    pub fn applies_to(&self, unit_index_in_job: u64) -> bool {
        match self {
            Self::Every => true,
            Self::FirstOff => unit_index_in_job == 0,
            // `EveryNth(0)` would be a divide by zero; treat it as every unit
            // rather than panicking on bad configuration mid-shift.
            Self::EveryNth(0) => true,
            Self::EveryNth(n) => unit_index_in_job.is_multiple_of(u64::from(*n)),
        }
    }
}

/// A single thing that must be captured at an operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataCollectionPoint {
    /// Surrogate key.
    pub id: RowId,
    /// Stable machine name, unique within the operation, e.g. `final_torque`.
    pub name: String,
    /// Operator-facing label.
    pub label: String,
    /// Engineering unit, e.g. `Nm`. Free text so we do not constrain customers.
    pub unit: Option<String>,
    /// Declared datatype.
    pub datatype: DataType,
    /// Acceptance limits. Only meaningful for [`DataType::Numeric`].
    pub limits: Limits,
    /// Capture frequency.
    pub sample_rule: SampleRule,
    /// Whether the operation may complete without this value.
    pub mandatory: bool,
    /// Device expected to supply it, if any. `None` implies manual entry.
    pub device_id: Option<RowId>,
}

/// Outcome of evaluating a value against its DCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// Within limits, or no limits apply.
    Pass,
    /// Outside limits.
    Fail,
}

impl Verdict {
    /// Whether this verdict is a pass.
    #[must_use]
    pub fn is_pass(self) -> bool {
        matches!(self, Self::Pass)
    }
}

impl DataCollectionPoint {
    /// Evaluate a captured value against this DCP.
    ///
    /// # Errors
    /// Returns [`CoreError::ValueTypeMismatch`] if the value's type does not
    /// match the declared datatype. A type mismatch is a configuration or
    /// driver bug, not a product defect, so it is an error rather than a fail
    /// verdict — it must not be recorded as the unit's fault.
    pub fn evaluate(&self, value: &Value) -> Result<Verdict> {
        if value.datatype() != self.datatype {
            return Err(CoreError::ValueTypeMismatch {
                value: value.canonical(),
                datatype: self.datatype.name(),
            });
        }
        Ok(match value {
            Value::Numeric(n) if self.limits.is_bounded() => {
                if self.limits.accepts(*n) {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                }
            }
            // Non-numeric values, and numerics with no limits configured, are
            // recorded rather than judged.
            _ => Verdict::Pass,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn torque_dcp() -> DataCollectionPoint {
        DataCollectionPoint {
            id: 1,
            name: "final_torque".into(),
            label: "Final torque".into(),
            unit: Some("Nm".into()),
            datatype: DataType::Numeric,
            limits: Limits {
                min: Some(10.0),
                max: Some(14.0),
                nominal: Some(12.0),
            },
            sample_rule: SampleRule::Every,
            mandatory: true,
            device_id: Some(5),
        }
    }

    #[test]
    fn value_inside_limits_passes() {
        let d = torque_dcp();
        assert_eq!(d.evaluate(&Value::Numeric(12.0)).unwrap(), Verdict::Pass);
    }

    #[test]
    fn limits_are_inclusive_at_both_bounds() {
        let d = torque_dcp();
        assert_eq!(d.evaluate(&Value::Numeric(10.0)).unwrap(), Verdict::Pass);
        assert_eq!(d.evaluate(&Value::Numeric(14.0)).unwrap(), Verdict::Pass);
    }

    #[test]
    fn value_outside_limits_fails() {
        let d = torque_dcp();
        assert_eq!(d.evaluate(&Value::Numeric(9.99)).unwrap(), Verdict::Fail);
        assert_eq!(d.evaluate(&Value::Numeric(14.01)).unwrap(), Verdict::Fail);
    }

    #[test]
    fn nan_never_passes() {
        // A NaN reading means a broken instrument, not a good part.
        let d = torque_dcp();
        assert_eq!(
            d.evaluate(&Value::Numeric(f64::NAN)).unwrap(),
            Verdict::Fail
        );
    }

    #[test]
    fn one_sided_limits_work() {
        let mut d = torque_dcp();
        d.limits = Limits {
            min: Some(10.0),
            max: None,
            nominal: None,
        };
        assert_eq!(d.evaluate(&Value::Numeric(1_000.0)).unwrap(), Verdict::Pass);
        assert_eq!(d.evaluate(&Value::Numeric(9.0)).unwrap(), Verdict::Fail);
    }

    #[test]
    fn unbounded_numeric_is_recorded_not_judged() {
        let mut d = torque_dcp();
        d.limits = Limits::default();
        assert_eq!(d.evaluate(&Value::Numeric(-500.0)).unwrap(), Verdict::Pass);
    }

    #[test]
    fn wrong_datatype_is_an_error_not_a_defect() {
        // A driver returning text where a number was declared is our bug.
        // It must never be recorded as the unit failing.
        let d = torque_dcp();
        let err = d.evaluate(&Value::Text("twelve".into())).unwrap_err();
        assert!(matches!(err, CoreError::ValueTypeMismatch { .. }));
    }

    #[test]
    fn incoherent_limits_are_detectable() {
        let bad = Limits {
            min: Some(14.0),
            max: Some(10.0),
            nominal: None,
        };
        assert!(!bad.is_coherent());
        assert!(torque_dcp().limits.is_coherent());
    }

    #[test]
    fn sample_rules_select_the_right_units() {
        assert!(SampleRule::Every.applies_to(7));
        assert!(SampleRule::FirstOff.applies_to(0));
        assert!(!SampleRule::FirstOff.applies_to(1));
        assert!(SampleRule::EveryNth(5).applies_to(10));
        assert!(!SampleRule::EveryNth(5).applies_to(11));
    }

    #[test]
    fn zero_nth_does_not_panic() {
        // Bad config must not divide by zero in the middle of a shift.
        assert!(SampleRule::EveryNth(0).applies_to(3));
    }

    #[test]
    fn numeric_canonical_form_round_trips() {
        assert_eq!(Value::Numeric(12.1).canonical(), "12.1");
        assert_eq!(Value::Numeric(0.1 + 0.2).canonical(), "0.30000000000000004");
    }
}
