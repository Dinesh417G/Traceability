//! Shared application state.

use crate::config::EdgeConfig;
use std::sync::Arc;
use std::sync::RwLock;
use trace_license::{Enforcement, LicenseStatus};
use trace_store::Store;

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// Database.
    pub store: Store,
    /// Configuration.
    pub config: Arc<EdgeConfig>,
    /// Current entitlement state.
    ///
    /// Behind a lock because a new entitlement can be installed at runtime —
    /// over the network or from a USB stick — without restarting the service
    /// and therefore without interrupting a shift.
    license: Arc<RwLock<LicenseStatus>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("plant_id", &self.config.plant_id)
            .field("enforcement", &self.enforcement())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Build state.
    #[must_use]
    pub fn new(store: Store, config: EdgeConfig, license: LicenseStatus) -> Self {
        Self {
            store,
            config: Arc::new(config),
            license: Arc::new(RwLock::new(license)),
        }
    }

    /// Current licence status.
    ///
    /// A poisoned lock falls back to `unlicensed`, which is restricted but
    /// still allows production. Panicking here would take a line down over a
    /// licensing detail, which is precisely the outcome the whole licensing
    /// design exists to avoid.
    #[must_use]
    pub fn license(&self) -> LicenseStatus {
        self.license
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| LicenseStatus::unlicensed())
    }

    /// Current enforcement level.
    #[must_use]
    pub fn enforcement(&self) -> Enforcement {
        self.license().enforcement
    }

    /// Install a new entitlement without restarting.
    pub fn set_license(&self, status: LicenseStatus) {
        if let Ok(mut g) = self.license.write() {
            *g = status;
        }
    }
}
