//! Stripe webhook verification.
//!
//! Webhooks are how the control plane learns that a subscription was renewed,
//! cancelled, or that a payment failed. They arrive as unauthenticated HTTP
//! POSTs from the public internet, so **an unverified webhook is dropped and
//! never processed**. Anything less would let anyone who knows the endpoint
//! grant themselves an Enterprise licence.
//!
//! Stripe signs with HMAC-SHA256 over `{timestamp}.{raw_body}` and sends the
//! result in a `Stripe-Signature` header:
//!
//! ```text
//! Stripe-Signature: t=1614556800,v1=5257a8...,v1=a2114d...
//! ```
//!
//! Two details are easy to get wrong and are handled explicitly here:
//!
//! * **The signature covers the raw body.** Parsing to JSON and re-serialising
//!   before verifying would change the bytes and break it — or, worse, let a
//!   payload verify while meaning something different from what was signed. The
//!   verifier therefore takes `&[u8]` and the caller must hand it the body
//!   exactly as received.
//! * **Comparison must be constant time.** A byte-by-byte early return leaks
//!   how much of a guess was right, which is enough to forge a signature over
//!   many attempts.

use crate::error::BillingError;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Default replay window. Matches Stripe's own recommendation.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// Verifies `Stripe-Signature` headers against an endpoint secret.
#[derive(Clone)]
pub struct WebhookVerifier {
    secret: String,
    tolerance_secs: i64,
}

// The endpoint secret must never reach a log line or a panic message.
impl std::fmt::Debug for WebhookVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookVerifier")
            .field("secret", &"<redacted>")
            .field("tolerance_secs", &self.tolerance_secs)
            .finish()
    }
}

impl WebhookVerifier {
    /// Build a verifier from the endpoint's signing secret (`whsec_...`).
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            tolerance_secs: DEFAULT_TOLERANCE_SECS,
        }
    }

    /// Override the replay tolerance.
    #[must_use]
    pub fn with_tolerance_secs(mut self, secs: i64) -> Self {
        self.tolerance_secs = secs;
        self
    }

    /// Verify a signature header against the raw request body, then parse.
    ///
    /// `body` must be the bytes exactly as received.
    ///
    /// # Errors
    /// Returns [`BillingError::WebhookSignature`] if the header is malformed or
    /// no signature matches, [`BillingError::WebhookReplay`] if it is outside
    /// the tolerance window, or [`BillingError::Parse`] if the verified body is
    /// not a Stripe event.
    pub fn verify(
        &self,
        body: &[u8],
        signature_header: &str,
        now: DateTime<Utc>,
    ) -> Result<StripeEvent, BillingError> {
        let header = SignatureHeader::parse(signature_header)?;

        // Check freshness before spending time on HMAC work.
        let age = now.timestamp() - header.timestamp;
        if age.abs() > self.tolerance_secs {
            return Err(BillingError::WebhookReplay {
                age_secs: age,
                tolerance_secs: self.tolerance_secs,
            });
        }

        let expected = self.compute(header.timestamp, body);
        let expected_bytes = expected.as_bytes();

        // Compare against every v1 signature present: Stripe sends more than
        // one while an endpoint secret is being rotated.
        let matched = header.v1.iter().any(|candidate| {
            candidate.len() == expected_bytes.len()
                && bool::from(candidate.as_bytes().ct_eq(expected_bytes))
        });

        if !matched {
            return Err(BillingError::WebhookSignature(
                "no v1 signature matched the computed digest".into(),
            ));
        }

        Ok(serde_json::from_slice(body)?)
    }

    /// Compute the expected hex digest for a timestamp and body.
    fn compute(&self, timestamp: i64, body: &[u8]) -> String {
        // `new_from_slice` accepts any key length for HMAC, so this cannot fail
        // in practice; treating it as fatal would be worse than proceeding with
        // a digest that simply will not match.
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(self.secret.as_bytes())
            .unwrap_or_else(|_| unreachable!("HMAC accepts keys of any length"));
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Produce a valid header for a body. Test helper, and useful for
    /// exercising a staging endpoint without Stripe.
    #[must_use]
    pub fn sign_for_test(&self, body: &[u8], timestamp: i64) -> String {
        format!("t={timestamp},v1={}", self.compute(timestamp, body))
    }
}

/// Parsed `Stripe-Signature` header.
#[derive(Debug)]
struct SignatureHeader {
    timestamp: i64,
    v1: Vec<String>,
}

impl SignatureHeader {
    fn parse(raw: &str) -> Result<Self, BillingError> {
        let mut timestamp = None;
        let mut v1 = Vec::new();

        for part in raw.split(',') {
            let Some((k, v)) = part.trim().split_once('=') else {
                continue;
            };
            match k {
                "t" => {
                    timestamp = v.parse::<i64>().ok();
                }
                "v1" => v1.push(v.to_owned()),
                // v0 is Stripe's test-mode scheme; other versions may appear.
                // Ignoring them is correct, but we must not then accept a
                // header that carried no v1 at all.
                _ => {}
            }
        }

        let timestamp = timestamp.ok_or_else(|| {
            BillingError::WebhookSignature("header has no valid t= timestamp".into())
        })?;
        if v1.is_empty() {
            return Err(BillingError::WebhookSignature(
                "header carries no v1 signature".into(),
            ));
        }
        Ok(Self { timestamp, v1 })
    }
}

/// A Stripe event, reduced to the fields this product acts on.
#[derive(Debug, Clone, Deserialize)]
pub struct StripeEvent {
    /// Stripe event id, e.g. `evt_...`. Use it for idempotency.
    pub id: String,
    /// Event type, e.g. `customer.subscription.updated`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Unix timestamp Stripe created the event.
    pub created: i64,
    /// The object the event concerns.
    pub data: EventData,
}

/// Wrapper Stripe puts the affected object in.
#[derive(Debug, Clone, Deserialize)]
pub struct EventData {
    /// The affected object, left as raw JSON so that unmodelled fields survive.
    pub object: serde_json::Value,
}

impl StripeEvent {
    /// Whether this event should cause an entitlement to be reissued.
    ///
    /// Deliberately a small allowlist: reacting to every event type would mean
    /// reissuing licences for changes that do not affect entitlement.
    #[must_use]
    pub fn affects_entitlement(&self) -> bool {
        matches!(
            self.event_type.as_str(),
            "customer.subscription.created"
                | "customer.subscription.updated"
                | "customer.subscription.deleted"
                | "customer.subscription.paused"
                | "customer.subscription.resumed"
                | "invoice.paid"
                | "invoice.payment_failed"
                | "checkout.session.completed"
        )
    }

    /// The Stripe subscription id this event concerns, if any.
    #[must_use]
    pub fn subscription_id(&self) -> Option<&str> {
        // On a subscription event the object *is* the subscription; on an
        // invoice or checkout session it is a field.
        self.data
            .object
            .get("subscription")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                let is_sub = self
                    .data
                    .object
                    .get("object")
                    .and_then(serde_json::Value::as_str)
                    == Some("subscription");
                is_sub.then(|| {
                    self.data
                        .object
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                })?
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use chrono::TimeZone;

    const BODY: &[u8] = br#"{"id":"evt_1","type":"customer.subscription.updated","created":1614556800,"data":{"object":{"object":"subscription","id":"sub_123","status":"active"}}}"#;

    fn now(ts: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(ts, 0).unwrap()
    }

    #[test]
    fn a_valid_signature_verifies_and_parses() {
        let v = WebhookVerifier::new("whsec_test");
        let header = v.sign_for_test(BODY, 1_614_556_800);
        let ev = v.verify(BODY, &header, now(1_614_556_800)).unwrap();
        assert_eq!(ev.id, "evt_1");
        assert_eq!(ev.event_type, "customer.subscription.updated");
        assert_eq!(ev.subscription_id(), Some("sub_123"));
    }

    #[test]
    fn a_forged_signature_is_rejected() {
        let real = WebhookVerifier::new("whsec_real");
        let attacker = WebhookVerifier::new("whsec_guess");
        let header = attacker.sign_for_test(BODY, 1_614_556_800);
        let err = real.verify(BODY, &header, now(1_614_556_800)).unwrap_err();
        assert!(matches!(err, BillingError::WebhookSignature(_)));
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        // The attack this defends against: sign a cheap plan, then swap the
        // body for an expensive one.
        let v = WebhookVerifier::new("whsec_test");
        let header = v.sign_for_test(BODY, 1_614_556_800);
        let tampered = br#"{"id":"evt_1","type":"customer.subscription.updated","created":1614556800,"data":{"object":{"object":"subscription","id":"sub_EVIL","status":"active"}}}"#;
        assert!(v.verify(tampered, &header, now(1_614_556_800)).is_err());
    }

    #[test]
    fn an_old_webhook_is_rejected_as_a_replay() {
        let v = WebhookVerifier::new("whsec_test");
        let header = v.sign_for_test(BODY, 1_614_556_800);
        // Same signature, replayed an hour later.
        let err = v
            .verify(BODY, &header, now(1_614_556_800 + 3_600))
            .unwrap_err();
        assert!(matches!(err, BillingError::WebhookReplay { .. }));
    }

    #[test]
    fn a_future_dated_webhook_is_also_rejected() {
        let v = WebhookVerifier::new("whsec_test");
        let header = v.sign_for_test(BODY, 1_614_556_800 + 3_600);
        assert!(v.verify(BODY, &header, now(1_614_556_800)).is_err());
    }

    #[test]
    fn rotation_works_because_all_v1_signatures_are_checked() {
        // During a secret rotation Stripe signs with both secrets.
        let old = WebhookVerifier::new("whsec_old");
        let new = WebhookVerifier::new("whsec_new");
        let ts = 1_614_556_800;
        let combined = format!(
            "t={ts},v1={},v1={}",
            old.sign_for_test(BODY, ts).split("v1=").nth(1).unwrap(),
            new.sign_for_test(BODY, ts).split("v1=").nth(1).unwrap()
        );
        assert!(old.verify(BODY, &combined, now(ts)).is_ok());
        assert!(new.verify(BODY, &combined, now(ts)).is_ok());
    }

    #[test]
    fn a_header_with_no_v1_is_rejected() {
        // A header carrying only a scheme we ignore must not be treated as
        // "nothing to check, therefore fine".
        let v = WebhookVerifier::new("whsec_test");
        let err = v
            .verify(BODY, "t=1614556800,v0=deadbeef", now(1_614_556_800))
            .unwrap_err();
        assert!(matches!(err, BillingError::WebhookSignature(_)));
    }

    #[test]
    fn a_malformed_header_is_rejected() {
        let v = WebhookVerifier::new("whsec_test");
        for header in ["", "garbage", "v1=abc", "t=notanumber,v1=abc"] {
            assert!(
                v.verify(BODY, header, now(1_614_556_800)).is_err(),
                "header {header:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_secret_never_appears_in_debug_output() {
        let v = WebhookVerifier::new("whsec_super_secret_value");
        let rendered = format!("{v:?}");
        assert!(
            !rendered.contains("super_secret"),
            "secret leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn only_entitlement_relevant_events_trigger_reissue() {
        let make = |ty: &str| StripeEvent {
            id: "evt".into(),
            event_type: ty.into(),
            created: 0,
            data: EventData {
                object: serde_json::json!({}),
            },
        };
        assert!(make("customer.subscription.updated").affects_entitlement());
        assert!(make("invoice.payment_failed").affects_entitlement());
        assert!(!make("customer.created").affects_entitlement());
        assert!(!make("charge.refunded").affects_entitlement());
    }
}
