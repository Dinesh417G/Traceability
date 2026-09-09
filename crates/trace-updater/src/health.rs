//! Deciding whether a freshly installed version actually works.
//!
//! An update that installs cleanly and then fails to start is the worst case:
//! the box looks updated and the line is down. So an update is not finished
//! when the files are in place — it is finished when the new version has
//! proved it runs.

use async_trait::async_trait;
use std::time::Duration;

/// The outcome of a health check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    /// Whether the new version is serving correctly.
    pub healthy: bool,
    /// Why not, when unhealthy.
    pub detail: String,
}

impl HealthReport {
    /// A passing report.
    #[must_use]
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            detail: String::new(),
        }
    }

    /// A failing report.
    #[must_use]
    pub fn unhealthy(detail: impl Into<String>) -> Self {
        Self {
            healthy: false,
            detail: detail.into(),
        }
    }
}

/// Checks that an installed version is serving.
///
/// A trait rather than a fixed HTTP call, so the rollback path can be tested
/// deterministically instead of by breaking a real service.
#[async_trait]
pub trait HealthProbe: Send + Sync + std::fmt::Debug {
    /// Check once.
    async fn check(&self) -> HealthReport;

    /// Poll until healthy or the deadline passes.
    ///
    /// The default polls every two seconds. A version that never becomes
    /// healthy therefore costs the deadline, not forever.
    async fn wait_until_healthy(&self, deadline: Duration) -> HealthReport {
        let started = std::time::Instant::now();
        let mut last = HealthReport::unhealthy("health probe never ran");
        while started.elapsed() < deadline {
            last = self.check().await;
            if last.healthy {
                return last;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        HealthReport::unhealthy(format!(
            "still unhealthy after {}s: {}",
            deadline.as_secs(),
            last.detail
        ))
    }
}

/// A probe that always passes. For boxes with nothing to check, and for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysHealthy;

#[async_trait]
impl HealthProbe for AlwaysHealthy {
    async fn check(&self) -> HealthReport {
        HealthReport::healthy()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug)]
    struct HealthyAfter {
        calls: Arc<AtomicU32>,
        threshold: u32,
    }

    #[async_trait]
    impl HealthProbe for HealthyAfter {
        async fn check(&self) -> HealthReport {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= self.threshold {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy(format!("starting up, attempt {n}"))
            }
        }
    }

    #[tokio::test]
    async fn a_service_that_takes_a_moment_to_start_still_passes() {
        let probe = HealthyAfter {
            calls: Arc::new(AtomicU32::new(0)),
            threshold: 2,
        };
        let report = probe.wait_until_healthy(Duration::from_secs(30)).await;
        assert!(report.healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn a_service_that_never_starts_fails_at_the_deadline() {
        let probe = HealthyAfter {
            calls: Arc::new(AtomicU32::new(0)),
            threshold: u32::MAX,
        };
        let report = probe.wait_until_healthy(Duration::from_secs(10)).await;
        assert!(!report.healthy);
        assert!(report.detail.contains("still unhealthy after 10s"));
    }

    #[tokio::test]
    async fn always_healthy_passes_immediately() {
        assert!(AlwaysHealthy.check().await.healthy);
    }
}
