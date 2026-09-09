//! HTTP-level behaviour against a real database.
//!
//! The assertion that matters most here: a lapsed subscription blocks
//! configuration and **does not** block production. That rule is easy to state
//! in prose and easy to break in a refactor, so it is pinned at the layer a
//! customer would actually feel it.
#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use sqlx::Row as _;
use std::collections::BTreeSet;
use tower::ServiceExt as _;
use trace_license::{Enforcement, Entitlement, Feature, LicenseStatus, Limits, Tier};
use trace_store::Store;

use trace_edge::{AppState, EdgeConfig, routes};

struct Fixture {
    state: AppState,
    revision_id: i64,
    job_card_id: i64,
}

async fn setup(license: LicenseStatus) -> Option<Fixture> {
    let url = std::env::var("TRACE_TEST_DATABASE_URL").ok()?;
    let store = Store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();

    let tag = trace_core::PublicId::generate().to_string()[16..].to_lowercase();

    let mut tx = store.pool().begin().await.unwrap();
    let tenant_id: i64 = sqlx::query("SELECT trace.provision_tenant($1,$2) AS id")
        .bind(format!("EDGE-{tag}"))
        .bind("Edge Test")
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get("id");
    tx.commit().await.unwrap();

    let mut t = store.tenant(tenant_id).await.unwrap();
    let ex = t.executor();

    let plant_id: i64 = sqlx::query(
        "INSERT INTO trace.plant (tenant_id, code, name, trace_base_url)
         VALUES ($1,$2,'P','http://trace.plant.local') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("P-{tag}"))
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    let product_id: i64 = sqlx::query(
        "INSERT INTO trace.product (tenant_id, model_no, name) VALUES ($1,$2,'V') RETURNING id",
    )
    .bind(tenant_id)
    .bind(format!("M-{tag}"))
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
         VALUES ($1,$2,$3,$4,10) RETURNING id",
    )
    .bind(tenant_id)
    .bind(plant_id)
    .bind(format!("JC-{tag}"))
    .bind(revision_id)
    .fetch_one(&mut *ex)
    .await
    .unwrap()
    .get("id");

    t.commit().await.unwrap();

    let cfg = EdgeConfig {
        database_url: url,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        tenant_id,
        plant_id,
        trace_base_url: "http://trace.plant.local".into(),
        license_pubkey_hex: None,
        update_pubkey_hex: None,
        update_cohort: "default".into(),
        stripe_webhook_secret: None,
    };

    Some(Fixture {
        state: AppState::new(store, cfg, license),
        revision_id,
        job_card_id,
    })
}

fn licensed(enforcement: Enforcement) -> LicenseStatus {
    let now = Utc::now();
    let (expires, grace) = match enforcement {
        Enforcement::Full => (now + Duration::days(30), 30),
        Enforcement::Grace => (now - Duration::days(1), 30),
        Enforcement::Restricted => (now - Duration::days(400), 30),
    };
    let features: BTreeSet<Feature> = [Feature::DeviceDrivers].into_iter().collect();
    Entitlement {
        schema: trace_license::SCHEMA_VERSION,
        license_id: "01J000000000000000000000AA".into(),
        tenant_code: "EDGE".into(),
        node_id: None,
        tier: Tier::Professional,
        limits: Limits {
            max_stations: 12,
            max_plants: 1,
            max_users: 50,
        },
        features,
        issued_at: now - Duration::days(60),
        not_before: now - Duration::days(60),
        expires_at: expires,
        grace_days: grace,
        customer_name: "Acme Pumps Pvt Ltd".into(),
    }
    .evaluate(now)
}

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = routes::router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn birth(f: &Fixture, serial: &str) -> serde_json::Value {
    let (status, body) = call(
        &f.state,
        post(
            "/api/units",
            serde_json::json!({
                "product_revision_id": f.revision_id,
                "job_card_id": f.job_card_id,
                "serial": serial,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "birth failed: {body}");
    serde_json::from_str(&body).unwrap()
}

// ============================================== the licence boundary

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn production_capture_works_at_every_enforcement_level() {
    // The rule the whole billing design rests on. If this ever fails, a lapsed
    // invoice is taking a production line down.
    for enforcement in [
        Enforcement::Full,
        Enforcement::Grace,
        Enforcement::Restricted,
    ] {
        let Some(f) = setup(licensed(enforcement)).await else {
            return;
        };

        let unit = birth(&f, "PROD-0001").await;
        assert!(
            unit["uid"].as_str().is_some(),
            "{enforcement:?} must allow birthing a unit"
        );

        // And the trace page still serves it.
        let (status, _) = call(
            &f.state,
            get(&format!("/t/{}", unit["uid"].as_str().unwrap())),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{enforcement:?} must still serve the trace page"
        );

        // And recall still works: a safety query is never withheld for money.
        let (status, _) = call(&f.state, get("/api/recall/lot/LOT-ANY")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{enforcement:?} must still answer a recall"
        );
    }
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn configuration_changes_are_blocked_only_past_grace() {
    for (enforcement, expected) in [
        (Enforcement::Full, StatusCode::OK),
        (Enforcement::Grace, StatusCode::OK),
        (Enforcement::Restricted, StatusCode::PAYMENT_REQUIRED),
    ] {
        let Some(f) = setup(licensed(enforcement)).await else {
            return;
        };
        let model = format!("NEW-{}", trace_core::PublicId::generate());
        let (status, body) = call(
            &f.state,
            post(
                "/api/config/products",
                serde_json::json!({"model_no": model, "name": "New"}),
            ),
        )
        .await;
        assert_eq!(status, expected, "{enforcement:?} gave {status}: {body}");
    }
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn a_blocked_configuration_change_says_production_is_unaffected() {
    // An operator seeing this error must not conclude the line is stopping.
    let Some(f) = setup(licensed(Enforcement::Restricted)).await else {
        return;
    };
    let (_, body) = call(
        &f.state,
        post(
            "/api/config/products",
            serde_json::json!({"model_no": "X", "name": "X"}),
        ),
    )
    .await;
    assert!(body.contains("Production"), "message must reassure: {body}");
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn the_licence_endpoint_states_both_permissions_explicitly() {
    let Some(f) = setup(licensed(Enforcement::Restricted)).await else {
        return;
    };
    let (status, body) = call(&f.state, get("/api/license")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["production_allowed"], true);
    assert_eq!(v["configuration_allowed"], false);
    assert_eq!(v["enforcement"], "Restricted");
}

// ==================================================== the trace page

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn scanning_a_unit_returns_its_history_page() {
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let unit = birth(&f, "EX24A0001537").await;
    let uid = unit["uid"].as_str().unwrap();

    let (status, html) = call(&f.state, get(&format!("/t/{uid}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("EX24A0001537"));
    assert!(html.contains(uid));
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn the_qr_url_uses_this_plants_configured_base() {
    // Per-plant, never a constant: labels must resolve on this LAN.
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let unit = birth(&f, "URL-0001").await;
    let url = unit["trace_url"].as_str().unwrap();
    assert!(url.starts_with("http://trace.plant.local/t/"), "got {url}");
    assert!(url.ends_with(unit["uid"].as_str().unwrap()));
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn an_unknown_code_gets_a_helpful_page_not_a_stack_trace() {
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let (status, html) = call(&f.state, get("/t/01JX8P2K7F3QW9ABCDEFGHJKMN")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No unit found"));
    assert!(
        html.contains("human-readable text"),
        "tells the operator what to do next"
    );
}

// ======================================================== recall

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn recall_returns_the_affected_units() {
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let unit = birth(&f, "RECALL-0001").await;
    let unit_id = unit["unit_id"].as_i64().unwrap();
    let lot = format!("LOT-{}", trace_core::PublicId::generate());

    let mut t = f
        .state
        .store
        .tenant(f.state.config.tenant_id)
        .await
        .unwrap();
    let stamps = trace_core::clock::Stamps::new(Utc::now(), Utc::now());
    t.link_lot(
        unit_id,
        &lot,
        4.0,
        None,
        f.state.config.plant_id,
        10,
        stamps,
    )
    .await
    .unwrap();
    t.commit().await.unwrap();

    let (status, body) = call(&f.state, get(&format!("/api/recall/lot/{lot}"))).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["affected_count"], 1);
    assert_eq!(v["units"][0]["uid"], unit["uid"]);
}

// ======================================================== health

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn health_actually_touches_the_database() {
    // A process that is up but cannot reach Postgres is not healthy, and an
    // OTA rollback should trigger on that rather than on liveness alone.
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let (status, body) = call(&f.state, get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert!(v["version"].as_str().is_some());
}

// ======================================================== billing

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn a_factory_box_does_not_advertise_a_billing_endpoint() {
    // No webhook secret configured: this is not a control-plane node.
    let Some(f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let (status, _) = call(
        &f.state,
        post("/api/billing/webhook", serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn an_unsigned_webhook_is_rejected_on_a_billing_node() {
    let Some(mut f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let mut cfg = (*f.state.config).clone();
    cfg.stripe_webhook_secret = Some("whsec_test".into());
    f.state = AppState::new(f.state.store.clone(), cfg, licensed(Enforcement::Full));

    // No signature header at all.
    let (status, _) = call(
        &f.state,
        post("/api/billing/webhook", serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A forged signature.
    let req = Request::builder()
        .method("POST")
        .uri("/api/billing/webhook")
        .header("content-type", "application/json")
        .header("stripe-signature", "t=1614556800,v1=deadbeef")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = call(&f.state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !body.contains("deadbeef"),
        "must not echo the attacker's input back"
    );
}

#[tokio::test]
#[ignore = "requires TRACE_TEST_DATABASE_URL"]
async fn a_correctly_signed_webhook_is_accepted() {
    let Some(mut f) = setup(licensed(Enforcement::Full)).await else {
        return;
    };
    let mut cfg = (*f.state.config).clone();
    cfg.stripe_webhook_secret = Some("whsec_test".into());
    f.state = AppState::new(f.state.store.clone(), cfg, licensed(Enforcement::Full));

    let body = serde_json::json!({
        "id": "evt_1",
        "type": "customer.subscription.updated",
        "created": Utc::now().timestamp(),
        "data": {"object": {"object": "subscription", "id": "sub_1", "status": "active"}}
    })
    .to_string();

    let verifier = trace_billing::WebhookVerifier::new("whsec_test");
    let sig = verifier.sign_for_test(body.as_bytes(), Utc::now().timestamp());

    let req = Request::builder()
        .method("POST")
        .uri("/api/billing/webhook")
        .header("content-type", "application/json")
        .header("stripe-signature", sig)
        .body(Body::from(body))
        .unwrap();

    let (status, resp) = call(&f.state, req).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["received"], true);
    assert_eq!(v["affects_entitlement"], true);
}
