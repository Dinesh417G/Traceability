//! The entitlement document and how it is evaluated.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;
use trace_sign::{Envelope, TrustedKey};

/// Errors from evaluating an entitlement.
#[derive(Debug, Error)]
pub enum LicenseError {
    /// The signature did not verify, or the envelope was malformed.
    #[error("entitlement is not trustworthy: {0}")]
    Signature(#[from] trace_sign::SignError),

    /// The entitlement was issued for a different edge box.
    #[error("entitlement is node-locked to {expected}, this box is {actual}")]
    WrongNode {
        /// Node the entitlement names.
        expected: String,
        /// Node actually running.
        actual: String,
    },

    /// The document uses a schema this build does not understand.
    ///
    /// Refusing here is deliberate: silently ignoring unknown fields could mean
    /// ignoring a restriction the issuer intended to apply.
    #[error("entitlement schema {found} is newer than this build supports ({supported})")]
    UnsupportedSchema {
        /// Schema version in the document.
        found: u32,
        /// Highest schema this build understands.
        supported: u32,
    },
}

/// Schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Commercial tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Tier {
    /// Single line, manual stations, labels. The MSME entry point.
    Starter,
    /// Multiple lines, device drivers, full genealogy and recall.
    Professional,
    /// Multi-plant, cloud sync, API access.
    Enterprise,
}

/// A capability an entitlement may unlock.
///
/// Features gate *configuration and integration*, never data capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Feature {
    /// Serial, Modbus and PLC device drivers.
    DeviceDrivers,
    /// Laser direct part marking. Deferred, but the entitlement models it now
    /// so enabling it later is a billing change rather than a release.
    LaserMarking,
    /// Outbox drain to the cloud aggregator.
    CloudSync,
    /// Cross-plant recall queries.
    MultiPlant,
    /// External REST API access.
    ApiAccess,
    /// Remote diagnostics and support bundles.
    RemoteDiagnostics,
}

/// Numeric caps carried by an entitlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Maximum concurrently active stations.
    pub max_stations: u32,
    /// Maximum plants on this box.
    pub max_plants: u32,
    /// Maximum named operator accounts.
    pub max_users: u32,
}

impl Default for Limits {
    fn default() -> Self {
        // Starter-shaped defaults, used when a document omits limits.
        Self {
            max_stations: 3,
            max_plants: 1,
            max_users: 10,
        }
    }
}

/// What a customer has paid for, as signed by the control plane.
///
/// This document is produced from Stripe subscription state by `trace-billing`
/// and consumed here. The edge box never talks to Stripe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    /// Document schema version.
    pub schema: u32,
    /// Unique id of this entitlement, for support and audit.
    pub license_id: String,
    /// Tenant this entitlement belongs to, by natural key.
    pub tenant_code: String,
    /// Edge box this entitlement is locked to. `None` means unlocked, which is
    /// only appropriate for evaluation licences.
    pub node_id: Option<String>,
    /// Commercial tier.
    pub tier: Tier,
    /// Numeric caps.
    pub limits: Limits,
    /// Unlocked capabilities.
    pub features: BTreeSet<Feature>,
    /// When it was issued.
    pub issued_at: DateTime<Utc>,
    /// Not valid before this instant.
    pub not_before: DateTime<Utc>,
    /// Paid-through instant.
    pub expires_at: DateTime<Utc>,
    /// Days after `expires_at` during which everything still works normally.
    ///
    /// Generous by design: a plant should never discover a billing problem as a
    /// production outage.
    pub grace_days: u32,
    /// Customer-facing name, for the admin UI.
    pub customer_name: String,
}

/// How much capability is currently available.
///
/// **There is no variant that stops production.** See the crate documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Everything the tier allows.
    Full,
    /// Past expiry but inside the grace window. Everything still works; the UI
    /// warns.
    Grace,
    /// Past grace, or no valid entitlement at all. **Production capture,
    /// marking and the trace page continue.** Configuration changes, adding
    /// stations and cloud sync are blocked until the licence is renewed.
    Restricted,
}

impl Enforcement {
    /// Whether production data capture is permitted.
    ///
    /// Always true. This method exists so call sites read honestly and so the
    /// invariant is visible in the test suite rather than only in prose.
    #[must_use]
    pub fn allows_production(self) -> bool {
        true
    }

    /// Whether configuration may be changed.
    #[must_use]
    pub fn allows_configuration_change(self) -> bool {
        matches!(self, Self::Full | Self::Grace)
    }

    /// Whether the outbox may be drained to the cloud.
    #[must_use]
    pub fn allows_cloud_sync(self) -> bool {
        matches!(self, Self::Full | Self::Grace)
    }

    /// Whether the operator should be shown a licensing warning.
    #[must_use]
    pub fn should_warn(self) -> bool {
        !matches!(self, Self::Full)
    }
}

/// The evaluated state of an entitlement at a point in time.
#[derive(Debug, Clone)]
pub struct LicenseStatus {
    /// The entitlement, if one verified. `None` when the box has none.
    pub entitlement: Option<Entitlement>,
    /// Current enforcement level.
    pub enforcement: Enforcement,
    /// Human-readable explanation, safe to show an operator.
    pub summary: String,
    /// Days until expiry; negative once past it.
    pub days_to_expiry: Option<i64>,
}

impl LicenseStatus {
    /// The status of a box with no entitlement installed at all.
    ///
    /// Restricted, not stopped: a brand-new box must still be able to run a
    /// line while the paperwork catches up.
    #[must_use]
    pub fn unlicensed() -> Self {
        Self {
            entitlement: None,
            enforcement: Enforcement::Restricted,
            summary: "No licence installed. Production continues; configuration is locked."
                .to_owned(),
            days_to_expiry: None,
        }
    }

    /// Whether a feature is available right now.
    #[must_use]
    pub fn has_feature(&self, f: Feature) -> bool {
        if self.enforcement == Enforcement::Restricted {
            return false;
        }
        self.entitlement
            .as_ref()
            .is_some_and(|e| e.features.contains(&f))
    }

    /// The station cap in force, if any.
    #[must_use]
    pub fn max_stations(&self) -> Option<u32> {
        self.entitlement.as_ref().map(|e| e.limits.max_stations)
    }
}

impl Entitlement {
    /// Verify a signed envelope and evaluate it for this box at this instant.
    ///
    /// Verification is local and offline: an Ed25519 check against a public key
    /// the box already holds.
    ///
    /// # Errors
    /// Returns [`LicenseError::Signature`] if the envelope does not verify,
    /// [`LicenseError::WrongNode`] if it is locked to a different box, or
    /// [`LicenseError::UnsupportedSchema`] for a newer document format.
    pub fn verify(
        envelope: &Envelope,
        key: &TrustedKey,
        node_id: &str,
        now: DateTime<Utc>,
    ) -> Result<LicenseStatus, LicenseError> {
        let ent: Self = key.verify(envelope)?;

        if ent.schema > SCHEMA_VERSION {
            return Err(LicenseError::UnsupportedSchema {
                found: ent.schema,
                supported: SCHEMA_VERSION,
            });
        }

        if let Some(locked) = &ent.node_id
            && locked != node_id
        {
            return Err(LicenseError::WrongNode {
                expected: locked.clone(),
                actual: node_id.to_owned(),
            });
        }

        Ok(ent.evaluate(now))
    }

    /// Evaluate an already-trusted entitlement at an instant.
    #[must_use]
    pub fn evaluate(self, now: DateTime<Utc>) -> LicenseStatus {
        let grace_end = self.expires_at + Duration::days(i64::from(self.grace_days));
        let days_to_expiry = (self.expires_at - now).num_days();

        let (enforcement, summary) = if now < self.not_before {
            // A future-dated licence is not yet in force. Restricted rather
            // than rejected, so a box provisioned early still runs.
            (
                Enforcement::Restricted,
                format!(
                    "Licence is not valid until {}.",
                    self.not_before.date_naive()
                ),
            )
        } else if now <= self.expires_at {
            (
                Enforcement::Full,
                format!(
                    "{:?} licence for {}, valid to {}.",
                    self.tier,
                    self.customer_name,
                    self.expires_at.date_naive()
                ),
            )
        } else if now <= grace_end {
            (
                Enforcement::Grace,
                format!(
                    "Licence expired on {}. Grace period ends {}. Production is unaffected.",
                    self.expires_at.date_naive(),
                    grace_end.date_naive()
                ),
            )
        } else {
            (
                Enforcement::Restricted,
                format!(
                    "Licence expired on {} and the grace period ended {}. \
                     Production continues; configuration is locked until renewal.",
                    self.expires_at.date_naive(),
                    grace_end.date_naive()
                ),
            )
        };

        LicenseStatus {
            entitlement: Some(self),
            enforcement,
            summary,
            days_to_expiry: Some(days_to_expiry),
        }
    }

    /// Stable node identity derived from a machine fingerprint.
    ///
    /// Hashed rather than stored raw so an entitlement file does not leak a
    /// customer's hardware identifiers to anyone who reads it.
    #[must_use]
    pub fn node_id_from_fingerprint(fingerprint: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(fingerprint.as_bytes());
        hex::encode(&h.finalize()[..8])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use trace_sign::Issuer;

    fn ent(expires_in_days: i64, grace_days: u32, node: Option<&str>) -> Entitlement {
        let now = Utc::now();
        Entitlement {
            schema: SCHEMA_VERSION,
            license_id: "01J000000000000000000000AA".into(),
            tenant_code: "ACME".into(),
            node_id: node.map(str::to_owned),
            tier: Tier::Professional,
            limits: Limits {
                max_stations: 12,
                max_plants: 1,
                max_users: 50,
            },
            features: [Feature::DeviceDrivers, Feature::RemoteDiagnostics]
                .into_iter()
                .collect(),
            issued_at: now - Duration::days(30),
            not_before: now - Duration::days(30),
            expires_at: now + Duration::days(expires_in_days),
            grace_days,
            customer_name: "Acme Pumps Pvt Ltd".into(),
        }
    }

    #[test]
    fn a_valid_licence_grants_everything_in_its_tier() {
        let issuer = Issuer::generate();
        let env = issuer.sign(&ent(30, 30, Some("box-1"))).unwrap();
        let st = Entitlement::verify(&env, &issuer.verifier(), "box-1", Utc::now()).unwrap();

        assert_eq!(st.enforcement, Enforcement::Full);
        assert!(st.has_feature(Feature::DeviceDrivers));
        assert!(
            !st.has_feature(Feature::CloudSync),
            "not in this tier's feature set"
        );
        assert_eq!(st.max_stations(), Some(12));
        assert!(!st.enforcement.should_warn());
    }

    #[test]
    fn expiry_enters_grace_and_production_is_untouched() {
        let issuer = Issuer::generate();
        let env = issuer.sign(&ent(-5, 30, Some("box-1"))).unwrap();
        let st = Entitlement::verify(&env, &issuer.verifier(), "box-1", Utc::now()).unwrap();

        assert_eq!(st.enforcement, Enforcement::Grace);
        assert!(
            st.enforcement.allows_production(),
            "grace must never stop the line"
        );
        assert!(st.enforcement.allows_configuration_change());
        assert!(st.enforcement.should_warn());
        assert!(st.summary.contains("Production is unaffected"));
    }

    #[test]
    fn past_grace_restricts_configuration_but_never_production() {
        // The single most important assertion in this crate.
        let issuer = Issuer::generate();
        let env = issuer.sign(&ent(-60, 30, Some("box-1"))).unwrap();
        let st = Entitlement::verify(&env, &issuer.verifier(), "box-1", Utc::now()).unwrap();

        assert_eq!(st.enforcement, Enforcement::Restricted);
        assert!(
            st.enforcement.allows_production(),
            "an unpaid invoice must never destroy the traceability record for units \
             physically on the line"
        );
        assert!(!st.enforcement.allows_configuration_change());
        assert!(!st.enforcement.allows_cloud_sync());
    }

    #[test]
    fn an_unlicensed_box_still_runs_the_line() {
        let st = LicenseStatus::unlicensed();
        assert_eq!(st.enforcement, Enforcement::Restricted);
        assert!(st.enforcement.allows_production());
        assert!(!st.has_feature(Feature::DeviceDrivers));
    }

    #[test]
    fn every_enforcement_level_allows_production() {
        // Guards the invariant against a future variant being added carelessly.
        for e in [
            Enforcement::Full,
            Enforcement::Grace,
            Enforcement::Restricted,
        ] {
            assert!(e.allows_production(), "{e:?} must allow production");
        }
    }

    #[test]
    fn a_licence_for_another_box_is_rejected() {
        let issuer = Issuer::generate();
        let env = issuer.sign(&ent(30, 30, Some("box-1"))).unwrap();
        let err = Entitlement::verify(&env, &issuer.verifier(), "box-2", Utc::now()).unwrap_err();
        assert!(matches!(err, LicenseError::WrongNode { .. }));
    }

    #[test]
    fn an_unlocked_licence_runs_on_any_box() {
        let issuer = Issuer::generate();
        let env = issuer.sign(&ent(30, 30, None)).unwrap();
        assert!(Entitlement::verify(&env, &issuer.verifier(), "any-box", Utc::now()).is_ok());
    }

    #[test]
    fn a_forged_licence_is_rejected() {
        let issuer = Issuer::generate();
        let attacker = Issuer::generate();
        let env = attacker.sign(&ent(3650, 30, Some("box-1"))).unwrap();
        let err = Entitlement::verify(&env, &issuer.verifier(), "box-1", Utc::now()).unwrap_err();
        assert!(matches!(err, LicenseError::Signature(_)));
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_partially_understood() {
        // Silently ignoring unknown fields could mean ignoring a restriction.
        let issuer = Issuer::generate();
        let mut e = ent(30, 30, Some("box-1"));
        e.schema = SCHEMA_VERSION + 1;
        let env = issuer.sign(&e).unwrap();
        let err = Entitlement::verify(&env, &issuer.verifier(), "box-1", Utc::now()).unwrap_err();
        assert!(matches!(err, LicenseError::UnsupportedSchema { .. }));
    }

    #[test]
    fn a_future_dated_licence_is_not_yet_in_force() {
        let issuer = Issuer::generate();
        let now = Utc::now();
        let mut e = ent(60, 30, Some("box-1"));
        e.not_before = now + Duration::days(10);
        let env = issuer.sign(&e).unwrap();
        let st = Entitlement::verify(&env, &issuer.verifier(), "box-1", now).unwrap();
        assert_eq!(st.enforcement, Enforcement::Restricted);
        assert!(st.enforcement.allows_production());
    }

    #[test]
    fn node_id_does_not_leak_the_raw_fingerprint() {
        let id = Entitlement::node_id_from_fingerprint("SERIAL-ABC-123:MAC-00:11:22");
        assert_eq!(id.len(), 16);
        assert!(!id.contains("SERIAL"));
        assert_eq!(
            id,
            Entitlement::node_id_from_fingerprint("SERIAL-ABC-123:MAC-00:11:22")
        );
    }
}
