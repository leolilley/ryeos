use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use lillux::crypto::{load_signing_key, SigningKey, fingerprint as compute_fp};

use crate::error::CliTransportError;

/// Loaded signing material for request signing.
pub struct Signer {
    signing_key: SigningKey,
    pub fingerprint: String,
}

impl Signer {
    /// Resolve the signing key:
    ///   1. RYEOS_CLI_KEY_PATH env var
    ///   2. <state_dir>/.ai/node/identity/private_key.pem
    ///      (matches `Config::node_signing_key_path` defaults in
    ///      `ryeosd/src/config.rs` and `ryeosd/src/bootstrap.rs`)
    ///   3. Fail
    pub fn resolve(state_dir: &Path) -> Result<Self, CliTransportError> {
        if let Ok(p) = std::env::var("RYEOS_CLI_KEY_PATH") {
            let pb = PathBuf::from(&p);
            return Self::load_from(&pb);
        }
        let default = state_dir
            .join(".ai")
            .join("node")
            .join("identity")
            .join("private_key.pem");
        Self::load_from(&default)
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

    /// Build canonical request matching ryeosd/src/auth.rs exactly and produce
    /// the four `x-rye-*` headers.
    ///
    /// Canonical string:
    ///   "ryeos-request-v1\n{METHOD}\n{canonical_path}\n{sha256(body)}\n{timestamp}\n{nonce}\n{audience}"
    ///
    /// Signature = Ed25519(sk, sha256(canonical_string).as_bytes())
    pub fn sign(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
    ) -> Result<SignHeaders, CliTransportError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let nonce_bytes = rand::Rng::gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = hex::encode(nonce_bytes);

        let body_hash = lillux::cas::sha256_hex(body);

        let canon_path = canonicalize_path(path_and_query);

        // Audience: currently auth is disabled so this value is unused by the daemon.
        // When require_auth flips on (auth-capability-gap item 3), the CLI must
        // provide the daemon's principal_id. For now, use the node fingerprint.
        let audience = self.fingerprint.clone();

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
/// Matches ryeosd/src/auth.rs::canonical_path.
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
            .map(|(k, v)| {
                if v.is_empty() {
                    k
                } else {
                    format!("{k}={v}")
                }
            })
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
