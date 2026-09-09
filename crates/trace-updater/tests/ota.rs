//! End-to-end OTA behaviour against a real filesystem.
//!
//! These exercise the paths that actually cost a shift when they go wrong:
//! a tampered artifact, a replayed old manifest, and a release that installs
//! cleanly and then fails to start.
#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use chrono::Utc;
use semver::Version;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;
use trace_sign::{Envelope, Issuer, TrustedKey};
use trace_updater::health::{AlwaysHealthy, HealthProbe, HealthReport};
use trace_updater::manifest::{Artifact, Manifest, SCHEMA_VERSION, sha256_hex};
use trace_updater::source::{ArtifactSource, DirectorySource};
use trace_updater::{InstallLayout, UpdateError, UpdateOutcome, Updater};

/// Build a signed bundle in a directory: the shape a USB stick would carry.
struct Bundle {
    dir: TempDir,
}

impl Bundle {
    async fn new(
        issuer: &Issuer,
        version: &str,
        body: &[u8],
        mutate: impl FnOnce(&mut Manifest),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("trace-edge"), body)
            .await
            .unwrap();

        let mut manifest = Manifest {
            schema: SCHEMA_VERSION,
            version: version.to_owned(),
            released_at: Utc::now(),
            notes: format!("release {version}"),
            artifacts: vec![Artifact {
                name: "trace-edge".into(),
                sha256: sha256_hex(body),
                size: body.len() as u64,
                path: "trace-edge".into(),
                delta_from: None,
            }],
            min_installed_version: None,
            rollback: false,
            cohorts: vec![],
            health_deadline_secs: 5,
        };
        mutate(&mut manifest);

        let envelope = issuer.sign(&manifest).unwrap();
        tokio::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .await
        .unwrap();

        Self { dir }
    }

    fn source(&self) -> DirectorySource {
        DirectorySource::new(self.dir.path())
    }

    /// Replace an artifact's bytes after the manifest was signed.
    async fn swap_artifact(&self, body: &[u8]) {
        tokio::fs::write(self.dir.path().join("trace-edge"), body)
            .await
            .unwrap();
    }
}

/// A probe whose answer the test controls.
#[derive(Debug)]
struct Switchable(Arc<AtomicBool>);

#[async_trait]
impl HealthProbe for Switchable {
    async fn check(&self) -> HealthReport {
        if self.0.load(Ordering::SeqCst) {
            HealthReport::healthy()
        } else {
            HealthReport::unhealthy("service did not come up")
        }
    }
}

async fn installed(layout: &InstallLayout) -> Option<Version> {
    layout.installed_version().await.unwrap()
}

async fn current_bytes(layout: &InstallLayout) -> Vec<u8> {
    tokio::fs::read(layout.current_link().join("trace-edge"))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_signed_release_installs_and_goes_live() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let bundle = Bundle::new(&issuer, "1.0.0", b"v1 binary", |_| {}).await;
    let outcome = updater
        .update(&bundle.source(), &AlwaysHealthy)
        .await
        .unwrap();

    assert_eq!(
        outcome,
        UpdateOutcome::Installed {
            version: Version::new(1, 0, 0),
            replaced: None
        }
    );
    assert_eq!(installed(&layout).await, Some(Version::new(1, 0, 0)));
    assert_eq!(current_bytes(&layout).await, b"v1 binary");
}

#[tokio::test]
async fn a_second_update_retains_the_previous_release() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let v1 = Bundle::new(&issuer, "1.0.0", b"v1 binary", |_| {}).await;
    updater.update(&v1.source(), &AlwaysHealthy).await.unwrap();

    let v2 = Bundle::new(&issuer, "1.1.0", b"v2 binary", |_| {}).await;
    let outcome = updater.update(&v2.source(), &AlwaysHealthy).await.unwrap();

    assert_eq!(
        outcome,
        UpdateOutcome::Installed {
            version: Version::new(1, 1, 0),
            replaced: Some(Version::new(1, 0, 0))
        }
    );
    assert_eq!(current_bytes(&layout).await, b"v2 binary");
    // The old release is still on disk, which is what makes rollback possible.
    assert!(
        tokio::fs::try_exists(layout.release_dir(&Version::new(1, 0, 0)))
            .await
            .unwrap()
    );
    assert!(tokio::fs::try_exists(layout.previous_link()).await.unwrap());
}

#[tokio::test]
async fn an_unhealthy_release_is_rolled_back_automatically() {
    // The scenario this whole crate exists for: the update installs cleanly and
    // then the service will not start.
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let healthy = Arc::new(AtomicBool::new(true));
    let probe = Switchable(Arc::clone(&healthy));

    let v1 = Bundle::new(&issuer, "1.0.0", b"good binary", |_| {}).await;
    updater.update(&v1.source(), &probe).await.unwrap();

    // v2 installs but does not come up.
    healthy.store(false, Ordering::SeqCst);
    let v2 = Bundle::new(&issuer, "2.0.0", b"broken binary", |m| {
        m.health_deadline_secs = 1
    })
    .await;
    let outcome = updater.update(&v2.source(), &probe).await.unwrap();

    match outcome {
        UpdateOutcome::RolledBack {
            failed, restored, ..
        } => {
            assert_eq!(failed, Version::new(2, 0, 0));
            assert_eq!(restored, Version::new(1, 0, 0));
        }
        other => panic!("expected a rollback, got {other:?}"),
    }

    // The box is back on the working version, and running it.
    assert_eq!(installed(&layout).await, Some(Version::new(1, 0, 0)));
    assert_eq!(current_bytes(&layout).await, b"good binary");
}

#[tokio::test]
async fn a_first_release_that_fails_health_is_not_removed() {
    // Rolling back to nothing would leave the box with no software at all.
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let probe = Switchable(Arc::new(AtomicBool::new(false)));
    let v1 = Bundle::new(&issuer, "1.0.0", b"broken", |m| m.health_deadline_secs = 1).await;
    let err = updater.update(&v1.source(), &probe).await.unwrap_err();

    assert!(matches!(err, UpdateError::RollbackImpossible { .. }));
    assert_eq!(installed(&layout).await, Some(Version::new(1, 0, 0)));
}

#[tokio::test]
async fn an_artifact_swapped_after_signing_is_rejected() {
    // The manifest is signed, so an attacker cannot change the digest. What
    // they can do is serve different bytes for the same name.
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let bundle = Bundle::new(&issuer, "1.0.0", b"legitimate binary", |_| {}).await;
    bundle.swap_artifact(b"malicious binary!").await;

    let err = updater
        .update(&bundle.source(), &AlwaysHealthy)
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpdateError::DigestMismatch { .. }),
        "got {err}"
    );
    // Nothing was installed.
    assert_eq!(installed(&layout).await, None);
}

#[tokio::test]
async fn a_truncated_artifact_is_rejected_on_size_before_hashing() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let updater = Updater::new(
        InstallLayout::new(root.path()),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let bundle = Bundle::new(&issuer, "1.0.0", b"the full binary contents", |_| {}).await;
    bundle.swap_artifact(b"trunc").await;

    let err = updater
        .update(&bundle.source(), &AlwaysHealthy)
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("bytes"),
        "size mismatch should be named: {err}"
    );
}

#[tokio::test]
async fn a_manifest_from_an_untrusted_key_is_refused() {
    let ours = Issuer::generate();
    let attacker = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&ours.public_key_hex()).unwrap(),
        "default",
    );

    let bundle = Bundle::new(&attacker, "9.9.9", b"attacker payload", |_| {}).await;
    let err = updater
        .update(&bundle.source(), &AlwaysHealthy)
        .await
        .unwrap_err();

    assert!(matches!(err, UpdateError::Signature(_)));
    assert_eq!(installed(&layout).await, None, "nothing may be installed");
}

#[tokio::test]
async fn a_replayed_old_manifest_cannot_downgrade_the_box() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let v2 = Bundle::new(&issuer, "2.0.0", b"current binary", |_| {}).await;
    updater.update(&v2.source(), &AlwaysHealthy).await.unwrap();

    // Last year's genuinely signed release, served again.
    let v1 = Bundle::new(&issuer, "1.0.0", b"old vulnerable binary", |_| {}).await;
    let err = updater
        .update(&v1.source(), &AlwaysHealthy)
        .await
        .unwrap_err();

    assert!(matches!(err, UpdateError::DowngradeRefused { .. }));
    assert_eq!(installed(&layout).await, Some(Version::new(2, 0, 0)));
    assert_eq!(current_bytes(&layout).await, b"current binary");
}

#[tokio::test]
async fn a_deliberate_rollback_release_is_accepted() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    updater
        .update(
            &Bundle::new(&issuer, "2.0.0", b"bad release", |_| {})
                .await
                .source(),
            &AlwaysHealthy,
        )
        .await
        .unwrap();

    let fix = Bundle::new(&issuer, "1.0.0", b"known good", |m| m.rollback = true).await;
    updater.update(&fix.source(), &AlwaysHealthy).await.unwrap();

    assert_eq!(installed(&layout).await, Some(Version::new(1, 0, 0)));
    assert_eq!(current_bytes(&layout).await, b"known good");
}

#[tokio::test]
async fn re_running_the_same_release_is_a_no_op() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let v1 = Bundle::new(&issuer, "1.0.0", b"v1", |_| {}).await;
    updater.update(&v1.source(), &AlwaysHealthy).await.unwrap();
    let again = updater.update(&v1.source(), &AlwaysHealthy).await.unwrap();
    assert_eq!(
        again,
        UpdateOutcome::UpToDate {
            version: Version::new(1, 0, 0)
        }
    );
}

#[tokio::test]
async fn a_staged_rollout_holds_boxes_outside_the_cohort() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());

    let bundle = Bundle::new(&issuer, "1.0.0", b"canary build", |m| {
        m.cohorts = vec!["canary".into()]
    })
    .await;
    let key = TrustedKey::from_hex(&issuer.public_key_hex()).unwrap();

    let held = Updater::new(layout.clone(), key.clone(), "default");
    assert!(matches!(
        held.update(&bundle.source(), &AlwaysHealthy).await.unwrap(),
        UpdateOutcome::NotInCohort { .. }
    ));
    assert_eq!(installed(&layout).await, None);

    let canary = Updater::new(layout.clone(), key, "canary");
    assert!(matches!(
        canary
            .update(&bundle.source(), &AlwaysHealthy)
            .await
            .unwrap(),
        UpdateOutcome::Installed { .. }
    ));
}

#[tokio::test]
async fn an_interrupted_previous_attempt_does_not_contaminate_the_next() {
    // Simulates power loss mid-download: a stale staging directory is left
    // behind and must not be mistaken for a complete release.
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    layout.ensure().await.unwrap();

    let stale = layout.staging_dir().join("1.0.0");
    tokio::fs::create_dir_all(&stale).await.unwrap();
    tokio::fs::write(stale.join("trace-edge"), b"half-written garbage")
        .await
        .unwrap();
    tokio::fs::write(stale.join("leftover-file"), b"junk")
        .await
        .unwrap();

    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );
    let bundle = Bundle::new(&issuer, "1.0.0", b"clean binary", |_| {}).await;
    updater
        .update(&bundle.source(), &AlwaysHealthy)
        .await
        .unwrap();

    assert_eq!(current_bytes(&layout).await, b"clean binary");
    assert!(
        !tokio::fs::try_exists(
            layout
                .release_dir(&Version::new(1, 0, 0))
                .join("leftover-file")
        )
        .await
        .unwrap(),
        "stale staging content must not survive into the release"
    );
}

#[tokio::test]
async fn the_offline_usb_path_uses_the_same_verification_as_http() {
    // A bundle on removable media gets no shortcut: an attacker who can write
    // to a USB stick is not more trusted than one who can serve HTTP.
    let issuer = Issuer::generate();
    let attacker = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let updater = Updater::new(
        InstallLayout::new(root.path()),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );

    let usb = Bundle::new(&attacker, "5.0.0", b"payload from a car park", |_| {}).await;
    let src = usb.source();
    assert!(
        src.describe().contains("directory"),
        "this is the local-media path"
    );
    assert!(matches!(
        updater.update(&src, &AlwaysHealthy).await.unwrap_err(),
        UpdateError::Signature(_)
    ));
}

#[tokio::test]
async fn a_bundle_missing_its_manifest_fails_cleanly() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let updater = Updater::new(
        InstallLayout::new(root.path()),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );
    let err = updater
        .update(&DirectorySource::new(empty.path()), &AlwaysHealthy)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Io { .. }));
}

#[tokio::test]
async fn current_is_a_symlink_so_the_swap_is_atomic() {
    let issuer = Issuer::generate();
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    let updater = Updater::new(
        layout.clone(),
        TrustedKey::from_hex(&issuer.public_key_hex()).unwrap(),
        "default",
    );
    updater
        .update(
            &Bundle::new(&issuer, "1.0.0", b"v1", |_| {}).await.source(),
            &AlwaysHealthy,
        )
        .await
        .unwrap();

    let meta = tokio::fs::symlink_metadata(layout.current_link())
        .await
        .unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "current must be a symlink, not a copied directory"
    );
}

/// Envelope round-trip, so a bundle written by the control plane parses here.
#[tokio::test]
async fn manifest_envelope_round_trips_through_json_on_disk() {
    let issuer = Issuer::generate();
    let bundle = Bundle::new(&issuer, "1.0.0", b"v1", |_| {}).await;
    let raw = tokio::fs::read(bundle.dir.path().join("manifest.json"))
        .await
        .unwrap();
    let env: Envelope = serde_json::from_slice(&raw).unwrap();
    let m = Manifest::verify(&env, &issuer.verifier()).unwrap();
    assert_eq!(m.version, "1.0.0");
}
