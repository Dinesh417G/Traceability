//! The manual pseudo-driver.
//!
//! A station with no PLC is not a degraded station. The operator keys the value
//! into the touchscreen, and it travels the same code path, against the same
//! limits, with the same audit fields as a value read off a wrench. Only
//! `source` differs, and the trace output must never treat manual data as
//! second class.
//!
//! Modelling manual entry as a driver rather than a special case is what makes
//! that true by construction instead of by discipline.

use crate::driver::{DeviceConfig, DeviceDriver, DeviceError, DeviceHealth, PointRef, RawSample};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use tokio::sync::mpsc;
use trace_core::dcp::Value;

/// A driver whose "read" is a UI input event.
#[derive(Debug)]
pub struct ManualDriver {
    code: String,
    health: DeviceHealth,
    /// Values the UI has supplied but the engine has not consumed.
    pending: HashMap<String, Value>,
    rx: Option<mpsc::UnboundedReceiver<(String, Value)>>,
}

/// Handle the station UI uses to submit operator entries.
#[derive(Debug, Clone)]
pub struct ManualInput {
    tx: mpsc::UnboundedSender<(String, Value)>,
}

impl ManualInput {
    /// Submit a value the operator entered.
    ///
    /// Returns false if the driver has been dropped, which the UI should treat
    /// as "this station is shutting down" rather than an error to show.
    pub fn submit(&self, point: impl Into<String>, value: Value) -> bool {
        self.tx.send((point.into(), value)).is_ok()
    }
}

impl ManualDriver {
    /// Build a manual driver and the handle the UI submits through.
    #[must_use]
    pub fn new(code: impl Into<String>) -> (Self, ManualInput) {
        let (tx, rx) = mpsc::unbounded_channel();
        let driver = Self {
            code: code.into(),
            health: DeviceHealth::default(),
            pending: HashMap::new(),
            rx: Some(rx),
        };
        (driver, ManualInput { tx })
    }

    /// Drain whatever the UI has submitted since the last look.
    fn drain(&mut self) {
        if let Some(rx) = self.rx.as_mut() {
            while let Ok((point, value)) = rx.try_recv() {
                self.pending.insert(point, value);
            }
        }
    }
}

#[async_trait]
impl DeviceDriver for ManualDriver {
    async fn connect(&mut self, _cfg: &DeviceConfig) -> Result<(), DeviceError> {
        // An operator is always "connected": there is no cable to fail.
        self.health.record_success(Utc::now());
        Ok(())
    }

    async fn read(&mut self, point: &PointRef) -> Result<RawSample, DeviceError> {
        self.drain();
        match self.pending.remove(&point.name) {
            Some(value) => {
                let now = Utc::now();
                self.health.record_success(now);
                Ok(RawSample {
                    point: point.clone(),
                    raw: value.canonical(),
                    value,
                    recorded_at: now,
                })
            }
            None => {
                // Not an error state to alarm on: it simply means the operator
                // has not typed it yet. The UI is expected to prompt.
                Err(DeviceError::Timeout {
                    device: self.code.clone(),
                    point: point.name.clone(),
                    timeout_ms: 0,
                })
            }
        }
    }

    async fn write(&mut self, _point: &PointRef, _value: Value) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported {
            device: self.code.clone(),
            kind: "MANUAL",
            operation: "write",
        })
    }

    fn health(&self) -> DeviceHealth {
        self.health.clone()
    }

    fn kind(&self) -> &'static str {
        "MANUAL"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cfg() -> DeviceConfig {
        DeviceConfig {
            code: "ST-01-MANUAL".into(),
            kind: "MANUAL".into(),
            settings: serde_json::json!({}),
            timeout_ms: 0,
        }
    }

    #[tokio::test]
    async fn an_operator_entry_reads_back_like_any_device_value() {
        let (mut d, ui) = ManualDriver::new("ST-01-MANUAL");
        d.connect(&cfg()).await.unwrap();

        ui.submit("final_torque", Value::Numeric(12.1));
        let sample = d.read(&PointRef::new("final_torque")).await.unwrap();

        assert_eq!(sample.value, Value::Numeric(12.1));
        // Even a typed value keeps a verbatim record of what was entered.
        assert_eq!(sample.raw, "12.1");
        assert_eq!(d.kind(), "MANUAL");
    }

    #[tokio::test]
    async fn reading_before_the_operator_types_is_a_wait_not_a_fault() {
        let (mut d, _ui) = ManualDriver::new("ST-01");
        d.connect(&cfg()).await.unwrap();
        let err = d.read(&PointRef::new("final_torque")).await.unwrap_err();
        assert!(
            err.is_transient(),
            "the UI should prompt, not raise an alarm"
        );
    }

    #[tokio::test]
    async fn each_entry_is_consumed_once() {
        let (mut d, ui) = ManualDriver::new("ST-01");
        d.connect(&cfg()).await.unwrap();

        ui.submit("t", Value::Numeric(1.0));
        assert!(d.read(&PointRef::new("t")).await.is_ok());
        // A second read must not silently reuse the first unit's reading.
        assert!(d.read(&PointRef::new("t")).await.is_err());
    }

    #[tokio::test]
    async fn a_corrected_entry_replaces_an_uncollected_one() {
        // Operator types 11.0, notices the mistake, types 12.1 before submit.
        let (mut d, ui) = ManualDriver::new("ST-01");
        d.connect(&cfg()).await.unwrap();
        ui.submit("t", Value::Numeric(11.0));
        ui.submit("t", Value::Numeric(12.1));
        assert_eq!(
            d.read(&PointRef::new("t")).await.unwrap().value,
            Value::Numeric(12.1)
        );
    }

    #[tokio::test]
    async fn a_manual_station_cannot_drive_an_interlock() {
        let (mut d, _ui) = ManualDriver::new("ST-01");
        let err = d
            .write(&PointRef::new("interlock"), Value::Boolean(true))
            .await
            .unwrap_err();
        assert!(matches!(err, DeviceError::Unsupported { .. }));
    }
}
