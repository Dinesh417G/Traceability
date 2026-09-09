//! The device abstraction.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Errors a driver can raise. None of them may panic a station.
#[derive(Debug, Error, Clone)]
pub enum DeviceError {
    /// The device could not be reached.
    #[error("device {device}: not connected ({detail})")]
    NotConnected {
        /// Device code.
        device: String,
        /// What went wrong.
        detail: String,
    },

    /// The device did not answer within the deadline.
    ///
    /// A separate variant from [`Self::NotConnected`] because the operator
    /// response differs: a timeout usually means "press the tool trigger", not
    /// "check the cable".
    #[error("device {device}: timed out after {timeout_ms}ms waiting for {point}")]
    Timeout {
        /// Device code.
        device: String,
        /// Point being read.
        point: String,
        /// Deadline that elapsed.
        timeout_ms: u64,
    },

    /// The device answered, but not in a form we could parse.
    ///
    /// Carries the raw payload, because an unparseable reading is exactly when
    /// the raw bytes matter most.
    #[error("device {device}: cannot parse response {raw:?}")]
    Unparseable {
        /// Device code.
        device: String,
        /// Exactly what came back.
        raw: String,
    },

    /// The point is not defined on this device.
    #[error("device {device}: unknown point {point}")]
    UnknownPoint {
        /// Device code.
        device: String,
        /// Point requested.
        point: String,
    },

    /// The driver does not implement this operation.
    #[error("device {device}: {operation} is not supported by the {kind} driver")]
    Unsupported {
        /// Device code.
        device: String,
        /// Driver kind.
        kind: &'static str,
        /// Operation attempted.
        operation: &'static str,
    },

    /// Configuration was missing or wrong.
    #[error("device {device}: bad configuration: {detail}")]
    BadConfig {
        /// Device code.
        device: String,
        /// What is wrong.
        detail: String,
    },
}

impl DeviceError {
    /// Whether retrying might succeed. A configuration error will not fix
    /// itself; a disconnected cable might.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::NotConnected { .. } | Self::Timeout { .. })
    }
}

/// Names a readable or writable point on a device.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PointRef {
    /// Point name, e.g. `final_torque`, `DB10.DBD4`, `40001`.
    pub name: String,
}

impl PointRef {
    /// Build a point reference.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl std::fmt::Display for PointRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// One reading, with the bytes the device actually sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSample {
    /// Point read.
    pub point: PointRef,
    /// Parsed value.
    pub value: trace_core::dcp::Value,
    /// **Exactly** what the device returned, before parsing. Never normalised,
    /// never trimmed into oblivion: this is the evidence.
    pub raw: String,
    /// Device clock, or the station's if the device has none.
    pub recorded_at: DateTime<Utc>,
}

/// Connection state, for the station UI and the support bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHealth {
    /// Whether the driver currently believes it is connected.
    pub connected: bool,
    /// Consecutive failures since the last success.
    pub consecutive_failures: u32,
    /// Last successful read.
    pub last_success: Option<DateTime<Utc>>,
    /// Most recent error message.
    pub last_error: Option<String>,
}

impl DeviceHealth {
    /// Record a success.
    pub fn record_success(&mut self, at: DateTime<Utc>) {
        self.connected = true;
        self.consecutive_failures = 0;
        self.last_success = Some(at);
        self.last_error = None;
    }

    /// Record a failure.
    pub fn record_failure(&mut self, err: &DeviceError) {
        self.connected = false;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(err.to_string());
    }
}

/// How to reach a device. Driver-specific settings stay in `settings` as JSON,
/// so adding a driver never requires a schema migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Device code, unique within the tenant.
    pub code: String,
    /// Driver discriminator, e.g. `TCP_LINE`, `MANUAL`, `SIMULATED`.
    pub kind: String,
    /// Driver-specific settings.
    #[serde(default)]
    pub settings: serde_json::Value,
    /// Per-read deadline.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5_000
}

impl DeviceConfig {
    /// Read a string setting.
    ///
    /// # Errors
    /// Returns [`DeviceError::BadConfig`] if absent or not a string.
    pub fn require_str(&self, key: &str) -> Result<String, DeviceError> {
        self.settings
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| DeviceError::BadConfig {
                device: self.code.clone(),
                detail: format!("missing string setting {key:?}"),
            })
    }

    /// The per-read deadline.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// A source of process data.
///
/// Deliberately small. Every driver, from a torque wrench on a serial port to
/// an operator typing on a touchscreen, presents this same surface, which is
/// what lets the route engine stay ignorant of hardware.
#[async_trait]
pub trait DeviceDriver: Send + Sync + std::fmt::Debug {
    /// Establish or re-establish the connection.
    ///
    /// # Errors
    /// Returns [`DeviceError::NotConnected`] if the device cannot be reached.
    async fn connect(&mut self, cfg: &DeviceConfig) -> Result<(), DeviceError>;

    /// Read one point.
    ///
    /// # Errors
    /// Returns [`DeviceError::Timeout`] if the deadline passes, or
    /// [`DeviceError::Unparseable`] if the response cannot be interpreted.
    async fn read(&mut self, point: &PointRef) -> Result<RawSample, DeviceError>;

    /// Write one point, e.g. releasing an interlock.
    ///
    /// # Errors
    /// Returns [`DeviceError::Unsupported`] for read-only devices.
    async fn write(
        &mut self,
        point: &PointRef,
        value: trace_core::dcp::Value,
    ) -> Result<(), DeviceError>;

    /// Current connection state.
    fn health(&self) -> DeviceHealth;

    /// Driver kind, for diagnostics.
    fn kind(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn transient_errors_are_distinguished_from_permanent_ones() {
        let timeout = DeviceError::Timeout {
            device: "DEV-1".into(),
            point: "torque".into(),
            timeout_ms: 5_000,
        };
        assert!(timeout.is_transient(), "a timeout might succeed on retry");

        let bad_config = DeviceError::BadConfig {
            device: "DEV-1".into(),
            detail: "no host".into(),
        };
        assert!(
            !bad_config.is_transient(),
            "configuration will not fix itself"
        );
    }

    #[test]
    fn an_unparseable_response_keeps_the_raw_bytes() {
        // Precisely when a reading cannot be parsed, the raw bytes matter most.
        let err = DeviceError::Unparseable {
            device: "DEV-1".into(),
            raw: "T:\u{1}\u{2}GARBAGE".into(),
        };
        assert!(format!("{err}").contains("GARBAGE"));
    }

    #[test]
    fn health_tracks_consecutive_failures_and_clears_on_success() {
        let mut h = DeviceHealth::default();
        assert!(!h.connected);

        let err = DeviceError::NotConnected {
            device: "D".into(),
            detail: "refused".into(),
        };
        h.record_failure(&err);
        h.record_failure(&err);
        assert_eq!(h.consecutive_failures, 2);
        assert!(h.last_error.is_some());

        h.record_success(Utc::now());
        assert!(h.connected);
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_error.is_none(), "a success clears the stale error");
    }

    #[test]
    fn missing_configuration_is_reported_with_the_key_name() {
        let cfg = DeviceConfig {
            code: "DEV-1".into(),
            kind: "TCP_LINE".into(),
            settings: serde_json::json!({}),
            timeout_ms: 1_000,
        };
        let err = cfg.require_str("host").unwrap_err();
        assert!(format!("{err}").contains("host"));
    }
}
