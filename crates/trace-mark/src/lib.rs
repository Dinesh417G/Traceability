//! # trace-mark
//!
//! Templates, the binding engine, and native ZPL II generation.
//!
//! ## One binding engine, every output
//!
//! `{{unit.uid}}` means the same thing on a paper label, in a laser mark, and
//! on the trace page. That is not tidiness — it is what stops the identity on
//! the part disagreeing with the identity in the database. The renderer varies;
//! the binding never does.
//!
//! ## ZPL is generated natively
//!
//! No printer driver, no OS print subsystem, no vendor DLL. Trace emits ZPL II
//! and writes it to TCP port 9100. That is what keeps the station app identical
//! on Windows and Linux, and it means a printer can be swapped without touching
//! an operator terminal.
//!
//! ## Reprinting is a controlled event
//!
//! A duplicate label in the field is a genuine traceability failure: two
//! physical objects carrying one identity destroys the record for both. So a
//! reprint needs a reason code and a supervisor, and every attempt is recorded.
//! Treat it as a security feature, not a convenience.
//!
//! ## Laser marking
//!
//! Deferred — see `docs/LASER-DPM-DEFERRED.md`. [`marker::MarkerDriver`] is the
//! seam it will plug into, and it exists now so that adding a laser is one new
//! file rather than a refactor. Do not remove it.

#![warn(missing_docs)]

pub mod binding;
pub mod marker;
pub mod sink;
pub mod template;
pub mod zpl;

pub use binding::{BindingContext, BindingError, render_expression};
pub use marker::{MarkerDriver, MarkerError, SimulatedMarker};
pub use sink::{FileSink, LabelSink, TcpSink};
pub use template::{Element, LabelTemplate, TemplateError};
pub use zpl::render_zpl;
