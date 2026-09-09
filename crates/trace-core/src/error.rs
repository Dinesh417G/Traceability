//! Domain error types. `trace-core` performs no I/O, so every error here is a
//! rule violation rather than a failure of the outside world.

use thiserror::Error;

/// Errors raised by the domain model and the route engine.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// A public identifier was not a canonical 26-character Crockford ULID.
    #[error("malformed identifier: {0}")]
    MalformedId(String),

    /// The route definition itself is invalid (cycle, dangling predecessor,
    /// duplicate sequence). Caught at load time, never mid-production.
    #[error("invalid route definition: {0}")]
    InvalidRoute(String),

    /// The unit is in a state that forbids the requested transition.
    #[error("illegal transition: unit is {from}, cannot {action}")]
    IllegalTransition {
        /// State the unit is currently in.
        from: &'static str,
        /// Action that was attempted.
        action: String,
    },

    /// A scrapped UID may never be reissued or resurrected.
    #[error("uid {0} is retired: scrapped units are never reissued")]
    RetiredUid(String),

    /// The station is not configured to perform any operation this unit needs.
    #[error("station {station} cannot serve unit at this point in its route")]
    StationCannotServe {
        /// Station code that was offered the unit.
        station: String,
    },

    /// A measurement value did not parse as the declared datatype.
    #[error("value {value:?} is not valid for datatype {datatype}")]
    ValueTypeMismatch {
        /// The offending raw value.
        value: String,
        /// Datatype declared on the data collection point.
        datatype: &'static str,
    },

    /// The append-only hash chain did not verify.
    #[error("hash chain broken at sequence {seq}: expected {expected}, found {found}")]
    HashChainBroken {
        /// Position in the chain where verification failed.
        seq: u64,
        /// Hash recomputed from the row contents.
        expected: String,
        /// Hash stored on the row.
        found: String,
    },
}

/// Convenience result alias for domain operations.
pub type Result<T> = std::result::Result<T, CoreError>;
