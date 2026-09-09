//! Holding the drivers a station owns, and keeping them alive.
//!
//! The registry is what turns "a driver exists" into "a station keeps working
//! through a switch reboot": it owns reconnection, tracks health for the
//! operator UI, and guarantees that a device failure surfaces as a gate result
//! rather than as a crashed station.

use crate::driver::{DeviceConfig, DeviceDriver, DeviceError, DeviceHealth, PointRef, RawSample};
use crate::retry::Backoff;
use std::collections::BTreeMap;

/// Owns a station's drivers by device code.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: BTreeMap<String, Entry>,
}

#[derive(Debug)]
struct Entry {
    driver: Box<dyn DeviceDriver>,
    config: DeviceConfig,
    backoff: Backoff,
}

impl DeviceRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver under its device code.
    pub fn insert(&mut self, config: DeviceConfig, driver: Box<dyn DeviceDriver>) {
        self.devices.insert(
            config.code.clone(),
            Entry {
                driver,
                config,
                backoff: Backoff::default(),
            },
        );
    }

    /// Device codes currently registered.
    #[must_use]
    pub fn codes(&self) -> Vec<&str> {
        self.devices.keys().map(String::as_str).collect()
    }

    /// Health of every device, for the station UI and the support bundle.
    #[must_use]
    pub fn health(&self) -> BTreeMap<String, DeviceHealth> {
        self.devices
            .iter()
            .map(|(k, e)| (k.clone(), e.driver.health()))
            .collect()
    }

    /// Connect every registered device.
    ///
    /// Failures are collected rather than propagated: one dead device must not
    /// stop a station bringing up the rest, because the operator can still work
    /// the manual points while a cable is found.
    pub async fn connect_all(&mut self) -> Vec<(String, DeviceError)> {
        let mut failures = Vec::new();
        for (code, entry) in &mut self.devices {
            if let Err(e) = entry.driver.connect(&entry.config).await {
                tracing::warn!(device = %code, error = %e, "device failed to connect");
                failures.push((code.clone(), e));
            }
        }
        failures
    }

    /// Read a point, reconnecting once if the device has dropped.
    ///
    /// One retry, not many: an operator waiting at a station needs an answer,
    /// and a device that is genuinely gone should surface as a failed gate the
    /// operator can act on rather than a spinner.
    ///
    /// # Errors
    /// Returns the underlying [`DeviceError`], or [`DeviceError::UnknownPoint`]
    /// if the device is not registered.
    pub async fn read(
        &mut self,
        device_code: &str,
        point: &PointRef,
    ) -> Result<RawSample, DeviceError> {
        let entry = self
            .devices
            .get_mut(device_code)
            .ok_or_else(|| DeviceError::UnknownPoint {
                device: device_code.to_owned(),
                point: point.name.clone(),
            })?;

        match entry.driver.read(point).await {
            Ok(sample) => {
                entry.backoff.reset();
                Ok(sample)
            }
            Err(e) if e.is_transient() => {
                let delay = entry.backoff.next_delay();
                tracing::debug!(
                    device = %device_code, ?delay, attempt = entry.backoff.attempts(),
                    "device read failed, reconnecting"
                );
                tokio::time::sleep(delay).await;
                entry.driver.connect(&entry.config).await?;
                entry.driver.read(point).await
            }
            Err(e) => Err(e),
        }
    }

    /// Write a point, e.g. releasing an interlock.
    ///
    /// # Errors
    /// Returns the underlying [`DeviceError`].
    pub async fn write(
        &mut self,
        device_code: &str,
        point: &PointRef,
        value: trace_core::dcp::Value,
    ) -> Result<(), DeviceError> {
        let entry = self
            .devices
            .get_mut(device_code)
            .ok_or_else(|| DeviceError::UnknownPoint {
                device: device_code.to_owned(),
                point: point.name.clone(),
            })?;
        entry.driver.write(point, value).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::simulator::{Scripted, SimulatedDriver};
    use trace_core::dcp::Value;

    fn cfg(code: &str) -> DeviceConfig {
        DeviceConfig {
            code: code.into(),
            kind: "SIMULATED".into(),
            settings: serde_json::json!({}),
            timeout_ms: 50,
        }
    }

    #[tokio::test]
    async fn a_registry_serves_reads_by_device_code() {
        let mut r = DeviceRegistry::new();
        r.insert(
            cfg("TORQUE-1"),
            Box::new(SimulatedDriver::new("TORQUE-1").with_default("t", Value::Numeric(12.0))),
        );
        r.connect_all().await;

        let s = r.read("TORQUE-1", &PointRef::new("t")).await.unwrap();
        assert_eq!(s.value, Value::Numeric(12.0));
        assert_eq!(r.codes(), vec!["TORQUE-1"]);
    }

    #[tokio::test]
    async fn a_dropped_device_is_reconnected_transparently() {
        // A switch reboot mid-shift must not need operator intervention.
        let mut r = DeviceRegistry::new();
        r.insert(
            cfg("TORQUE-1"),
            Box::new(
                SimulatedDriver::new("TORQUE-1")
                    .script("t", Scripted::Disconnect)
                    .with_default("t", Value::Numeric(12.0)),
            ),
        );
        r.connect_all().await;

        let s = r.read("TORQUE-1", &PointRef::new("t")).await.unwrap();
        assert_eq!(
            s.value,
            Value::Numeric(12.0),
            "recovered without the operator noticing"
        );
    }

    #[tokio::test]
    async fn one_dead_device_does_not_stop_the_others_coming_up() {
        let mut r = DeviceRegistry::new();
        r.insert(
            cfg("GOOD"),
            Box::new(SimulatedDriver::new("GOOD").with_default("t", Value::Numeric(1.0))),
        );
        // A TCP device pointed at a refused port.
        #[cfg(feature = "tcp")]
        {
            let mut bad = cfg("BAD");
            bad.kind = "TCP_LINE".into();
            bad.settings = serde_json::json!({"host": "127.0.0.1", "port": 1});
            bad.timeout_ms = 500;
            r.insert(bad, Box::new(crate::tcp::TcpLineDriver::new("BAD")));
        }

        let failures = r.connect_all().await;
        #[cfg(feature = "tcp")]
        assert_eq!(failures.len(), 1, "the bad device is reported");
        #[cfg(not(feature = "tcp"))]
        assert!(failures.is_empty());

        // The healthy device still works.
        assert!(r.read("GOOD", &PointRef::new("t")).await.is_ok());
    }

    #[tokio::test]
    async fn an_unregistered_device_is_an_error_not_a_panic() {
        let mut r = DeviceRegistry::new();
        assert!(r.read("NOPE", &PointRef::new("t")).await.is_err());
    }

    #[tokio::test]
    async fn interlock_writes_reach_the_driver() {
        let mut r = DeviceRegistry::new();
        r.insert(cfg("PLC-1"), Box::new(SimulatedDriver::new("PLC-1")));
        r.connect_all().await;
        r.write("PLC-1", &PointRef::new("interlock"), Value::Boolean(true))
            .await
            .unwrap();
    }
}
