//! # trace-devices
//!
//! One trait for every data source on the plant floor, and the simulators that
//! let the whole system be tested with no hardware at all.
//!
//! ## Two rules that shape everything here
//!
//! **Every raw payload is stored verbatim beside the parsed value.** When a
//! customer disputes a reading in two years, the raw bytes are the answer.
//! [`RawSample`] therefore always carries what the device actually said, and a
//! driver that "helpfully" normalises before returning is broken.
//!
//! **A driver never panics the station and never blocks forever.** A torque
//! wrench being unplugged mid-shift is a Tuesday, not an exception. Drivers
//! reconnect with exponential backoff and every read has a deadline.
//!
//! ## What is built
//!
//! | Driver | State | Notes |
//! |---|---|---|
//! | [`manual::ManualDriver`] | built | operator keys the value in; same path, same validation |
//! | [`tcp::TcpLineDriver`] | built | raw ASCII line protocol over TCP |
//! | [`simulator::SimulatedDriver`] | built | scriptable virtual device for CI and `route-sim` |
//! | Serial RS232/RS485, Modbus RTU/TCP, OPC UA, Siemens S7, EtherNet/IP, FOCAS | **declared, not built** | see below |
//!
//! The unbuilt drivers are deliberately absent rather than half-written. Each
//! needs real hardware to validate, and a driver that has never seen its device
//! is a liability on a production line — it looks like coverage and behaves
//! like a bug. The seam is [`DeviceDriver`]; adding one is a new file, not a
//! refactor.
//!
//! **Manual entry is not second class.** A station with no PLC captures the
//! same data collection points, against the same limits, with the same audit
//! fields. Only `source` differs.

#![warn(missing_docs)]

pub mod driver;
pub mod manual;
pub mod registry;
pub mod retry;

#[cfg(feature = "tcp")]
pub mod tcp;

#[cfg(feature = "simulator")]
pub mod simulator;

pub use driver::{DeviceConfig, DeviceDriver, DeviceError, DeviceHealth, PointRef, RawSample};
pub use registry::DeviceRegistry;
pub use retry::Backoff;
