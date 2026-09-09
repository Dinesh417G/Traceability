//! Turning Stripe subscription state into a signed entitlement.
//!
//! This is the hinge between the two planes. Everything above it talks to
//! Stripe; everything below it is a document that verifies offline.

use crate::catalog::PlanCatalog;
use crate::error::BillingError;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use trace_license::{Entitlement, SCHEMA_VERSION_EXPORT as SCHEMA_VERSION};
use trace_sign::{Envelope, Issuer};

/// Stripe subscription status, reduced to what affects entitlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    /// Paid and current.
    Active,
    /// In a trial period.
    Trialing,
    /// Payment failed; Stripe is retrying.
    PastDue,
    /// Payment failed for long enough that Stripe gave up.
    Unpaid,
    /// Cancelled.
    Canceled,
    /// Never activated: the first payment failed.
    Incomplete,
    /// Paused by the customer or by us.
    Paused,
}

impl SubscriptionStatus {
    /// Whether this status should still grant a full-length entitlement.
    ///
    /// `PastDue` counts as entitled: Stripe is still retrying the card, and a
    /// retry that succeeds tomorrow must not have cost the customer a day of
    /// locked configuration. The entitlement's own grace period then covers the
    /// gap if the retries ultimately fail.
    #[must_use]
    pub fn is_entitled(self) -> bool {
        matches!(self, Self::Active | Self::Trialing | Self::PastDue)
    }
}

/// The subscription facts the issuer needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionState {
    /// Stripe subscription id.
    pub subscription_id: String,
    /// Stripe customer id.
    pub customer_id: String,
    /// Stripe price the customer is on.
    pub price_id: String,
    /// Current status.
    pub status: SubscriptionStatus,
    /// End of the paid period. The entitlement expires here.
    pub current_period_end: DateTime<Utc>,
    /// Whether the customer has asked to cancel at period end.
    pub cancel_at_period_end: bool,
}

/// Everything needed to issue one entitlement.
#[derive(Debug, Clone)]
pub struct IssueRequest {
    /// Subscription facts from Stripe.
    pub subscription: SubscriptionState,
    /// Tenant natural key.
    pub tenant_code: String,
    /// Customer-facing name for the admin UI.
    pub customer_name: String,
    /// Edge box to lock to. `None` issues an unlocked evaluation licence.
    pub node_id: Option<String>,
    /// Unique id for this entitlement document.
    pub license_id: String,
}

/// Issues signed entitlements from subscription state.
#[derive(Debug)]
pub struct EntitlementIssuer {
    issuer: Issuer,
    catalog: PlanCatalog,
}

impl EntitlementIssuer {
    /// Build an issuer over a signing key and a plan catalog.
    #[must_use]
    pub fn new(issuer: Issuer, catalog: PlanCatalog) -> Self {
        Self { issuer, catalog }
    }

    /// The public key edge boxes must be configured with.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        self.issuer.public_key_hex()
    }

    /// Build and sign an entitlement.
    ///
    /// A subscription that is not entitled still produces a **signed document**
    /// rather than nothing: it is simply already expired. That matters because
    /// an edge box distinguishes "expired licence, enter grace then restrict"
    /// from "no licence at all", and a cancelled customer should land in the
    /// first case with a clear expiry date, not the second.
    ///
    /// # Errors
    /// Returns [`BillingError::UnknownPrice`] if the price is not in the
    /// catalog, or [`BillingError::Signing`] if signing fails.
    pub fn issue(&self, req: &IssueRequest, now: DateTime<Utc>) -> Result<Envelope, BillingError> {
        let plan = self.catalog.plan_for_price(&req.subscription.price_id)?;

        let expires_at = if req.subscription.status.is_entitled() {
            req.subscription.current_period_end
        } else {
            // Cancelled or unpaid: expire at the end of what was actually paid
            // for, never later. If that is already past, the box enters grace
            // and then restricts, exactly as intended.
            req.subscription.current_period_end.min(now)
        };

        let features: BTreeSet<_> = plan.features.iter().copied().collect();

        let entitlement = Entitlement {
            schema: SCHEMA_VERSION,
            license_id: req.license_id.clone(),
            tenant_code: req.tenant_code.clone(),
            node_id: req.node_id.clone(),
            tier: plan.tier,
            limits: plan.limits,
            features,
            issued_at: now,
            // Backdated by a day so a box whose clock is slightly behind the
            // control plane does not reject a licence it has just been given.
            not_before: now - Duration::days(1),
            expires_at,
            grace_days: plan.grace_days,
            customer_name: req.customer_name.clone(),
        };

        Ok(self.issuer.sign(&entitlement)?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use trace_license::{Enforcement, Feature, Tier};

    fn req(status: SubscriptionStatus, period_end: DateTime<Utc>, price: &str) -> IssueRequest {
        IssueRequest {
            subscription: SubscriptionState {
                subscription_id: "sub_123".into(),
                customer_id: "cus_123".into(),
                price_id: price.into(),
                status,
                current_period_end: period_end,
                cancel_at_period_end: false,
            },
            tenant_code: "ACME".into(),
            customer_name: "Acme Pumps Pvt Ltd".into(),
            node_id: Some("box-1".into()),
            license_id: "01J000000000000000000000AA".into(),
        }
    }

    fn issuer() -> EntitlementIssuer {
        EntitlementIssuer::new(Issuer::generate(), PlanCatalog::example())
    }

    #[test]
    fn an_active_subscription_yields_a_full_licence_the_edge_box_accepts() {
        // The full loop: Stripe state -> signed document -> offline verification.
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::Active,
                    now + Duration::days(30),
                    "price_professional_monthly",
                ),
                now,
            )
            .unwrap();

        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        let status = Entitlement::verify(&env, &key, "box-1", now).unwrap();

        assert_eq!(status.enforcement, Enforcement::Full);
        assert_eq!(
            status.entitlement.as_ref().unwrap().tier,
            Tier::Professional
        );
        assert!(status.has_feature(Feature::DeviceDrivers));
        assert_eq!(status.max_stations(), Some(15));
    }

    #[test]
    fn a_past_due_subscription_still_grants_the_full_period() {
        // Stripe is still retrying the card. A retry that succeeds tomorrow
        // must not have cost the customer a day of locked configuration.
        let iss = issuer();
        let now = Utc::now();
        let period_end = now + Duration::days(10);
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::PastDue,
                    period_end,
                    "price_starter_monthly",
                ),
                now,
            )
            .unwrap();

        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        let status = Entitlement::verify(&env, &key, "box-1", now).unwrap();
        assert_eq!(status.enforcement, Enforcement::Full);
    }

    #[test]
    fn a_cancelled_subscription_expires_now_and_enters_grace() {
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::Canceled,
                    now + Duration::days(30),
                    "price_starter_monthly",
                ),
                now,
            )
            .unwrap();

        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        let status = Entitlement::verify(&env, &key, "box-1", now + Duration::seconds(1)).unwrap();

        assert_eq!(
            status.enforcement,
            Enforcement::Grace,
            "grace, not an immediate cliff"
        );
        assert!(
            status.enforcement.allows_production(),
            "cancelling a subscription must never stop a production line"
        );
    }

    #[test]
    fn a_cancelled_subscription_never_extends_past_what_was_paid_for() {
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::Canceled,
                    now + Duration::days(30),
                    "price_starter_monthly",
                ),
                now,
            )
            .unwrap();
        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        let status = Entitlement::verify(&env, &key, "box-1", now).unwrap();
        assert!(status.entitlement.unwrap().expires_at <= now);
    }

    #[test]
    fn long_after_cancellation_configuration_locks_but_production_does_not() {
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(SubscriptionStatus::Unpaid, now, "price_starter_monthly"),
                now,
            )
            .unwrap();
        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();

        // 30 days grace on Starter, so 100 days later we are well past it.
        let status = Entitlement::verify(&env, &key, "box-1", now + Duration::days(100)).unwrap();
        assert_eq!(status.enforcement, Enforcement::Restricted);
        assert!(!status.enforcement.allows_configuration_change());
        assert!(
            status.enforcement.allows_production(),
            "the line keeps running"
        );
    }

    #[test]
    fn an_entitlement_is_locked_to_the_box_it_was_issued_for() {
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::Active,
                    now + Duration::days(30),
                    "price_starter_monthly",
                ),
                now,
            )
            .unwrap();
        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        assert!(Entitlement::verify(&env, &key, "some-other-box", now).is_err());
    }

    #[test]
    fn an_unmapped_price_refuses_to_issue() {
        let iss = issuer();
        let now = Utc::now();
        let err = iss
            .issue(
                &req(
                    SubscriptionStatus::Active,
                    now + Duration::days(30),
                    "price_unknown",
                ),
                now,
            )
            .unwrap_err();
        assert!(matches!(err, BillingError::UnknownPrice(_)));
    }

    #[test]
    fn a_slightly_slow_box_clock_still_accepts_a_fresh_licence() {
        // not_before is backdated a day for exactly this case.
        let iss = issuer();
        let now = Utc::now();
        let env = iss
            .issue(
                &req(
                    SubscriptionStatus::Active,
                    now + Duration::days(30),
                    "price_starter_monthly",
                ),
                now,
            )
            .unwrap();
        let key = trace_sign::TrustedKey::from_hex(&iss.public_key_hex()).unwrap();
        let status = Entitlement::verify(&env, &key, "box-1", now - Duration::hours(6)).unwrap();
        assert_eq!(status.enforcement, Enforcement::Full);
    }

    #[test]
    fn entitlement_statuses_map_as_documented() {
        assert!(SubscriptionStatus::Active.is_entitled());
        assert!(SubscriptionStatus::Trialing.is_entitled());
        assert!(SubscriptionStatus::PastDue.is_entitled());
        assert!(!SubscriptionStatus::Canceled.is_entitled());
        assert!(!SubscriptionStatus::Unpaid.is_entitled());
        assert!(!SubscriptionStatus::Paused.is_entitled());
        assert!(!SubscriptionStatus::Incomplete.is_entitled());
    }
}
