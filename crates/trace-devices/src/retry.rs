//! Reconnect backoff.
//!
//! A device that has gone away must be retried, but not in a tight loop: a
//! station hammering a dead PLC burns CPU that the operator UI needs, and can
//! keep a flapping switch saturated. Backoff is exponential with a cap and
//! full jitter.
//!
//! Jitter matters more than it looks on a plant network. Without it, twenty
//! stations that lost the same switch all retry at exactly the same instants
//! forever, so the network recovers into a thundering herd and drops them all
//! again.

use std::time::Duration;

/// Exponential backoff with a cap and jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Longest delay between retries.
    pub max: Duration,
    /// Multiplier applied to each successive delay.
    pub factor: u32,
    attempt: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(30), 2)
    }
}

impl Backoff {
    /// Build a backoff policy.
    #[must_use]
    pub fn new(initial: Duration, max: Duration, factor: u32) -> Self {
        Self {
            initial,
            max,
            factor: factor.max(1),
            attempt: 0,
        }
    }

    /// Delay for the next attempt, advancing the sequence.
    ///
    /// Applies full jitter: the result is uniformly distributed between zero
    /// and the computed ceiling.
    #[must_use]
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        jitter(ceiling)
    }

    /// The un-jittered ceiling for the current attempt.
    #[must_use]
    pub fn ceiling(&self) -> Duration {
        let mut d = self.initial;
        for _ in 0..self.attempt {
            d = d.saturating_mul(self.factor);
            if d >= self.max {
                return self.max;
            }
        }
        d.min(self.max)
    }

    /// Attempts made since the last reset.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Reset after a success.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Full jitter over `[0, ceiling]`.
///
/// Uses a cheap time-seeded value rather than pulling in an RNG dependency:
/// the requirement is that stations desynchronise, not cryptographic quality.
fn jitter(ceiling: Duration) -> Duration {
    let nanos = ceiling.as_nanos().max(1);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    // Mix the low bits so successive calls within the same millisecond differ.
    let mixed = seed.wrapping_mul(6_364_136_223_846_793_005).rotate_left(17);
    Duration::from_nanos((mixed % nanos) as u64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_ceiling_grows_exponentially_then_stops_at_max() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(10), 2);
        let mut seen = vec![b.ceiling()];
        for _ in 0..12 {
            let _ = b.next_delay();
            seen.push(b.ceiling());
        }
        assert_eq!(seen[0], Duration::from_millis(100));
        assert_eq!(seen[1], Duration::from_millis(200));
        assert_eq!(seen[2], Duration::from_millis(400));
        assert_eq!(
            *seen.last().unwrap(),
            Duration::from_secs(10),
            "must cap, not grow forever"
        );
    }

    #[test]
    fn jitter_keeps_every_delay_inside_the_ceiling() {
        let mut b = Backoff::new(Duration::from_millis(50), Duration::from_secs(5), 2);
        for _ in 0..50 {
            let ceiling = b.ceiling();
            let d = b.next_delay();
            assert!(d <= ceiling, "{d:?} exceeded ceiling {ceiling:?}");
        }
    }

    #[test]
    fn a_success_resets_the_sequence() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            let _ = b.next_delay();
        }
        assert_eq!(b.attempts(), 5);
        b.reset();
        assert_eq!(b.attempts(), 0);
        assert_eq!(b.ceiling(), b.initial);
    }

    #[test]
    fn a_zero_factor_does_not_stall_the_sequence() {
        // Bad configuration must degrade to constant retries, not to zero.
        let mut b = Backoff::new(Duration::from_millis(10), Duration::from_secs(1), 0);
        let _ = b.next_delay();
        assert!(b.ceiling() >= Duration::from_millis(10));
    }

    #[test]
    fn overflow_does_not_panic_after_many_attempts() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60), 10);
        for _ in 0..200 {
            let _ = b.next_delay();
        }
        assert_eq!(b.ceiling(), Duration::from_secs(60));
    }
}
