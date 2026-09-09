//! The marker seam, kept alive for deferred laser DPM.
//!
//! Laser direct part marking is **deferred, not cancelled** — the full design
//! is in `docs/LASER-DPM-DEFERRED.md`. This module exists so that adding it is
//! one new file implementing [`MarkerDriver`], rather than a refactor of
//! everything that touches identity.
//!
//! Deferring the *device* did not require deferring the *discipline*: the
//! `MARK_VERIFIED` gate in `trace-core` is fully implemented and tested, and it
//! works today against printed labels. [`SimulatedMarker`] can be told to
//! produce a bad mark on demand, which is how the verify-and-quarantine path
//! stays tested before any laser exists.
//!
//! **Do not delete this seam** because nothing implements it yet.

use async_trait::async_trait;
use thiserror::Error;

/// Errors from a marking device.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MarkerError {
    /// The marker reported a fault.
    #[error("marker {device}: device fault: {detail}")]
    DeviceFault {
        /// Device code.
        device: String,
        /// Fault reported.
        detail: String,
    },

    /// The marker never reported completion.
    ///
    /// A file-drop integration with no completion signal is not an integration:
    /// without it we cannot tell "marked" from "about to mark".
    #[error("marker {device}: no completion signal within {timeout_ms}ms")]
    NoCompletion {
        /// Device code.
        device: String,
        /// Deadline that elapsed.
        timeout_ms: u64,
    },

    /// This strategy is declared but not implemented in this release.
    #[error(
        "marker strategy {0} is not implemented in this release; see docs/LASER-DPM-DEFERRED.md"
    )]
    NotImplemented(&'static str),
}

/// The result of applying a mark, before verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkApplied {
    /// Exact payload sent to the device, recorded on `mark_record`.
    pub payload_sent: String,
    /// Exact response received.
    pub response: String,
}

/// Applies a permanent mark to a part.
///
/// Separate from [`crate::sink::LabelSink`] because the contract differs in one
/// decisive way: a marker must report completion or fault, and a label printer
/// generally cannot. Marking without a completion signal would let a part leave
/// the station with no mark and no record of that.
#[async_trait]
pub trait MarkerDriver: Send + Sync + std::fmt::Debug {
    /// Apply a mark carrying `content`, and wait for completion.
    ///
    /// # Errors
    /// Returns [`MarkerError`] if the device faults or never reports done.
    async fn mark(&mut self, content: &str) -> Result<MarkApplied, MarkerError>;

    /// Strategy name, recorded against `mark_record`.
    fn strategy(&self) -> &'static str;
}

/// A virtual marker, so the verify gate is testable with no hardware.
#[derive(Debug, Default)]
pub struct SimulatedMarker {
    device: String,
    /// When set, the mark "applies" but decodes to this instead — the
    /// spoiled-mark case the route must catch.
    corrupt_as: Option<String>,
    /// When true, the next mark reports a device fault.
    fault: bool,
    /// Everything marked so far.
    marked: Vec<String>,
}

impl SimulatedMarker {
    /// Build a simulated marker.
    #[must_use]
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            ..Self::default()
        }
    }

    /// Make the next mark readable but wrong — the case that quietly destroys
    /// traceability if the verify gate is missing.
    #[must_use]
    pub fn corrupting_to(mut self, decoded: impl Into<String>) -> Self {
        self.corrupt_as = Some(decoded.into());
        self
    }

    /// Make the next mark fault.
    #[must_use]
    pub fn faulting(mut self) -> Self {
        self.fault = true;
        self
    }

    /// What a reader would decode from the last mark applied.
    #[must_use]
    pub fn read_back(&self) -> Option<String> {
        if let Some(c) = &self.corrupt_as {
            return Some(c.clone());
        }
        self.marked.last().cloned()
    }

    /// Everything marked so far.
    #[must_use]
    pub fn marked(&self) -> &[String] {
        &self.marked
    }
}

#[async_trait]
impl MarkerDriver for SimulatedMarker {
    async fn mark(&mut self, content: &str) -> Result<MarkApplied, MarkerError> {
        if self.fault {
            self.fault = false;
            return Err(MarkerError::DeviceFault {
                device: self.device.clone(),
                detail: "simulated fault".into(),
            });
        }
        self.marked.push(content.to_owned());
        Ok(MarkApplied {
            payload_sent: content.to_owned(),
            response: "OK".to_owned(),
        })
    }

    fn strategy(&self) -> &'static str {
        "SIMULATED"
    }
}

/// File-drop marking: the v1 default strategy when a laser is chosen.
///
/// Declared, not implemented. The contract it must honour is written up in
/// `docs/LASER-DPM-DEFERRED.md`, and the part that matters most is this: write
/// to a temporary name on the same filesystem, `fsync`, then **atomically
/// rename** into the watched folder. A laser watching a directory will happily
/// pick up a half-written file and mark garbage onto a customer's part.
#[derive(Debug, Default)]
pub struct FileDropMarker;

#[async_trait]
impl MarkerDriver for FileDropMarker {
    async fn mark(&mut self, _content: &str) -> Result<MarkApplied, MarkerError> {
        Err(MarkerError::NotImplemented("FILE_DROP"))
    }

    fn strategy(&self) -> &'static str {
        "FILE_DROP"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn a_simulated_mark_reads_back_as_what_was_marked() {
        let mut m = SimulatedMarker::new("LASER-1");
        let applied = m.mark("01JX8P2K7F3QW9ABCDEFGHJKMN").await.unwrap();
        assert_eq!(applied.payload_sent, "01JX8P2K7F3QW9ABCDEFGHJKMN");
        assert_eq!(m.read_back().as_deref(), Some("01JX8P2K7F3QW9ABCDEFGHJKMN"));
    }

    #[tokio::test]
    async fn a_spoiled_mark_reads_back_as_something_else() {
        // This is the case the MARK_VERIFIED gate exists to catch: the mark is
        // perfectly readable and belongs to a different unit.
        let mut m = SimulatedMarker::new("LASER-1").corrupting_to("WRONGUID0000000000000000AA");
        m.mark("01JX8P2K7F3QW9ABCDEFGHJKMN").await.unwrap();
        assert_ne!(m.read_back().as_deref(), Some("01JX8P2K7F3QW9ABCDEFGHJKMN"));
    }

    #[tokio::test]
    async fn a_device_fault_is_reported_not_silently_swallowed() {
        let mut m = SimulatedMarker::new("LASER-1").faulting();
        assert!(matches!(
            m.mark("X").await.unwrap_err(),
            MarkerError::DeviceFault { .. }
        ));
        // And it recovers for the next attempt, so remark paths are testable.
        assert!(m.mark("X").await.is_ok());
    }

    #[tokio::test]
    async fn the_deferred_file_drop_strategy_says_so_rather_than_pretending() {
        let mut m = FileDropMarker;
        assert_eq!(
            m.mark("X").await.unwrap_err(),
            MarkerError::NotImplemented("FILE_DROP")
        );
        assert_eq!(m.strategy(), "FILE_DROP");
    }

    #[tokio::test]
    async fn markers_are_interchangeable_behind_the_trait() {
        // Proves the seam holds: adding a real laser is one new file.
        let drivers: Vec<Box<dyn MarkerDriver>> = vec![
            Box::new(SimulatedMarker::new("SIM")),
            Box::new(FileDropMarker),
        ];
        let names: Vec<_> = drivers.iter().map(|d| d.strategy()).collect();
        assert_eq!(names, vec!["SIMULATED", "FILE_DROP"]);
    }
}
