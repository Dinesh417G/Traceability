//! Detecting a clock that has been wound back.
//!
//! Expiry is a time comparison, so the obvious way to extend a licence for
//! free is to set the system clock back. Trusting the RTC is therefore not
//! enough — but neither is refusing to run when the clock looks wrong, because
//! a dead RTC battery on a panel PC is far more common than fraud, and a plant
//! must keep producing either way.
//!
//! So the guard uses a **monotonic high-water mark**: the latest instant this
//! box has ever observed, persisted by the caller. Time can move forward
//! freely. If it appears to move backwards by more than a tolerance, that is
//! reported and the high-water mark is used for licence decisions instead of
//! the reported time — the licence neither silently extends nor stops the line.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Backward movement tolerated without comment, covering NTP corrections.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// What the guard concluded about the reported time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockVerdict {
    /// Time moved forward, or backward within tolerance.
    Trusted,
    /// Time moved backward beyond tolerance. Licence decisions use the
    /// high-water mark; production is unaffected.
    WoundBack {
        /// How far back the clock appears to have gone.
        by_seconds: i64,
    },
}

impl ClockVerdict {
    /// Whether the reported time can be believed.
    #[must_use]
    pub fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

/// Persisted monotonic high-water mark.
///
/// The caller is responsible for storing and reloading this — usually a small
/// file next to the entitlement, or a row in the edge database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockGuard {
    /// Latest instant this box has ever observed.
    pub high_water: DateTime<Utc>,
    /// Backward movement tolerated without comment.
    pub tolerance_secs: i64,
}

impl ClockGuard {
    /// Start a guard at a known-good instant.
    #[must_use]
    pub fn new(seed: DateTime<Utc>) -> Self {
        Self {
            high_water: seed,
            tolerance_secs: DEFAULT_TOLERANCE_SECS,
        }
    }

    /// Observe the current time and return the instant to use for licence
    /// decisions, along with the verdict.
    ///
    /// Always returns a usable instant. There is no failure mode that leaves a
    /// caller unable to proceed.
    #[must_use]
    pub fn observe(&mut self, reported: DateTime<Utc>) -> (DateTime<Utc>, ClockVerdict) {
        if reported >= self.high_water {
            self.high_water = reported;
            return (reported, ClockVerdict::Trusted);
        }

        let back = (self.high_water - reported).num_seconds();
        if back <= self.tolerance_secs {
            // Ordinary NTP correction. Believe it, but do not lower the mark:
            // the high-water mark only ever advances.
            (reported, ClockVerdict::Trusted)
        } else {
            // Use the high-water mark, so winding the clock back cannot buy
            // extra licence validity. Production is untouched either way.
            (
                self.high_water,
                ClockVerdict::WoundBack { by_seconds: back },
            )
        }
    }

    /// Advance the mark without evaluating, e.g. after a trusted NTP sync.
    pub fn bump(&mut self, t: DateTime<Utc>) {
        if t > self.high_water {
            self.high_water = t;
        }
    }

    /// How far the mark is behind a reported time.
    #[must_use]
    pub fn drift_behind(&self, reported: DateTime<Utc>) -> Duration {
        reported - self.high_water
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
    fn time_moving_forward_is_trusted_and_advances_the_mark() {
        let mut g = ClockGuard::new(t(0));
        let (used, v) = g.observe(t(3_600));
        assert_eq!(used, t(3_600));
        assert!(v.is_trusted());
        assert_eq!(g.high_water, t(3_600));
    }

    #[test]
    fn small_backward_correction_is_tolerated() {
        // NTP nudging the clock back a minute is normal, not an attack.
        let mut g = ClockGuard::new(t(1_000));
        let (used, v) = g.observe(t(940));
        assert_eq!(used, t(940));
        assert!(v.is_trusted());
        assert_eq!(g.high_water, t(1_000), "the mark must never go backwards");
    }

    #[test]
    fn winding_the_clock_back_a_year_does_not_buy_licence_validity() {
        let mut g = ClockGuard::new(t(0));
        g.bump(t(31_536_000)); // a year of normal running
        let (used, v) = g.observe(t(0)); // someone sets the clock back

        assert_eq!(
            used,
            t(31_536_000),
            "licence decisions must use the high-water mark"
        );
        match v {
            ClockVerdict::WoundBack { by_seconds } => assert_eq!(by_seconds, 31_536_000),
            ClockVerdict::Trusted => panic!("a year backwards must not be trusted"),
        }
    }

    #[test]
    fn a_wound_back_clock_still_yields_a_usable_instant() {
        // The guard must never leave a caller unable to proceed.
        let mut g = ClockGuard::new(t(10_000));
        let (used, _) = g.observe(t(0));
        assert!(used >= t(10_000));
    }

    #[test]
    fn guard_survives_a_round_trip_through_storage() {
        let g = ClockGuard::new(t(500));
        let json = serde_json::to_string(&g).unwrap();
        let back: ClockGuard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }
}
