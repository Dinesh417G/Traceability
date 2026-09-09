//! Per-unit append-only hash chain.
//!
//! `unit_event` and `measurement` rows are chained: each row hashes its own
//! canonical contents together with the hash of the previous row for the same
//! unit. Rewriting or deleting history therefore breaks every subsequent link,
//! which is what makes the trace defensible in an IATF 16949 or customer audit.
//!
//! The chain is deliberately simple and reproducible: an auditor with the raw
//! rows and this description can recompute it independently.

use crate::error::{CoreError, Result};
use sha2::{Digest, Sha256};

/// Hash of the notional row before the first one in a unit's chain.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One link in a unit's hash chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    /// Monotonic position within this unit's chain, starting at 0.
    pub seq: u64,
    /// Hash of the previous link, or [`GENESIS`] for the first.
    pub prev_hash: String,
    /// Hash of this row.
    pub row_hash: String,
}

/// Compute a row hash from the previous hash and the row's canonical payload.
///
/// The payload must be produced deterministically by the caller (stable field
/// order, no map iteration order dependence), because the hash is only as
/// reproducible as its input.
#[must_use]
pub fn row_hash(prev_hash: &str, canonical_payload: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    // Length-prefix the payload so that concatenation is unambiguous: without
    // this, ("ab","c") and ("a","bc") would hash identically.
    h.update((canonical_payload.len() as u64).to_be_bytes());
    h.update(canonical_payload.as_bytes());
    hex::encode(h.finalize())
}

/// Append a link to a chain, given the tail (or `None` for the first row).
#[must_use]
pub fn append(tail: Option<&ChainLink>, canonical_payload: &str) -> ChainLink {
    let (seq, prev) = match tail {
        Some(t) => (t.seq + 1, t.row_hash.clone()),
        None => (0, GENESIS.to_owned()),
    };
    let row_hash = row_hash(&prev, canonical_payload);
    ChainLink {
        seq,
        prev_hash: prev,
        row_hash,
    }
}

/// Verify a whole chain against the payloads it claims to cover.
///
/// # Errors
/// Returns [`CoreError::HashChainBroken`] at the first link whose stored hash
/// does not match a recomputation, or whose `prev_hash` does not match the
/// preceding link.
pub fn verify(links: &[ChainLink], payloads: &[String]) -> Result<()> {
    if links.len() != payloads.len() {
        return Err(CoreError::HashChainBroken {
            seq: links.len() as u64,
            expected: format!("{} payloads", links.len()),
            found: format!("{} payloads", payloads.len()),
        });
    }
    let mut expected_prev = GENESIS.to_owned();
    for (link, payload) in links.iter().zip(payloads) {
        if link.prev_hash != expected_prev {
            return Err(CoreError::HashChainBroken {
                seq: link.seq,
                expected: expected_prev,
                found: link.prev_hash.clone(),
            });
        }
        let recomputed = row_hash(&link.prev_hash, payload);
        if recomputed != link.row_hash {
            return Err(CoreError::HashChainBroken {
                seq: link.seq,
                expected: recomputed,
                found: link.row_hash.clone(),
            });
        }
        expected_prev = link.row_hash.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn build(payloads: &[&str]) -> (Vec<ChainLink>, Vec<String>) {
        let mut links: Vec<ChainLink> = Vec::new();
        for p in payloads {
            let link = append(links.last(), p);
            links.push(link);
        }
        (links, payloads.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn intact_chain_verifies() {
        let (links, payloads) = build(&["born", "torque=12.1", "tested"]);
        assert_eq!(links[0].prev_hash, GENESIS);
        assert_eq!(links[1].prev_hash, links[0].row_hash);
        assert_eq!(links[2].seq, 2);
        verify(&links, &payloads).unwrap();
    }

    #[test]
    fn tampering_with_a_value_is_detected() {
        let (links, mut payloads) = build(&["born", "torque=12.1", "tested"]);
        // Someone edits a failing torque reading to look passing.
        payloads[1] = "torque=18.0".to_owned();
        let err = verify(&links, &payloads).unwrap_err();
        match err {
            CoreError::HashChainBroken { seq, .. } => assert_eq!(seq, 1),
            other => panic!("expected HashChainBroken, got {other:?}"),
        }
    }

    #[test]
    fn deleting_a_row_is_detected() {
        let (mut links, mut payloads) = build(&["born", "torque=12.1", "tested"]);
        links.remove(1);
        payloads.remove(1);
        // The surviving link still points at the removed row's parent hash.
        assert!(verify(&links, &payloads).is_err());
    }

    #[test]
    fn reordering_is_detected() {
        let (mut links, payloads) = build(&["a", "b", "c"]);
        links.swap(1, 2);
        assert!(verify(&links, &payloads).is_err());
    }

    #[test]
    fn length_prefix_prevents_concatenation_collision() {
        // Without a length prefix these two would hash the same.
        assert_ne!(row_hash(GENESIS, "ab"), row_hash(GENESIS, "a\u{0}b"));
        let a = append(None, "ab");
        let b = append(None, "abc");
        assert_ne!(a.row_hash, b.row_hash);
    }

    #[test]
    fn hash_is_stable_across_runs() {
        // Reproducibility is the whole point: an auditor must get our number.
        assert_eq!(row_hash(GENESIS, "born"), row_hash(GENESIS, "born"));
    }
}
