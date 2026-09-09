//! Route engine behaviour, exercised through the public API with no hardware.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use trace_core::config::{MarkMethod, Material, Operator, ProductRevision, Station, TrackingMode};
use trace_core::dcp::{DataCollectionPoint, DataType, Limits, SampleRule, Value, Verdict};
use trace_core::engine::{
    Advance, CapturedValue, ComponentScan, Disposition, Effect, MarkVerification, OperationContext,
    RouteEngine, apply_disposition,
};
use trace_core::route::{
    FailurePath, Gate, OperationDef, OperationSeq, Requirement, Route, RouteOperation,
};
use trace_core::unit::{Unit, UnitState};
use trace_core::{Code, RowId, UnitUid};

// ---------------------------------------------------------------- fixtures

const OP_DEF_MARK: RowId = 100;
const OP_DEF_ASSY: RowId = 200;
const OP_DEF_TEST: RowId = 300;
const SKILL_TORQUE: RowId = 900;
const DCP_TORQUE: RowId = 1;
const BOM_SCREW: RowId = 11;

fn code(s: &str) -> Code {
    Code::new(s).unwrap()
}

fn revision(mark_method: MarkMethod, min_grade: Option<char>) -> ProductRevision {
    ProductRevision {
        id: 1,
        product_id: 1,
        revision: code("A"),
        material: Material::Aluminium,
        mark_method,
        min_mark_grade: min_grade,
        active: true,
    }
}

fn station(ops: &[RowId]) -> Station {
    Station {
        id: 1,
        line_id: 1,
        code: code("ST-01"),
        name: "Station 1".into(),
        operations: ops.iter().copied().collect(),
        active: true,
    }
}

fn operator(certs: &[RowId]) -> Operator {
    Operator {
        id: 7,
        tenant_id: 1,
        username: "R.KUMAR".into(),
        display_name: "R. Kumar".into(),
        certifications: certs.iter().copied().collect(),
        active: true,
    }
}

fn op_def(id: RowId, name: &str, skill: Option<RowId>) -> OperationDef {
    OperationDef {
        id,
        name: name.into(),
        label: name.into(),
        dcp_ids: vec![],
        required_skill: skill,
    }
}

fn torque_dcp(mandatory: bool, rule: SampleRule) -> DataCollectionPoint {
    DataCollectionPoint {
        id: DCP_TORQUE,
        name: "final_torque".into(),
        label: "Final torque".into(),
        unit: Some("Nm".into()),
        datatype: DataType::Numeric,
        limits: Limits {
            min: Some(10.0),
            max: Some(14.0),
            nominal: Some(12.0),
        },
        sample_rule: rule,
        mandatory,
        device_id: Some(5),
    }
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

fn route_op(
    seq: OperationSeq,
    def: RowId,
    preds: &[OperationSeq],
    gates: Vec<Gate>,
    failure: FailurePath,
) -> RouteOperation {
    RouteOperation {
        seq,
        operation_def_id: def,
        requirement: Requirement::Required,
        predecessors: preds.to_vec(),
        gates,
        failure_path: failure,
    }
}

fn route(ops: Vec<RouteOperation>) -> Route {
    let r = Route {
        id: 1,
        product_revision_id: 1,
        line_id: 1,
        revision: "A".into(),
        operations: ops,
    };
    r.validate().unwrap();
    r
}

/// Everything a gate evaluation needs, so tests can vary one thing at a time.
struct Harness {
    unit: Unit,
    revision: ProductRevision,
    route: Route,
    station: Station,
    operator: Option<Operator>,
    completed: BTreeSet<OperationSeq>,
    scanned: Option<UnitUid>,
    scans: Vec<ComponentScan>,
    captured: Vec<CapturedValue>,
    dcps: Vec<DataCollectionPoint>,
    verdicts: BTreeMap<String, Verdict>,
    mark: Option<MarkVerification>,
    attempt: u32,
    op_def: OperationDef,
}

impl Harness {
    /// A single-operation route whose gates are supplied by the test.
    fn new(gates: Vec<Gate>, failure: FailurePath) -> Self {
        let u = unit(UnitState::InProcess);
        Self {
            scanned: Some(u.uid),
            unit: u,
            revision: revision(MarkMethod::Label, None),
            route: route(vec![route_op(10, OP_DEF_ASSY, &[], gates, failure)]),
            station: station(&[OP_DEF_ASSY]),
            operator: Some(operator(&[SKILL_TORQUE])),
            completed: BTreeSet::new(),
            scans: vec![],
            captured: vec![],
            dcps: vec![],
            verdicts: BTreeMap::new(),
            mark: None,
            attempt: 1,
            op_def: op_def(OP_DEF_ASSY, "assemble", None),
        }
    }

    fn advance(&self) -> Advance {
        let operation = self.route.operation(10).unwrap();
        let ctx = OperationContext {
            unit: &self.unit,
            revision: &self.revision,
            route: &self.route,
            operation,
            operation_def: &self.op_def,
            station: &self.station,
            operator: self.operator.as_ref(),
            completed: &self.completed,
            scanned_identity: self.scanned,
            component_scans: &self.scans,
            captured: &self.captured,
            dcps: &self.dcps,
            test_verdicts: &self.verdicts,
            mark_verification: self.mark.as_ref(),
            attempt: self.attempt,
        };
        RouteEngine.try_advance(&ctx)
    }
}

fn failure_reasons(a: &Advance) -> Vec<String> {
    match a {
        Advance::Blocked { failures, .. } => failures
            .iter()
            .map(|f| format!("{}: {}", f.gate, f.reason.clone().unwrap_or_default()))
            .collect(),
        Advance::Completed { .. } => vec![],
    }
}

// ---------------------------------------------------------------- identity

#[test]
fn identity_gate_passes_when_the_scan_matches() {
    let h = Harness::new(vec![Gate::Identity], FailurePath::Quarantine);
    assert!(h.advance().is_completed());
}

#[test]
fn identity_gate_blocks_when_nothing_was_scanned() {
    let mut h = Harness::new(vec![Gate::Identity], FailurePath::Quarantine);
    h.scanned = None;
    let a = h.advance();
    assert!(!a.is_completed());
    assert!(failure_reasons(&a)[0].contains("no unit identity"));
}

#[test]
fn identity_gate_blocks_when_a_different_unit_was_scanned() {
    // The operator has one part in the fixture and scanned another.
    let mut h = Harness::new(vec![Gate::Identity], FailurePath::Quarantine);
    h.scanned = Some(UnitUid::generate());
    assert!(!h.advance().is_completed());
}

// ------------------------------------------------------------ operator auth

#[test]
fn operator_gate_blocks_when_nobody_is_logged_in() {
    let mut h = Harness::new(vec![Gate::OperatorAuth], FailurePath::Quarantine);
    h.operator = None;
    assert!(failure_reasons(&h.advance())[0].contains("no operator"));
}

#[test]
fn operator_gate_blocks_a_deactivated_account() {
    let mut h = Harness::new(vec![Gate::OperatorAuth], FailurePath::Quarantine);
    h.operator = Some(Operator {
        active: false,
        ..operator(&[SKILL_TORQUE])
    });
    assert!(failure_reasons(&h.advance())[0].contains("deactivated"));
}

#[test]
fn operator_gate_enforces_skill_certification() {
    let mut h = Harness::new(vec![Gate::OperatorAuth], FailurePath::Quarantine);
    h.op_def = op_def(OP_DEF_ASSY, "torque", Some(SKILL_TORQUE));

    h.operator = Some(operator(&[]));
    assert!(failure_reasons(&h.advance())[0].contains("not certified"));

    h.operator = Some(operator(&[SKILL_TORQUE]));
    assert!(h.advance().is_completed());
}

// -------------------------------------------------------- component verify

#[test]
fn component_verify_passes_when_every_bom_line_is_scanned() {
    let mut h = Harness::new(
        vec![Gate::ComponentVerify {
            bom_line_ids: vec![BOM_SCREW],
        }],
        FailurePath::Quarantine,
    );
    h.scans = vec![ComponentScan {
        part_no: code("SCR-M6-20"),
        bom_line_id: Some(BOM_SCREW),
        raw: "SCR-M6-20".into(),
    }];
    assert!(h.advance().is_completed());
}

#[test]
fn component_verify_blocks_a_missing_component() {
    let h = Harness::new(
        vec![Gate::ComponentVerify {
            bom_line_ids: vec![BOM_SCREW],
        }],
        FailurePath::Quarantine,
    );
    assert!(failure_reasons(&h.advance())[0].contains("not verified"));
}

#[test]
fn wrong_part_is_a_hard_stop_not_a_warning() {
    // Poka-yoke: scanning a part that is not on the BOM must block, even when
    // every required line has also been scanned.
    let mut h = Harness::new(
        vec![Gate::ComponentVerify {
            bom_line_ids: vec![BOM_SCREW],
        }],
        FailurePath::Quarantine,
    );
    h.scans = vec![
        ComponentScan {
            part_no: code("SCR-M6-20"),
            bom_line_id: Some(BOM_SCREW),
            raw: "SCR-M6-20".into(),
        },
        ComponentScan {
            part_no: code("SCR-M8-30"),
            bom_line_id: None,
            raw: "SCR-M8-30".into(),
        },
    ];
    let a = h.advance();
    assert!(
        !a.is_completed(),
        "a part not on the BOM must stop the line"
    );
    assert!(failure_reasons(&a)[0].contains("not on the BOM"));
}

// ------------------------------------------------------------ data capture

#[test]
fn data_capture_passes_when_readings_are_in_limits() {
    let mut h = Harness::new(vec![Gate::DataCapture], FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(true, SampleRule::Every)];
    h.captured = vec![CapturedValue {
        dcp_id: DCP_TORQUE,
        value: Value::Numeric(12.0),
        verdict: Verdict::Pass,
    }];
    assert!(h.advance().is_completed());
}

#[test]
fn data_capture_blocks_a_missing_mandatory_reading() {
    let mut h = Harness::new(vec![Gate::DataCapture], FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(true, SampleRule::Every)];
    assert!(failure_reasons(&h.advance())[0].contains("not captured"));
}

#[test]
fn data_capture_blocks_an_out_of_limits_reading() {
    let mut h = Harness::new(vec![Gate::DataCapture], FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(true, SampleRule::Every)];
    h.captured = vec![CapturedValue {
        dcp_id: DCP_TORQUE,
        value: Value::Numeric(18.0),
        verdict: Verdict::Fail,
    }];
    assert!(failure_reasons(&h.advance())[0].contains("outside limits"));
}

#[test]
fn optional_reading_may_be_absent() {
    let mut h = Harness::new(vec![Gate::DataCapture], FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(false, SampleRule::Every)];
    assert!(h.advance().is_completed());
}

#[test]
fn sampling_rule_excuses_units_it_does_not_apply_to() {
    // First-off inspection: unit 0 must be measured, unit 5 need not be.
    let mut h = Harness::new(vec![Gate::DataCapture], FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(true, SampleRule::FirstOff)];

    h.unit.index_in_job = 0;
    assert!(!h.advance().is_completed(), "first unit must be measured");

    h.unit.index_in_job = 5;
    assert!(
        h.advance().is_completed(),
        "later units are excused by the sample rule"
    );
}

// -------------------------------------------------------------- test pass

#[test]
fn test_gate_requires_a_recorded_pass() {
    let mut h = Harness::new(
        vec![Gate::TestPass {
            test_name: "EOL".into(),
        }],
        FailurePath::Quarantine,
    );
    assert!(failure_reasons(&h.advance())[0].contains("no verdict"));

    h.verdicts.insert("EOL".into(), Verdict::Fail);
    assert!(failure_reasons(&h.advance())[0].contains("failed"));

    h.verdicts.insert("EOL".into(), Verdict::Pass);
    assert!(h.advance().is_completed());
}

// ------------------------------------------------------------ mark verified

#[test]
fn mark_gate_requires_a_read_back() {
    let h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    assert!(failure_reasons(&h.advance())[0].contains("not read back"));
}

#[test]
fn mark_gate_passes_when_the_read_back_matches() {
    let mut h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    h.mark = Some(MarkVerification {
        decoded: h.unit.uid.to_string(),
        grade: None,
    });
    assert!(h.advance().is_completed());
}

#[test]
fn a_readable_mark_carrying_the_wrong_identity_never_advances() {
    // The quietly catastrophic case: the mark reads fine, but it is not this
    // unit. Advancing here would corrupt the trace for two units at once.
    let mut h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    h.mark = Some(MarkVerification {
        decoded: UnitUid::generate().to_string(),
        grade: None,
    });
    let a = h.advance();
    assert!(!a.is_completed());
    assert!(failure_reasons(&a)[0].contains("but this unit is"));
}

#[test]
fn mark_grade_below_the_revision_minimum_is_rejected() {
    // Grades run A (best) to F. The aluminium and plastic revisions of the same
    // product legitimately carry different minimums.
    let mut h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    h.revision = revision(MarkMethod::LaserDpm, Some('C'));

    h.mark = Some(MarkVerification {
        decoded: h.unit.uid.to_string(),
        grade: Some('D'),
    });
    assert!(failure_reasons(&h.advance())[0].contains("below the minimum"));

    h.mark = Some(MarkVerification {
        decoded: h.unit.uid.to_string(),
        grade: Some('B'),
    });
    assert!(h.advance().is_completed());
}

#[test]
fn revision_requiring_a_grade_rejects_a_reader_that_cannot_grade() {
    let mut h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    h.revision = revision(MarkMethod::LaserDpm, Some('C'));
    h.mark = Some(MarkVerification {
        decoded: h.unit.uid.to_string(),
        grade: None,
    });
    assert!(failure_reasons(&h.advance())[0].contains("reported none"));
}

#[test]
fn unmarked_revision_is_not_gated_on_a_mark() {
    let mut h = Harness::new(vec![Gate::MarkVerified], FailurePath::Quarantine);
    h.revision = revision(MarkMethod::None, None);
    assert!(h.advance().is_completed());
}

// --------------------------------------------------------------- interlock

#[test]
fn interlock_fires_only_after_every_other_gate_is_green() {
    // Safety-critical ordering: releasing a fixture on a failed gate would let
    // a bad part move. Prove both directions.
    let gates = vec![
        Gate::Identity,
        Gate::DataCapture,
        Gate::InterlockOut { device_id: 42 },
    ];

    let mut h = Harness::new(gates.clone(), FailurePath::Quarantine);
    h.dcps = vec![torque_dcp(true, SampleRule::Every)];

    // Reading missing -> blocked, and no interlock effect is produced at all.
    match h.advance() {
        Advance::Blocked {
            gates: evaluated, ..
        } => {
            assert!(
                !evaluated.iter().any(|g| g.gate == "INTERLOCK_OUT"),
                "interlock must not even be evaluated while a gate is red"
            );
        }
        Advance::Completed { .. } => panic!("must not advance with a missing reading"),
    }

    // Reading present and in limits -> advance, interlock released.
    h.captured = vec![CapturedValue {
        dcp_id: DCP_TORQUE,
        value: Value::Numeric(12.0),
        verdict: Verdict::Pass,
    }];
    match h.advance() {
        Advance::Completed { effects, .. } => {
            assert_eq!(effects, vec![Effect::ReleaseInterlock { device_id: 42 }]);
        }
        Advance::Blocked { .. } => panic!("should advance once the reading is good"),
    }
}

// --------------------------------------------------------- failure paths

#[test]
fn retry_counts_down_then_quarantines() {
    let mut h = Harness::new(vec![Gate::Identity], FailurePath::Retry { max_attempts: 3 });
    h.scanned = None;

    for (attempt, remaining) in [(1, 2), (2, 1)] {
        h.attempt = attempt;
        match h.advance() {
            Advance::Blocked { disposition, .. } => {
                assert_eq!(
                    disposition,
                    Disposition::Retry {
                        attempts_remaining: remaining
                    }
                );
            }
            Advance::Completed { .. } => panic!("must not advance"),
        }
    }

    // Attempts exhausted: quarantine rather than loop forever.
    h.attempt = 3;
    match h.advance() {
        Advance::Blocked { disposition, .. } => assert_eq!(disposition, Disposition::Quarantine),
        Advance::Completed { .. } => panic!("must not advance"),
    }
}

#[test]
fn rework_path_returns_the_unit_and_voids_the_invalidated_work() {
    let mut h = Harness::new(
        vec![Gate::Identity],
        FailurePath::Rework {
            to_operation: 10,
            invalidates: vec![10, 20],
        },
    );
    h.scanned = None;
    let Advance::Blocked { disposition, .. } = h.advance() else {
        panic!("must not advance");
    };
    assert_eq!(
        disposition,
        Disposition::Rework {
            to_operation: 10,
            invalidates: vec![10, 20]
        }
    );

    let mut u = unit(UnitState::InProcess);
    apply_disposition(&mut u, &disposition).unwrap();
    assert_eq!(u.state, UnitState::Reworking);
    assert_eq!(u.current_operation, Some(10));
}

#[test]
fn scrap_path_retires_the_unit_permanently() {
    let mut h = Harness::new(vec![Gate::Identity], FailurePath::Scrap);
    h.scanned = None;
    let Advance::Blocked { disposition, .. } = h.advance() else {
        panic!("must not advance");
    };

    let mut u = unit(UnitState::InProcess);
    apply_disposition(&mut u, &disposition).unwrap();
    assert_eq!(u.state, UnitState::Scrapped);
    // And it can never come back.
    assert!(apply_disposition(&mut u, &Disposition::Quarantine).is_err());
}

// ------------------------------------------------------- station topology

#[test]
fn a_single_station_can_run_an_entire_route() {
    // One of the two extremes that break naive designs.
    let r = route(vec![
        route_op(10, OP_DEF_MARK, &[], vec![], FailurePath::Quarantine),
        route_op(20, OP_DEF_ASSY, &[10], vec![], FailurePath::Quarantine),
        route_op(30, OP_DEF_TEST, &[20], vec![], FailurePath::Quarantine),
    ]);
    let st = station(&[OP_DEF_MARK, OP_DEF_ASSY, OP_DEF_TEST]);
    let u = unit(UnitState::InProcess);

    let mut completed = BTreeSet::new();
    for expected in [10, 20, 30] {
        let op = RouteEngine
            .resolve_operation(&r, &u, &st, &completed)
            .unwrap();
        assert_eq!(op.seq, expected);
        completed.insert(op.seq);
    }
    assert!(r.is_complete(&completed));
}

#[test]
fn one_operation_duplicated_across_parallel_stations() {
    // The other extreme: whichever station is free takes the unit.
    let r = route(vec![route_op(
        10,
        OP_DEF_ASSY,
        &[],
        vec![],
        FailurePath::Quarantine,
    )]);
    let u = unit(UnitState::InProcess);
    let completed = BTreeSet::new();

    for id in [1, 2, 3] {
        let st = Station {
            id,
            code: code(&format!("ST-{id:02}")),
            ..station(&[OP_DEF_ASSY])
        };
        assert_eq!(
            RouteEngine
                .resolve_operation(&r, &u, &st, &completed)
                .unwrap()
                .seq,
            10
        );
    }
}

#[test]
fn a_station_not_equipped_for_the_next_operation_is_told_so() {
    let r = route(vec![route_op(
        10,
        OP_DEF_TEST,
        &[],
        vec![],
        FailurePath::Quarantine,
    )]);
    let st = station(&[OP_DEF_ASSY]);
    let u = unit(UnitState::InProcess);
    let err = RouteEngine
        .resolve_operation(&r, &u, &st, &BTreeSet::new())
        .unwrap_err();
    assert!(format!("{err}").contains("ST-01"));
}

#[test]
fn a_retired_station_accepts_no_new_work() {
    let r = route(vec![route_op(
        10,
        OP_DEF_ASSY,
        &[],
        vec![],
        FailurePath::Quarantine,
    )]);
    let st = Station {
        active: false,
        ..station(&[OP_DEF_ASSY])
    };
    let u = unit(UnitState::InProcess);
    assert!(
        RouteEngine
            .resolve_operation(&r, &u, &st, &BTreeSet::new())
            .is_err()
    );
}

#[test]
fn a_scrapped_unit_is_never_offered_work() {
    let r = route(vec![route_op(
        10,
        OP_DEF_ASSY,
        &[],
        vec![],
        FailurePath::Quarantine,
    )]);
    let st = station(&[OP_DEF_ASSY]);
    let u = unit(UnitState::Scrapped);
    assert!(
        RouteEngine
            .resolve_operation(&r, &u, &st, &BTreeSet::new())
            .is_err()
    );
}

#[test]
fn parallel_branches_are_both_offered_then_the_join_opens() {
    let r = route(vec![
        route_op(10, OP_DEF_MARK, &[], vec![], FailurePath::Quarantine),
        route_op(20, OP_DEF_ASSY, &[10], vec![], FailurePath::Quarantine),
        route_op(30, OP_DEF_TEST, &[10], vec![], FailurePath::Quarantine),
        route_op(40, OP_DEF_MARK, &[20, 30], vec![], FailurePath::Quarantine),
    ]);
    let u = unit(UnitState::InProcess);
    let completed: BTreeSet<OperationSeq> = [10].into_iter().collect();

    // Two differently equipped stations each pick up their own branch.
    let assy = station(&[OP_DEF_ASSY]);
    let test = station(&[OP_DEF_TEST]);
    assert_eq!(
        RouteEngine
            .resolve_operation(&r, &u, &assy, &completed)
            .unwrap()
            .seq,
        20
    );
    assert_eq!(
        RouteEngine
            .resolve_operation(&r, &u, &test, &completed)
            .unwrap()
            .seq,
        30
    );

    // The join is not reachable until both branches close.
    let mark = station(&[OP_DEF_MARK]);
    let partial: BTreeSet<OperationSeq> = [10, 20].into_iter().collect();
    assert!(
        RouteEngine
            .resolve_operation(&r, &u, &mark, &partial)
            .is_err()
    );
    let both: BTreeSet<OperationSeq> = [10, 20, 30].into_iter().collect();
    assert_eq!(
        RouteEngine
            .resolve_operation(&r, &u, &mark, &both)
            .unwrap()
            .seq,
        40
    );
}

// ------------------------------------------------------------ end to end

#[test]
fn a_unit_runs_a_five_operation_route_with_every_gate_type() {
    // The definition-of-done shape: born, marked, assembled, tested, completed,
    // across a mix of gates, with no hardware involved.
    let gates_by_seq: Vec<(OperationSeq, RowId, Vec<Gate>)> = vec![
        (10, OP_DEF_MARK, vec![Gate::Identity, Gate::MarkVerified]),
        (
            20,
            OP_DEF_ASSY,
            vec![
                Gate::Identity,
                Gate::OperatorAuth,
                Gate::Precondition,
                Gate::ComponentVerify {
                    bom_line_ids: vec![BOM_SCREW],
                },
            ],
        ),
        (30, OP_DEF_ASSY, vec![Gate::Precondition, Gate::DataCapture]),
        (
            40,
            OP_DEF_TEST,
            vec![
                Gate::Precondition,
                Gate::TestPass {
                    test_name: "EOL".into(),
                },
            ],
        ),
        (
            50,
            OP_DEF_TEST,
            vec![Gate::Precondition, Gate::InterlockOut { device_id: 42 }],
        ),
    ];

    let ops: Vec<RouteOperation> = gates_by_seq
        .iter()
        .enumerate()
        .map(|(i, (seq, def, gates))| {
            let preds: Vec<OperationSeq> = if i == 0 {
                vec![]
            } else {
                vec![gates_by_seq[i - 1].0]
            };
            route_op(*seq, *def, &preds, gates.clone(), FailurePath::Quarantine)
        })
        .collect();

    let r = route(ops);
    let mut u = unit(UnitState::Born);
    let rev = revision(MarkMethod::Label, None);
    let st = station(&[OP_DEF_MARK, OP_DEF_ASSY, OP_DEF_TEST]);
    let opr = operator(&[SKILL_TORQUE]);
    let dcps = vec![torque_dcp(true, SampleRule::Every)];
    let mut verdicts = BTreeMap::new();
    verdicts.insert("EOL".to_string(), Verdict::Pass);
    let mark = MarkVerification {
        decoded: u.uid.to_string(),
        grade: None,
    };
    let scans = vec![ComponentScan {
        part_no: code("SCR-M6-20"),
        bom_line_id: Some(BOM_SCREW),
        raw: "SCR-M6-20".into(),
    }];
    let captured = vec![CapturedValue {
        dcp_id: DCP_TORQUE,
        value: Value::Numeric(12.0),
        verdict: Verdict::Pass,
    }];

    u.transition_to(UnitState::InProcess).unwrap();
    let mut completed = BTreeSet::new();
    let mut released = vec![];

    for (seq, def, _) in &gates_by_seq {
        let operation = RouteEngine
            .resolve_operation(&r, &u, &st, &completed)
            .unwrap();
        assert_eq!(operation.seq, *seq);

        let def_row = op_def(*def, "op", None);
        let ctx = OperationContext {
            unit: &u,
            revision: &rev,
            route: &r,
            operation,
            operation_def: &def_row,
            station: &st,
            operator: Some(&opr),
            completed: &completed,
            scanned_identity: Some(u.uid),
            component_scans: &scans,
            captured: &captured,
            dcps: &dcps,
            test_verdicts: &verdicts,
            mark_verification: Some(&mark),
            attempt: 1,
        };

        match RouteEngine.try_advance(&ctx) {
            Advance::Completed {
                effects,
                route_complete,
                ..
            } => {
                released.extend(effects);
                completed.insert(*seq);
                if route_complete {
                    u.transition_to(UnitState::Completed).unwrap();
                }
            }
            Advance::Blocked { failures, .. } => {
                panic!("operation {seq} blocked unexpectedly: {failures:?}")
            }
        }
    }

    assert_eq!(u.state, UnitState::Completed);
    assert!(r.is_complete(&completed));
    assert_eq!(released, vec![Effect::ReleaseInterlock { device_id: 42 }]);
}

#[test]
fn tracking_modes_are_all_representable() {
    // Serial, lot and non-tracked must coexist on one BOM.
    let modes = [
        TrackingMode::Serialised,
        TrackingMode::LotTracked,
        TrackingMode::NonTracked,
    ];
    assert_eq!(modes.len(), 3);
}
