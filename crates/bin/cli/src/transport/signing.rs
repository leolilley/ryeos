use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use lillux::crypto::{fingerprint as compute_fp, load_signing_key, SigningKey};
use ryeos_engine::AI_DIR;

use crate::error::CliTransportError;

/// Loaded signing material for request signing.
pub struct Signer {
    signing_key: SigningKey,
    pub fingerprint: String,
}

impl Signer {
    /// Resolve the signing key:
    ///   1. `<app_root>/<AI_DIR>/config/keys/signing/private_key.pem`.
    ///      This is the operator's persistent identity, created by `ryeos init`.
    ///   2. Fail
    ///
    /// This is the operator key — the operator's identity for authenticating to
    /// daemons. The node key is the daemon's own identity; the CLI must NOT
    /// silently fall back to it.
    ///
    /// The app root is supplied by the transport after applying CLI/env config
    /// resolution; no separate `RYEOS_APP_ROOT` lookup is performed here.
    pub fn resolve(app_root: &Path) -> Result<Self, CliTransportError> {
        let operator_key = app_root
            .join(AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem");
        Self::load_from(&operator_key)
    }

    fn load_from(path: &Path) -> Result<Self, CliTransportError> {
        if !path.exists() {
            return Err(CliTransportError::SigningKeyMissing {
                path: path.to_path_buf(),
            });
        }
        let sk = load_signing_key(path).map_err(|e| CliTransportError::SigningKeyUnloadable {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        let fp = compute_fp(&sk.verifying_key());
        Ok(Self {
            signing_key: sk,
            fingerprint: fp,
        })
    }

    /// Build canonical request matching crates/bin/daemon/src/auth.rs exactly and produce
    /// the four `x-ryeos-*` headers.
    ///
    /// Canonical string:
    ///   "ryeos-request-v1\n{METHOD}\n{canonical_path}\n{sha256(body)}\n{timestamp}\n{nonce}\n{audience}"
    ///
    /// Signature = Ed25519(sk, sha256(canonical_string).as_bytes())
    ///
    /// `audience` MUST be the target daemon's `principal_id` (discovered via
    /// GET /public-key), NOT the caller's own fingerprint.
    pub fn sign(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        audience: &str,
    ) -> Result<SignHeaders, CliTransportError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let nonce_bytes = rand::Rng::gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = hex::encode(nonce_bytes);

        let body_hash = lillux::cas::sha256_hex(body);

        let canon_path = canonicalize_path(path_and_query);

        let string_to_sign = format!(
            "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_uppercase(),
            canon_path,
            body_hash,
            timestamp,
            nonce,
            audience,
        );

        let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
        let sig = lillux::crypto::Signer::sign(&self.signing_key, content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        Ok(SignHeaders {
            key_id: format!("fp:{}", self.fingerprint),
            timestamp: timestamp.to_string(),
            nonce,
            signature: sig_b64,
        })
    }
}

pub struct SignHeaders {
    pub key_id: String,
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
}

/// Sort query parameters alphabetically and normalise the path.
/// Matches crates/bin/daemon/src/auth.rs::canonical_path.
fn canonicalize_path(path_and_query: &str) -> String {
    if let Some((path, query)) = path_and_query.split_once('?') {
        let mut params: BTreeMap<String, String> = BTreeMap::new();
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                params.insert(k.to_string(), v.to_string());
            } else if !pair.is_empty() {
                params.insert(pair.to_string(), String::new());
            }
        }
        let sorted: Vec<String> = params
            .into_iter()
            .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
            .collect();
        format!("{}?{}", path, sorted.join("&"))
    } else {
        path_and_query.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_path_no_query() {
        assert_eq!(canonicalize_path("/execute"), "/execute");
    }

    #[test]
    fn canonicalize_path_sorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc/events/stream?after=42&limit=10"),
            "/threads/abc/events/stream?after=42&limit=10"
        );
    }

    #[test]
    fn canonicalize_path_unsorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc?limit=10&after=42"),
            "/threads/abc?after=42&limit=10"
        );
    }
}
