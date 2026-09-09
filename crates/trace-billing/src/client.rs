//! Stripe REST client.
//!
//! **Control plane only.** Nothing on a factory edge box may call any of this;
//! see the crate documentation for why. The `http` cargo feature exists so an
//! edge build can depend on the types and the webhook verifier without linking
//! an HTTP client at all.
//!
//! Only the endpoints this product actually uses are implemented. A generic
//! Stripe SDK would be a much larger surface to keep correct for no benefit.

use crate::error::BillingError;
use crate::issue::{SubscriptionState, SubscriptionStatus};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::time::Duration;

const STRIPE_API: &str = "https://api.stripe.com/v1";

/// Talks to Stripe on behalf of the control plane.
#[derive(Clone)]
pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    api_base: String,
}

// The secret key must never reach a log line or a panic message.
impl std::fmt::Debug for StripeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeClient")
            .field("secret_key", &"<redacted>")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl StripeClient {
    /// Build a client for the given secret key (`sk_live_...` / `sk_test_...`).
    ///
    /// # Errors
    /// Returns [`BillingError::Transport`] if the HTTP client cannot be built.
    pub fn new(secret_key: impl Into<String>) -> Result<Self, BillingError> {
        let http = reqwest::Client::builder()
            // A hung billing call must not hold a control-plane worker open.
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("electronix-trace/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BillingError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            secret_key: secret_key.into(),
            api_base: STRIPE_API.to_owned(),
        })
    }

    /// Point the client at a different base URL, for a mock server in tests.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<T, BillingError> {
        let resp = self
            .http
            .post(format!("{}{path}", self.api_base))
            .basic_auth(&self.secret_key, None::<&str>)
            .form(form)
            .send()
            .await
            .map_err(|e| BillingError::Transport(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, BillingError> {
        let resp = self
            .http
            .get(format!("{}{path}", self.api_base))
            .basic_auth(&self.secret_key, None::<&str>)
            .send()
            .await
            .map_err(|e| BillingError::Transport(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, BillingError> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| BillingError::Transport(e.to_string()))?;

        if !status.is_success() {
            // Surface Stripe's own message: it is far more useful than the
            // status code alone when diagnosing a declined card or a bad key.
            let message = serde_json::from_str::<StripeErrorEnvelope>(&body)
                .ok()
                .map_or_else(|| body.clone(), |e| e.error.message);
            return Err(BillingError::Api {
                status: status.as_u16(),
                message,
            });
        }

        serde_json::from_str(&body).map_err(BillingError::Parse)
    }

    /// Start a Checkout session so a customer can subscribe.
    ///
    /// # Errors
    /// Returns [`BillingError::Api`] or [`BillingError::Transport`] on failure.
    pub async fn create_checkout_session(
        &self,
        price_id: &str,
        tenant_code: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<CheckoutSession, BillingError> {
        self.post(
            "/checkout/sessions",
            &[
                ("mode", "subscription".into()),
                ("line_items[0][price]", price_id.to_owned()),
                ("line_items[0][quantity]", "1".into()),
                ("success_url", success_url.to_owned()),
                ("cancel_url", cancel_url.to_owned()),
                // Carry the tenant through so the webhook can attribute the
                // subscription without a second lookup.
                ("client_reference_id", tenant_code.to_owned()),
                ("metadata[tenant_code]", tenant_code.to_owned()),
                (
                    "subscription_data[metadata][tenant_code]",
                    tenant_code.to_owned(),
                ),
            ],
        )
        .await
    }

    /// Open the Customer Portal so a customer can manage their own billing.
    ///
    /// # Errors
    /// Returns [`BillingError::Api`] or [`BillingError::Transport`] on failure.
    pub async fn create_portal_session(
        &self,
        customer_id: &str,
        return_url: &str,
    ) -> Result<PortalSession, BillingError> {
        self.post(
            "/billing_portal/sessions",
            &[
                ("customer", customer_id.to_owned()),
                ("return_url", return_url.to_owned()),
            ],
        )
        .await
    }

    /// Fetch a subscription and reduce it to the facts the issuer needs.
    ///
    /// # Errors
    /// Returns [`BillingError::Api`] on a Stripe error, or
    /// [`BillingError::Parse`] if the subscription has no price, which would
    /// mean it was created outside this product's assumptions.
    pub async fn fetch_subscription(
        &self,
        subscription_id: &str,
    ) -> Result<SubscriptionState, BillingError> {
        let raw: RawSubscription = self
            .get(&format!("/subscriptions/{subscription_id}"))
            .await?;
        raw.into_state()
    }
}

#[derive(Debug, Deserialize)]
struct StripeErrorEnvelope {
    error: StripeErrorBody,
}

#[derive(Debug, Deserialize)]
struct StripeErrorBody {
    message: String,
}

/// A Checkout session. The customer is redirected to `url`.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutSession {
    /// Session id.
    pub id: String,
    /// Hosted Checkout URL to redirect the customer to.
    pub url: Option<String>,
}

/// A Customer Portal session.
#[derive(Debug, Clone, Deserialize)]
pub struct PortalSession {
    /// Hosted portal URL.
    pub url: String,
}

/// Stripe's subscription shape, narrowed to what we read.
#[derive(Debug, Deserialize)]
pub(crate) struct RawSubscription {
    pub id: String,
    pub customer: String,
    pub status: String,
    pub current_period_end: i64,
    #[serde(default)]
    pub cancel_at_period_end: bool,
    pub items: RawItems,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawItems {
    pub data: Vec<RawItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawItem {
    pub price: RawPrice,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawPrice {
    pub id: String,
}

impl RawSubscription {
    pub(crate) fn into_state(self) -> Result<SubscriptionState, BillingError> {
        let price_id = self
            .items
            .data
            .first()
            .map(|i| i.price.id.clone())
            .ok_or_else(|| BillingError::Api {
                status: 200,
                message: format!("subscription {} has no line items", self.id),
            })?;

        Ok(SubscriptionState {
            subscription_id: self.id,
            customer_id: self.customer,
            price_id,
            status: parse_status(&self.status),
            current_period_end: to_utc(self.current_period_end),
            cancel_at_period_end: self.cancel_at_period_end,
        })
    }
}

/// Map Stripe's status string.
///
/// An unrecognised status maps to `Incomplete` — the least entitled option —
/// so a status Stripe adds in future cannot silently grant access.
pub(crate) fn parse_status(s: &str) -> SubscriptionStatus {
    match s {
        "active" => SubscriptionStatus::Active,
        "trialing" => SubscriptionStatus::Trialing,
        "past_due" => SubscriptionStatus::PastDue,
        "unpaid" => SubscriptionStatus::Unpaid,
        "canceled" => SubscriptionStatus::Canceled,
        "paused" => SubscriptionStatus::Paused,
        _ => SubscriptionStatus::Incomplete,
    }
}

fn to_utc(unix: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(unix, 0).single().unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stripe_statuses_map_to_our_model() {
        assert_eq!(parse_status("active"), SubscriptionStatus::Active);
        assert_eq!(parse_status("past_due"), SubscriptionStatus::PastDue);
        assert_eq!(parse_status("canceled"), SubscriptionStatus::Canceled);
    }

    #[test]
    fn an_unknown_status_is_treated_as_least_entitled() {
        // A status Stripe adds next year must not silently grant access.
        let s = parse_status("some_future_status");
        assert_eq!(s, SubscriptionStatus::Incomplete);
        assert!(!s.is_entitled());
    }

    #[test]
    fn a_subscription_payload_reduces_to_the_facts_we_need() {
        let raw: RawSubscription = serde_json::from_str(
            r#"{
                "id": "sub_123",
                "customer": "cus_456",
                "status": "active",
                "current_period_end": 1760000000,
                "cancel_at_period_end": false,
                "items": {"data":[{"price":{"id":"price_professional_monthly"}}]}
            }"#,
        )
        .unwrap();

        let state = raw.into_state().unwrap();
        assert_eq!(state.subscription_id, "sub_123");
        assert_eq!(state.customer_id, "cus_456");
        assert_eq!(state.price_id, "price_professional_monthly");
        assert_eq!(state.status, SubscriptionStatus::Active);
    }

    #[test]
    fn a_subscription_with_no_line_items_is_an_error() {
        let raw: RawSubscription = serde_json::from_str(
            r#"{"id":"sub_1","customer":"cus_1","status":"active",
                "current_period_end":1760000000,"items":{"data":[]}}"#,
        )
        .unwrap();
        assert!(raw.into_state().is_err());
    }

    #[test]
    fn the_secret_key_never_appears_in_debug_output() {
        let c = StripeClient::new("sk_test_51ABCsupersecret").unwrap();
        let rendered = format!("{c:?}");
        assert!(
            !rendered.contains("supersecret"),
            "secret leaked: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }
}
