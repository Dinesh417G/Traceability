//! The signed update manifest, and the rules for accepting one.

use crate::error::UpdateError;
use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use trace_sign::{Envelope, TrustedKey};

/// One file in a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Name within the release, e.g. `trace-edge`.
    pub name: String,
    /// Lowercase hex SHA-256 of the exact bytes. This is what makes the
    /// artifact content-addressed, and what a substituted file fails.
    pub sha256: String,
    /// Size in bytes, checked before hashing so a wildly wrong file is
    /// rejected without reading all of it.
    pub size: u64,
    /// Path relative to the artifact source root.
    pub path: String,
    /// Whether this artifact is a delta against the installed version.
    #[serde(default)]
    pub delta_from: Option<String>,
}

/// A signed description of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest schema version.
    pub schema: u32,
    /// Release version, semver.
    pub version: String,
    /// When the release was published.
    pub released_at: DateTime<Utc>,
    /// Operator-facing release notes.
    #[serde(default)]
    pub notes: String,
    /// Files making up the release.
    pub artifacts: Vec<Artifact>,
    /// Minimum installed version required to take this update directly.
    ///
    /// Lets a release require an intermediate step, for example when a
    /// migration cannot be skipped.
    #[serde(default)]
    pub min_installed_version: Option<String>,
    /// Marks this manifest as a deliberate rollback, which is the only way a
    /// downgrade is accepted.
    #[serde(default)]
    pub rollback: bool,
    /// Staged rollout cohorts this release is offered to. Empty means all.
    #[serde(default)]
    pub cohorts: Vec<String>,
    /// Seconds the new version has to pass its health probe before it is rolled
    /// back automatically.
    #[serde(default = "default_health_deadline")]
    pub health_deadline_secs: u64,
}

fn default_health_deadline() -> u64 {
    120
}

/// Schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// What to do with a manifest that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Install it.
    Install {
        /// Version to install.
        version: Version,
    },
    /// Already running this version or newer; nothing to do.
    UpToDate,
    /// This box is not in the release's rollout cohort yet.
    NotInCohort {
        /// Cohorts the release is offered to.
        cohorts: Vec<String>,
    },
}

impl Manifest {
    /// Verify a signed manifest envelope.
    ///
    /// **This runs before anything is downloaded.** An attacker who can serve
    /// bytes should not be able to make the box spend bandwidth on their
    /// payload, let alone unpack it.
    ///
    /// # Errors
    /// Returns [`UpdateError::Signature`] if it does not verify, or
    /// [`UpdateError::Malformed`] for an unsupported schema or empty release.
    pub fn verify(envelope: &Envelope, key: &TrustedKey) -> Result<Self, UpdateError> {
        let manifest: Self = key.verify(envelope)?;

        if manifest.schema > SCHEMA_VERSION {
            return Err(UpdateError::Malformed(format!(
                "manifest schema {} is newer than this build supports ({SCHEMA_VERSION})",
                manifest.schema
            )));
        }
        if manifest.artifacts.is_empty() {
            return Err(UpdateError::Malformed("manifest lists no artifacts".into()));
        }
        // A digest that is not 64 hex characters cannot match anything, and
        // catching it here gives a far clearer error than a mismatch later.
        for a in &manifest.artifacts {
            if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(UpdateError::Malformed(format!(
                    "artifact {} has a malformed sha256 digest",
                    a.name
                )));
            }
        }
        manifest.parsed_version()?;
        Ok(manifest)
    }

    /// The release version, parsed.
    ///
    /// # Errors
    /// Returns [`UpdateError::MalformedVersion`] if it is not valid semver.
    pub fn parsed_version(&self) -> Result<Version, UpdateError> {
        Version::parse(&self.version).map_err(|e| UpdateError::MalformedVersion {
            value: self.version.clone(),
            detail: e.to_string(),
        })
    }

    /// Decide whether this box should install this release.
    ///
    /// # Errors
    /// Returns [`UpdateError::DowngradeRefused`] for an unmarked downgrade, or
    /// [`UpdateError::UpgradePathRequired`] when an intermediate release is
    /// required first.
    pub fn decide(&self, installed: &Version, cohort: &str) -> Result<UpdateDecision, UpdateError> {
        if !self.cohorts.is_empty() && !self.cohorts.iter().any(|c| c == cohort) {
            return Ok(UpdateDecision::NotInCohort {
                cohorts: self.cohorts.clone(),
            });
        }

        let offered = self.parsed_version()?;

        if let Some(min) = &self.min_installed_version {
            let min = Version::parse(min).map_err(|e| UpdateError::MalformedVersion {
                value: min.clone(),
                detail: e.to_string(),
            })?;
            if installed < &min {
                return Err(UpdateError::UpgradePathRequired {
                    installed: installed.to_string(),
                    min_installed: min.to_string(),
                    target: offered.to_string(),
                });
            }
        }

        if offered == *installed {
            return Ok(UpdateDecision::UpToDate);
        }

        if offered < *installed {
            // A replayed old manifest must not be able to walk a box backwards
            // into a version with a known vulnerability.
            if !self.rollback {
                return Err(UpdateError::DowngradeRefused {
                    installed: installed.to_string(),
                    offered: offered.to_string(),
                });
            }
            return Ok(UpdateDecision::Install { version: offered });
        }

        Ok(UpdateDecision::Install { version: offered })
    }
}

/// Compute the SHA-256 of a byte slice, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use trace_sign::Issuer;

    fn artifact(body: &[u8]) -> Artifact {
        Artifact {
            name: "trace-edge".into(),
            sha256: sha256_hex(body),
            size: body.len() as u64,
            path: "trace-edge".into(),
            delta_from: None,
        }
    }

    fn manifest(version: &str) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            version: version.into(),
            released_at: Utc::now(),
            notes: "test release".into(),
            artifacts: vec![artifact(b"binary bytes")],
            min_installed_version: None,
            rollback: false,
            cohorts: vec![],
            health_deadline_secs: 120,
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let issuer = Issuer::generate();
        let env = issuer.sign(&manifest("1.2.0")).unwrap();
        let m = Manifest::verify(&env, &issuer.verifier()).unwrap();
        assert_eq!(m.version, "1.2.0");
    }

    #[test]
    fn a_manifest_signed_by_anyone_else_is_rejected_before_any_download() {
        let ours = Issuer::generate();
        let attacker = Issuer::generate();
        let env = attacker.sign(&manifest("9.9.9")).unwrap();
        assert!(matches!(
            Manifest::verify(&env, &ours.verifier()),
            Err(UpdateError::Signature(_))
        ));
    }

    #[test]
    fn tampering_with_an_artifact_digest_breaks_the_signature() {
        let issuer = Issuer::generate();
        let mut m = manifest("1.2.0");
        let env = issuer.sign(&m).unwrap();

        // Swap in an attacker's payload digest.
        m.artifacts[0].sha256 = sha256_hex(b"malicious payload");
        let forged = trace_sign::Envelope {
            payload_b64: {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .encode(trace_sign::canonical_json(&m).unwrap())
            },
            ..env
        };
        assert!(Manifest::verify(&forged, &issuer.verifier()).is_err());
    }

    #[test]
    fn a_malformed_digest_is_caught_early_with_a_clear_message() {
        let issuer = Issuer::generate();
        let mut m = manifest("1.2.0");
        m.artifacts[0].sha256 = "not-a-digest".into();
        let env = issuer.sign(&m).unwrap();
        let err = Manifest::verify(&env, &issuer.verifier()).unwrap_err();
        assert!(format!("{err}").contains("malformed sha256"));
    }

    #[test]
    fn an_empty_release_is_rejected() {
        let issuer = Issuer::generate();
        let mut m = manifest("1.2.0");
        m.artifacts.clear();
        let env = issuer.sign(&m).unwrap();
        assert!(Manifest::verify(&env, &issuer.verifier()).is_err());
    }

    #[test]
    fn a_newer_release_installs() {
        let m = manifest("1.3.0");
        assert_eq!(
            m.decide(&v("1.2.0"), "default").unwrap(),
            UpdateDecision::Install {
                version: v("1.3.0")
            }
        );
    }

    #[test]
    fn the_same_version_is_a_no_op() {
        assert_eq!(
            manifest("1.2.0").decide(&v("1.2.0"), "default").unwrap(),
            UpdateDecision::UpToDate
        );
    }

    #[test]
    fn a_replayed_old_manifest_cannot_walk_the_box_backwards() {
        // The attack: capture last year's signed manifest, serve it again, and
        // downgrade the box into a version with a known hole. The signature is
        // perfectly valid, so only the version rule stops this.
        let err = manifest("1.0.0")
            .decide(&v("1.2.0"), "default")
            .unwrap_err();
        assert!(matches!(err, UpdateError::DowngradeRefused { .. }));
    }

    #[test]
    fn a_deliberate_rollback_is_allowed() {
        let mut m = manifest("1.0.0");
        m.rollback = true;
        assert_eq!(
            m.decide(&v("1.2.0"), "default").unwrap(),
            UpdateDecision::Install {
                version: v("1.0.0")
            }
        );
    }

    #[test]
    fn a_required_intermediate_release_is_enforced() {
        // Some upgrades cannot be skipped, e.g. an unskippable migration.
        let mut m = manifest("3.0.0");
        m.min_installed_version = Some("2.0.0".into());
        let err = m.decide(&v("1.5.0"), "default").unwrap_err();
        assert!(matches!(err, UpdateError::UpgradePathRequired { .. }));
        assert!(m.decide(&v("2.1.0"), "default").is_ok());
    }

    #[test]
    fn staged_rollout_holds_boxes_outside_the_cohort() {
        let mut m = manifest("1.3.0");
        m.cohorts = vec!["canary".into()];
        assert!(matches!(
            m.decide(&v("1.2.0"), "default").unwrap(),
            UpdateDecision::NotInCohort { .. }
        ));
        assert!(matches!(
            m.decide(&v("1.2.0"), "canary").unwrap(),
            UpdateDecision::Install { .. }
        ));
    }

    #[test]
    fn an_empty_cohort_list_means_everyone() {
        assert!(matches!(
            manifest("1.3.0").decide(&v("1.2.0"), "anything").unwrap(),
            UpdateDecision::Install { .. }
        ));
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let issuer = Issuer::generate();
        let mut m = manifest("1.3.0");
        m.schema = SCHEMA_VERSION + 1;
        let env = issuer.sign(&m).unwrap();
        assert!(Manifest::verify(&env, &issuer.verifier()).is_err());
    }
}
