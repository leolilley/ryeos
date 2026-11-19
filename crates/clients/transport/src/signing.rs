use std::path::{Path, PathBuf};

use base64::Engine;
use lillux::crypto::{SigningKey, fingerprint as compute_fp, load_signing_key};

use crate::error::TransportError;

/// Location of the operator's persistent signing key under an app root,
/// `<app_root>/.ai/node/operator/signing/private_key.pem`, as created by
/// `ryeos init`. The single client-side owner of this layout: this crate's
/// tests pin the exact path components, and the CLI's tests compare it with
/// the engine's runtime-root layout, so a change on either side fails a test.
pub fn operator_signing_key_path(app_root: &Path) -> PathBuf {
    app_root
        .join(".ai")
        .join("node")
        .join("operator")
        .join("signing")
        .join("private_key.pem")
}

/// Loaded signing material for request signing.
pub struct Signer {
    signing_key: SigningKey,
    pub fingerprint: String,
}

impl Signer {
    /// Resolve the signing key:
    ///   1. [`operator_signing_key_path`] under `app_root`.
    ///      This is the operator's persistent identity, created by `ryeos init`.
    ///   2. Fail
    ///
    /// This is the operator key — the operator's identity for authenticating to
    /// daemons. The node key is the daemon's own identity; the CLI must NOT
    /// silently fall back to it.
    ///
    /// The app root is supplied by the transport after applying CLI/env config
    /// resolution; no separate `RYEOS_APP_ROOT` lookup is performed here.
    pub fn resolve(app_root: &Path) -> Result<Self, TransportError> {
        Self::load(&operator_signing_key_path(app_root))
    }

    /// Load signing material from an explicit key file.
    pub fn load(path: &Path) -> Result<Self, TransportError> {
        if !path.exists() {
            return Err(TransportError::SigningKeyMissing {
                path: path.to_path_buf(),
            });
        }
        let sk = load_signing_key(path).map_err(|e| TransportError::SigningKeyUnloadable {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        let fp = compute_fp(&sk.verifying_key());
        Ok(Self {
            signing_key: sk,
            fingerprint: fp,
        })
    }

    /// Sign raw bytes with this key and return the base64 signature. The caller
    /// owns what the bytes mean; this is how a principal signs a statement of
    /// its own, such as a remote delegation, without exposing the key.
    pub fn sign_bytes_base64(&self, bytes: &[u8]) -> String {
        let signature = lillux::crypto::Signer::sign(&self.signing_key, bytes);
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }

    /// Build the canonical request string and produce the four `x-ryeos-*`
    /// headers. The daemon rebuilds this exact string from the request it
    /// receives, using its own principal id as the audience, and accepts the
    /// request only when the key id names an authorized key, the timestamp
    /// is within its freshness window and the signature verifies under that
    /// key. Any difference in the string below makes the request unsigned.
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
    ) -> Result<SignHeaders, TransportError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let nonce_bytes = rand::Rng::r#gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = hex::encode(nonce_bytes);

        self.sign_with(method, path_and_query, body, audience, timestamp, &nonce)
    }

    /// [`Signer::sign`] with the timestamp and nonce supplied, so the exact
    /// bytes signed can be pinned by fixed vectors.
    fn sign_with(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        audience: &str,
        timestamp: u64,
        nonce: &str,
    ) -> Result<SignHeaders, TransportError> {
        use ryeos_ui_presentation_contract::request_signing as rs;
        let content_hash = rs::content_hash(
            method,
            &rs::canonical_path_and_query(path_and_query),
            &rs::body_digest(body),
            &timestamp.to_string(),
            nonce,
            audience,
        );
        let sig = lillux::crypto::Signer::sign(&self.signing_key, content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        Ok(SignHeaders {
            key_id: format!("fp:{}", self.fingerprint),
            timestamp: timestamp.to_string(),
            nonce: nonce.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A change to this layout must be a deliberate change to the node's
    /// operator key location, made on both sides together.
    #[test]
    fn operator_signing_key_path_is_the_init_layout() {
        let app_root = Path::new("/srv/ryeos-app-root");
        let path = operator_signing_key_path(app_root);
        assert_eq!(
            path.strip_prefix(app_root)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>(),
            [".ai", "node", "operator", "signing", "private_key.pem"]
        );
    }

    #[test]
    fn signed_headers_match_shared_vectors() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../presentation-contract/tests/vectors/request_signing.json"
        ))
        .unwrap();
        let seed = vectors["signing_key_seed_byte"].as_u64().unwrap() as u8;
        let key = SigningKey::from_bytes(&[seed; 32]);
        let signer = Signer {
            fingerprint: compute_fp(&key.verifying_key()),
            signing_key: key,
        };
        for v in vectors["vectors"].as_array().unwrap() {
            let s = |k: &str| v[k].as_str().unwrap();
            let headers = signer
                .sign_with(
                    s("method"),
                    s("path_and_query"),
                    s("body").as_bytes(),
                    s("audience"),
                    v["timestamp"].as_u64().unwrap(),
                    s("nonce"),
                )
                .unwrap();
            assert_eq!(headers.signature, s("signature"), "{}", s("name"));
            assert_eq!(headers.nonce, s("nonce"));
            assert_eq!(headers.timestamp, v["timestamp"].to_string());
        }
    }
}
