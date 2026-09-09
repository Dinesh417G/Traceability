//! Downloading, verifying, applying and rolling back a release.
//!
//! ## Why the symlink dance
//!
//! The running install is never modified in place. A release unpacks into
//! `releases/<version>/`, and going live is a single `rename` over the
//! `current` symlink — which is atomic on Linux. Power loss at any point
//! before that rename leaves the old version completely untouched; power loss
//! after it leaves the new version fully in place. There is no window in which
//! the box has half of each.

use crate::error::UpdateError;
use crate::health::HealthProbe;
use crate::manifest::{Artifact, Manifest, UpdateDecision, sha256_hex};
use crate::source::ArtifactSource;
use semver::Version;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use trace_sign::TrustedKey;

/// Where releases live on disk.
///
/// ```text
/// <root>/
///   releases/1.2.0/       an installed release
///   releases/1.3.0/       the new one
///   current -> releases/1.3.0
///   previous -> releases/1.2.0     retained until 1.3.0 is proven healthy
///   staging/                       partially written, never linked
/// ```
#[derive(Debug, Clone)]
pub struct InstallLayout {
    root: PathBuf,
}

impl InstallLayout {
    /// Build a layout under an install root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The install root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding all releases.
    #[must_use]
    pub fn releases_dir(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// Directory for a specific release.
    #[must_use]
    pub fn release_dir(&self, version: &Version) -> PathBuf {
        self.releases_dir().join(version.to_string())
    }

    /// The `current` symlink.
    #[must_use]
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// The `previous` symlink.
    #[must_use]
    pub fn previous_link(&self) -> PathBuf {
        self.root.join("previous")
    }

    /// Scratch space for partially written releases.
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// Create the directory skeleton.
    ///
    /// # Errors
    /// Returns [`UpdateError::Io`] if a directory cannot be created.
    pub async fn ensure(&self) -> Result<(), UpdateError> {
        for d in [self.releases_dir(), self.staging_dir()] {
            tokio::fs::create_dir_all(&d)
                .await
                .map_err(|e| UpdateError::io(&d, e))?;
        }
        Ok(())
    }

    /// The version `current` points at, if any.
    ///
    /// # Errors
    /// Returns [`UpdateError::Io`] if the link exists but cannot be read.
    pub async fn installed_version(&self) -> Result<Option<Version>, UpdateError> {
        let link = self.current_link();
        match tokio::fs::read_link(&link).await {
            Ok(target) => Ok(target
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| Version::parse(n).ok())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(UpdateError::io(&link, e)),
        }
    }
}

/// What an update attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// A new version is live and healthy.
    Installed {
        /// Version now running.
        version: Version,
        /// Version it replaced, if any.
        replaced: Option<Version>,
    },
    /// Already on this version or newer.
    UpToDate {
        /// Version running.
        version: Version,
    },
    /// Held back by a staged rollout.
    NotInCohort {
        /// Cohorts the release targets.
        cohorts: Vec<String>,
    },
    /// The new version failed its health probe and the previous one was
    /// restored.
    RolledBack {
        /// Version that failed.
        failed: Version,
        /// Version restored.
        restored: Version,
        /// Why it failed.
        reason: String,
    },
}

/// Drives the update process.
#[derive(Debug)]
pub struct Updater {
    layout: InstallLayout,
    key: TrustedKey,
    cohort: String,
}

impl Updater {
    /// Build an updater over an install root and the trusted public key.
    ///
    /// The box holds a **public** key only. A signing key here would let
    /// anyone who opens the cabinet install their own software.
    #[must_use]
    pub fn new(layout: InstallLayout, key: TrustedKey, cohort: impl Into<String>) -> Self {
        Self {
            layout,
            key,
            cohort: cohort.into(),
        }
    }

    /// Check a source for an update, apply it if appropriate, and roll back if
    /// the result is unhealthy.
    ///
    /// The order is the security property: signature first, then the version
    /// decision, then bytes, then digests, then an atomic swap, then health.
    ///
    /// # Errors
    /// Returns the first [`UpdateError`] encountered. A digest mismatch or a
    /// refused downgrade leaves the installed version untouched.
    pub async fn update(
        &self,
        source: &dyn ArtifactSource,
        probe: &dyn HealthProbe,
    ) -> Result<UpdateOutcome, UpdateError> {
        self.layout.ensure().await?;

        // 1. Verify the manifest BEFORE fetching a single artifact byte.
        let envelope = source.fetch_manifest().await?;
        let manifest = Manifest::verify(&envelope, &self.key)?;

        let installed = self
            .layout
            .installed_version()
            .await?
            .unwrap_or_else(|| Version::new(0, 0, 0));

        // 2. Decide, which is where downgrade protection and cohorts apply.
        let target = match manifest.decide(&installed, &self.cohort)? {
            UpdateDecision::UpToDate => return Ok(UpdateOutcome::UpToDate { version: installed }),
            UpdateDecision::NotInCohort { cohorts } => {
                return Ok(UpdateOutcome::NotInCohort { cohorts });
            }
            UpdateDecision::Install { version } => version,
        };

        tracing::info!(
            from = %installed, to = %target, source = %source.describe(),
            "applying update"
        );

        // 3 & 4. Fetch into staging and verify every digest.
        let staged = self.stage(source, &manifest, &target).await?;

        // 5. Atomic swap.
        let previous = self.activate(&staged, &target, &installed).await?;

        // 6. Health gate. The previous release is still on disk.
        let report = probe
            .wait_until_healthy(Duration::from_secs(manifest.health_deadline_secs))
            .await;

        if report.healthy {
            tracing::info!(version = %target, "update healthy");
            return Ok(UpdateOutcome::Installed {
                version: target,
                replaced: previous,
            });
        }

        tracing::error!(version = %target, reason = %report.detail, "update unhealthy, rolling back");

        let Some(restore) = previous else {
            // Nothing to go back to. Leaving the new version in place is the
            // least bad option: removing it would leave no software at all.
            return Err(UpdateError::RollbackImpossible {
                version: target.to_string(),
                reason: report.detail,
            });
        };

        self.point_current_at(&restore).await?;
        Ok(UpdateOutcome::RolledBack {
            failed: target,
            restored: restore,
            reason: report.detail,
        })
    }

    /// Fetch every artifact into a staging directory, verifying digests.
    async fn stage(
        &self,
        source: &dyn ArtifactSource,
        manifest: &Manifest,
        target: &Version,
    ) -> Result<PathBuf, UpdateError> {
        let staging = self.layout.staging_dir().join(target.to_string());

        // A leftover staging directory from an interrupted attempt must not be
        // mistaken for a complete release.
        if tokio::fs::try_exists(&staging).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&staging)
                .await
                .map_err(|e| UpdateError::io(&staging, e))?;
        }
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|e| UpdateError::io(&staging, e))?;

        for artifact in &manifest.artifacts {
            let bytes = source.fetch_artifact(artifact).await?;
            verify_digest(artifact, &bytes)?;
            self.write_artifact(&staging, artifact, &bytes).await?;
        }

        Ok(staging)
    }

    /// Write one verified artifact, fsyncing so it survives power loss.
    async fn write_artifact(
        &self,
        staging: &Path,
        artifact: &Artifact,
        bytes: &[u8],
    ) -> Result<(), UpdateError> {
        let dest = staging.join(&artifact.name);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| UpdateError::io(parent, e))?;
        }

        let mut f = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| UpdateError::io(&dest, e))?;
        f.write_all(bytes)
            .await
            .map_err(|e| UpdateError::io(&dest, e))?;
        // Without this the bytes may still be in the page cache when the
        // symlink swap happens, and a power cut would leave a live symlink
        // pointing at a truncated binary.
        f.sync_all().await.map_err(|e| UpdateError::io(&dest, e))?;
        Ok(())
    }

    /// Move a staged release into place and point `current` at it.
    async fn activate(
        &self,
        staged: &Path,
        target: &Version,
        installed: &Version,
    ) -> Result<Option<Version>, UpdateError> {
        let release_dir = self.layout.release_dir(target);

        if tokio::fs::try_exists(&release_dir).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&release_dir)
                .await
                .map_err(|e| UpdateError::io(&release_dir, e))?;
        }
        tokio::fs::rename(staged, &release_dir)
            .await
            .map_err(|e| UpdateError::io(&release_dir, e))?;

        let had_previous = self.layout.installed_version().await?.is_some();

        // Retain the outgoing release so rollback has somewhere to go.
        if had_previous {
            let prev_dir = self.layout.release_dir(installed);
            replace_symlink(&prev_dir, &self.layout.previous_link()).await?;
        }

        self.point_current_at(target).await?;
        Ok(had_previous.then(|| installed.clone()))
    }

    /// Point `current` at a release directory, atomically.
    async fn point_current_at(&self, version: &Version) -> Result<(), UpdateError> {
        replace_symlink(
            &self.layout.release_dir(version),
            &self.layout.current_link(),
        )
        .await
    }
}

/// Verify an artifact's bytes against the digest the signed manifest named.
fn verify_digest(artifact: &Artifact, bytes: &[u8]) -> Result<(), UpdateError> {
    // Cheap check first: a wildly wrong file is rejected without hashing it.
    if bytes.len() as u64 != artifact.size {
        return Err(UpdateError::DigestMismatch {
            name: artifact.name.clone(),
            expected: format!("{} bytes", artifact.size),
            actual: format!("{} bytes", bytes.len()),
        });
    }
    let actual = sha256_hex(bytes);
    if actual != artifact.sha256 {
        return Err(UpdateError::DigestMismatch {
            name: artifact.name.clone(),
            expected: artifact.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// Atomically replace a symlink.
///
/// `symlink` fails if the path exists, so the new link is created under a
/// temporary name and `rename`d over the old one. On Linux that rename is
/// atomic: a reader either sees the old target or the new one, never neither.
async fn replace_symlink(target: &Path, link: &Path) -> Result<(), UpdateError> {
    let tmp = link.with_extension(format!("tmp-{}", std::process::id()));
    if tokio::fs::try_exists(&tmp).await.unwrap_or(false) {
        tokio::fs::remove_file(&tmp)
            .await
            .map_err(|e| UpdateError::io(&tmp, e))?;
    }
    tokio::fs::symlink(target, &tmp)
        .await
        .map_err(|e| UpdateError::io(&tmp, e))?;
    tokio::fs::rename(&tmp, link)
        .await
        .map_err(|e| UpdateError::io(link, e))?;
    Ok(())
}
