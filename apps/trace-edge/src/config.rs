//! Edge configuration.
//!
//! Everything here comes from the environment or a config file. The rule from
//! the charter applies: if it varies by plant, it is configuration.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Edge service settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeConfig {
    /// Postgres connection string.
    pub database_url: String,
    /// Address to bind.
    pub bind_addr: SocketAddr,
    /// Tenant this box serves. One factory, one box, one tenant in v1.
    pub tenant_id: i64,
    /// Plant this box serves.
    pub plant_id: i64,
    /// Base URL printed into every QR.
    ///
    /// Per-plant, never a constant: on-premise this is the box's LAN name, and
    /// labels printed with it must still resolve years later.
    pub trace_base_url: String,
    /// Ed25519 public key that signs entitlements. Hex. Public key only —
    /// an edge box must never hold a signing key.
    pub license_pubkey_hex: Option<String>,
    /// Ed25519 public key that signs OTA manifests. Hex.
    pub update_pubkey_hex: Option<String>,
    /// Rollout cohort this box belongs to.
    pub update_cohort: String,
    /// Stripe webhook signing secret. Only set on a control-plane deployment;
    /// a factory box has no business holding one.
    pub stripe_webhook_secret: Option<String>,
}

impl EdgeConfig {
    /// Load from the environment.
    ///
    /// # Errors
    /// Returns an error if a required variable is missing or unparseable.
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("TRACE_DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("TRACE_DATABASE_URL is required"))?;

        let bind_addr = std::env::var("TRACE_BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()?;

        Ok(Self {
            database_url,
            bind_addr,
            tenant_id: parse_env("TRACE_TENANT_ID", 1)?,
            plant_id: parse_env("TRACE_PLANT_ID", 1)?,
            trace_base_url: std::env::var("TRACE_BASE_URL")
                .unwrap_or_else(|_| "http://trace.plant.local".to_owned()),
            license_pubkey_hex: non_empty("TRACE_LICENSE_PUBKEY_HEX"),
            update_pubkey_hex: non_empty("TRACE_UPDATE_PUBKEY_HEX"),
            update_cohort: std::env::var("TRACE_UPDATE_COHORT")
                .unwrap_or_else(|_| "default".to_owned()),
            stripe_webhook_secret: non_empty("STRIPE_WEBHOOK_SECRET"),
        })
    }
}

fn parse_env<T: std::str::FromStr>(key: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|e| anyhow::anyhow!("{key}: {e}")),
        Err(_) => Ok(default),
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
