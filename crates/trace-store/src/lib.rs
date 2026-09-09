//! # trace-store
//!
//! Postgres persistence for ElectronIx Trace: migrations, Row Level Security,
//! and repositories.
//!
//! ## Tenant scoping is structural, not a convention
//!
//! Every table has RLS keyed on `app.tenant_id`. Rather than hoping callers
//! remember to set it, this crate makes it impossible to get a query handle
//! without one: [`Store::tenant`] opens a transaction, issues
//! `SET LOCAL app.tenant_id`, and hands back a [`TenantTx`]. All repository
//! methods hang off that handle.
//!
//! An unset tenant denies every row rather than raising, so a bug cannot
//! degrade into "returns everything".
//!
//! ## Immutability
//!
//! `unit_event`, `measurement`, `genealogy_link`, `mark_record`, `test_result`,
//! `defect` and `scrap_record` are append-only, enforced by database triggers
//! as well as by this crate. Corrections are new rows that supersede old ones.

#![warn(missing_docs)]

pub mod error;
pub mod query;
pub mod repo;
pub mod store;

pub use error::{Result, StoreError};
pub use store::{Store, TenantTx};

/// Embedded migrations, applied by [`Store::migrate`].
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
