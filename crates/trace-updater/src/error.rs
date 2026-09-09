//! Update errors.

use thiserror::Error;

/// Errors raised while checking for, downloading or applying an update.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// The manifest signature did not verify.
    #[error("update manifest is not trustworthy: {0}")]
    Signature(#[from] trace_sign::SignError),

    /// An artifact's content did not match the digest the manifest named.
    ///
    /// Never retried against the same bytes: whether it is corruption or
    /// substitution, re-downloading the same URL is not the remedy.
    #[error("artifact {name}: expected sha256 {expected}, got {actual}")]
    DigestMismatch {
        /// Artifact name.
        name: String,
        /// Digest the signed manifest named.
        expected: String,
        /// Digest actually computed.
        actual: String,
    },

    /// The manifest offers a version older than the one installed.
    #[error(
        "refusing downgrade from {installed} to {offered}; manifest is not marked as a rollback"
    )]
    DowngradeRefused {
        /// Version currently installed.
        installed: String,
        /// Version the manifest offers.
        offered: String,
    },

    /// The installed version does not satisfy the manifest's minimum.
    #[error("update to {target} requires at least {min_installed}, but {installed} is installed")]
    UpgradePathRequired {
        /// Version currently installed.
        installed: String,
        /// Minimum the manifest requires.
        min_installed: String,
        /// Version being offered.
        target: String,
    },

    /// A version string was not valid semver.
    #[error("malformed version {value}: {detail}")]
    MalformedVersion {
        /// Offending value.
        value: String,
        /// Parser message.
        detail: String,
    },

    /// The new version failed its health probe and was rolled back.
    #[error("update to {version} failed its health probe ({reason}); rolled back to {restored}")]
    RolledBack {
        /// Version that failed.
        version: String,
        /// Why it failed.
        reason: String,
        /// Version restored.
        restored: String,
    },

    /// A rollback was needed but there was nothing to roll back to.
    ///
    /// The box is left on the new version because it is the only version:
    /// removing it would leave no software at all.
    #[error(
        "update to {version} failed its health probe ({reason}) and no previous release exists"
    )]
    RollbackImpossible {
        /// Version that failed.
        version: String,
        /// Why it failed.
        reason: String,
    },

    /// Filesystem failure.
    #[error("update io error at {path}: {source}")]
    Io {
        /// Path involved.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// The artifact could not be fetched.
    #[error("cannot fetch artifact {name}: {detail}")]
    Fetch {
        /// Artifact name.
        name: String,
        /// What went wrong.
        detail: String,
    },

    /// The manifest was malformed.
    #[error("malformed manifest: {0}")]
    Malformed(String),
}

impl UpdateError {
    /// Attach a path to an io error.
    pub(crate) fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
