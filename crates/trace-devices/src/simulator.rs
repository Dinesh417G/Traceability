//! Virtual devices, so the whole system is testable with zero hardware.
//!
//! This is what lets CI exercise a full route, and what lets `route-sim` push a
//! unit end to end on a laptop. It is also how failure paths get tested: a
//! device that returns an out-of-limits reading on demand, or drops the
//! connection at a chosen moment, is the only practical way to prove the
//! quarantine and rework paths work before a real line depends on them.

use crate::driver::{DeviceConfig, DeviceDriver, DeviceError, DeviceHealth, PointRef, RawSample};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use trace_core::dcp::Value;

/// What a simulated device should do on the next read of a point.
#[derive(Debug, Clone, PartialEq)]
pub enum Scripted {
    /// Return this value.
    Value(Value),
    /// Return this value with a specific raw payload, for parser tests.
    Raw {
        /// Parsed value.
        value: Value,
        /// Bytes the device "sent".
        raw: String,
    },
    /// Fail as if the cable were pulled.
    Disconnect,
    /// Fail as if the device never answered.
    Timeout,
    /// Return something the parser cannot read.
    Garbage(String),
}

/// A device that does exactly what a test tells it to.
#[derive(Debug, Default)]
pub struct SimulatedDriver {
    code: String,
    /// Queued behaviours per point, consumed in order.
    script: HashMap<String, Vec<Scripted>>,
    /// Used when a point's script runs out.
    defaults: HashMap<String, Value>,
    health: DeviceHealth,
    /// Values written by the engine, so interlock releases can be asserted.
    writes: Vec<(String, Value)>,
    connected: bool,
}

impl SimulatedDriver {
    /// Build a simulator.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            ..Self::default()
        }
    }

    /// Always return this value for a point once its script is exhausted.
    #[must_use]
    pub fn with_default(mut self, point: &str, value: Value) -> Self {
        self.defaults.insert(point.to_owned(), value);
        self
    }

    /// Queue a behaviour for a point.
    #[must_use]
    pub fn script(mut self, point: &str, step: Scripted) -> Self {
        self.script.entry(point.to_owned()).or_default().push(step);
        self
    }

    /// Everything the engine has written, e.g. interlock releases.
    #[must_use]
    pub fn writes(&self) -> &[(String, Value)] {
        &self.writes
    }
}

#[async_trait]
impl DeviceDriver for SimulatedDriver {
    async fn connect(&mut self, cfg: &DeviceConfig) -> Result<(), DeviceError> {
        self.code = cfg.code.clone();
        self.connected = true;
        self.health.record_success(Utc::now());
        Ok(())
    }

    async fn read(&mut self, point: &PointRef) -> Result<RawSample, DeviceError> {
        if !self.connected {
            let e = DeviceError::NotConnected {
                device: self.code.clone(),
                detail: "simulator not connected".into(),
            };
            self.health.record_failure(&e);
            return Err(e);
        }

        let step = self.script.get_mut(&point.name).and_then(|q| {
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });

        let now = Utc::now();
        let result = match step {
            Some(Scripted::Value(v)) => Ok(RawSample {
                point: point.clone(),
                raw: v.canonical(),
                value: v,
                recorded_at: now,
            }),
            Some(Scripted::Raw { value, raw }) => Ok(RawSample {
                point: point.clone(),
                value,
                raw,
                recorded_at: now,
            }),
            Some(Scripted::Disconnect) => {
                self.connected = false;
                Err(DeviceError::NotConnected {
                    device: self.code.clone(),
                    detail: "simulated cable pull".into(),
                })
            }
            Some(Scripted::Timeout) => Err(DeviceError::Timeout {
                device: self.code.clone(),
                point: point.name.clone(),
                timeout_ms: 5_000,
            }),
            Some(Scripted::Garbage(raw)) => Err(DeviceError::Unparseable {
                device: self.code.clone(),
                raw,
            }),
            None => match self.defaults.get(&point.name) {
                Some(v) => Ok(RawSample {
                    point: point.clone(),
                    raw: v.canonical(),
                    value: v.clone(),
                    recorded_at: now,
                }),
                None => Err(DeviceError::UnknownPoint {
                    device: self.code.clone(),
                    point: point.name.clone(),
                }),
            },
        };

        match &result {
            Ok(s) => self.health.record_success(s.recorded_at),
            Err(e) => self.health.record_failure(e),
        }
        result
    }

    async fn write(&mut self, point: &PointRef, value: Value) -> Result<(), DeviceError> {
        if !self.connected {
            return Err(DeviceError::NotConnected {
                device: self.code.clone(),
                detail: "simulator not connected".into(),
            });
        }
        self.writes.push((point.name.clone(), value));
        Ok(())
    }

    fn health(&self) -> DeviceHealth {
        self.health.clone()
    }

    fn kind(&self) -> &'static str {
        "SIMULATED"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cfg() -> DeviceConfig {
        DeviceConfig {
            code: "SIM-1".into(),
            kind: "SIMULATED".into(),
            settings: serde_json::json!({}),
            timeout_ms: 100,
        }
    }

    #[tokio::test]
    async fn a_scripted_sequence_plays_in_order_then_falls_back_to_the_default() {
        let mut d = SimulatedDriver::new("SIM-1")
            .script("torque", Scripted::Value(Value::Numeric(11.0)))
            .script("torque", Scripted::Value(Value::Numeric(12.5)))
            .with_default("torque", Value::Numeric(12.0));
        d.connect(&cfg()).await.unwrap();

        assert_eq!(
            d.read(&PointRef::new("torque")).await.unwrap().value,
            Value::Numeric(11.0)
        );
        assert_eq!(
            d.read(&PointRef::new("torque")).await.unwrap().value,
            Value::Numeric(12.5)
        );
        assert_eq!(
            d.read(&PointRef::new("torque")).await.unwrap().value,
            Value::Numeric(12.0)
        );
    }

    #[tokio::test]
    async fn a_simulated_cable_pull_is_how_offline_paths_get_tested() {
        let mut d = SimulatedDriver::new("SIM-1")
            .script("torque", Scripted::Value(Value::Numeric(12.0)))
            .script("torque", Scripted::Disconnect);
        d.connect(&cfg()).await.unwrap();

        assert!(d.read(&PointRef::new("torque")).await.is_ok());
        assert!(d.read(&PointRef::new("torque")).await.is_err());
        assert!(!d.health().connected, "the driver must notice it is gone");

        // And it stays down until someone reconnects.
        assert!(d.read(&PointRef::new("torque")).await.is_err());
        d.connect(&cfg()).await.unwrap();
        assert!(d.health().connected);
    }

    #[tokio::test]
    async fn a_forced_bad_reading_is_how_the_quarantine_path_gets_tested() {
        let mut d =
            SimulatedDriver::new("SIM-1").script("torque", Scripted::Value(Value::Numeric(18.0)));
        d.connect(&cfg()).await.unwrap();
        // Out of a 10..14 limit: the route engine must quarantine on this.
        assert_eq!(
            d.read(&PointRef::new("torque")).await.unwrap().value,
            Value::Numeric(18.0)
        );
    }

    #[tokio::test]
    async fn raw_payloads_can_be_scripted_independently_of_the_parsed_value() {
        let mut d = SimulatedDriver::new("SIM-1").script(
            "torque",
            Scripted::Raw {
                value: Value::Numeric(12.1),
                raw: "T: 12.1 NM\r\n".into(),
            },
        );
        d.connect(&cfg()).await.unwrap();
        let s = d.read(&PointRef::new("torque")).await.unwrap();
        assert_eq!(s.raw, "T: 12.1 NM\r\n");
        assert_eq!(s.value, Value::Numeric(12.1));
    }

    #[tokio::test]
    async fn interlock_writes_are_recorded_so_tests_can_assert_them() {
        let mut d = SimulatedDriver::new("SIM-1");
        d.connect(&cfg()).await.unwrap();
        d.write(&PointRef::new("interlock"), Value::Boolean(true))
            .await
            .unwrap();
        assert_eq!(
            d.writes(),
            &[("interlock".to_string(), Value::Boolean(true))]
        );
    }

    #[tokio::test]
    async fn an_unscripted_point_is_reported_rather_than_guessed() {
        let mut d = SimulatedDriver::new("SIM-1");
        d.connect(&cfg()).await.unwrap();
        assert!(matches!(
            d.read(&PointRef::new("nothing_here")).await.unwrap_err(),
            DeviceError::UnknownPoint { .. }
        ));
    }
}
