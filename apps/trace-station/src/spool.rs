//! The offline spool.
//!
//! A station must keep working with the network, the edge and the internet all
//! down. That means every event it produces is written to durable local storage
//! **before** anything is attempted over the network, and replayed when the
//! edge comes back.
//!
//! ## Why a file rather than a database
//!
//! The spool is an append-only queue with one writer. That is the one shape
//! where a plain file genuinely beats a database: fewer moving parts to corrupt
//! on a hard power cut, no schema to migrate on a terminal nobody can reach,
//! and a support engineer can read it with `cat`.
//!
//! Each record is one line of JSON, fsynced on write. A torn final line from a
//! power cut is skipped on load rather than poisoning the whole queue — losing
//! the one event that was mid-write is unavoidable; losing the other nine
//! hundred would not be.
//!
//! ## Idempotency
//!
//! Every record carries a ULID assigned at creation. Replay after a reconnect
//! can therefore repeat safely: the edge deduplicates on that id, so a station
//! that dies between "sent" and "acknowledged" does not create a duplicate
//! event.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

/// Spool errors.
#[derive(Debug, Error)]
pub enum SpoolError {
    /// Filesystem failure.
    #[error("spool io error at {path}: {source}")]
    Io {
        /// Path involved.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A record could not be encoded.
    #[error("cannot encode spool record: {0}")]
    Encode(#[from] serde_json::Error),
}

fn io(path: impl AsRef<Path>, source: std::io::Error) -> SpoolError {
    SpoolError::Io {
        path: path.as_ref().display().to_string(),
        source,
    }
}

/// One queued event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpoolRecord {
    /// Idempotency key, assigned when the event happened.
    pub id: String,
    /// Edge endpoint this belongs to, e.g. `unit.event`.
    pub topic: String,
    /// Payload to deliver.
    pub payload: serde_json::Value,
    /// Device time the event occurred.
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    /// Delivery attempts so far.
    #[serde(default)]
    pub attempts: u32,
}

impl SpoolRecord {
    /// Build a record, assigning an idempotency key.
    #[must_use]
    pub fn new(topic: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            id: trace_core::PublicId::generate().to_string(),
            topic: topic.into(),
            payload,
            recorded_at: chrono::Utc::now(),
            attempts: 0,
        }
    }
}

/// A durable append-only queue on local disk.
#[derive(Debug, Clone)]
pub struct Spool {
    path: PathBuf,
}

impl Spool {
    /// Open (or create) a spool file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The spool file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a record and fsync it.
    ///
    /// Returns only once the bytes are on disk. That is the whole point: an
    /// operation must not be reported as captured until it would survive the
    /// power going out a millisecond later.
    ///
    /// # Errors
    /// Returns [`SpoolError::Io`] if the write or sync fails.
    pub async fn append(&self, record: &SpoolRecord) -> Result<(), SpoolError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io(parent, e))?;
        }

        let mut line = serde_json::to_string(record)?;
        line.push('\n');

        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| io(&self.path, e))?;

        f.write_all(line.as_bytes())
            .await
            .map_err(|e| io(&self.path, e))?;
        f.sync_all().await.map_err(|e| io(&self.path, e))?;
        Ok(())
    }

    /// Load every intact record.
    ///
    /// A trailing torn line, which a power cut mid-write leaves behind, is
    /// skipped with a warning rather than failing the load.
    ///
    /// # Errors
    /// Returns [`SpoolError::Io`] if the file exists but cannot be read.
    pub async fn load(&self) -> Result<Vec<SpoolRecord>, SpoolError> {
        let raw = match tokio::fs::read_to_string(&self.path).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&self.path, e)),
        };

        let mut out = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SpoolRecord>(line) {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::warn!(
                        line = i + 1,
                        error = %e,
                        "skipping unreadable spool record; likely a torn write from a power cut"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Number of queued records.
    ///
    /// # Errors
    /// Returns [`SpoolError::Io`] on read failure.
    pub async fn len(&self) -> Result<usize, SpoolError> {
        Ok(self.load().await?.len())
    }

    /// Whether the spool is empty.
    ///
    /// # Errors
    /// Returns [`SpoolError::Io`] on read failure.
    pub async fn is_empty(&self) -> Result<bool, SpoolError> {
        Ok(self.len().await? == 0)
    }

    /// Rewrite the spool without the acknowledged ids.
    ///
    /// Written to a temporary file and atomically renamed, so a crash during
    /// compaction leaves the previous spool intact. Losing the queue while
    /// tidying it would be a spectacular own goal.
    ///
    /// # Errors
    /// Returns [`SpoolError::Io`] if the rewrite fails.
    pub async fn remove_acknowledged(&self, acked: &[String]) -> Result<usize, SpoolError> {
        let acked: std::collections::BTreeSet<&str> = acked.iter().map(String::as_str).collect();
        let remaining: Vec<SpoolRecord> = self
            .load()
            .await?
            .into_iter()
            .filter(|r| !acked.contains(r.id.as_str()))
            .collect();

        let mut buf = String::new();
        for r in &remaining {
            buf.push_str(&serde_json::to_string(r)?);
            buf.push('\n');
        }

        let tmp = self.path.with_extension("compacting");
        {
            let mut f = tokio::fs::File::create(&tmp)
                .await
                .map_err(|e| io(&tmp, e))?;
            f.write_all(buf.as_bytes()).await.map_err(|e| io(&tmp, e))?;
            f.sync_all().await.map_err(|e| io(&tmp, e))?;
        }
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| io(&self.path, e))?;

        Ok(remaining.len())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rec(topic: &str) -> SpoolRecord {
        SpoolRecord::new(topic, serde_json::json!({"v": 1}))
    }

    #[tokio::test]
    async fn records_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool.jsonl");

        {
            let s = Spool::new(&path);
            s.append(&rec("unit.born")).await.unwrap();
            s.append(&rec("unit.measured")).await.unwrap();
        }

        // A brand new handle, as after a station reboot.
        let s = Spool::new(&path);
        let loaded = s.load().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].topic, "unit.born");
        assert_eq!(loaded[1].topic, "unit.measured");
    }

    #[tokio::test]
    async fn an_empty_spool_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::new(dir.path().join("nothing-here.jsonl"));
        assert!(s.is_empty().await.unwrap());
        assert!(s.load().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_torn_final_line_does_not_destroy_the_queue() {
        // The power-cut case: the last write was interrupted mid-line. Losing
        // that one event is unavoidable; losing the rest would not be.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool.jsonl");
        let s = Spool::new(&path);

        s.append(&rec("unit.born")).await.unwrap();
        s.append(&rec("unit.measured")).await.unwrap();

        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        f.write_all(b"{\"id\":\"01ABC\",\"topic\":\"unit.compl")
            .await
            .unwrap();
        f.sync_all().await.unwrap();

        let loaded = s.load().await.unwrap();
        assert_eq!(loaded.len(), 2, "the intact records must still load");
    }

    #[tokio::test]
    async fn acknowledged_records_are_removed_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::new(dir.path().join("spool.jsonl"));

        let a = rec("a");
        let b = rec("b");
        let c = rec("c");
        for r in [&a, &b, &c] {
            s.append(r).await.unwrap();
        }

        let left = s
            .remove_acknowledged(&[a.id.clone(), c.id.clone()])
            .await
            .unwrap();
        assert_eq!(left, 1);

        let remaining = s.load().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, b.id);
    }

    #[tokio::test]
    async fn compaction_leaves_a_valid_spool_that_can_still_be_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::new(dir.path().join("spool.jsonl"));

        let a = rec("a");
        s.append(&a).await.unwrap();
        s.remove_acknowledged(std::slice::from_ref(&a.id))
            .await
            .unwrap();
        assert!(s.is_empty().await.unwrap());

        let b = rec("b");
        s.append(&b).await.unwrap();
        assert_eq!(s.load().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn every_record_carries_a_distinct_idempotency_key() {
        // Replay after a reconnect must not create duplicates.
        let a = rec("x");
        let b = rec("x");
        assert_ne!(a.id, b.id);
        assert_eq!(a.id.len(), 26, "ULID text form");
    }

    #[tokio::test]
    async fn replaying_the_same_records_is_safe_because_ids_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::new(dir.path().join("spool.jsonl"));
        let r = rec("unit.born");
        s.append(&r).await.unwrap();

        let first = s.load().await.unwrap();
        let second = s.load().await.unwrap();
        assert_eq!(
            first[0].id, second[0].id,
            "the id must not change between reads"
        );
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn the_spool_directory_is_created_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let s = Spool::new(dir.path().join("nested/deeper/spool.jsonl"));
        s.append(&rec("x")).await.unwrap();
        assert_eq!(s.len().await.unwrap(), 1);
    }
}
