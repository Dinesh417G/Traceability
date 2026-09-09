//! The route: a data-driven state machine, not code branches.
//!
//! A route is a **DAG** of operations for one product revision on one line.
//! Edges are declared as explicit predecessors, which is what lets a route
//! express sequential work, optional steps, and genuinely parallel branches
//! without any of it being special-cased in code.
//!
//! A route is validated once at load time. A cycle or a dangling predecessor is
//! a configuration error that must surface then — never in the middle of a
//! shift with a part in the fixture.

use crate::error::{CoreError, Result};
use crate::id::RowId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Position of an operation within a route. Stable across route edits, and the
/// key used by predecessors.
pub type OperationSeq = u32;

/// A reusable operation template, e.g. "M6 torque, 4 screws".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationDef {
    /// Surrogate key.
    pub id: RowId,
    /// Stable machine name.
    pub name: String,
    /// Operator-facing label.
    pub label: String,
    /// Data collection points captured at this operation.
    pub dcp_ids: Vec<RowId>,
    /// Skill certification an operator must hold. `None` means no restriction.
    pub required_skill: Option<RowId>,
}

/// Whether an operation must be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Requirement {
    /// Must be completed before any successor may start.
    Required,
    /// May be skipped. Successors do not wait for it.
    Optional,
}

/// What to do when a gate fails.
///
/// Every failure produces a defect and routes the unit down one of these
/// paths. Rework loops must re-run the operations they invalidated, and must
/// remain visible in the final trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", tag = "kind")]
pub enum FailurePath {
    /// Let the operator try again, up to `max_attempts` at this operation.
    Retry {
        /// Attempts allowed before the unit is quarantined.
        max_attempts: u32,
    },
    /// Send the unit back to an earlier operation, invalidating the listed
    /// operations so they must be performed again.
    Rework {
        /// Operation the unit returns to.
        to_operation: OperationSeq,
        /// Operations whose completion is voided and must be redone.
        invalidates: Vec<OperationSeq>,
    },
    /// Hold the unit for a supervisor decision.
    Quarantine,
    /// Scrap the unit. Terminal: the UID is retired forever.
    Scrap,
}

/// A gate that must be satisfied before a unit may advance.
///
/// Gates are evaluated as pure functions of the operation context, which is
/// what keeps the engine testable with no hardware.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", tag = "kind")]
pub enum Gate {
    /// A valid unit UID has been scanned, or read back from a mark.
    Identity,
    /// An operator is logged in and certified for this operation.
    OperatorAuth,
    /// Required predecessor operations are complete.
    Precondition,
    /// Scanned components match the expected BOM lines. Poka-yoke: a wrong
    /// part is a hard stop, not a warning.
    ComponentVerify {
        /// BOM lines that must each be satisfied by a scan.
        bom_line_ids: Vec<RowId>,
    },
    /// All mandatory DCPs are captured and within limits.
    DataCapture,
    /// The named test's verdict is a pass.
    TestPass {
        /// Test whose verdict gates this operation.
        test_name: String,
    },
    /// The applied mark was read back and matches the intended UID.
    MarkVerified,
    /// Release an interlock signal to a PLC or fixture.
    ///
    /// This is an **output**, not a precondition: the engine emits it only
    /// after every other gate on the operation is green. It is modelled as a
    /// gate so that the configuration reads as one list.
    InterlockOut {
        /// Device that receives the release signal.
        device_id: RowId,
    },
}

impl Gate {
    /// Stable discriminant, used in defect codes and diagnostics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Identity => "IDENTITY",
            Self::OperatorAuth => "OPERATOR_AUTH",
            Self::Precondition => "PRECONDITION",
            Self::ComponentVerify { .. } => "COMPONENT_VERIFY",
            Self::DataCapture => "DATA_CAPTURE",
            Self::TestPass { .. } => "TEST_PASS",
            Self::MarkVerified => "MARK_VERIFIED",
            Self::InterlockOut { .. } => "INTERLOCK_OUT",
        }
    }

    /// Whether this gate is an output effect rather than an entry condition.
    #[must_use]
    pub fn is_output(&self) -> bool {
        matches!(self, Self::InterlockOut { .. })
    }
}

/// One operation as it appears in a specific route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteOperation {
    /// Position within the route.
    pub seq: OperationSeq,
    /// Template this operation instantiates.
    pub operation_def_id: RowId,
    /// Whether it may be skipped.
    pub requirement: Requirement,
    /// Operations that must complete first. Empty means an entry point.
    /// Multiple successors sharing a predecessor form a parallel branch.
    pub predecessors: Vec<OperationSeq>,
    /// Gates guarding advancement.
    pub gates: Vec<Gate>,
    /// Where the unit goes when a gate fails.
    pub failure_path: FailurePath,
}

/// An ordered, validated set of operations for a product revision on a line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Surrogate key.
    pub id: RowId,
    /// Product revision this route builds.
    pub product_revision_id: RowId,
    /// Line the route runs on.
    pub line_id: RowId,
    /// Route revision, for auditability.
    pub revision: String,
    /// Operations making up the route.
    pub operations: Vec<RouteOperation>,
}

impl Route {
    /// Validate the route's shape.
    ///
    /// Checks for duplicate sequences, dangling predecessors, self-references
    /// and cycles. Call this at load time, so a bad route can never be handed
    /// to the engine.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidRoute`] describing the first problem found.
    pub fn validate(&self) -> Result<()> {
        if self.operations.is_empty() {
            return Err(CoreError::InvalidRoute("route has no operations".into()));
        }

        let mut seen: BTreeSet<OperationSeq> = BTreeSet::new();
        for op in &self.operations {
            if !seen.insert(op.seq) {
                return Err(CoreError::InvalidRoute(format!(
                    "duplicate operation seq {}",
                    op.seq
                )));
            }
        }

        for op in &self.operations {
            for p in &op.predecessors {
                if *p == op.seq {
                    return Err(CoreError::InvalidRoute(format!(
                        "operation {} lists itself as a predecessor",
                        op.seq
                    )));
                }
                if !seen.contains(p) {
                    return Err(CoreError::InvalidRoute(format!(
                        "operation {} has unknown predecessor {p}",
                        op.seq
                    )));
                }
            }
        }

        if !self.operations.iter().any(|o| o.predecessors.is_empty()) {
            return Err(CoreError::InvalidRoute(
                "no entry operation: every operation has a predecessor, so the route cannot start"
                    .into(),
            ));
        }

        self.assert_acyclic()?;
        Ok(())
    }

    /// Kahn's algorithm: if we cannot topologically order every node, the graph
    /// contains a cycle.
    fn assert_acyclic(&self) -> Result<()> {
        let mut indegree: BTreeMap<OperationSeq, usize> = self
            .operations
            .iter()
            .map(|o| (o.seq, o.predecessors.len()))
            .collect();
        let mut successors: BTreeMap<OperationSeq, Vec<OperationSeq>> = BTreeMap::new();
        for op in &self.operations {
            for p in &op.predecessors {
                successors.entry(*p).or_default().push(op.seq);
            }
        }

        let mut queue: VecDeque<OperationSeq> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(s, _)| *s)
            .collect();
        let mut visited = 0usize;

        while let Some(seq) = queue.pop_front() {
            visited += 1;
            for next in successors.get(&seq).into_iter().flatten() {
                if let Some(d) = indegree.get_mut(next) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(*next);
                    }
                }
            }
        }

        if visited == self.operations.len() {
            Ok(())
        } else {
            let stuck: Vec<_> = indegree
                .iter()
                .filter(|(_, d)| **d > 0)
                .map(|(s, _)| *s)
                .collect();
            Err(CoreError::InvalidRoute(format!(
                "cycle detected involving operations {stuck:?}"
            )))
        }
    }

    /// Look up an operation by sequence.
    #[must_use]
    pub fn operation(&self, seq: OperationSeq) -> Option<&RouteOperation> {
        self.operations.iter().find(|o| o.seq == seq)
    }

    /// Operations with no unmet required predecessors, given what is complete.
    ///
    /// This is the set the unit could work on next. It has more than one member
    /// exactly when the route has parallel branches open.
    #[must_use]
    pub fn ready_operations(&self, completed: &BTreeSet<OperationSeq>) -> Vec<&RouteOperation> {
        self.operations
            .iter()
            .filter(|op| !completed.contains(&op.seq))
            .filter(|op| self.predecessors_satisfied(op, completed))
            .collect()
    }

    /// Whether every *required* predecessor of `op` is complete.
    ///
    /// Optional predecessors do not block: that is what makes a step skippable.
    #[must_use]
    pub fn predecessors_satisfied(
        &self,
        op: &RouteOperation,
        completed: &BTreeSet<OperationSeq>,
    ) -> bool {
        op.predecessors.iter().all(|p| {
            if completed.contains(p) {
                return true;
            }
            match self.operation(*p) {
                Some(pred) => pred.requirement == Requirement::Optional,
                // Validation rules this out; treat unknown as blocking.
                None => false,
            }
        })
    }

    /// Whether every required operation has been completed.
    #[must_use]
    pub fn is_complete(&self, completed: &BTreeSet<OperationSeq>) -> bool {
        self.operations
            .iter()
            .filter(|o| o.requirement == Requirement::Required)
            .all(|o| completed.contains(&o.seq))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn op(seq: OperationSeq, preds: &[OperationSeq]) -> RouteOperation {
        RouteOperation {
            seq,
            operation_def_id: i64::from(seq),
            requirement: Requirement::Required,
            predecessors: preds.to_vec(),
            gates: vec![],
            failure_path: FailurePath::Quarantine,
        }
    }

    fn route(ops: Vec<RouteOperation>) -> Route {
        Route {
            id: 1,
            product_revision_id: 1,
            line_id: 1,
            revision: "A".into(),
            operations: ops,
        }
    }

    #[test]
    fn linear_route_validates() {
        route(vec![op(10, &[]), op(20, &[10]), op(30, &[20])])
            .validate()
            .unwrap();
    }

    #[test]
    fn empty_route_is_rejected() {
        assert!(route(vec![]).validate().is_err());
    }

    #[test]
    fn duplicate_seq_is_rejected() {
        let err = route(vec![op(10, &[]), op(10, &[])])
            .validate()
            .unwrap_err();
        assert!(format!("{err}").contains("duplicate"));
    }

    #[test]
    fn dangling_predecessor_is_rejected() {
        let err = route(vec![op(10, &[]), op(20, &[99])])
            .validate()
            .unwrap_err();
        assert!(format!("{err}").contains("unknown predecessor"));
    }

    #[test]
    fn self_reference_is_rejected() {
        let err = route(vec![op(10, &[10])]).validate().unwrap_err();
        assert!(format!("{err}").contains("itself"));
    }

    #[test]
    fn cycle_is_rejected_at_load_time_not_mid_shift() {
        // A valid entry point (10) with a cycle behind it: 20 -> 30 -> 40 -> 20.
        // The entry check therefore passes and the cycle detector is what must
        // catch this.
        let r = route(vec![
            op(10, &[]),
            op(20, &[10, 40]),
            op(30, &[20]),
            op(40, &[30]),
        ]);
        let err = r.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cycle"), "got {msg}");
        assert!(
            msg.contains("20") && msg.contains("30") && msg.contains("40"),
            "got {msg}"
        );
    }

    #[test]
    fn parallel_branches_are_both_ready_at_once() {
        //        20
        //      /    \
        //    10      40
        //      \    /
        //        30
        let r = route(vec![
            op(10, &[]),
            op(20, &[10]),
            op(30, &[10]),
            op(40, &[20, 30]),
        ]);
        r.validate().unwrap();

        let done: BTreeSet<_> = [10].into_iter().collect();
        let ready: Vec<_> = r.ready_operations(&done).iter().map(|o| o.seq).collect();
        assert_eq!(ready, vec![20, 30], "both branches open in parallel");

        // The join waits for both.
        let done: BTreeSet<_> = [10, 20].into_iter().collect();
        let ready: Vec<_> = r.ready_operations(&done).iter().map(|o| o.seq).collect();
        assert_eq!(
            ready,
            vec![30],
            "join must not open until both branches finish"
        );
    }

    #[test]
    fn optional_operation_does_not_block_its_successor() {
        let mut ops = vec![op(10, &[]), op(20, &[10]), op(30, &[20])];
        ops[1].requirement = Requirement::Optional;
        let r = route(ops);
        r.validate().unwrap();

        // 20 is optional and skipped; 30 must still become available.
        let done: BTreeSet<_> = [10].into_iter().collect();
        let ready: Vec<_> = r.ready_operations(&done).iter().map(|o| o.seq).collect();
        assert!(
            ready.contains(&30),
            "skippable step must not block the route: {ready:?}"
        );
    }

    #[test]
    fn completion_ignores_skipped_optional_operations() {
        let mut ops = vec![op(10, &[]), op(20, &[10])];
        ops[1].requirement = Requirement::Optional;
        let r = route(ops);
        let done: BTreeSet<_> = [10].into_iter().collect();
        assert!(r.is_complete(&done));
    }

    #[test]
    fn completion_requires_every_required_operation() {
        let r = route(vec![op(10, &[]), op(20, &[10])]);
        assert!(!r.is_complete(&[10].into_iter().collect()));
        assert!(r.is_complete(&[10, 20].into_iter().collect()));
    }

    #[test]
    fn route_with_no_entry_point_is_rejected() {
        // Every operation waiting on another means nothing can ever start.
        let r = route(vec![op(10, &[20]), op(20, &[10])]);
        assert!(r.validate().is_err());
    }

    #[test]
    fn interlock_is_the_only_output_gate() {
        assert!(Gate::InterlockOut { device_id: 1 }.is_output());
        assert!(!Gate::Identity.is_output());
        assert!(!Gate::DataCapture.is_output());
    }
}
