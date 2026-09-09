//! Public and internal identifiers.
//!
//! Two rules, from the charter:
//!
//! * internal keys are `BIGINT` surrogates and are **never** exposed;
//! * anything printed, marked, or put in a URL is a [`Ulid`]-backed public id.
//!
//! ULIDs are stored and transported in their canonical 26-character Crockford
//! base32 form, which sorts lexicographically by creation time.

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A surrogate database key. Internal only — never rendered to an operator,
/// a label, or a URL.
pub type RowId = i64;

/// A public, time-sortable identifier safe to print on a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicId(ulid::Ulid);

impl PublicId {
    /// Generate a new identifier from the current clock.
    ///
    /// Generation is local by design: a station must be able to birth a unit
    /// with no network and no server.
    #[must_use]
    pub fn generate() -> Self {
        Self(ulid::Ulid::generate())
    }

    /// Parse a canonical 26-character Crockford base32 ULID.
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedId`] if the text is not a valid ULID.
    pub fn parse(s: &str) -> Result<Self> {
        ulid::Ulid::from_string(s)
            .map(Self)
            .map_err(|_| CoreError::MalformedId(s.to_owned()))
    }

    /// The creation timestamp embedded in the identifier, in milliseconds
    /// since the Unix epoch.
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }

    /// Borrow the underlying ULID.
    #[must_use]
    pub fn as_ulid(&self) -> ulid::Ulid {
        self.0
    }
}

impl fmt::Display for PublicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for PublicId {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Declares a distinct newtype over [`PublicId`] so that a unit id can never be
/// passed where an event id is expected.
macro_rules! public_id_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub PublicId);

        impl $name {
            /// Generate a fresh identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(PublicId::generate())
            }

            /// Parse from canonical ULID text.
            ///
            /// # Errors
            /// Returns [`CoreError::MalformedId`] if the text is not a valid ULID.
            pub fn parse(s: &str) -> Result<Self> {
                PublicId::parse(s).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = CoreError;
            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }
    };
}

public_id_newtype!(
    /// The identity of a traced physical unit. This is the value encoded into
    /// the mark or label and printed as human-readable text beside it.
    UnitUid
);
public_id_newtype!(
    /// Identity of a single append-only unit event. Doubles as the
    /// idempotency key when a station replays its offline spool.
    EventId
);
public_id_newtype!(
    /// Identity of a single captured measurement.
    MeasurementId
);
public_id_newtype!(
    /// Identity of one label print or mark attempt.
    MarkId
);

/// Human-facing code used as a natural key in the MES-aligned hierarchy
/// (`tenant`, `plant`, `line`, `station`). Stable across a merge with MES.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Code(String);

impl Code {
    /// Build a code, normalising to uppercase and trimming surrounding space.
    ///
    /// # Errors
    /// Returns [`CoreError::MalformedId`] if empty or over 64 characters.
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let raw = s.into();
        let norm = raw.trim().to_uppercase();
        if norm.is_empty() || norm.chars().count() > 64 {
            return Err(CoreError::MalformedId(raw));
        }
        Ok(Self(norm))
    }

    /// Borrow the normalised code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn public_ids_are_26_char_crockford() {
        let id = PublicId::generate();
        let s = id.to_string();
        assert_eq!(s.len(), 26, "ULID text form must be 26 chars: {s}");
        assert_eq!(PublicId::parse(&s).unwrap(), id);
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(PublicId::parse("not-a-ulid").is_err());
        assert!(PublicId::parse("").is_err());
    }

    #[test]
    fn ulid_text_sorts_by_time() {
        // Lexicographic order == time order is why we store ULIDs as text
        // (DECISIONS.md D-007). Guard the property we rely on.
        let mut earlier = PublicId::generate().to_string();
        let mut later = PublicId::generate().to_string();
        for _ in 0..200 {
            if earlier < later {
                break;
            }
            earlier = PublicId::generate().to_string();
            later = PublicId::generate().to_string();
        }
        assert!(earlier <= later, "{earlier} should sort before {later}");
    }

    #[test]
    fn newtypes_do_not_interchange() {
        let u = UnitUid::generate();
        let round = UnitUid::parse(&u.to_string()).unwrap();
        assert_eq!(u, round);
    }

    #[test]
    fn codes_normalise_and_validate() {
        assert_eq!(Code::new("  line-a ").unwrap().as_str(), "LINE-A");
        assert!(Code::new("").is_err());
        assert!(Code::new("x".repeat(65)).is_err());
    }

    #[test]
    fn serde_roundtrip_is_plain_string() {
        let u = UnitUid::generate();
        let json = serde_json::to_string(&u).unwrap();
        assert!(
            json.starts_with('"'),
            "must serialise as a bare string: {json}"
        );
        assert_eq!(serde_json::from_str::<UnitUid>(&json).unwrap(), u);
    }
}
