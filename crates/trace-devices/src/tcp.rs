//! Raw ASCII line protocol over TCP.
//!
//! This covers a large share of what is actually on an Indian MSME shop floor:
//! torque wrenches, weighing scales and barcode scanners that emit a line of
//! text per reading, reached over Ethernet or through a serial-to-Ethernet
//! converter.
//!
//! Driving them as raw sockets rather than through a vendor SDK is what keeps
//! the station app identical on Windows and Linux.

use crate::driver::{DeviceConfig, DeviceDriver, DeviceError, DeviceHealth, PointRef, RawSample};
use async_trait::async_trait;
use chrono::Utc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use trace_core::dcp::Value;

/// Reads newline-terminated ASCII readings from a TCP endpoint.
#[derive(Debug)]
pub struct TcpLineDriver {
    code: String,
    addr: String,
    timeout: Duration,
    /// Optional command written before reading, for devices that must be
    /// polled rather than devices that stream.
    poll_command: Option<String>,
    stream: Option<BufReader<TcpStream>>,
    health: DeviceHealth,
}

impl TcpLineDriver {
    /// Build an unconnected driver.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            addr: String::new(),
            timeout: Duration::from_secs(5),
            poll_command: None,
            stream: None,
            health: DeviceHealth::default(),
        }
    }

    /// Parse a reading out of a device line.
    ///
    /// Deliberately forgiving about surrounding text and strict about the
    /// number: instruments emit things like `T: 12.1 NM`, `+0012.1 kg` or
    /// `ST,GS,+  12.1 g`, and the useful part is the first numeric token.
    ///
    /// The raw line is always preserved separately, so being liberal here never
    /// costs evidence.
    fn parse_line(line: &str) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut current = String::new();

        let flush = |cur: &mut String, best: &mut Option<f64>| {
            if !cur.is_empty() {
                if best.is_none()
                    && let Ok(v) = cur.parse::<f64>()
                {
                    *best = Some(v);
                }
                cur.clear();
            }
        };

        for ch in line.chars() {
            if ch.is_ascii_digit() || ch == '.' || ((ch == '-' || ch == '+') && current.is_empty())
            {
                current.push(ch);
            } else {
                flush(&mut current, &mut best);
            }
        }
        flush(&mut current, &mut best);
        best
    }
}

#[async_trait]
impl DeviceDriver for TcpLineDriver {
    async fn connect(&mut self, cfg: &DeviceConfig) -> Result<(), DeviceError> {
        let host = cfg.require_str("host")?;
        let port = cfg
            .settings
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| DeviceError::BadConfig {
                device: cfg.code.clone(),
                detail: "missing numeric setting \"port\"".into(),
            })?;

        self.code = cfg.code.clone();
        self.addr = format!("{host}:{port}");
        self.timeout = cfg.timeout();
        self.poll_command = cfg
            .settings
            .get("poll_command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        // A connect that hangs must not hang the station.
        let stream = tokio::time::timeout(self.timeout, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| DeviceError::NotConnected {
                device: self.code.clone(),
                detail: format!("connect to {} timed out", self.addr),
            })?
            .map_err(|e| DeviceError::NotConnected {
                device: self.code.clone(),
                detail: format!("{}: {e}", self.addr),
            })?;

        // Readings are small and latency matters more than packing.
        let _ = stream.set_nodelay(true);
        self.stream = Some(BufReader::new(stream));
        self.health.record_success(Utc::now());
        Ok(())
    }

    async fn read(&mut self, point: &PointRef) -> Result<RawSample, DeviceError> {
        let timeout = self.timeout;
        let code = self.code.clone();
        let poll = self.poll_command.clone();

        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| DeviceError::NotConnected {
                device: code.clone(),
                detail: "connect() has not been called".into(),
            })?;

        let result: Result<RawSample, DeviceError> = async {
            if let Some(cmd) = poll {
                stream
                    .get_mut()
                    .write_all(cmd.as_bytes())
                    .await
                    .map_err(|e| DeviceError::NotConnected {
                        device: code.clone(),
                        detail: e.to_string(),
                    })?;
            }

            let mut line = String::new();
            let n = tokio::time::timeout(timeout, stream.read_line(&mut line))
                .await
                .map_err(|_| DeviceError::Timeout {
                    device: code.clone(),
                    point: point.name.clone(),
                    timeout_ms: timeout.as_millis() as u64,
                })?
                .map_err(|e| DeviceError::NotConnected {
                    device: code.clone(),
                    detail: e.to_string(),
                })?;

            if n == 0 {
                return Err(DeviceError::NotConnected {
                    device: code.clone(),
                    detail: "device closed the connection".into(),
                });
            }

            // Keep the line verbatim, including its terminator: that is the
            // evidence. Parse from a trimmed copy.
            let raw = line.clone();
            let value = Self::parse_line(line.trim()).ok_or_else(|| DeviceError::Unparseable {
                device: code.clone(),
                raw: raw.clone(),
            })?;

            Ok(RawSample {
                point: point.clone(),
                value: Value::Numeric(value),
                raw,
                recorded_at: Utc::now(),
            })
        }
        .await;

        match &result {
            Ok(s) => self.health.record_success(s.recorded_at),
            Err(e) => {
                self.health.record_failure(e);
                // Force a reconnect on the next attempt rather than reusing a
                // socket we no longer trust.
                if !matches!(e, DeviceError::Unparseable { .. }) {
                    self.stream = None;
                }
            }
        }
        result
    }

    async fn write(&mut self, point: &PointRef, value: Value) -> Result<(), DeviceError> {
        let code = self.code.clone();
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| DeviceError::NotConnected {
                device: code.clone(),
                detail: "connect() has not been called".into(),
            })?;

        let line = format!("{}={}\n", point.name, value.canonical());
        stream
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .map_err(|e| DeviceError::NotConnected {
                device: code,
                detail: e.to_string(),
            })
    }

    fn health(&self) -> DeviceHealth {
        self.health.clone()
    }

    fn kind(&self) -> &'static str {
        "TCP_LINE"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::net::TcpListener;

    /// A fake instrument that emits fixed lines.
    async fn fake_instrument(lines: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                for l in lines {
                    if sock.write_all(l.as_bytes()).await.is_err() {
                        break;
                    }
                }
                let _ = sock.flush().await;
                // Hold the socket briefly so the client can read before EOF.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        addr
    }

    fn cfg_for(addr: &str) -> DeviceConfig {
        let (host, port) = addr.rsplit_once(':').unwrap();
        DeviceConfig {
            code: "TORQUE-1".into(),
            kind: "TCP_LINE".into(),
            settings: serde_json::json!({"host": host, "port": port.parse::<u64>().unwrap()}),
            timeout_ms: 2_000,
        }
    }

    #[test]
    fn readings_are_extracted_from_the_formats_instruments_actually_emit() {
        assert_eq!(TcpLineDriver::parse_line("12.1"), Some(12.1));
        assert_eq!(TcpLineDriver::parse_line("T: 12.1 NM"), Some(12.1));
        assert_eq!(TcpLineDriver::parse_line("+0012.1 kg"), Some(12.1));
        assert_eq!(TcpLineDriver::parse_line("ST,GS,+  12.1 g"), Some(12.1));
        assert_eq!(TcpLineDriver::parse_line("-3.5"), Some(-3.5));
    }

    #[test]
    fn a_line_with_no_number_is_not_invented() {
        assert_eq!(TcpLineDriver::parse_line("ERROR"), None);
        assert_eq!(TcpLineDriver::parse_line(""), None);
        assert_eq!(TcpLineDriver::parse_line("---"), None);
    }

    #[tokio::test]
    async fn a_real_socket_reading_round_trips_with_its_raw_bytes() {
        let addr = fake_instrument(vec!["T: 12.1 NM\r\n".into()]).await;
        let mut d = TcpLineDriver::new("TORQUE-1");
        d.connect(&cfg_for(&addr)).await.unwrap();

        let sample = d.read(&PointRef::new("final_torque")).await.unwrap();
        assert_eq!(sample.value, Value::Numeric(12.1));
        // The verbatim line, terminator included, is the evidence.
        assert_eq!(sample.raw, "T: 12.1 NM\r\n");
        assert!(d.health().connected);
    }

    #[tokio::test]
    async fn successive_readings_are_distinct() {
        let addr = fake_instrument(vec!["11.0\n".into(), "12.5\n".into()]).await;
        let mut d = TcpLineDriver::new("TORQUE-1");
        d.connect(&cfg_for(&addr)).await.unwrap();

        assert_eq!(
            d.read(&PointRef::new("t")).await.unwrap().value,
            Value::Numeric(11.0)
        );
        assert_eq!(
            d.read(&PointRef::new("t")).await.unwrap().value,
            Value::Numeric(12.5)
        );
    }

    #[tokio::test]
    async fn an_unparseable_line_keeps_the_connection_and_the_bytes() {
        // A garbled reading is not a dead cable: stay connected, report the
        // raw payload, let the operator retry.
        let addr = fake_instrument(vec!["ERR CAL\n".into(), "12.1\n".into()]).await;
        let mut d = TcpLineDriver::new("TORQUE-1");
        d.connect(&cfg_for(&addr)).await.unwrap();

        let err = d.read(&PointRef::new("t")).await.unwrap_err();
        match err {
            DeviceError::Unparseable { raw, .. } => assert!(raw.contains("ERR CAL")),
            other => panic!("expected Unparseable, got {other}"),
        }
        // Still usable for the next reading.
        assert_eq!(
            d.read(&PointRef::new("t")).await.unwrap().value,
            Value::Numeric(12.1)
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_reported_not_panicked() {
        let mut d = TcpLineDriver::new("TORQUE-1");
        // Port 1 on loopback: reliably refused.
        let cfg = DeviceConfig {
            code: "TORQUE-1".into(),
            kind: "TCP_LINE".into(),
            settings: serde_json::json!({"host": "127.0.0.1", "port": 1}),
            timeout_ms: 1_000,
        };
        let err = d.connect(&cfg).await.unwrap_err();
        assert!(err.is_transient());
        assert!(!d.health().connected);
    }

    #[tokio::test]
    async fn a_device_that_hangs_up_forces_a_reconnect() {
        let addr = fake_instrument(vec![]).await;
        let mut d = TcpLineDriver::new("TORQUE-1");
        d.connect(&cfg_for(&addr)).await.unwrap();

        let err = d.read(&PointRef::new("t")).await.unwrap_err();
        assert!(err.is_transient());
        // The stale socket must not be reused.
        let second = d.read(&PointRef::new("t")).await.unwrap_err();
        assert!(matches!(second, DeviceError::NotConnected { .. }));
    }

    #[tokio::test]
    async fn reading_before_connecting_is_an_error_not_a_panic() {
        let mut d = TcpLineDriver::new("TORQUE-1");
        assert!(d.read(&PointRef::new("t")).await.is_err());
    }

    #[tokio::test]
    async fn missing_port_configuration_names_the_setting() {
        let mut d = TcpLineDriver::new("TORQUE-1");
        let cfg = DeviceConfig {
            code: "TORQUE-1".into(),
            kind: "TCP_LINE".into(),
            settings: serde_json::json!({"host": "127.0.0.1"}),
            timeout_ms: 500,
        };
        assert!(format!("{}", d.connect(&cfg).await.unwrap_err()).contains("port"));
    }
}
