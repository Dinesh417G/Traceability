//! Storage errors.

use thiserror::Error;

/// Errors raised by the persistence layer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database rejected or failed the statement.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Migrations failed to apply.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// A domain rule was violated on the way in or out of the database.
    #[error(transparent)]
    Domain(#[from] trace_core::CoreError),

    /// The row exists but could not be mapped to a domain type. Almost always
    /// a schema/code drift, so the message names the column.
    #[error("cannot decode {entity}.{field}: {detail}")]
    Decode {
        /// Entity being decoded.
        entity: &'static str,
        /// Offending field.
        field: &'static str,
        /// What went wrong.
        detail: String,
    },

    /// Nothing matched the lookup.
    #[error("{entity} not found: {key}")]
    NotFound {
        /// Entity being looked up.
        entity: &'static str,
        /// Key that missed.
        key: String,
    },

    /// An attempt to mutate an append-only record.
    #[error("{0} is append-only; corrections are new rows that supersede old ones")]
    AppendOnly(&'static str),
}

impl StoreError {
    /// Whether this error is the database refusing to mutate an append-only
    /// row. Useful in tests and in operator-facing messages.
    #[must_use]
    pub fn is_append_only_violation(&self) -> bool {
        match self {
            Self::AppendOnly(_) => true,
            Self::Database(sqlx::Error::Database(e)) => {
                e.message().contains("append-only") || e.message().contains("is retired")
            }
            _ => false,
        }
    }
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, StoreError>;
