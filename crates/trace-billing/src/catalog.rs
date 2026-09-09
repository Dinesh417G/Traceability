//! Mapping Stripe prices to what a customer actually gets.
//!
//! This is **configuration, not code**. Pricing changes constantly — a new
//! tier, a promotional price, a different station cap for one large customer —
//! and none of that should require a release. The catalog is loaded from
//! config and can be reloaded without restarting.

use crate::error::BillingError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use trace_license::{Feature, Limits, Tier};

/// What one Stripe price entitles a customer to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDefinition {
    /// Stripe price id, e.g. `price_1P...`.
    pub stripe_price_id: String,
    /// Operator-facing plan name.
    pub display_name: String,
    /// Commercial tier.
    pub tier: Tier,
    /// Numeric caps.
    pub limits: Limits,
    /// Unlocked capabilities.
    pub features: Vec<Feature>,
    /// Days of grace granted after expiry before configuration locks.
    ///
    /// Generous on purpose: a plant should never meet a billing problem as a
    /// production surprise.
    pub grace_days: u32,
}

/// All plans currently on sale.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanCatalog {
    plans: BTreeMap<String, PlanDefinition>,
}

impl PlanCatalog {
    /// Build from a list of plan definitions.
    #[must_use]
    pub fn new(plans: Vec<PlanDefinition>) -> Self {
        Self {
            plans: plans
                .into_iter()
                .map(|p| (p.stripe_price_id.clone(), p))
                .collect(),
        }
    }

    /// Look up the plan for a Stripe price.
    ///
    /// # Errors
    /// Returns [`BillingError::UnknownPrice`] when a price is not in the
    /// catalog. That is a configuration gap — someone added a price in Stripe
    /// without adding it here — and is surfaced rather than defaulted, because
    /// silently granting the cheapest plan to a paying Enterprise customer
    /// would be worse than an error in the log.
    pub fn plan_for_price(&self, price_id: &str) -> Result<&PlanDefinition, BillingError> {
        self.plans
            .get(price_id)
            .ok_or_else(|| BillingError::UnknownPrice(price_id.to_owned()))
    }

    /// Every plan, for rendering a pricing page.
    #[must_use]
    pub fn all(&self) -> Vec<&PlanDefinition> {
        self.plans.values().collect()
    }

    /// Whether the catalog has any plans.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// A starting catalog matching the tiers in `DECISIONS.md` Q3.
    ///
    /// The price ids are placeholders: real ones come from the Stripe
    /// dashboard and belong in configuration, never in a binary.
    #[must_use]
    pub fn example() -> Self {
        Self::new(vec![
            PlanDefinition {
                stripe_price_id: "price_starter_monthly".into(),
                display_name: "Starter".into(),
                tier: Tier::Starter,
                limits: Limits {
                    max_stations: 3,
                    max_plants: 1,
                    max_users: 10,
                },
                features: vec![],
                grace_days: 30,
            },
            PlanDefinition {
                stripe_price_id: "price_professional_monthly".into(),
                display_name: "Professional".into(),
                tier: Tier::Professional,
                limits: Limits {
                    max_stations: 15,
                    max_plants: 1,
                    max_users: 100,
                },
                features: vec![Feature::DeviceDrivers, Feature::RemoteDiagnostics],
                grace_days: 30,
            },
            PlanDefinition {
                stripe_price_id: "price_enterprise_monthly".into(),
                display_name: "Enterprise".into(),
                tier: Tier::Enterprise,
                limits: Limits {
                    max_stations: 200,
                    max_plants: 25,
                    max_users: 1_000,
                },
                features: vec![
                    Feature::DeviceDrivers,
                    Feature::RemoteDiagnostics,
                    Feature::CloudSync,
                    Feature::MultiPlant,
                    Feature::ApiAccess,
                    Feature::LaserMarking,
                ],
                grace_days: 45,
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_example_catalog_resolves_each_tier() {
        let c = PlanCatalog::example();
        assert_eq!(
            c.plan_for_price("price_starter_monthly").unwrap().tier,
            Tier::Starter
        );
        assert_eq!(
            c.plan_for_price("price_enterprise_monthly").unwrap().tier,
            Tier::Enterprise
        );
        assert_eq!(c.all().len(), 3);
    }

    #[test]
    fn an_unmapped_price_is_an_error_not_a_silent_downgrade() {
        let c = PlanCatalog::example();
        let err = c
            .plan_for_price("price_created_in_dashboard_last_week")
            .unwrap_err();
        assert!(matches!(err, BillingError::UnknownPrice(_)));
    }

    #[test]
    fn tiers_are_ordered_so_upgrades_are_comparable() {
        assert!(Tier::Starter < Tier::Professional);
        assert!(Tier::Professional < Tier::Enterprise);
    }

    #[test]
    fn the_catalog_round_trips_through_config() {
        // Pricing must be reloadable without a release.
        let c = PlanCatalog::example();
        let json = serde_json::to_string(&c).unwrap();
        let back: PlanCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.all().len(), 3);
        assert_eq!(
            back.plan_for_price("price_professional_monthly")
                .unwrap()
                .limits
                .max_stations,
            15
        );
    }

    #[test]
    fn laser_marking_is_modelled_even_though_it_is_deferred() {
        // Enabling it later should be a billing change, not a release.
        let c = PlanCatalog::example();
        let ent = c.plan_for_price("price_enterprise_monthly").unwrap();
        assert!(ent.features.contains(&Feature::LaserMarking));
    }
}
