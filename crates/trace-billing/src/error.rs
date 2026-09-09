//! Billing errors.

use thiserror::Error;

/// Errors from the billing control plane.
#[derive(Debug, Error)]
pub enum BillingError {
    /// A webhook signature header was missing, malformed, or did not match.
    #[error("webhook signature rejected: {0}")]
    WebhookSignature(String),

    /// A webhook arrived outside the replay tolerance window.
    #[error("webhook timestamp {age_secs}s outside the {tolerance_secs}s replay window")]
    WebhookReplay {
        /// How old the webhook claims to be.
        age_secs: i64,
        /// Configured tolerance.
        tolerance_secs: i64,
    },

    /// The payload was not the JSON we expected.
    #[error("cannot parse Stripe payload: {0}")]
    Parse(#[from] serde_json::Error),

    /// A Stripe price is not mapped to a plan in the catalog.
    ///
    /// This is a configuration gap, not a customer problem: someone created a
    /// price in Stripe without adding it to the catalog.
    #[error("stripe price {0} is not mapped to any plan in the catalog")]
    UnknownPrice(String),

    /// The Stripe API returned an error.
    #[error("stripe api error ({status}): {message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Message Stripe returned.
        message: String,
    },

    /// The HTTP request itself failed.
    #[error("stripe request failed: {0}")]
    Transport(String),

    /// Signing the entitlement failed.
    #[error("cannot issue entitlement: {0}")]
    Signing(#[from] trace_sign::SignError),
}
