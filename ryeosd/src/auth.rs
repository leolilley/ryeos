use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use base64::Engine;
use lillux::crypto::{Signature, Verifier, VerifyingKey};

use crate::identity::NodeIdentity;
use crate::state::AppState;

const TIMESTAMP_MAX_AGE_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
    pub owner: String,
}

// ---------------------------------------------------------------------------
// Replay guard
// ---------------------------------------------------------------------------

struct ReplayGuard {
    seen: HashMap<String, Vec<(String, Instant)>>,
    max_age: Duration,
}

impl ReplayGuard {
    fn new() -> Self {
        Self {
            seen: HashMap::new(),
            max_age: Duration::from_secs(TIMESTAMP_MAX_AGE_SECS + 60),
        }
    }

    fn check_and_record(&mut self, fingerprint: &str, nonce: &str) -> bool {
        let now = Instant::now();
        let entries = self.seen.entry(fingerprint.to_string()).or_default();

        // Prune expired entries
        entries.retain(|(_, ts)| now.duration_since(*ts) < self.max_age);

        // Check for replay
        if entries.iter().any(|(n, _)| n == nonce) {
            return false;
        }

        entries.push((nonce.to_string(), now));
        true
    }
}

static REPLAY_GUARD: LazyLock<Mutex<ReplayGuard>> =
    LazyLock::new(|| Mutex::new(ReplayGuard::new()));

// ---------------------------------------------------------------------------
// Authorized key file loading
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AuthorizedKey {
    public_key: VerifyingKey,
    scopes: Vec<String>,
    owner: String,
}

/// Parse a simple TOML body with key = "value" lines.
/// Handles string values (quoted) and string arrays.
fn parse_toml_value(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim();
            // Strip surrounding quotes
            let value = if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                value[1..value.len() - 1].to_string()
            } else {
                value.to_string()
            };
            map.insert(key, value);
        }
    }
    map
}

/// Parse a TOML array value like `["a", "b"]` into Vec<String>.
fn parse_toml_string_array(body: &str, key: &str) -> Vec<String> {
    for line in body.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() != key {
                continue;
            }
            let v = v.trim();
            if v.starts_with('[') && v.ends_with(']') {
                let inner = &v[1..v.len() - 1];
                return inner
                    .split(',')
                    .map(|s| {
                        let s = s.trim();
                        if (s.starts_with('"') && s.ends_with('"'))
                            || (s.starts_with('\'') && s.ends_with('\''))
                        {
                            s[1..s.len() - 1].to_string()
                        } else {
                            s.to_string()
                        }
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

fn load_authorized_key(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<AuthorizedKey> {
    let key_file = auth_dir.join(format!("{fingerprint}.toml"));
    if !key_file.exists() {
        bail!("unknown principal");
    }

    let raw = fs::read_to_string(&key_file)?;
    let (sig_line, body) = raw.split_once('\n').unwrap_or(("", &raw));

    // Verify node signature header
    let sig_line = sig_line.trim();
    if !sig_line.starts_with("# ryeos:signed:") {
        bail!("unsigned key file");
    }

    // Format: # ryeos:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>
    // Timestamp may contain colons (ISO 8601), so rsplit from the right.
    let remainder = &sig_line["# ryeos:signed:".len()..];
    let parts: Vec<&str> = remainder.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        bail!("malformed signature header");
    }
    let signer_fp = parts[0];
    let sig_b64 = parts[1];
    let content_hash = parts[2];

    // Verify signer is this node
    if signer_fp != node_identity.fingerprint() {
        bail!("wrong signer");
    }

    // Verify content hash
    let actual_hash = lillux::cas::sha256_hex(body.as_bytes());
    if actual_hash != content_hash {
        bail!("tampered key file");
    }

    // Verify signature over the content hash
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let signature = Signature::from_slice(&sig_bytes)?;
    node_identity.verify_hash(content_hash, &signature)?;

    // Parse TOML body
    let values = parse_toml_value(body);

    // Verify fingerprint matches
    let file_fp = values.get("fingerprint").map(|s| s.as_str()).unwrap_or("");
    if file_fp != fingerprint {
        bail!("fingerprint mismatch");
    }

    // Extract public key
    let public_key_str = values
        .get("public_key")
        .map(|s| s.as_str())
        .unwrap_or("");
    if !public_key_str.starts_with("ed25519:") {
        bail!("invalid public key format");
    }
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(&public_key_str[8..])?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let public_key = VerifyingKey::from_bytes(&key_array)?;

    let computed_fp = lillux::signature::compute_fingerprint(&public_key);
    if computed_fp != fingerprint {
        bail!(
            "authorized-key file {} declares fingerprint {} but \
             its public_key fingerprint computes to {} — refusing to load",
            key_file.display(),
            fingerprint,
            computed_fp
        );
    }

    let scopes = parse_toml_string_array(body, "scopes");
    let owner = values
        .get("label")
        .cloned()
        .unwrap_or_default();

    Ok(AuthorizedKey {
        public_key,
        scopes,
        owner,
    })
}

// ---------------------------------------------------------------------------
// Request verification
// ---------------------------------------------------------------------------

fn canonical_path(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    match uri.query() {
        None | Some("") => path.to_string(),
        Some(query) => {
            let mut params: Vec<(&str, &str)> = query
                .split('&')
                .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
                .collect();
            params.sort();
            let sorted: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect();
            format!("{path}?{}", sorted.join("&"))
        }
    }
}

pub(crate) fn verify_request(state: &AppState, method: &str, uri: &axum::http::Uri, headers: &axum::http::HeaderMap, body: &[u8]) -> Result<Principal, String> {
    let key_id = headers
        .get("x-ryeos-key-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let timestamp = headers
        .get("x-ryeos-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let nonce = headers
        .get("x-ryeos-nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let signature = headers
        .get("x-ryeos-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if key_id.is_empty() || timestamp.is_empty() || nonce.is_empty() || signature.is_empty() {
        return Err("missing auth headers".to_string());
    }

    // Extract fingerprint
    if !key_id.starts_with("fp:") {
        return Err("invalid key ID format".to_string());
    }
    let fingerprint = &key_id[3..];

    // Check timestamp freshness
    let req_time: u64 = timestamp
        .parse()
        .map_err(|_| "invalid timestamp".to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now.abs_diff(req_time) > TIMESTAMP_MAX_AGE_SECS {
        return Err("request expired".to_string());
    }

    // Load authorized key file
    let auth_key = load_authorized_key(
        fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|e| e.to_string())?;

    // Compute audience (this node's identity)
    let audience = state.identity.principal_id();

    // Build string-to-sign
    let body_hash = lillux::cas::sha256_hex(body);
    let canon = canonical_path(uri);
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canon,
        body_hash,
        timestamp,
        nonce,
        audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());

    // Verify signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|_| "invalid signature encoding".to_string())?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "invalid signature".to_string())?;
    auth_key
        .public_key
        .verify(content_hash.as_bytes(), &sig)
        .map_err(|_| "invalid signature".to_string())?;

    // Replay check
    {
        let mut guard = match REPLAY_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("replay guard mutex was poisoned, recovering");
                poisoned.into_inner()
            }
        };
        if !guard.check_and_record(fingerprint, nonce) {
            return Err("replayed request".to_string());
        }
    }

    Ok(Principal {
        fingerprint: fingerprint.to_string(),
        scopes: auth_key.scopes,
        owner: auth_key.owner,
    })
}

// ---------------------------------------------------------------------------
// Auth middleware was deleted in v0.4.0.
// All auth now happens per-route inside the dispatcher's auth_invoker chain.
// verify_request() is still used by CompiledRyeosSignedVerifier.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use base64::Engine;
    use lillux::crypto::{EncodePrivateKey, SigningKey};
    use tempfile::TempDir;

    use super::*;
    use crate::identity::NodeIdentity;

    fn make_node_identity(sk: &SigningKey, dir: &std::path::Path) -> NodeIdentity {
        let pem = sk.to_pkcs8_pem(Default::default()).unwrap();
        let key_path = dir.join("node_key.pem");
        std::fs::write(&key_path, pem.as_bytes()).unwrap();
        NodeIdentity::load(&key_path).unwrap()
    }

    #[test]
    fn load_authorized_key_rejects_fingerprint_public_key_mismatch() {
        let real_subject = SigningKey::from_bytes(&[1u8; 32]);
        let attacker_subject = SigningKey::from_bytes(&[2u8; 32]);
        let node_signer = SigningKey::from_bytes(&[3u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let real_fp = lillux::signature::compute_fingerprint(&real_subject.verifying_key());
        let attacker_vk = attacker_subject.verifying_key();
        let attacker_key_b64 = base64::engine::general_purpose::STANDARD
            .encode(attacker_vk.as_bytes());

        let toml_body = format!(
            "fingerprint = \"{real_fp}\"\npublic_key = \"ed25519:{attacker_key_b64}\"\nscopes = [\"*\"]\nlabel = \"evil\"\n"
        );

        let signed = lillux::signature::sign_content_at(
            &toml_body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );

        let file_path = auth_dir.join(format!("{real_fp}.toml"));
        std::fs::write(&file_path, signed).unwrap();

        let result = load_authorized_key(&real_fp, &auth_dir, &node_identity);
        let err = result.expect_err("should reject mismatched fingerprint/public_key");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint"),
            "error message should mention 'fingerprint', got: {msg}"
        );
    }
}
