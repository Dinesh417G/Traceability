//! HTTP surface.
//!
//! ## The rule these routes encode
//!
//! Production endpoints are **never** gated on licensing. Configuration
//! endpoints are. [`require_config_change`] is the only place enforcement is
//! applied, so the boundary is one function rather than a convention scattered
//! across handlers — and a test can prove where it is applied and where it is
//! deliberately not.

use crate::page;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use trace_core::UnitUid;
use trace_store::repo::NewUnit;

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // --- public, unauthenticated, must work on a phone on the plant LAN
        .route("/t/{code}", get(trace_page))
        // --- operational
        .route("/health", get(health))
        .route("/api/license", get(license_status))
        // --- production capture: never gated on licensing
        .route("/api/units", post(birth_unit))
        .route("/api/units/{uid}", get(unit_history))
        .route("/api/recall/lot/{lot_no}", get(recall_by_lot))
        // --- configuration: gated on licensing
        .route("/api/config/products", post(create_product))
        // --- billing (control plane deployments only)
        .route("/api/billing/webhook", post(stripe_webhook))
        .with_state(state)
}

/// Error type that renders as JSON.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl From<trace_store::StoreError> for ApiError {
    fn from(e: trace_store::StoreError) -> Self {
        // Storage errors are logged in full and reported in summary: a database
        // message can leak schema detail to whoever scanned a QR code.
        tracing::error!(error = %e, "storage error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "storage error")
    }
}

// ---------------------------------------------------------------- health

/// Liveness and readiness, and the probe the OTA updater uses to decide
/// whether a freshly installed version is healthy.
async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // Actually touch the database: a process that is up but cannot reach
    // Postgres is not healthy, and an OTA rollback should trigger on that.
    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let ok = tx.outbox_pending().await.is_ok();
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "status": if ok { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "plant_id": state.config.plant_id,
        "enforcement": format!("{:?}", state.enforcement()),
    })))
}

// ----------------------------------------------------------- trace page

/// The public trace page.
///
/// Accepts a bare ULID as well as a full URL path, so a handheld scanner that
/// emits only the code works with no URL at all.
async fn trace_page(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Html<String>, ApiError> {
    let uid = normalise_scanned_code(&code);

    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let history = tx.unit_history(&uid).await?;
    tx.commit().await?;

    Ok(Html(match history {
        Some(h) => page::render(&h),
        None => page::not_found(&uid),
    }))
}

/// Reduce whatever a scanner produced to a bare ULID.
///
/// Handhelds emit the full URL, just the path, or only the code depending on
/// how they are configured, and a plant will have all three. Accepting each is
/// cheaper than trying to standardise every scanner on site.
#[must_use]
pub fn normalise_scanned_code(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let tail = trimmed.rsplit('/').next().unwrap_or(trimmed);
    tail.to_uppercase()
}

// ------------------------------------------------------------- licensing

/// Current entitlement, for the admin UI and support bundles.
async fn license_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let st = state.license();
    Json(serde_json::json!({
        "enforcement": format!("{:?}", st.enforcement),
        "summary": st.summary,
        "days_to_expiry": st.days_to_expiry,
        "tier": st.entitlement.as_ref().map(|e| format!("{:?}", e.tier)),
        "customer": st.entitlement.as_ref().map(|e| e.customer_name.clone()),
        "max_stations": st.max_stations(),
        // Stated explicitly so an operator reading this is never in doubt.
        "production_allowed": st.enforcement.allows_production(),
        "configuration_allowed": st.enforcement.allows_configuration_change(),
    }))
}

/// Gate a configuration change on the licence.
///
/// The **only** place enforcement is applied. Production handlers deliberately
/// do not call it.
fn require_config_change(state: &AppState) -> Result<(), ApiError> {
    if state.enforcement().allows_configuration_change() {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::PAYMENT_REQUIRED,
        format!(
            "{} Production and traceability capture are unaffected.",
            state.license().summary
        ),
    ))
}

// ------------------------------------------------------- production paths

/// Request to birth a unit.
#[derive(Debug, Deserialize)]
pub struct BirthUnitRequest {
    /// Product revision to build.
    pub product_revision_id: i64,
    /// Job card to build against.
    pub job_card_id: i64,
    /// Human-readable serial printed beside the code.
    pub serial: String,
    /// Position within the job card, for sampling rules.
    #[serde(default)]
    pub index_in_job: i64,
}

/// A newly born unit.
#[derive(Debug, Serialize)]
pub struct BirthUnitResponse {
    /// Public identity.
    pub uid: String,
    /// Surrogate key.
    pub unit_id: i64,
    /// URL to encode in the QR. Uses this plant's configured base.
    pub trace_url: String,
}

/// Birth a unit.
///
/// **Not licence gated.** A unit is a physical object that already exists on
/// the line; refusing to record it would destroy its traceability rather than
/// prevent anything.
async fn birth_unit(
    State(state): State<AppState>,
    Json(req): Json<BirthUnitRequest>,
) -> Result<Json<BirthUnitResponse>, ApiError> {
    // Allocated locally: a station must be able to birth a unit with no
    // network and no server.
    let uid = UnitUid::generate();

    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let unit_id = tx
        .insert_unit(&NewUnit {
            uid,
            plant_id: state.config.plant_id,
            product_revision_id: req.product_revision_id,
            job_card_id: req.job_card_id,
            serial: req.serial,
            index_in_job: req.index_in_job,
        })
        .await?;
    tx.enqueue_outbox(
        state.config.plant_id,
        "unit.born",
        &serde_json::json!({ "uid": uid.to_string(), "unit_id": unit_id }),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(BirthUnitResponse {
        trace_url: format!(
            "{}/t/{uid}",
            state.config.trace_base_url.trim_end_matches('/')
        ),
        uid: uid.to_string(),
        unit_id,
    }))
}

/// Backward trace as JSON.
async fn unit_history(
    State(state): State<AppState>,
    Path(uid): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let uid = normalise_scanned_code(&uid);
    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let history = tx.unit_history(&uid).await?;
    tx.commit().await?;

    history.map(|h| Json(serde_json::json!(h))).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no unit {uid} at this plant"),
        )
    })
}

/// The recall query.
///
/// **Not licence gated.** A recall is a safety matter, and a lapsed
/// subscription is not a reason to withhold the list of affected units.
async fn recall_by_lot(
    State(state): State<AppState>,
    Path(lot_no): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let affected = tx.units_affected_by_lot(&lot_no).await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "lot_no": lot_no,
        "affected_count": affected.len(),
        "units": affected,
    })))
}

// ------------------------------------------------------ configuration paths

/// Request to create a product.
#[derive(Debug, Deserialize)]
pub struct CreateProductRequest {
    /// Model number, the natural key.
    pub model_no: String,
    /// Display name.
    pub name: String,
}

/// Create a product. **Licence gated**: this is configuration, not production.
async fn create_product(
    State(state): State<AppState>,
    Json(req): Json<CreateProductRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_config_change(&state)?;

    let mut tx = state.store.tenant(state.config.tenant_id).await?;
    let product_id = tx.insert_product(&req.model_no, &req.name).await?;
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "product_id": product_id })))
}

// ---------------------------------------------------------------- billing

/// Stripe webhook receiver.
///
/// Only meaningful on a control-plane deployment. A factory edge box has no
/// webhook secret configured and answers 404, which is the right answer: it is
/// not a billing endpoint and should not advertise itself as one.
///
/// The body is read as raw bytes because the signature covers exactly those
/// bytes; parsing first and re-serialising would break verification.
async fn stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(secret) = state.config.stripe_webhook_secret.as_ref() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "billing is not enabled on this node",
        ));
    };

    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing Stripe-Signature"))?;

    let verifier = trace_billing::WebhookVerifier::new(secret.clone());
    let event = verifier
        .verify(&body, signature, chrono::Utc::now())
        .map_err(|e| {
            // Log the reason, return a bare rejection: telling an attacker which
            // half of the signature failed is free help.
            tracing::warn!(error = %e, "rejected stripe webhook");
            ApiError::new(StatusCode::BAD_REQUEST, "signature verification failed")
        })?;

    tracing::info!(event = %event.event_type, id = %event.id, "stripe webhook accepted");

    Ok(Json(serde_json::json!({
        "received": true,
        "id": event.id,
        "affects_entitlement": event.affects_entitlement(),
    })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn scanned_codes_are_normalised_however_the_scanner_is_configured() {
        let expected = "01JX8P2K7F3QW9ABCDEFGHJKMN";
        for input in [
            "01JX8P2K7F3QW9ABCDEFGHJKMN",
            "http://trace.plant.local/t/01JX8P2K7F3QW9ABCDEFGHJKMN",
            "https://trace.example.com/t/01JX8P2K7F3QW9ABCDEFGHJKMN/",
            "/t/01JX8P2K7F3QW9ABCDEFGHJKMN",
            "  01jx8p2k7f3qw9abcdefghjkmn  ",
        ] {
            assert_eq!(normalise_scanned_code(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn a_legacy_on_prem_url_still_resolves() {
        // Labels printed in year one must still resolve in year five, even
        // after the plant moves to a different base URL.
        assert_eq!(
            normalise_scanned_code("http://old-edge-box.local/t/01JX8P2K7F3QW9ABCDEFGHJKMN"),
            "01JX8P2K7F3QW9ABCDEFGHJKMN"
        );
    }
}
