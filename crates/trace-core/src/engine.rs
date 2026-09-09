//! The route engine: a data-driven state machine over a validated route DAG.
//!
//! Every gate is a **pure function** of an [`OperationContext`]. Nothing here
//! performs I/O, which is what lets the whole route be exercised in tests and
//! in `route-sim` with no plant, no PLC and no database.
//!
//! The engine answers two questions:
//!
//! 1. *What can this station do with the unit in front of it?*
//!    — [`RouteEngine::resolve_operation`]
//! 2. *May this unit advance, and if not, where does it go?*
//!    — [`RouteEngine::try_advance`]

use crate::config::{MarkMethod, Operator, ProductRevision, Station};
use crate::dcp::{DataCollectionPoint, Value, Verdict};
use crate::error::{CoreError, Result};
use crate::id::{Code, RowId, UnitUid};
use crate::route::{FailurePath, Gate, OperationDef, OperationSeq, Route, RouteOperation};
use crate::unit::{Unit, UnitState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A component the operator scanned at this operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentScan {
    /// Part number decoded from the barcode.
    pub part_no: Code,
    /// BOM line this scan was matched to, if it matched one at all.
    pub bom_line_id: Option<RowId>,
    /// Raw scanned payload, kept verbatim.
    pub raw: String,
}

/// A value captured at this operation, before it is persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedValue {
    /// Data collection point captured.
    pub dcp_id: RowId,
    /// Parsed value.
    pub value: Value,
    /// Verdict against the DCP's limits.
    pub verdict: Verdict,
}

/// Result of reading a mark back after applying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkVerification {
    /// Value decoded from the mark.
    pub decoded: String,
    /// ISO/IEC 29158 grade, when a grading-capable reader is present.
    pub grade: Option<char>,
}

/// Everything a gate is allowed to look at.
///
/// Assembled by the caller from what has actually been gathered at the station.
/// Gates never fetch anything themselves.
#[derive(Debug)]
pub struct OperationContext<'a> {
    /// Unit in front of the station.
    pub unit: &'a Unit,
    /// Product revision being built, which decides mark strategy.
    pub revision: &'a ProductRevision,
    /// Route being executed.
    pub route: &'a Route,
    /// Operation being attempted.
    pub operation: &'a RouteOperation,
    /// Template behind that operation.
    pub operation_def: &'a OperationDef,
    /// Station attempting it.
    pub station: &'a Station,
    /// Logged-in operator, if any.
    pub operator: Option<&'a Operator>,
    /// Operations already completed for this unit.
    pub completed: &'a BTreeSet<OperationSeq>,
    /// UID scanned or read back at this station.
    pub scanned_identity: Option<UnitUid>,
    /// Components scanned at this operation.
    pub component_scans: &'a [ComponentScan],
    /// Values captured at this operation.
    pub captured: &'a [CapturedValue],
    /// Data collection points defined for this operation.
    pub dcps: &'a [DataCollectionPoint],
    /// Test verdicts available, keyed by test name.
    pub test_verdicts: &'a BTreeMap<String, Verdict>,
    /// Mark read-back result, if the unit was marked here.
    pub mark_verification: Option<&'a MarkVerification>,
    /// Attempt number at this operation, starting at 1.
    pub attempt: u32,
}

/// Outcome of one gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    /// Which gate this was.
    pub gate: String,
    /// Whether it passed.
    pub passed: bool,
    /// Why it failed. `None` when it passed.
    pub reason: Option<String>,
}

impl GateResult {
    fn pass(gate: &str) -> Self {
        Self {
            gate: gate.to_owned(),
            passed: true,
            reason: None,
        }
    }
    fn fail(gate: &str, reason: impl Into<String>) -> Self {
        Self {
            gate: gate.to_owned(),
            passed: false,
            reason: Some(reason.into()),
        }
    }
}

/// A side effect the caller must perform once all gates are green.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// Release an interlock so the fixture or PLC may proceed.
    ReleaseInterlock {
        /// Device to signal.
        device_id: RowId,
    },
}

/// What to do with a unit that failed a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// Let the operator try again at this operation.
    Retry {
        /// Attempts left before the unit is quarantined.
        attempts_remaining: u32,
    },
    /// Return to an earlier operation, voiding the listed completions.
    Rework {
        /// Operation to return to.
        to_operation: OperationSeq,
        /// Completions that are voided and must be redone.
        invalidates: Vec<OperationSeq>,
    },
    /// Hold for a supervisor decision.
    Quarantine,
    /// Scrap. Terminal: the UID is retired forever.
    Scrap,
}

/// The result of attempting to advance a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Advance {
    /// Every gate passed.
    Completed {
        /// Gate-by-gate detail, retained for the audit trail.
        gates: Vec<GateResult>,
        /// Effects the caller must perform, e.g. releasing an interlock.
        effects: Vec<Effect>,
        /// Operations the unit could work on next.
        next_ready: Vec<OperationSeq>,
        /// Whether every required operation is now done.
        route_complete: bool,
    },
    /// At least one gate failed.
    Blocked {
        /// Gate-by-gate detail, including the ones that passed.
        gates: Vec<GateResult>,
        /// The failures, for defect creation.
        failures: Vec<GateResult>,
        /// Where the unit goes now.
        disposition: Disposition,
    },
}

impl Advance {
    /// Whether the unit advanced.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// Stateless evaluator over a validated route.
#[derive(Debug, Clone, Copy, Default)]
pub struct RouteEngine;

impl RouteEngine {
    /// Decide which operation this station should perform for this unit.
    ///
    /// This is what makes station topology configuration rather than code. It
    /// handles a single terminal running an entire route, one operation
    /// duplicated across parallel stations, and everything between: the route
    /// says what is ready, the station says what it is equipped for, and the
    /// intersection is the answer.
    ///
    /// # Errors
    /// Returns [`CoreError::IllegalTransition`] if the unit's state does not
    /// accept work, or [`CoreError::StationCannotServe`] if the station is not
    /// equipped for anything the unit needs next.
    pub fn resolve_operation<'r>(
        &self,
        route: &'r Route,
        unit: &Unit,
        station: &Station,
        completed: &BTreeSet<OperationSeq>,
    ) -> Result<&'r RouteOperation> {
        if !unit.state.accepts_work() {
            return Err(CoreError::IllegalTransition {
                from: unit.state.name(),
                action: "start an operation".into(),
            });
        }
        if !station.active {
            return Err(CoreError::StationCannotServe {
                station: station.code.to_string(),
            });
        }

        route
            .ready_operations(completed)
            .into_iter()
            .find(|op| station.can_perform(op.operation_def_id))
            .ok_or_else(|| CoreError::StationCannotServe {
                station: station.code.to_string(),
            })
    }

    /// Evaluate every gate on the operation and decide whether the unit may
    /// advance.
    #[must_use]
    pub fn try_advance(&self, ctx: &OperationContext<'_>) -> Advance {
        let mut gates = Vec::with_capacity(ctx.operation.gates.len());
        let mut effects = Vec::new();

        // Entry conditions first. Output gates are deferred, because an
        // interlock must only be released once everything else is green.
        for gate in ctx.operation.gates.iter().filter(|g| !g.is_output()) {
            gates.push(evaluate_gate(gate, ctx));
        }

        let failures: Vec<GateResult> = gates.iter().filter(|g| !g.passed).cloned().collect();

        if !failures.is_empty() {
            let disposition = disposition_for(&ctx.operation.failure_path, ctx.attempt);
            return Advance::Blocked {
                gates,
                failures,
                disposition,
            };
        }

        // All entry gates green: now the outputs may fire.
        for gate in ctx.operation.gates.iter().filter(|g| g.is_output()) {
            if let Gate::InterlockOut { device_id } = gate {
                effects.push(Effect::ReleaseInterlock {
                    device_id: *device_id,
                });
                gates.push(GateResult::pass(gate.kind()));
            }
        }

        let mut completed_after = ctx.completed.clone();
        completed_after.insert(ctx.operation.seq);
        let next_ready: Vec<OperationSeq> = ctx
            .route
            .ready_operations(&completed_after)
            .iter()
            .map(|o| o.seq)
            .collect();

        Advance::Completed {
            gates,
            effects,
            next_ready,
            route_complete: ctx.route.is_complete(&completed_after),
        }
    }
}

/// Map a configured failure path plus the attempt count onto a disposition.
///
/// A retry path that has run out of attempts becomes a quarantine rather than
/// an infinite loop.
fn disposition_for(path: &FailurePath, attempt: u32) -> Disposition {
    match path {
        FailurePath::Retry { max_attempts } => {
            if attempt < *max_attempts {
                Disposition::Retry {
                    attempts_remaining: max_attempts - attempt,
                }
            } else {
                Disposition::Quarantine
            }
        }
        FailurePath::Rework {
            to_operation,
            invalidates,
        } => Disposition::Rework {
            to_operation: *to_operation,
            invalidates: invalidates.clone(),
        },
        FailurePath::Quarantine => Disposition::Quarantine,
        FailurePath::Scrap => Disposition::Scrap,
    }
}

/// Evaluate a single gate. Pure: it reads only what the context carries.
fn evaluate_gate(gate: &Gate, ctx: &OperationContext<'_>) -> GateResult {
    let kind = gate.kind();
    match gate {
        Gate::Identity => match ctx.scanned_identity {
            Some(scanned) if scanned == ctx.unit.uid => GateResult::pass(kind),
            Some(other) => GateResult::fail(
                kind,
                format!(
                    "scanned {other} but the unit at this station is {}",
                    ctx.unit.uid
                ),
            ),
            None => GateResult::fail(kind, "no unit identity was scanned"),
        },

        Gate::OperatorAuth => match ctx.operator {
            None => GateResult::fail(kind, "no operator is logged in"),
            Some(op) if !op.active => {
                GateResult::fail(kind, format!("operator {} is deactivated", op.username))
            }
            Some(op) => match ctx.operation_def.required_skill {
                Some(skill) if !op.is_certified_for(skill) => GateResult::fail(
                    kind,
                    format!(
                        "operator {} is not certified for skill {skill} required by {}",
                        op.username, ctx.operation_def.name
                    ),
                ),
                _ => GateResult::pass(kind),
            },
        },

        Gate::Precondition => {
            if ctx
                .route
                .predecessors_satisfied(ctx.operation, ctx.completed)
            {
                GateResult::pass(kind)
            } else {
                let missing: Vec<_> = ctx
                    .operation
                    .predecessors
                    .iter()
                    .filter(|p| !ctx.completed.contains(p))
                    .collect();
                GateResult::fail(
                    kind,
                    format!("required predecessors not complete: {missing:?}"),
                )
            }
        }

        Gate::ComponentVerify { bom_line_ids } => {
            // Poka-yoke, and deliberately strict in both directions: a missing
            // component blocks, and so does a scan that matches nothing in the
            // BOM. A wrong part is a hard stop, never a warning.
            if let Some(bad) = ctx.component_scans.iter().find(|s| s.bom_line_id.is_none()) {
                return GateResult::fail(
                    kind,
                    format!(
                        "scanned part {} is not on the BOM for this operation",
                        bad.part_no
                    ),
                );
            }
            let scanned: BTreeSet<RowId> = ctx
                .component_scans
                .iter()
                .filter_map(|s| s.bom_line_id)
                .collect();
            let missing: Vec<RowId> = bom_line_ids
                .iter()
                .copied()
                .filter(|id| !scanned.contains(id))
                .collect();
            if missing.is_empty() {
                GateResult::pass(kind)
            } else {
                GateResult::fail(kind, format!("BOM lines not verified: {missing:?}"))
            }
        }

        Gate::DataCapture => {
            let captured: BTreeMap<RowId, &CapturedValue> =
                ctx.captured.iter().map(|c| (c.dcp_id, c)).collect();

            let mut problems: Vec<String> = Vec::new();
            for dcp in ctx.dcps {
                // Sampling decides whether this unit needed the value at all.
                if !dcp.sample_rule.applies_to(ctx.unit.index_in_job) {
                    continue;
                }
                match captured.get(&dcp.id) {
                    None if dcp.mandatory => problems.push(format!("{} not captured", dcp.name)),
                    None => {}
                    Some(c) if c.verdict == Verdict::Fail => problems.push(format!(
                        "{} = {} is outside limits {:?}..{:?}",
                        dcp.name, c.value, dcp.limits.min, dcp.limits.max
                    )),
                    Some(_) => {}
                }
            }
            if problems.is_empty() {
                GateResult::pass(kind)
            } else {
                GateResult::fail(kind, problems.join("; "))
            }
        }

        Gate::TestPass { test_name } => match ctx.test_verdicts.get(test_name) {
            Some(Verdict::Pass) => GateResult::pass(kind),
            Some(Verdict::Fail) => GateResult::fail(kind, format!("test {test_name} failed")),
            None => GateResult::fail(kind, format!("test {test_name} has no verdict")),
        },

        Gate::MarkVerified => {
            // A revision that carries no physical mark cannot be gated on one.
            if ctx.revision.mark_method == MarkMethod::None {
                return GateResult::pass(kind);
            }
            let Some(v) = ctx.mark_verification else {
                return GateResult::fail(kind, "mark was not read back");
            };
            let intended = ctx.unit.uid.to_string();
            if v.decoded != intended {
                // The dangerous case: a readable mark carrying the wrong
                // identity. Never let this advance.
                return GateResult::fail(
                    kind,
                    format!("mark decoded as {} but this unit is {intended}", v.decoded),
                );
            }
            match (ctx.revision.min_mark_grade, v.grade) {
                // Grades run A (best) to F. A larger letter is a worse mark.
                (Some(min), Some(actual)) if actual > min => GateResult::fail(
                    kind,
                    format!("mark graded {actual}, below the minimum {min} for this revision"),
                ),
                (Some(min), None) => GateResult::fail(
                    kind,
                    format!("revision requires grade {min} but the reader reported none"),
                ),
                _ => GateResult::pass(kind),
            }
        }

        // Handled after every entry gate passes; never evaluated here.
        Gate::InterlockOut { .. } => GateResult::pass(kind),
    }
}

/// Apply a disposition to a unit's state.
///
/// # Errors
/// Propagates [`CoreError`] from the underlying state transition, including
/// the refusal to move a scrapped unit.
pub fn apply_disposition(unit: &mut Unit, disposition: &Disposition) -> Result<()> {
    match disposition {
        Disposition::Retry { .. } => Ok(()),
        Disposition::Rework { to_operation, .. } => {
            unit.transition_to(UnitState::Reworking)?;
            unit.current_operation = Some(*to_operation);
            Ok(())
        }
        Disposition::Quarantine => unit.transition_to(UnitState::Quarantined),
        Disposition::Scrap => unit.transition_to(UnitState::Scrapped),
    }
}
