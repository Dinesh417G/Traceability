//! # trace-billing
//!
//! Stripe integration for ElectronIx Trace.
//!
//! ## Where this code runs, and where it must not
//!
//! ```text
//!    Stripe  <--- HTTPS + webhooks --->  CONTROL PLANE  (this crate)
//!                                              |
//!                                        issues an Ed25519-signed
//!                                        Entitlement
//!                                              |
//!                                              v
//!                                     EDGE BOX on a plant LAN
//!                                     verifies locally, offline,
//!                                     with trace-license.
//! ```
//!
//! **The edge box never calls Stripe.** It cannot: a factory LAN may have no
//! internet at all, and the charter forbids anything in the runtime path from
//! blocking on a remote call. Putting a Stripe API call anywhere a station can
//! reach would make a payment provider's availability a dependency of a
//! production line, which is not a trade anyone would accept if stated plainly.
//!
//! So this crate turns subscription state into a **signed entitlement**, and
//! that document — not an API call — is what travels to the plant. It can
//! arrive over the network when there is one, or on a USB stick when there is
//! not; the verification path is identical either way, with no "trusted because
//! it came from a local disk" shortcut.
//!
//! ## What a lapsed subscription does
//!
//! It restricts configuration changes and cloud sync. **It never stops
//! production.** See `trace-license` for why that is not negotiable.

#![warn(missing_docs)]

pub mod catalog;
pub mod error;
pub mod issue;
pub mod webhook;

#[cfg(feature = "http")]
pub mod client;

pub use catalog::{PlanCatalog, PlanDefinition};
pub use error::BillingError;
pub use issue::{IssueRequest, SubscriptionState, SubscriptionStatus};
pub use webhook::{StripeEvent, WebhookVerifier};
