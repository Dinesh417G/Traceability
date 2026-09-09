//! Integration tests against a real PostgreSQL with the real migrations.
//!
//! These are `#[ignore]`d by default and run with:
//!
//! ```bash
//! TRACE_TEST_DATABASE_URL=postgres://trace_app:pw@localhost/trace_test \
//!   cargo test -p trace-store -- --include-ignored
//! ```
//!
//! **The role in that URL must not be a superuser.** Superusers bypass RLS, so
//! running these as one would make the isolation tests silently vacuous.
#![allow(clippy::unwrap_used)]

use chrono::{DateTime, TimeZone, Utc};
use sqlx::Row;
use trace_core::clock::Stamps;
use trace_core::dcp::{Value, ValueSource, Verdict};
use trace_core::unit::{EventKind, MarkOutcome, UnitState};
use trace_store::repo::{NewEvent, NewMeasurement, NewUnit};
use trace_store::{Store, TenantTx};

fn ts(offset_secs: i64) -> Stamps {
    let base: DateTime<Utc> = Utc.timestamp_opt(1_760_000_000, 0).unwrap();
    let t = base + chrono::Duration::seconds(offset_secs);
    Stamps::new(t, t)
}

/// Connect and migrate, or return `None` when no test database is configured.
async fn store() -> Option<Store> {
    let url = std::env::var("TRACE_TEST_DATABASE_URL").ok()?;
    let s = Store::connect(&url)
        .await
        .expect("connect to test database");
    s.migrate().await.expect("apply migrations");
    Some(s)
}

/// Guard against the classic silent failure: a superuser bypasses RLS, which
/// would make every isolation assertion below pass for the wrong reason.
async fn assert_not_superuser(s: &Store) {
    let row = sqlx::query("SELECT usesuper FROM pg_user WHERE usename = current_user")
        .fetch_one(s.pool())
        .await
        .unwrap();
    let is_super: bool = row.get("usesuper");
    assert!(
        !is_super,
        "TRACE_TEST_DATABASE_URL must use a non-superuser role: superusers bypass RLS \
         and would make these isolation tests meaningless"
    );
}

/// A provisioned tenant with the minimum configuration to birth units.
struct Fixture {
    tenant_id: i64,
    plant_id: i64,
    station_id: i64,
    operator_id: i64,
    revision_id: i64,
    job_card_id: i64,
    dcp_id: i64,
    device_id: i64,
}

/// Seed one tenant's configuration. Uses a unique suffix so tests can share a
/// database without colliding on natural keys.
async fn seed(s: &Store, tag: &str) -> Fixture {
    // Provisioning replaces the transaction's tenant context by design, so it
    // gets a transaction of its own.
    let mut tx = s.pool().begin().await.unwrap();
    let tenant_id: i64 = sqlx::query("SELECT trace.provision_tenant($1, $2) AS id")
        .bind(format!("T-{tag}"))
        .bind(format!("Tenant {tag}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get("id");
    tx.commit().await.unwrap();

    let mut t = s.tenant(tenant_id).await.unwrap();
    let ex = t.executor();

    let plant_id: i64 = sqlx::query(
        "INSERT INTO trace.plant (tenant_id, code, name, trace_base_url)
         VALUES ($1,$2,'Plant','http://trace.plant.local') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("P-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let line_id: i64 = sqlx::query(
        "INSERT INTO trace.line (tenant_id, plant_id, code, name)
         VALUES ($1,$2,$3,'Line') RETURNING id",
    )
    .bind(tenant_id)
    .bind(plant_id)
    .bind(format!("L-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let station_id: i64 = sqlx::query(
        "INSERT INTO trace.station (tenant_id, line_id, code, name)
         VALUES ($1,$2,$3,'Station') RETURNING id",
    )
    .bind(tenant_id)
    .bind(line_id)
    .bind(format!("ST-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let operator_id: i64 = sqlx::query(
        "INSERT INTO trace.app_user (tenant_id, username, display_name)
         VALUES ($1,$2,'R. Kumar') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("U-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let product_id: i64 = sqlx::query(
        "INSERT INTO trace.product (tenant_id, model_no, name)
         VALUES ($1,$2,'Valve') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("EX-VLV-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let revision_id: i64 = sqlx::query(
        "INSERT INTO trace.product_revision (tenant_id, product_id, revision, material, mark_method)
         VALUES ($1,$2,'A','ALUMINIUM','LABEL') RETURNING id",
    )
    .bind(tenant_id)
    .bind(product_id)
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let job_card_id: i64 = sqlx::query(
        "INSERT INTO trace.job_card (tenant_id, plant_id, number, product_revision_id, qty)
         VALUES ($1,$2,$3,$4,100) RETURNING id",
    )
    .bind(tenant_id)
    .bind(plant_id)
    .bind(format!("JC-{tag}"))
    .bind(revision_id)
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let device_id: i64 = sqlx::query(
        "INSERT INTO trace.device (tenant_id, plant_id, station_id, code, name, kind)
         VALUES ($1,$2,$3,$4,'Torque tool','SERIAL_ASCII') RETURNING id",
    )
    .bind(tenant_id)
    .bind(plant_id)
    .bind(station_id)
    .bind(format!("DEV-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let op_def_id: i64 = sqlx::query(
        "INSERT INTO trace.operation_def (tenant_id, name, label)
         VALUES ($1,$2,'Torque') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("op-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let dcp_id: i64 = sqlx::query(
        "INSERT INTO trace.data_collection_point
             (tenant_id, operation_def_id, name, label, unit, datatype,
              min_value, max_value, nominal_value, mandatory, device_id)
         VALUES ($1,$2,'final_torque','Final torque','Nm','NUMERIC',10,14,12,true,$3)
         RETURNING id",
    )
    .bind(tenant_id)
    .bind(op_def_id)
    .bind(device_id)
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    t.commit().await.unwrap();

    Fixture {
        tenant_id,
        plant_id,
        station_id,
        operator_id,
        revision_id,
        job_card_id,
        dcp_id,
        device_id,
    }
}

async fn birth_unit(t: &mut TenantTx<'_>, f: &Fixture, serial: &str, idx: i64) -> (i64, String) {
    let uid = trace_core::UnitUid::generate();
    let id = t
        .insert_unit(&NewUnit {
            uid,
            plant_id: f.plant_id,
            product_revision_id: f.revision_id,
            job_card_id: f.job_card_id,
            serial: serial.to_owned(),
            index_in_job: idx,
        })
        .await
        .unwrap();
    (id, uid.to_string())
}

// ===================================================================== RLS

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn cross_tenant_reads_return_nothing() {
    let Some(s) = store().await else { return };
    assert_not_superuser(&s).await;

    let a = seed(&s, &format!("a{}", short_tag())).await;
    let b = seed(&s, &format!("b{}", short_tag())).await;

    // Tenant A births a unit.
    let mut t = s.tenant(a.tenant_id).await.unwrap();
    let (_id, uid) = birth_unit(&mut t, &a, "A-0001", 0).await;
    t.commit().await.unwrap();

    // Tenant A can see it.
    let mut t = s.tenant(a.tenant_id).await.unwrap();
    assert!(
        t.unit_by_uid(&uid).await.unwrap().is_some(),
        "owner must see its own unit"
    );
    t.commit().await.unwrap();

    // Tenant B cannot, even knowing the exact UID.
    let mut t = s.tenant(b.tenant_id).await.unwrap();
    assert!(
        t.unit_by_uid(&uid).await.unwrap().is_none(),
        "RLS must hide another tenant's unit even when the UID is known"
    );
    t.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn an_unset_tenant_context_denies_everything() {
    // The failure mode that matters: a bug that forgets the context must
    // return nothing, never everything.
    let Some(s) = store().await else { return };
    assert_not_superuser(&s).await;

    let f = seed(&s, &format!("n{}", short_tag())).await;
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (_id, uid) = birth_unit(&mut t, &f, "N-0001", 0).await;
    t.commit().await.unwrap();

    // Query with no SET LOCAL at all.
    let n: i64 = sqlx::query("SELECT count(*) AS n FROM trace.unit WHERE uid = $1")
        .bind(&uid)
        .fetch_one(s.pool())
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 0, "an unset tenant context must deny all rows");
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn cross_tenant_writes_are_rejected() {
    let Some(s) = store().await else { return };
    assert_not_superuser(&s).await;

    let a = seed(&s, &format!("wa{}", short_tag())).await;
    let b = seed(&s, &format!("wb{}", short_tag())).await;

    // Tenant B tries to insert a row tagged as tenant A's.
    let mut t = s.tenant(b.tenant_id).await.unwrap();
    let res = sqlx::query(
        "INSERT INTO trace.plant (tenant_id, code, name, trace_base_url)
         VALUES ($1,'HOSTILE','x','http://x') ",
    )
    .bind(a.tenant_id)
    .execute(t.executor())
    .await;
    assert!(
        res.is_err(),
        "WITH CHECK must reject a write attributed to another tenant"
    );
    t.rollback().await.ok();
}

// ============================================================ append-only

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn measurements_cannot_be_updated_or_deleted() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("ao{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, _uid) = birth_unit(&mut t, &f, "AO-0001", 0).await;
    t.append_measurement(&NewMeasurement {
        unit_id,
        plant_id: f.plant_id,
        dcp_id: f.dcp_id,
        operation_seq: 10,
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        device_id: Some(f.device_id),
        value: Value::Numeric(12.1),
        verdict: Verdict::Pass,
        source: ValueSource::Device,
        raw_payload: Some("T:12.1NM\r\n".into()),
        stamps: ts(0),
        supersedes: None,
        supersede_reason: None,
    })
    .await
    .unwrap();
    t.commit().await.unwrap();

    // An operator "fixing" a failing reading must be refused by the database.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let upd = sqlx::query("UPDATE trace.measurement SET num_value = 99 WHERE unit_id = $1")
        .bind(unit_id)
        .execute(t.executor())
        .await;
    assert!(upd.is_err(), "measurements must be immutable");
    t.rollback().await.ok();

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let del = sqlx::query("DELETE FROM trace.measurement WHERE unit_id = $1")
        .bind(unit_id)
        .execute(t.executor())
        .await;
    assert!(del.is_err(), "measurements must not be deletable");
    t.rollback().await.ok();
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn corrections_are_new_rows_that_supersede_old_ones() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("cor{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, uid) = birth_unit(&mut t, &f, "COR-0001", 0).await;

    let base = NewMeasurement {
        unit_id,
        plant_id: f.plant_id,
        dcp_id: f.dcp_id,
        operation_seq: 10,
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        device_id: Some(f.device_id),
        value: Value::Numeric(9.0),
        verdict: Verdict::Fail,
        source: ValueSource::Device,
        raw_payload: Some("T:9.0NM".into()),
        stamps: ts(0),
        supersedes: None,
        supersede_reason: None,
    };
    t.append_measurement(&base).await.unwrap();

    let original_id: String = sqlx::query(
        "SELECT id FROM trace.measurement WHERE unit_id = $1 ORDER BY chain_seq LIMIT 1",
    )
    .bind(unit_id)
    .fetch_one(t.executor())
    .await
    .unwrap()
    .get("id");

    t.append_measurement(&NewMeasurement {
        value: Value::Numeric(12.1),
        verdict: Verdict::Pass,
        stamps: ts(60),
        supersedes: Some(trace_core::MeasurementId::parse(&original_id).unwrap()),
        supersede_reason: Some("wrong tool selected; re-measured with calibrated wrench".into()),
        ..base
    })
    .await
    .unwrap();
    t.commit().await.unwrap();

    // Both rows survive: the original failure is still visible in the trace.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let h = t.unit_history(&uid).await.unwrap().unwrap();
    assert_eq!(
        h.measurements.len(),
        2,
        "the superseded reading must remain visible"
    );
    assert_eq!(h.measurements[0].verdict, "FAIL");
    assert_eq!(h.measurements[1].verdict, "PASS");
    assert_eq!(
        h.measurements[1].supersedes.as_deref(),
        Some(original_id.as_str())
    );
    t.commit().await.unwrap();
}

// ========================================================== retired UIDs

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn a_scrapped_uid_is_retired_forever() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("sc{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, uid) = birth_unit(&mut t, &f, "SC-0001", 0).await;
    // A marked raw part can fail incoming inspection before any assembly.
    t.set_unit_state(unit_id, UnitState::Scrapped, None)
        .await
        .unwrap();
    t.commit().await.unwrap();

    // The UID is on the retired ledger.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let n: i64 = sqlx::query("SELECT count(*) AS n FROM trace.retired_uid WHERE uid = $1")
        .bind(&uid)
        .fetch_one(t.executor())
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 1, "scrapping must retire the uid");
    t.commit().await.unwrap();

    // Reissuing it is refused.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let res = sqlx::query(
        "INSERT INTO trace.unit (uid, tenant_id, plant_id, product_revision_id,
                                 job_card_id, serial, state, index_in_job)
         VALUES ($1,$2,$3,$4,$5,'REISSUE','BORN',1)",
    )
    .bind(&uid)
    .bind(f.tenant_id)
    .bind(f.plant_id)
    .bind(f.revision_id)
    .bind(f.job_card_id)
    .execute(t.executor())
    .await;
    assert!(res.is_err(), "a retired uid must never be reissued");
    t.rollback().await.ok();

    // And the unit cannot be resurrected.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let res = t.set_unit_state(unit_id, UnitState::InProcess, None).await;
    assert!(res.is_err(), "scrapped is terminal");
    t.rollback().await.ok();
}

// =========================================================== hash chain

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn events_and_measurements_share_one_verifiable_chain() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("hc{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, _uid) = birth_unit(&mut t, &f, "HC-0001", 0).await;

    let ev = |kind, off| NewEvent {
        unit_id,
        plant_id: f.plant_id,
        kind,
        operation_seq: Some(10),
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        detail: None,
        stamps: ts(off),
    };

    let l0 = t.append_event(&ev(EventKind::Born, 0)).await.unwrap();
    let l1 = t
        .append_measurement(&NewMeasurement {
            unit_id,
            plant_id: f.plant_id,
            dcp_id: f.dcp_id,
            operation_seq: 10,
            station_id: Some(f.station_id),
            operator_id: Some(f.operator_id),
            device_id: Some(f.device_id),
            value: Value::Numeric(12.1),
            verdict: Verdict::Pass,
            source: ValueSource::Device,
            raw_payload: Some("T:12.1NM".into()),
            stamps: ts(10),
            supersedes: None,
            supersede_reason: None,
        })
        .await
        .unwrap();
    let l2 = t
        .append_event(&ev(EventKind::OperationCompleted, 20))
        .await
        .unwrap();

    // One chain, interleaved across both tables.
    assert_eq!((l0.seq, l1.seq, l2.seq), (0, 1, 2));
    assert_eq!(
        l1.prev_hash, l0.row_hash,
        "the measurement chains onto the event"
    );
    assert_eq!(
        l2.prev_hash, l1.row_hash,
        "the next event chains onto the measurement"
    );

    let links = t.verify_unit_chain(unit_id).await.unwrap();
    assert_eq!(links, 3);
    t.commit().await.unwrap();
}

// ======================================================== recall queries

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn recall_by_lot_finds_units_through_sub_assemblies() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("rc{}", short_tag())).await;
    let lot = format!("LOT-BAD-{}", short_tag());

    let mut t = s.tenant(f.tenant_id).await.unwrap();

    // A bad lot of screws goes into a sub-assembly, which goes into a finished
    // unit. The recall must find the finished unit, not just the sub-assembly.
    let (sub, sub_uid) = birth_unit(&mut t, &f, "SUB-0001", 0).await;
    let (top, top_uid) = birth_unit(&mut t, &f, "TOP-0001", 1).await;
    let (clean, clean_uid) = birth_unit(&mut t, &f, "CLEAN-0001", 2).await;

    t.link_lot(sub, &lot, 4.0, None, f.plant_id, 10, ts(0))
        .await
        .unwrap();
    t.link_child_unit(top, sub, None, f.plant_id, 20, ts(10))
        .await
        .unwrap();
    // An unrelated unit consumed a different lot and must not be implicated.
    t.link_lot(clean, "LOT-GOOD", 4.0, None, f.plant_id, 10, ts(20))
        .await
        .unwrap();
    t.commit().await.unwrap();

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let affected = t.units_affected_by_lot(&lot).await.unwrap();
    let uids: Vec<&str> = affected.iter().map(|a| a.uid.as_str()).collect();

    assert!(
        uids.contains(&sub_uid.as_str()),
        "the sub-assembly consumed the lot directly"
    );
    assert!(
        uids.contains(&top_uid.as_str()),
        "the finished unit consumed it indirectly and is what actually ships"
    );
    assert!(
        !uids.contains(&clean_uid.as_str()),
        "unrelated units must not be implicated"
    );
    assert_eq!(affected.len(), 2);
    t.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn recall_by_device_and_time_window() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("dv{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (inside, inside_uid) = birth_unit(&mut t, &f, "IN-0001", 0).await;
    let (outside, outside_uid) = birth_unit(&mut t, &f, "OUT-0001", 1).await;

    let m = |unit_id, off| NewMeasurement {
        unit_id,
        plant_id: f.plant_id,
        dcp_id: f.dcp_id,
        operation_seq: 10,
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        device_id: Some(f.device_id),
        value: Value::Numeric(12.0),
        verdict: Verdict::Pass,
        source: ValueSource::Device,
        raw_payload: None,
        stamps: ts(off),
        supersedes: None,
        supersede_reason: None,
    };
    t.append_measurement(&m(inside, 100)).await.unwrap();
    t.append_measurement(&m(outside, 100_000)).await.unwrap();
    t.commit().await.unwrap();

    let base: DateTime<Utc> = Utc.timestamp_opt(1_760_000_000, 0).unwrap();
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let hits = t
        .units_measured_by_device(f.device_id, base, base + chrono::Duration::seconds(1_000))
        .await
        .unwrap();
    let uids: Vec<&str> = hits.iter().map(|a| a.uid.as_str()).collect();
    assert!(uids.contains(&inside_uid.as_str()));
    assert!(
        !uids.contains(&outside_uid.as_str()),
        "outside the drift window"
    );
    t.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn recall_query_uses_an_index_at_realistic_volume() {
    // The spec requires recall to be a fast indexed query rather than a table
    // scan. Proving that needs enough rows for the index to actually win: on a
    // 50-row table a sequential scan is genuinely cheaper and Postgres is
    // right to choose one, so a small fixture makes this assertion vacuous.
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("pl{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (u, _) = birth_unit(&mut t, &f, "PLAN-0001", 0).await;

    const ROWS: i32 = 20_000;
    sqlx::query(
        "INSERT INTO trace.genealogy_link
             (tenant_id, plant_id, parent_unit_id, lot_no, qty, operation_seq,
              recorded_at, received_at)
         SELECT $1, $2, $3, 'LOT-' || g, 1.0, 10, now(), now()
           FROM generate_series(1, $4) g",
    )
    .bind(f.tenant_id)
    .bind(f.plant_id)
    .bind(u)
    .bind(ROWS)
    .execute(t.executor())
    .await
    .unwrap();
    t.commit().await.unwrap();

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    sqlx::query("ANALYZE trace.genealogy_link")
        .execute(t.executor())
        .await
        .ok();

    let plan: String = sqlx::query(
        "EXPLAIN (FORMAT TEXT)
         SELECT parent_unit_id FROM trace.genealogy_link
          WHERE tenant_id = $1 AND lot_no = $2",
    )
    .bind(f.tenant_id)
    .bind("LOT-7")
    .fetch_all(t.executor())
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get::<String, _>(0))
    .collect::<Vec<_>>()
    .join("\n");

    assert!(
        plan.contains("genealogy_lot_idx"),
        "the recall seed lookup must use genealogy_lot_idx, got plan:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on genealogy_link"),
        "recall must not fall back to a table scan, got plan:\n{plan}"
    );

    // And the whole recall, including the recursive roll-up to parents, must
    // come back well inside the 2 second budget.
    let started = std::time::Instant::now();
    let affected = t.units_affected_by_lot("LOT-7").await.unwrap();
    let elapsed = started.elapsed();
    t.commit().await.unwrap();

    assert_eq!(affected.len(), 1);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "recall took {elapsed:?}, budget is 2s"
    );
}

// ============================================================ mark records

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn a_reprint_without_a_reason_and_authoriser_is_refused() {
    // Uncontrolled duplicate labels in the field are a real traceability
    // failure, so this is a constraint rather than a UI convention.
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("mk{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, _uid) = birth_unit(&mut t, &f, "MK-0001", 0).await;

    // First print: fine, no reason needed.
    t.record_mark(
        unit_id,
        f.plant_id,
        None,
        None,
        Some(10),
        1,
        MarkOutcome::Verified,
        Some("readback"),
        Some('B'),
        "http://trace.plant.local",
        false,
        None,
        None,
        Some("^XA...^XZ"),
        ts(0),
    )
    .await
    .unwrap();
    t.commit().await.unwrap();

    // Reprint with no reason or authoriser: refused.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let bad = t
        .record_mark(
            unit_id,
            f.plant_id,
            None,
            None,
            Some(10),
            2,
            MarkOutcome::Verified,
            None,
            None,
            "http://trace.plant.local",
            true,
            None,
            None,
            None,
            ts(60),
        )
        .await;
    assert!(bad.is_err(), "an uncontrolled reprint must be refused");
    t.rollback().await.ok();

    // Reprint with both: allowed and recorded.
    let mut t = s.tenant(f.tenant_id).await.unwrap();
    t.record_mark(
        unit_id,
        f.plant_id,
        None,
        None,
        Some(10),
        2,
        MarkOutcome::Verified,
        Some("readback"),
        Some('B'),
        "http://trace.plant.local",
        true,
        Some("label damaged in handling"),
        Some(f.operator_id),
        None,
        ts(60),
    )
    .await
    .unwrap();
    t.commit().await.unwrap();
}

// ================================================================= outbox

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn outbox_accumulates_from_day_one() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("ob{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    t.enqueue_outbox(f.plant_id, "unit.born", &serde_json::json!({"uid": "X"}))
        .await
        .unwrap();
    t.enqueue_outbox(
        f.plant_id,
        "unit.completed",
        &serde_json::json!({"uid": "X"}),
    )
    .await
    .unwrap();
    assert_eq!(t.outbox_pending().await.unwrap(), 2);
    t.commit().await.unwrap();
}

// ========================================================== full history

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn backward_trace_returns_the_full_birth_certificate() {
    let Some(s) = store().await else { return };
    let f = seed(&s, &format!("bt{}", short_tag())).await;

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let (unit_id, uid) = birth_unit(&mut t, &f, "EX24A0001537", 0).await;

    t.append_event(&NewEvent {
        unit_id,
        plant_id: f.plant_id,
        kind: EventKind::Born,
        operation_seq: Some(10),
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        detail: None,
        stamps: ts(0),
    })
    .await
    .unwrap();

    t.append_measurement(&NewMeasurement {
        unit_id,
        plant_id: f.plant_id,
        dcp_id: f.dcp_id,
        operation_seq: 20,
        station_id: Some(f.station_id),
        operator_id: Some(f.operator_id),
        device_id: Some(f.device_id),
        value: Value::Numeric(12.1),
        verdict: Verdict::Pass,
        source: ValueSource::Device,
        raw_payload: Some("T:12.1NM\r\n".into()),
        stamps: ts(30),
        supersedes: None,
        supersede_reason: None,
    })
    .await
    .unwrap();

    t.link_lot(unit_id, "LOT-SCREW-42", 4.0, None, f.plant_id, 20, ts(25))
        .await
        .unwrap();
    t.commit().await.unwrap();

    let mut t = s.tenant(f.tenant_id).await.unwrap();
    let h = t.unit_history(&uid).await.unwrap().expect("history");

    assert_eq!(h.serial, "EX24A0001537");
    assert_eq!(h.events.len(), 1);
    assert_eq!(
        h.events[0].station.as_deref().map(str::len),
        Some(h.events[0].station.as_ref().unwrap().len())
    );
    assert_eq!(h.events[0].operator.as_deref(), Some("R. Kumar"));
    assert_eq!(h.measurements.len(), 1);
    assert_eq!(h.measurements[0].dcp_name, "final_torque");
    assert_eq!(h.measurements[0].unit_of_measure.as_deref(), Some("Nm"));
    // The raw device bytes are the answer to a dispute in two years.
    assert_eq!(
        h.measurements[0].raw_payload.as_deref(),
        Some("T:12.1NM\r\n")
    );
    assert_eq!(h.components.len(), 1);
    assert_eq!(h.components[0].lot_no.as_deref(), Some("LOT-SCREW-42"));
    assert_eq!(h.components[0].qty, Some(4.0));
    t.commit().await.unwrap();
}

/// Short unique suffix so tests can share one database without colliding.
fn short_tag() -> String {
    trace_core::PublicId::generate().to_string()[16..].to_lowercase()
}
