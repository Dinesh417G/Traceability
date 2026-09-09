//! Where rendered labels go.
//!
//! Zebra printers are driven by writing raw ZPL to TCP port 9100. No driver, no
//! spooler, no OS print subsystem — which is what keeps the station app
//! identical on Windows and Linux.
//!
//! A file sink exists for sites with no network printer and for capturing
//! exactly what was sent during commissioning.

use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Errors from sending a label.
#[derive(Debug, Error)]
pub enum SinkError {
    /// The printer could not be reached.
    #[error("printer {target}: {detail}")]
    Unreachable {
        /// Printer address or path.
        target: String,
        /// What went wrong.
        detail: String,
    },

    /// The write did not complete in time.
    #[error("printer {target}: timed out after {timeout_ms}ms")]
    Timeout {
        /// Printer address.
        target: String,
        /// Deadline that elapsed.
        timeout_ms: u64,
    },
}

/// Accepts a rendered label payload.
#[async_trait]
pub trait LabelSink: Send + Sync + std::fmt::Debug {
    /// Send one label.
    ///
    /// # Errors
    /// Returns [`SinkError`] if the label could not be delivered.
    async fn send(&self, payload: &str) -> Result<(), SinkError>;

    /// Description for logs and `mark_record`.
    fn describe(&self) -> String;
}

/// Raw ZPL to a network printer on TCP 9100.
#[derive(Debug, Clone)]
pub struct TcpSink {
    addr: String,
    timeout: Duration,
}

impl TcpSink {
    /// Zebra's raw-print port.
    pub const DEFAULT_PORT: u16 = 9100;

    /// Build a sink for a printer host.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            addr: format!("{}:{port}", host.into()),
            timeout: Duration::from_secs(10),
        }
    }

    /// Override the write deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl LabelSink for TcpSink {
    async fn send(&self, payload: &str) -> Result<(), SinkError> {
        let mut stream = tokio::time::timeout(self.timeout, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| SinkError::Timeout {
                target: self.addr.clone(),
                timeout_ms: self.timeout.as_millis() as u64,
            })?
            .map_err(|e| SinkError::Unreachable {
                target: self.addr.clone(),
                detail: e.to_string(),
            })?;

        tokio::time::timeout(self.timeout, stream.write_all(payload.as_bytes()))
            .await
            .map_err(|_| SinkError::Timeout {
                target: self.addr.clone(),
                timeout_ms: self.timeout.as_millis() as u64,
            })?
            .map_err(|e| SinkError::Unreachable {
                target: self.addr.clone(),
                detail: e.to_string(),
            })?;

        // Flush before dropping: a printer that receives a partial label prints
        // a partial label, and a half-printed identity is worse than none.
        stream.flush().await.map_err(|e| SinkError::Unreachable {
            target: self.addr.clone(),
            detail: e.to_string(),
        })?;

        Ok(())
    }

    fn describe(&self) -> String {
        format!("tcp {}", self.addr)
    }
}

/// Appends labels to a file. For sites with no printer, and for capturing
/// exactly what was sent during commissioning.
#[derive(Debug, Clone)]
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    /// Build a file sink.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl LabelSink for FileSink {
    async fn send(&self, payload: &str) -> Result<(), SinkError> {
        use tokio::fs::OpenOptions;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| SinkError::Unreachable {
                target: self.path.display().to_string(),
                detail: e.to_string(),
            })?;
        f.write_all(payload.as_bytes())
            .await
            .map_err(|e| SinkError::Unreachable {
                target: self.path.display().to_string(),
                detail: e.to_string(),
            })?;
        f.sync_all().await.map_err(|e| SinkError::Unreachable {
            target: self.path.display().to_string(),
            detail: e.to_string(),
        })?;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("file {}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn a_label_reaches_a_listening_printer_byte_for_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            sock.read_to_string(&mut buf).await.unwrap();
            buf
        });

        let sink = TcpSink::new("127.0.0.1", addr.port());
        sink.send("^XA^FO0,0^FDtest^FS^XZ").await.unwrap();

        assert_eq!(handle.await.unwrap(), "^XA^FO0,0^FDtest^FS^XZ");
        assert!(sink.describe().starts_with("tcp"));
    }

    #[tokio::test]
    async fn an_unreachable_printer_reports_rather_than_hangs() {
        let sink = TcpSink::new("127.0.0.1", 1).with_timeout(Duration::from_millis(500));
        assert!(sink.send("^XA^XZ").await.is_err());
    }

    #[tokio::test]
    async fn the_file_sink_captures_what_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.zpl");
        let sink = FileSink::new(&path);

        sink.send("^XA1^XZ").await.unwrap();
        sink.send("^XA2^XZ").await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            content, "^XA1^XZ^XA2^XZ",
            "labels append, they do not overwrite"
        );
    }

    #[tokio::test]
    async fn the_default_port_is_zebras_raw_port() {
        assert_eq!(TcpSink::DEFAULT_PORT, 9100);
    }
}
