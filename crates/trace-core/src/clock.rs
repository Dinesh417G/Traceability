//! Clock discipline.
//!
//! Every runtime row records **device time** (`recorded_at`, the clock of the
//! station that observed the event) and **server time** (`received_at`, the
//! clock of the edge that durably stored it).
//!
//! Keeping both is not redundancy. A panel PC with a dead RTC battery silently
//! corrupts traceability, and it is close to impossible to detect after the
//! fact if only one timestamp survives. Recording both lets us *flag* skew at
//! the moment it happens.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Default tolerance before an event is flagged as clock-skewed.
pub const DEFAULT_SKEW_THRESHOLD_SECS: i64 = 30;

/// The device/server timestamp pair carried by every runtime row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamps {
    /// Time as observed by the device or station that produced the event.
    pub recorded_at: DateTime<Utc>,
    /// Time the edge durably received it.
    pub received_at: DateTime<Utc>,
}

impl Stamps {
    /// Pair a device time with a server time.
    #[must_use]
    pub fn new(recorded_at: DateTime<Utc>, received_at: DateTime<Utc>) -> Self {
        Self {
            recorded_at,
            received_at,
        }
    }

    /// Signed skew, device minus server. Positive means the device clock runs
    /// ahead of the edge.
    #[must_use]
    pub fn skew(&self) -> Duration {
        self.recorded_at - self.received_at
    }

    /// Whether the skew exceeds `threshold_secs` in either direction.
    ///
    /// A flagged event is still stored — we never drop production data over a
    /// clock problem — but it is marked so an auditor can see it.
    #[must_use]
    pub fn is_skewed(&self, threshold_secs: i64) -> bool {
        self.skew().num_seconds().abs() > threshold_secs
    }

    /// Whether the skew exceeds the default threshold.
    #[must_use]
    pub fn is_skewed_default(&self) -> bool {
        self.is_skewed(DEFAULT_SKEW_THRESHOLD_SECS)
    }

    /// How long the event spent queued in a station spool before the edge saw
    /// it. Returns zero when the device clock leads the server clock, since a
    /// negative queue time is meaningless.
    #[must_use]
    pub fn spool_latency(&self) -> Duration {
        let d = self.received_at - self.recorded_at;
        if d < Duration::zero() {
            Duration::zero()
        } else {
            d
        }
    }
}

/// Source of time, so tests can drive the engine deterministically without
/// `trace-core` ever reaching for the system clock behind your back.
pub trait Clock: std::fmt::Debug + Send + Sync {
    /// Current instant.
    fn now(&self) -> DateTime<Utc>;
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock pinned to a fixed instant, for tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn aligned_clocks_are_not_skewed() {
        let s = Stamps::new(t(0), t(0));
        assert!(!s.is_skewed_default());
        assert_eq!(s.skew().num_seconds(), 0);
    }

    #[test]
    fn skew_is_detected_in_both_directions() {
        assert!(
            Stamps::new(t(120), t(0)).is_skewed_default(),
            "device ahead"
        );
        assert!(
            Stamps::new(t(0), t(120)).is_skewed_default(),
            "device behind"
        );
    }

    #[test]
    fn offline_spool_latency_is_not_skew_when_within_threshold() {
        // Station queued the event 10s before the edge stored it: normal.
        let s = Stamps::new(t(0), t(10));
        assert!(!s.is_skewed_default());
        assert_eq!(s.spool_latency().num_seconds(), 10);
    }

    #[test]
    fn spool_latency_never_negative() {
        let s = Stamps::new(t(50), t(0));
        assert_eq!(s.spool_latency().num_seconds(), 0);
    }
}
