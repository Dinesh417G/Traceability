//! ElectronIx Trace edge service.
//!
//! One per plant. Owns the plant's Postgres database, its devices, its print
//! queue, and the public `/t/{ulid}` trace page.
//!
//! Nothing in this binary's runtime path calls the internet. Licensing is a
//! local signature check; billing lives in the control plane; OTA is pulled on
//! a schedule and never blocks a request.

#![warn(missing_docs)]

use anyhow::Context as _;
use trace_edge::{AppState, EdgeConfig, routes};
use trace_license::LicenseStatus;
use trace_store::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,trace_edge=debug".into()),
        )
        .init();

    let config = EdgeConfig::from_env().context("loading configuration")?;
    tracing::info!(
        plant_id = config.plant_id,
        bind = %config.bind_addr,
        base_url = %config.trace_base_url,
        "starting ElectronIx Trace edge"
    );

    let store = Store::connect(&config.database_url)
        .await
        .context("connecting to the plant database")?;
    store.migrate().await.context("applying migrations")?;

    let license = load_license(&config);
    tracing::info!(
        enforcement = ?license.enforcement,
        summary = %license.summary,
        "licence evaluated"
    );

    let state = AppState::new(store, config.clone(), license);
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;

    tracing::info!("edge ready on {}", config.bind_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;

    Ok(())
}

/// Evaluate the installed entitlement.
///
/// Every failure path here lands on `unlicensed`, which restricts configuration
/// and **still allows production**. A box with a missing, corrupt or expired
/// licence file must keep running the line.
fn load_license(config: &EdgeConfig) -> LicenseStatus {
    let Some(key_hex) = config.license_pubkey_hex.as_ref() else {
        tracing::warn!("no licence public key configured; running unlicensed");
        return LicenseStatus::unlicensed();
    };

    let path = std::env::var("TRACE_LICENSE_FILE")
        .unwrap_or_else(|_| "/etc/electronix-trace/license.json".to_owned());

    let Ok(raw) = std::fs::read(&path) else {
        tracing::warn!(path = %path, "no licence file; running unlicensed");
        return LicenseStatus::unlicensed();
    };

    let evaluate = || -> anyhow::Result<LicenseStatus> {
        let envelope: trace_sign::Envelope = serde_json::from_slice(&raw)?;
        let key = trace_sign::TrustedKey::from_hex(key_hex)?;
        let node_id = node_id();
        Ok(trace_license::Entitlement::verify(
            &envelope,
            &key,
            &node_id,
            chrono::Utc::now(),
        )?)
    };

    evaluate().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "licence could not be verified; running unlicensed");
        LicenseStatus::unlicensed()
    })
}

/// This box's node identity, hashed so the licence file does not carry the
/// customer's raw hardware identifiers.
fn node_id() -> String {
    let fingerprint = std::env::var("TRACE_NODE_FINGERPRINT").unwrap_or_else(|_| {
        std::fs::read_to_string("/etc/machine-id").unwrap_or_else(|_| "unknown-node".into())
    });
    trace_license::Entitlement::node_id_from_fingerprint(fingerprint.trim())
}

/// Wait for SIGINT or SIGTERM.
///
/// A graceful shutdown matters here: an in-flight operation completion should
/// be committed rather than lost when a box is restarted for an update.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        () = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
