//! Where update bytes come from.
//!
//! An HTTP artifact server when the plant has internet, a directory on a USB
//! stick when it does not. Both implement the same trait and both go through
//! the same digest verification: there is no "trusted because it came from
//! local media" path, because a USB stick found in a car park is not a trusted
//! source.

use crate::error::UpdateError;
use crate::manifest::Artifact;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Supplies manifest and artifact bytes.
#[async_trait]
pub trait ArtifactSource: Send + Sync + std::fmt::Debug {
    /// Fetch the signed manifest envelope.
    async fn fetch_manifest(&self) -> Result<trace_sign::Envelope, UpdateError>;

    /// Fetch one artifact's bytes.
    async fn fetch_artifact(&self, artifact: &Artifact) -> Result<Vec<u8>, UpdateError>;

    /// Human-readable description, for logs and the support bundle.
    fn describe(&self) -> String;
}

/// Reads a signed bundle from a directory: the offline USB path.
#[derive(Debug, Clone)]
pub struct DirectorySource {
    root: PathBuf,
    manifest_name: String,
}

impl DirectorySource {
    /// Point at a directory containing `manifest.json` and the artifacts.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            manifest_name: "manifest.json".to_owned(),
        }
    }

    /// Use a different manifest filename.
    #[must_use]
    pub fn with_manifest_name(mut self, name: impl Into<String>) -> Self {
        self.manifest_name = name.into();
        self
    }

    /// Resolve a manifest-supplied relative path safely.
    ///
    /// A signed manifest is trusted, but defence in depth is cheap: a path that
    /// escapes the bundle root is refused outright, so a manifest signed with a
    /// leaked key still cannot read `/etc/shadow` or write outside the bundle.
    fn resolve(&self, rel: &str) -> Result<PathBuf, UpdateError> {
        let candidate = Path::new(rel);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(UpdateError::Malformed(format!(
                "artifact path {rel} escapes the bundle root"
            )));
        }
        Ok(self.root.join(candidate))
    }
}

#[async_trait]
impl ArtifactSource for DirectorySource {
    async fn fetch_manifest(&self) -> Result<trace_sign::Envelope, UpdateError> {
        let path = self.root.join(&self.manifest_name);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| UpdateError::io(&path, e))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| UpdateError::Malformed(format!("{}: {e}", path.display())))
    }

    async fn fetch_artifact(&self, artifact: &Artifact) -> Result<Vec<u8>, UpdateError> {
        let path = self.resolve(&artifact.path)?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: format!("{}: {e}", path.display()),
            })
    }

    fn describe(&self) -> String {
        format!("directory {}", self.root.display())
    }
}

/// Fetches a bundle over HTTPS.
#[cfg(feature = "http")]
#[derive(Debug, Clone)]
pub struct HttpSource {
    base_url: String,
    manifest_url: String,
    client: reqwest::Client,
    max_artifact_bytes: u64,
}

#[cfg(feature = "http")]
impl HttpSource {
    /// Default cap on a single artifact, so a hostile or broken server cannot
    /// fill the box's disk. Overridable for genuinely large releases.
    pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

    /// Build a source over an artifact server base URL.
    ///
    /// # Errors
    /// Returns [`UpdateError::Fetch`] if the HTTP client cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        manifest_url: impl Into<String>,
    ) -> Result<Self, UpdateError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(concat!(
                "electronix-trace-updater/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| UpdateError::Fetch {
                name: "client".into(),
                detail: e.to_string(),
            })?;
        Ok(Self {
            base_url: base_url.into(),
            manifest_url: manifest_url.into(),
            client,
            max_artifact_bytes: Self::DEFAULT_MAX_ARTIFACT_BYTES,
        })
    }

    /// Override the per-artifact size cap.
    #[must_use]
    pub fn with_max_artifact_bytes(mut self, max: u64) -> Self {
        self.max_artifact_bytes = max;
        self
    }
}

#[cfg(feature = "http")]
#[async_trait]
impl ArtifactSource for HttpSource {
    async fn fetch_manifest(&self) -> Result<trace_sign::Envelope, UpdateError> {
        let resp = self
            .client
            .get(&self.manifest_url)
            .send()
            .await
            .map_err(|e| UpdateError::Fetch {
                name: "manifest".into(),
                detail: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(UpdateError::Fetch {
                name: "manifest".into(),
                detail: format!("HTTP {}", resp.status()),
            });
        }
        let bytes = resp.bytes().await.map_err(|e| UpdateError::Fetch {
            name: "manifest".into(),
            detail: e.to_string(),
        })?;
        serde_json::from_slice(&bytes).map_err(|e| UpdateError::Malformed(e.to_string()))
    }

    async fn fetch_artifact(&self, artifact: &Artifact) -> Result<Vec<u8>, UpdateError> {
        // The manifest is signed, so its declared size is trustworthy; refusing
        // anything larger stops a hostile server streaming until the disk fills.
        if artifact.size > self.max_artifact_bytes {
            return Err(UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: format!(
                    "declared size {} exceeds the {} byte cap",
                    artifact.size, self.max_artifact_bytes
                ),
            });
        }

        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), artifact.path);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: format!("HTTP {} for {url}", resp.status()),
            });
        }

        if let Some(len) = resp.content_length()
            && len > self.max_artifact_bytes
        {
            return Err(UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: format!("server offered {len} bytes, over the cap"),
            });
        }

        Ok(resp
            .bytes()
            .await
            .map_err(|e| UpdateError::Fetch {
                name: artifact.name.clone(),
                detail: e.to_string(),
            })?
            .to_vec())
    }

    fn describe(&self) -> String {
        format!("http {}", self.base_url)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::manifest::sha256_hex;

    fn art(path: &str) -> Artifact {
        Artifact {
            name: "x".into(),
            sha256: sha256_hex(b""),
            size: 0,
            path: path.into(),
            delta_from: None,
        }
    }

    #[tokio::test]
    async fn a_directory_bundle_reads_its_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("trace-edge"), b"binary")
            .await
            .unwrap();

        let src = DirectorySource::new(dir.path());
        let bytes = src.fetch_artifact(&art("trace-edge")).await.unwrap();
        assert_eq!(bytes, b"binary");
        assert!(src.describe().contains("directory"));
    }

    #[tokio::test]
    async fn a_path_escaping_the_bundle_is_refused() {
        // Defence in depth: even a manifest signed with a leaked key must not
        // be able to read outside its own bundle.
        let dir = tempfile::tempdir().unwrap();
        let src = DirectorySource::new(dir.path());

        for evil in ["../../../etc/passwd", "/etc/passwd", "sub/../../escape"] {
            let err = src.fetch_artifact(&art(evil)).await.unwrap_err();
            assert!(
                format!("{err}").contains("escapes the bundle root"),
                "path {evil} must be refused, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn nested_paths_inside_the_bundle_are_fine() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("bin"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("bin/tool"), b"ok")
            .await
            .unwrap();
        let src = DirectorySource::new(dir.path());
        assert_eq!(src.fetch_artifact(&art("bin/tool")).await.unwrap(), b"ok");
    }

    #[tokio::test]
    async fn a_missing_manifest_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = DirectorySource::new(dir.path())
            .fetch_manifest()
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("manifest.json"));
    }
}
