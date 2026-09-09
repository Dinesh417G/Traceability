//! Dry-run a route with no hardware.
//!
//! Define a route and a scenario as JSON, push a fake unit through it, and see
//! exactly which gates fired and where the unit ended up. No plant, no PLC, no
//! database, no printer.
//!
//! This is the cheapest place to find out that a route is wrong. Discovering a
//! missing precondition here costs a second; discovering it at commissioning
//! costs a day of a customer's line.
//!
//! ```bash
//! route-sim --scenario scenario.json          # run it
//! route-sim --example > scenario.json         # start from a working example
//! ```

#![warn(missing_docs)]

use anyhow::{Context as _, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use trace_core::config::{MarkMethod, Material, Operator, ProductRevision, Station};
use trace_core::dcp::{DataCollectionPoint, Value, Verdict};
use trace_core::engine::{
    Advance, CapturedValue, ComponentScan, MarkVerification, OperationContext, RouteEngine,
};
use trace_core::id::Code;
use trace_core::route::{OperationDef, OperationSeq, Route};
use trace_core::unit::{Unit, UnitState};

/// Command line.
#[derive(Debug, Parser)]
#[command(
    name = "route-sim",
    about = "Dry-run an ElectronIx Trace route with no hardware"
)]
struct Cli {
    /// Scenario file to run.
    #[arg(short, long)]
    scenario: Option<PathBuf>,

    /// Print a working example scenario and exit.
    #[arg(long)]
    example: bool,

    /// Print each gate result, not just the operation outcome.
    #[arg(short, long)]
    verbose: bool,
}

/// A complete, self-contained simulation input.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Scenario {
    /// Human-readable name.
    name: String,
    /// The route under test.
    route: Route,
    /// Operation templates referenced by the route.
    operation_defs: Vec<OperationDef>,
    /// Data collection points, by operation sequence.
    #[serde(default)]
    dcps: BTreeMap<String, Vec<DataCollectionPoint>>,
    /// Stations available, and what each is equipped for.
    stations: Vec<Station>,
    /// The operator at the terminal.
    operator: Operator,
    /// Product revision being built.
    revision: ProductRevision,
    /// Fixed unit identity, so a run is reproducible and a scenario can supply
    /// a matching mark read-back. Generated when absent.
    #[serde(default)]
    unit_uid: Option<String>,
    /// What the simulated station has gathered, by operation sequence.
    #[serde(default)]
    inputs: BTreeMap<String, OperationInput>,
}

/// What a station has to offer a given operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OperationInput {
    /// Whether the unit's identity was scanned.
    #[serde(default = "yes")]
    identity_scanned: bool,
    /// Components scanned here.
    #[serde(default)]
    component_scans: Vec<ComponentScan>,
    /// Values captured here.
    #[serde(default)]
    captured: Vec<CapturedValue>,
    /// Test verdicts available.
    #[serde(default)]
    test_verdicts: BTreeMap<String, Verdict>,
    /// Mark read-back result.
    #[serde(default)]
    mark_verification: Option<MarkVerification>,
}

fn yes() -> bool {
    true
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.example {
        println!("{}", serde_json::to_string_pretty(&example_scenario())?);
        return Ok(());
    }

    let scenario = match &cli.scenario {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str::<Scenario>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?
        }
        None => {
            eprintln!("no --scenario given; running the built-in example\n");
            example_scenario()
        }
    };

    let report = run(&scenario, cli.verbose)?;
    print!("{report}");

    if report.contains("BLOCKED") {
        // Non-zero exit so this is usable as a CI gate on route configuration.
        std::process::exit(1);
    }
    Ok(())
}

/// Push one unit through the whole route, reporting as it goes.
fn run(s: &Scenario, verbose: bool) -> anyhow::Result<String> {
    // Validate first: a cycle or a dangling predecessor must surface here, not
    // in the middle of a shift.
    s.route.validate().context("route definition is invalid")?;

    let defs: BTreeMap<i64, &OperationDef> = s.operation_defs.iter().map(|d| (d.id, d)).collect();
    let mut out = String::new();
    out.push_str(&format!("route-sim: {}\n", s.name));
    out.push_str(&format!(
        "route {} on line {}, {} operations, {} stations\n\n",
        s.route.revision,
        s.route.line_id,
        s.route.operations.len(),
        s.stations.len()
    ));

    let uid = match &s.unit_uid {
        Some(text) => trace_core::UnitUid::parse(text)
            .with_context(|| format!("scenario unit_uid {text:?} is not a valid ULID"))?,
        None => trace_core::UnitUid::generate(),
    };

    let mut unit = Unit {
        id: 1,
        uid,
        tenant_id: 1,
        plant_id: 1,
        product_revision_id: s.revision.id,
        job_card_id: 1,
        serial: "SIM-0000000001".into(),
        state: UnitState::InProcess,
        current_operation: None,
        index_in_job: 0,
    };
    out.push_str(&format!("unit {} ({})\n\n", unit.uid, unit.serial));

    let mut completed: BTreeSet<OperationSeq> = BTreeSet::new();
    let engine = RouteEngine;
    let mut guard = 0usize;

    loop {
        guard += 1;
        if guard > s.route.operations.len() * 4 {
            bail!("simulation did not converge; the route may loop");
        }

        // Which station can take this unit next?
        let Some((station, operation)) = s.stations.iter().find_map(|st| {
            engine
                .resolve_operation(&s.route, &unit, st, &completed)
                .ok()
                .map(|op| (st, op))
        }) else {
            break;
        };

        let def = defs
            .get(&operation.operation_def_id)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "operation {} references unknown def {}",
                    operation.seq,
                    operation.operation_def_id
                )
            })?;

        let key = operation.seq.to_string();
        let input = s.inputs.get(&key).cloned().unwrap_or_default();
        let dcps = s.dcps.get(&key).cloned().unwrap_or_default();

        let ctx = OperationContext {
            unit: &unit,
            revision: &s.revision,
            route: &s.route,
            operation,
            operation_def: def,
            station,
            operator: Some(&s.operator),
            completed: &completed,
            scanned_identity: input.identity_scanned.then_some(unit.uid),
            component_scans: &input.component_scans,
            captured: &input.captured,
            dcps: &dcps,
            test_verdicts: &input.test_verdicts,
            mark_verification: input.mark_verification.as_ref(),
            attempt: 1,
        };

        match engine.try_advance(&ctx) {
            Advance::Completed {
                gates,
                effects,
                next_ready,
                route_complete,
            } => {
                out.push_str(&format!(
                    "  op {:>3} {:<24} at {:<8} PASSED\n",
                    operation.seq, def.name, station.code
                ));
                if verbose {
                    for g in &gates {
                        out.push_str(&format!("        gate {:<18} ok\n", g.gate));
                    }
                }
                for e in &effects {
                    out.push_str(&format!("        effect {e:?}\n"));
                }
                completed.insert(operation.seq);
                unit.current_operation = Some(operation.seq);

                if route_complete {
                    unit.transition_to(UnitState::Completed)?;
                    out.push_str("\nroute complete\n");
                    break;
                }
                if verbose && !next_ready.is_empty() {
                    out.push_str(&format!("        next ready: {next_ready:?}\n"));
                }
            }

            Advance::Blocked {
                failures,
                disposition,
                ..
            } => {
                out.push_str(&format!(
                    "  op {:>3} {:<24} at {:<8} BLOCKED\n",
                    operation.seq, def.name, station.code
                ));
                for f in &failures {
                    out.push_str(&format!(
                        "        gate {:<18} FAILED: {}\n",
                        f.gate,
                        f.reason.as_deref().unwrap_or("(no reason)")
                    ));
                }
                out.push_str(&format!("        disposition: {disposition:?}\n"));
                trace_core::engine::apply_disposition(&mut unit, &disposition)?;
                break;
            }
        }
    }

    out.push_str(&format!("\nfinal state: {}\n", unit.state.name()));
    out.push_str(&format!("completed operations: {completed:?}\n"));
    Ok(out)
}

/// A working scenario that exercises most gate types.
fn example_scenario() -> Scenario {
    use trace_core::dcp::{DataType, Limits, SampleRule};
    use trace_core::route::{FailurePath, Gate, Requirement, RouteOperation};

    let code = |s: &str| Code::new(s).expect("valid code in the built-in example");

    let ops = vec![
        RouteOperation {
            seq: 10,
            operation_def_id: 100,
            requirement: Requirement::Required,
            predecessors: vec![],
            gates: vec![Gate::Identity, Gate::MarkVerified],
            failure_path: FailurePath::Retry { max_attempts: 3 },
        },
        RouteOperation {
            seq: 20,
            operation_def_id: 200,
            requirement: Requirement::Required,
            predecessors: vec![10],
            gates: vec![
                Gate::Identity,
                Gate::OperatorAuth,
                Gate::Precondition,
                Gate::ComponentVerify {
                    bom_line_ids: vec![11],
                },
            ],
            failure_path: FailurePath::Quarantine,
        },
        RouteOperation {
            seq: 30,
            operation_def_id: 300,
            requirement: Requirement::Required,
            predecessors: vec![20],
            gates: vec![Gate::Precondition, Gate::DataCapture],
            failure_path: FailurePath::Rework {
                to_operation: 20,
                invalidates: vec![20, 30],
            },
        },
        RouteOperation {
            seq: 40,
            operation_def_id: 400,
            requirement: Requirement::Required,
            predecessors: vec![30],
            gates: vec![
                Gate::Precondition,
                Gate::TestPass {
                    test_name: "EOL".into(),
                },
                Gate::InterlockOut { device_id: 42 },
            ],
            failure_path: FailurePath::Quarantine,
        },
    ];

    // A fixed identity so the example is reproducible and the mark read-back
    // can legitimately match.
    let example_uid = "01JX8P2K7F3QW9ABCDEFGHJKMN".to_string();

    let mut inputs = BTreeMap::new();
    inputs.insert(
        "10".to_string(),
        OperationInput {
            identity_scanned: true,
            mark_verification: Some(MarkVerification {
                decoded: example_uid.clone(),
                grade: Some('B'),
            }),
            ..Default::default()
        },
    );
    inputs.insert(
        "20".to_string(),
        OperationInput {
            identity_scanned: true,
            component_scans: vec![ComponentScan {
                part_no: code("SCR-M6-20"),
                bom_line_id: Some(11),
                raw: "SCR-M6-20".into(),
            }],
            ..Default::default()
        },
    );
    inputs.insert(
        "30".to_string(),
        OperationInput {
            captured: vec![CapturedValue {
                dcp_id: 1,
                value: Value::Numeric(12.1),
                verdict: Verdict::Pass,
            }],
            ..Default::default()
        },
    );
    let mut verdicts = BTreeMap::new();
    verdicts.insert("EOL".to_string(), Verdict::Pass);
    inputs.insert(
        "40".to_string(),
        OperationInput {
            test_verdicts: verdicts,
            ..Default::default()
        },
    );

    let mut dcps = BTreeMap::new();
    dcps.insert(
        "30".to_string(),
        vec![DataCollectionPoint {
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
        }],
    );

    Scenario {
        name: "EX-VLV-2200 rev A, four operations, one station".into(),
        route: Route {
            id: 1,
            product_revision_id: 1,
            line_id: 1,
            revision: "A".into(),
            operations: ops,
        },
        operation_defs: vec![
            OperationDef {
                id: 100,
                name: "label_and_verify".into(),
                label: "Label".into(),
                dcp_ids: vec![],
                required_skill: None,
            },
            OperationDef {
                id: 200,
                name: "assemble".into(),
                label: "Assemble".into(),
                dcp_ids: vec![],
                required_skill: None,
            },
            OperationDef {
                id: 300,
                name: "torque".into(),
                label: "Torque".into(),
                dcp_ids: vec![1],
                required_skill: None,
            },
            OperationDef {
                id: 400,
                name: "eol_test".into(),
                label: "EOL test".into(),
                dcp_ids: vec![],
                required_skill: None,
            },
        ],
        dcps,
        stations: vec![Station {
            id: 1,
            line_id: 1,
            code: code("ST-01"),
            name: "Single terminal running the whole route".into(),
            operations: [100, 200, 300, 400].into_iter().collect(),
            active: true,
        }],
        operator: Operator {
            id: 7,
            tenant_id: 1,
            username: "R.KUMAR".into(),
            display_name: "R. Kumar".into(),
            certifications: BTreeSet::new(),
            active: true,
        },
        revision: ProductRevision {
            id: 1,
            product_id: 1,
            revision: code("A"),
            material: Material::Aluminium,
            mark_method: MarkMethod::Label,
            min_mark_grade: Some('C'),
            active: true,
        },
        inputs,
        unit_uid: Some(example_uid),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn runnable_example() -> Scenario {
        example_scenario()
    }

    #[test]
    fn the_built_in_example_is_a_valid_route() {
        example_scenario().route.validate().unwrap();
    }

    #[test]
    fn a_good_scenario_runs_the_whole_route_to_completion() {
        let out = run(&runnable_example(), true).unwrap();
        assert!(out.contains("route complete"), "{out}");
        assert!(out.contains("final state: COMPLETED"), "{out}");
        assert!(!out.contains("BLOCKED"), "{out}");
        // The interlock fires only at the last operation.
        assert!(out.contains("ReleaseInterlock"), "{out}");
    }

    #[test]
    fn a_missing_component_scan_blocks_and_names_the_gate() {
        let mut s = runnable_example();
        s.inputs.get_mut("20").unwrap().component_scans.clear();
        let out = run(&s, false).unwrap();
        assert!(out.contains("BLOCKED"), "{out}");
        assert!(out.contains("COMPONENT_VERIFY"), "{out}");
        assert!(out.contains("Quarantine"), "{out}");
    }

    #[test]
    fn an_out_of_limits_reading_takes_the_configured_rework_path() {
        let mut s = runnable_example();
        s.inputs.get_mut("30").unwrap().captured = vec![CapturedValue {
            dcp_id: 1,
            value: Value::Numeric(18.0),
            verdict: Verdict::Fail,
        }];
        let out = run(&s, false).unwrap();
        assert!(out.contains("DATA_CAPTURE"), "{out}");
        assert!(out.contains("Rework"), "{out}");
        assert!(out.contains("final state: REWORKING"), "{out}");
    }

    #[test]
    fn a_wrong_part_is_reported_as_a_hard_stop() {
        let mut s = runnable_example();
        s.inputs
            .get_mut("20")
            .unwrap()
            .component_scans
            .push(ComponentScan {
                part_no: Code::new("WRONG-PART").unwrap(),
                bom_line_id: None,
                raw: "WRONG-PART".into(),
            });
        let out = run(&s, false).unwrap();
        assert!(out.contains("not on the BOM"), "{out}");
    }

    #[test]
    fn an_invalid_route_is_rejected_before_anything_runs() {
        let mut s = runnable_example();
        // Point an operation at a predecessor that does not exist.
        s.route.operations[1].predecessors = vec![999];
        let err = run(&s, false).unwrap_err();
        assert!(format!("{err:#}").contains("invalid"), "{err:#}");
    }

    #[test]
    fn a_scenario_round_trips_through_json() {
        // Scenarios are written by integrators by hand, so the format must be
        // stable and self-describing.
        let s = example_scenario();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back.route.operations.len(), s.route.operations.len());
        assert_eq!(back.name, s.name);
    }
}
