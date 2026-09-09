//! # trace-updater
//!
//! Over-the-air updates for an edge box that lives in a factory.
//!
//! The box being updated is running a production line. An update that half
//! applies, or that boots into something broken with no way back, does not
//! cost a reboot — it costs a shift. So the design is conservative in a
//! specific order:
//!
//! 1. **Verify the manifest signature before downloading anything.** An
//!    attacker who can serve bytes should not even be able to make the box
//!    spend bandwidth, let alone unpack their payload.
//! 2. **Verify each artifact's SHA-256 after downloading.** The manifest names
//!    every artifact by digest, so a truncated or substituted file is caught
//!    before it is unpacked. A digest mismatch is never retried against the
//!    same bytes.
//! 3. **Refuse downgrades** unless the manifest explicitly says it is a
//!    rollback, so a replayed old manifest cannot walk a box backwards into a
//!    known vulnerability.
//! 4. **Apply atomically.** Unpack into a staging directory, fsync, then swap a
//!    `current` symlink with `rename`. The running install is never mutated in
//!    place, so power loss mid-update leaves the old version intact.
//! 5. **Roll back automatically** if the new version fails a health probe
//!    within a deadline. The previous release is retained until the new one is
//!    confirmed healthy — not deleted the moment it is replaced.
//!
//! ## Offline is a first-class path
//!
//! Many Indian MSME plants have no internet on the shop floor. The same signed
//! bundle can arrive on a USB stick, and it goes through the **identical**
//! verification code. There is deliberately no "trusted because it came from
//! local media" shortcut: a USB stick found in a car park is not a trusted
//! source.

#![warn(missing_docs)]

pub mod apply;
pub mod error;
pub mod health;
pub mod manifest;
pub mod source;

pub use apply::{InstallLayout, UpdateOutcome, Updater};
pub use error::UpdateError;
pub use health::{HealthProbe, HealthReport};
pub use manifest::{Artifact, Manifest, UpdateDecision};
pub use source::ArtifactSource;
