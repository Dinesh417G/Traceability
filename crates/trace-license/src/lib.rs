//! # trace-license
//!
//! What a customer is entitled to, and how an edge box decides that offline.
//!
//! ## The rule that shapes this whole crate
//!
//! **A licence check never stops production.**
//!
//! An edge box sits on a plant LAN with units physically on the line. If a
//! subscription lapses, or an entitlement file expires, or the clock is wrong,
//! the worst thing this software can do is stop capturing traceability data:
//! that destroys the record for units that are being built right now, and those
//! units then cannot be shipped at all. An unpaid invoice is a commercial
//! problem with commercial remedies. A hole in the traceability record is
//! permanent.
//!
//! So enforcement degrades **non-production capability** — adding stations,
//! changing configuration, cloud sync — and never production capture. There is
//! deliberately no `Enforcement::Stopped` variant. That is not an oversight; it
//! is the design, and [`Enforcement`] is a closed enum precisely so nobody can
//! add one later without reading this.
//!
//! ## Offline by construction
//!
//! Verification is a local Ed25519 signature check against a public key the box
//! already holds. It performs no I/O, needs no network, and cannot block.

#![warn(missing_docs)]

pub mod clockguard;
pub mod entitlement;

pub use clockguard::{ClockGuard, ClockVerdict};
pub use entitlement::{
    Enforcement, Entitlement, Feature, LicenseError, LicenseStatus, Limits, SCHEMA_VERSION, Tier,
};

/// Alias kept so issuers can name the schema version unambiguously.
pub use entitlement::SCHEMA_VERSION as SCHEMA_VERSION_EXPORT;
