//! # trace-sign
//!
//! Canonical JSON plus Ed25519 detached signatures.
//!
//! Two things in this product are trusted only because they are signed: the
//! **entitlement** that says what a customer has paid for, and the **OTA
//! manifest** that says what code an edge box should run. Both are verified on
//! a factory box that may have no internet, so verification must be purely
//! local, fast, and impossible to get subtly wrong.
//!
//! ## Why canonical JSON
//!
//! A signature covers bytes, not meaning. If the issuer serialises
//! `{"a":1,"b":2}` and the verifier reconstructs `{"b":2,"a":1}`, the signature
//! fails for no good reason — or worse, a verifier that re-serialises loosely
//! can be made to accept a document that says something different from what was
//! signed. [`canonical_json`] fixes one byte-level representation: object keys
//! sorted, no insignificant whitespace, so the same document always produces
//! the same bytes.
//!
//! ## Key custody
//!
//! An edge box holds **public keys only**. Signing happens in the vendor's
//! control plane. A signing key on a factory box would let anyone who opens the
//! box mint their own licence or their own update.

#![warn(missing_docs)]

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

/// Errors from signing or verification.
#[derive(Debug, Error)]
pub enum SignError {
    /// The document could not be serialised.
    #[error("cannot serialise document: {0}")]
    Serialise(#[from] serde_json::Error),

    /// A key was not the expected length or encoding.
    #[error("malformed key: {0}")]
    MalformedKey(String),

    /// A signature was not the expected length or encoding.
    #[error("malformed signature: {0}")]
    MalformedSignature(String),

    /// The signature did not verify against the payload and key.
    ///
    /// Deliberately carries no detail: distinguishing "wrong key" from "wrong
    /// bytes" tells an attacker which half to keep trying.
    #[error("signature verification failed")]
    BadSignature,

    /// The envelope named a signing scheme this build does not implement.
    #[error("unsupported signature algorithm: {0}")]
    UnsupportedAlgorithm(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, SignError>;

/// Serialise a value to canonical JSON bytes: object keys sorted, compact.
///
/// This is the exact byte sequence that gets signed and verified.
///
/// # Errors
/// Returns [`SignError::Serialise`] if the value cannot be represented as JSON.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let v: serde_json::Value = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write_canonical(&v, &mut out);
    Ok(out)
}

/// Write a JSON value in canonical form.
///
/// `serde_json::Map` preserves insertion order unless the `preserve_order`
/// feature is off, so key order is normalised explicitly here rather than
/// assumed. Numbers and strings use serde_json's own encoder so escaping and
/// float formatting stay standard.
fn write_canonical(v: &serde_json::Value, out: &mut Vec<u8>) {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                // Key as a JSON string, using the standard escaping rules.
                let encoded = serde_json::Value::String((*k).clone());
                out.extend_from_slice(encoded.to_string().as_bytes());
                out.push(b':');
                write_canonical(&map[*k], out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

/// A signing key: the vendor's control-plane identity.
///
/// Never present on an edge box. A signing key on a factory box would let
/// anyone who opens the box mint their own licence or their own update.
#[derive(Debug)]
pub struct Issuer(SigningKey);

impl Issuer {
    /// Build from 32 raw seed bytes.
    ///
    /// # Errors
    /// Returns [`SignError::MalformedKey`] if the slice is not 32 bytes.
    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        let bytes: [u8; 32] = seed.try_into().map_err(|_| {
            SignError::MalformedKey(format!("expected 32 bytes, got {}", seed.len()))
        })?;
        Ok(Self(SigningKey::from_bytes(&bytes)))
    }

    /// Build from a 64-character hex seed.
    ///
    /// # Errors
    /// Returns [`SignError::MalformedKey`] if the hex is invalid or the wrong
    /// length.
    pub fn from_hex(seed_hex: &str) -> Result<Self> {
        let raw =
            hex::decode(seed_hex.trim()).map_err(|e| SignError::MalformedKey(e.to_string()))?;
        Self::from_seed(&raw)
    }

    /// Generate a fresh key. For tests and for out-of-band key ceremonies.
    #[must_use]
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::fill(&mut seed);
        Self(SigningKey::from_bytes(&seed))
    }

    /// The matching public key, hex encoded, for distribution to edge boxes.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.0.verifying_key().to_bytes())
    }

    /// The corresponding verifier.
    #[must_use]
    pub fn verifier(&self) -> TrustedKey {
        TrustedKey(self.0.verifying_key())
    }

    /// Sign a document, producing a detached envelope.
    ///
    /// # Errors
    /// Returns [`SignError::Serialise`] if the document cannot be serialised.
    pub fn sign<T: Serialize>(&self, doc: &T) -> Result<Envelope> {
        let payload = canonical_json(doc)?;
        let sig: Signature = self.0.sign(&payload);
        Ok(Envelope {
            alg: ALG_ED25519.to_owned(),
            payload_b64: b64().encode(&payload),
            signature_b64: b64().encode(sig.to_bytes()),
            key_id: short_key_id(&self.0.verifying_key()),
        })
    }
}

/// A trusted public key. This is what an edge box holds, and all it needs:
/// verification is local, offline, and takes microseconds.
#[derive(Debug, Clone)]
pub struct TrustedKey(VerifyingKey);

impl TrustedKey {
    /// Build from 32 raw public key bytes.
    ///
    /// # Errors
    /// Returns [`SignError::MalformedKey`] if the slice is not a valid key.
    pub fn from_bytes(raw: &[u8]) -> Result<Self> {
        let bytes: [u8; 32] = raw.try_into().map_err(|_| {
            SignError::MalformedKey(format!("expected 32 bytes, got {}", raw.len()))
        })?;
        VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|e| SignError::MalformedKey(e.to_string()))
    }

    /// Build from a 64-character hex public key.
    ///
    /// # Errors
    /// Returns [`SignError::MalformedKey`] if the hex is invalid.
    pub fn from_hex(key_hex: &str) -> Result<Self> {
        let raw =
            hex::decode(key_hex.trim()).map_err(|e| SignError::MalformedKey(e.to_string()))?;
        Self::from_bytes(&raw)
    }

    /// Hex form, for logging and config round-tripping.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    /// Short identifier used to tell keys apart during rotation.
    #[must_use]
    pub fn key_id(&self) -> String {
        short_key_id(&self.0)
    }

    /// Verify an envelope and decode the document it covers.
    ///
    /// The document is deserialised **from the signed bytes**, never from a
    /// separately supplied copy. That ordering is the whole point: it makes it
    /// impossible to verify one document and then act on another.
    ///
    /// # Errors
    /// Returns [`SignError::BadSignature`] if verification fails, or
    /// [`SignError::UnsupportedAlgorithm`] for an unknown `alg`.
    pub fn verify<T: DeserializeOwned>(&self, env: &Envelope) -> Result<T> {
        if env.alg != ALG_ED25519 {
            return Err(SignError::UnsupportedAlgorithm(env.alg.clone()));
        }
        let payload = b64()
            .decode(&env.payload_b64)
            .map_err(|e| SignError::MalformedSignature(e.to_string()))?;
        let sig_raw = b64()
            .decode(&env.signature_b64)
            .map_err(|e| SignError::MalformedSignature(e.to_string()))?;
        let sig_bytes: [u8; 64] = sig_raw.as_slice().try_into().map_err(|_| {
            SignError::MalformedSignature(format!("expected 64 bytes, got {}", sig_raw.len()))
        })?;

        self.0
            .verify(&payload, &Signature::from_bytes(&sig_bytes))
            .map_err(|_| SignError::BadSignature)?;

        serde_json::from_slice(&payload).map_err(SignError::Serialise)
    }
}

/// Algorithm identifier carried in every envelope.
pub const ALG_ED25519: &str = "ed25519";

/// A signed document: the exact bytes that were signed, plus the signature.
///
/// The payload travels base64-encoded rather than as inline JSON so that no
/// intermediate tool can reformat it and invalidate the signature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    /// Signature algorithm. Only [`ALG_ED25519`] is implemented.
    pub alg: String,
    /// Base64 of the canonical JSON that was signed.
    pub payload_b64: String,
    /// Base64 of the 64-byte Ed25519 signature.
    pub signature_b64: String,
    /// Short id of the signing key, so rotation is diagnosable.
    pub key_id: String,
}

impl Envelope {
    /// Decode the payload **without verifying it**.
    ///
    /// Only for diagnostics: printing what a rejected document claimed to say.
    /// Never act on the result.
    ///
    /// # Errors
    /// Returns [`SignError::Serialise`] if the payload is not valid JSON.
    pub fn peek_unverified(&self) -> Result<serde_json::Value> {
        let payload = b64()
            .decode(&self.payload_b64)
            .map_err(|e| SignError::MalformedSignature(e.to_string()))?;
        serde_json::from_slice(&payload).map_err(SignError::Serialise)
    }
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn short_key_id(vk: &VerifyingKey) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(vk.to_bytes());
    hex::encode(&h.finalize()[..4])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Doc {
        tier: String,
        seats: u32,
        node: String,
    }

    fn doc() -> Doc {
        Doc {
            tier: "PROFESSIONAL".into(),
            seats: 12,
            node: "edge-01".into(),
        }
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let signer = Issuer::generate();
        let env = signer.sign(&doc()).unwrap();
        let back: Doc = signer.verifier().verify(&env).unwrap();
        assert_eq!(back, doc());
    }

    #[test]
    fn a_different_key_does_not_verify() {
        let env = Issuer::generate().sign(&doc()).unwrap();
        let attacker = Issuer::generate();
        assert!(matches!(
            attacker.verifier().verify::<Doc>(&env),
            Err(SignError::BadSignature)
        ));
    }

    #[test]
    fn tampering_with_the_payload_is_caught() {
        let signer = Issuer::generate();
        let mut env = signer.sign(&doc()).unwrap();

        // Forge an upgrade to the top tier with more seats.
        let forged = Doc {
            tier: "ENTERPRISE".into(),
            seats: 9_999,
            node: "edge-01".into(),
        };
        env.payload_b64 = b64().encode(canonical_json(&forged).unwrap());

        assert!(matches!(
            signer.verifier().verify::<Doc>(&env),
            Err(SignError::BadSignature)
        ));
    }

    #[test]
    fn tampering_with_the_signature_is_caught() {
        let signer = Issuer::generate();
        let mut env = signer.sign(&doc()).unwrap();
        let mut raw = b64().decode(&env.signature_b64).unwrap();
        raw[0] ^= 0xff;
        env.signature_b64 = b64().encode(&raw);
        assert!(signer.verifier().verify::<Doc>(&env).is_err());
    }

    #[test]
    fn canonical_json_sorts_keys_so_field_order_cannot_break_a_signature() {
        let a: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(canonical_json(&a).unwrap(), canonical_json(&b).unwrap());
        assert_eq!(
            String::from_utf8(canonical_json(&a).unwrap()).unwrap(),
            r#"{"a":1,"b":2}"#
        );
    }

    #[test]
    fn canonical_json_is_recursive() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"z":{"y":1,"x":[{"b":1,"a":2}]},"a":3}"#).unwrap();
        assert_eq!(
            String::from_utf8(canonical_json(&v).unwrap()).unwrap(),
            r#"{"a":3,"z":{"x":[{"a":2,"b":1}],"y":1}}"#
        );
    }

    #[test]
    fn unsupported_algorithm_is_rejected_rather_than_ignored() {
        let signer = Issuer::generate();
        let mut env = signer.sign(&doc()).unwrap();
        env.alg = "none".into();
        assert!(matches!(
            signer.verifier().verify::<Doc>(&env),
            Err(SignError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn public_key_round_trips_through_hex() {
        let signer = Issuer::generate();
        let hex_key = signer.public_key_hex();
        let v = TrustedKey::from_hex(&hex_key).unwrap();
        assert_eq!(v.to_hex(), hex_key);
        let env = signer.sign(&doc()).unwrap();
        assert_eq!(v.verify::<Doc>(&env).unwrap(), doc());
        assert_eq!(
            env.key_id,
            v.key_id(),
            "envelope names the key that signed it"
        );
    }

    #[test]
    fn peek_does_not_imply_trust() {
        // Diagnostics must be able to show what a rejected document claimed.
        let env = Issuer::generate().sign(&doc()).unwrap();
        let seen = env.peek_unverified().unwrap();
        assert_eq!(seen["tier"], "PROFESSIONAL");
    }
}
