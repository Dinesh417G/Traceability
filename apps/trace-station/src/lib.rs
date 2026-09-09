//! # trace-station
//!
//! The headless half of an operator terminal.
//!
//! The Tauri 2 touch UI wraps this crate once terminal hardware is chosen.
//! Keeping the durability logic in a library rather than inside a GUI shell is
//! what makes it testable — and it is the part that actually decides whether a
//! plant loses data when a switch reboots. See `DECISIONS.md` D-006.

#![warn(missing_docs)]

pub mod spool;

pub use spool::{Spool, SpoolError, SpoolRecord};
