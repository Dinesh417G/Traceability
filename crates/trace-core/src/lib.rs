//! # trace-core
//!
//! Domain types, the route state machine and the gates, for ElectronIx Trace.
//!
//! This crate performs **no I/O**. That is not an aesthetic preference: it is
//! what lets an entire production route be exercised in unit tests and in
//! `route-sim` with no plant, no PLC, no printer and no database. If you find
//! yourself wanting to add `tokio`, `sqlx` or `reqwest` here, the logic you are
//! writing belongs in an outer crate.
//!
//! ## Shape of the model
//!
//! ```text
//!   job card ──> unit (ULID identity) ──> route (a validated DAG)
//!                  │                        │
//!                  │                        └─ operation ──> gates ──> advance
//!                  │                                            │
//!                  ├─ unit_event   (append-only, hash chained)   └─ or a failure path
//!                  ├─ measurement  (append-only, hash chained)
//!                  └─ genealogy    (what went into it)
//! ```
//!
//! ## The rules this crate enforces
//!
//! * **Records are immutable.** Corrections are new rows that supersede old
//!   ones ([`unit::Measurement::supersedes`]), never updates.
//! * **A scrapped UID is retired forever** ([`unit::UnitState::can_transition_to`]).
//! * **Nothing about a line's shape is hardcoded.** A station serves a *set* of
//!   operations, and the route decides what is valid for the unit in front of
//!   it ([`engine::RouteEngine::resolve_operation`]).
//! * **Every limit is configuration** ([`dcp::DataCollectionPoint`]), never a
//!   constant in code.

#![doc(html_no_source)]
#![warn(missing_docs)]

pub mod clock;
pub mod config;
pub mod dcp;
pub mod engine;
pub mod error;
pub mod hash;
pub mod id;
pub mod jobcard;
pub mod route;
pub mod unit;

pub use error::{CoreError, Result};
pub use id::{Code, EventId, MarkId, MeasurementId, PublicId, RowId, UnitUid};
