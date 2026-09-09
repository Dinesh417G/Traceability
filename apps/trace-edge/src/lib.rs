//! # trace-edge
//!
//! The plant's edge service: Axum HTTP, the public `/t/{ulid}` trace page, and
//! the composition root that wires the store, devices, labels, licensing and
//! OTA together.
//!
//! Exposed as a library so integration tests exercise the same code the binary
//! runs, rather than a re-compiled copy of it.
//!
//! Nothing in this crate's runtime path calls the internet. Licensing is a
//! local signature check, billing lives in the control plane, and OTA is pulled
//! on a schedule that never blocks a request.

#![warn(missing_docs)]

pub mod config;
pub mod page;
pub mod routes;
pub mod state;

pub use config::EdgeConfig;
pub use routes::router;
pub use state::AppState;
