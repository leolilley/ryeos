use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use base64::Engine;
use lillux::crypto::{Signature, Verifier, VerifyingKey};

use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use ryeos_app::state::AppState;

const TIMESTAMP_MAX_AGE_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
    pub owner: String,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub authenticated_site_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Replay guard
// ---------------------------------------------------------------------------

/// Ceiling on remembered nonces per fingerprint. Legitimate callers sit
/// far below this within the replay window; hitting it means abuse or a
/// runaway client, and requests are rejected (fail closed) rather than
/// evicting older nonces, which would re-open the replay window.
const MAX_NONCES_PER_FINGERPRINT: usize = 4096;

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
        // Drop fingerprints whose nonces have all expired so the map
        // doesn't grow with every principal ever seen.
        self.seen.retain(|_, entries| {
            entries.retain(|(_, ts)| now.duration_since(*ts) < self.max_age);
            !entries.is_empty()
        });

        let entries = self.seen.entry(fingerprint.to_string()).or_default();

        // Check for replay
        if entries.iter().any(|(n, _)| n == nonce) {
            return false;
        }

        if entries.len() >= MAX_NONCES_PER_FINGERPRINT {
            tracing::warn!(
                fingerprint = %fingerprint,
                "replay guard nonce ceiling reached; rejecting request"
            );
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
    principal_class: AuthorizedKeyPrincipalClass,
    configured_origin_site_id: Option<String>,
}

fn load_authorized_key(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<AuthorizedKey> {
    let grant =
        ryeos_app::identity::load_verified_authorized_key(fingerprint, auth_dir, node_identity)?
            .ok_or_else(|| anyhow::anyhow!("unknown principal"))?;
    Ok(AuthorizedKey {
        public_key: grant.public_key,
        scopes: grant.scopes,
        owner: grant.owner,
        principal_class: grant.principal_class,
        configured_origin_site_id: grant.configured_origin_site_id,
    })
}

/// Validate the complete authorized-key namespace during daemon startup.
/// Authentication is a boot invariant: accepting the node and discovering an
/// unusable legacy grant only on the first request would leave an apparently
/// healthy daemon that rejects its operator.
pub fn validate_authorized_key_directory(
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<()> {
    let metadata = fs::symlink_metadata(auth_dir)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!(
            "authorized-key namespace is not a regular directory: {}",
            auth_dir.display()
        );
    }
    let mut entries = fs::read_dir(auth_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "authorized-key namespace contains a non-regular entry: {}",
                path.display()
            );
        }
        let fingerprint = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".toml"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "authorized-key namespace contains an unexpected entry: {}",
                    path.display()
                )
            })?;
        load_authorized_key(fingerprint, auth_dir, node_identity).map_err(|error| {
            anyhow::anyhow!("invalid authorized-key grant {}: {error:#}", path.display())
        })?;
    }
    Ok(())
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

/// Hash the exact request facts authenticated by a source-node forwarding
/// co-signature. The primary signature is included so the source node attests
/// to this operator authorization, not merely to a coincident request body.
pub(crate) fn forwarding_request_content_hash(
    method: &str,
    canonical_path: &str,
    body_hash: &str,
    timestamp: &str,
    nonce: &str,
    audience: &str,
    primary_key_id: &str,
    primary_signature: &str,
    forwarding_key_id: &str,
    forwarding_site_id: &str,
) -> String {
    let string_to_sign = format!(
        "ryeos-forwarded-request-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_path,
        body_hash,
        timestamp,
        nonce,
        audience,
        primary_key_id,
        primary_signature,
        forwarding_key_id,
        forwarding_site_id,
    );
    lillux::cas::sha256_hex(string_to_sign.as_bytes())
}

pub(crate) fn verify_request(
    state: &AppState,
    method: &str,
    uri: &axum::http::Uri,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Principal, String> {
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
    let forwarding_key_id = headers
        .get("x-ryeos-forwarding-key-id")
        .and_then(|v| v.to_str().ok());
    let forwarding_site_id = headers
        .get("x-ryeos-forwarding-site-id")
        .and_then(|v| v.to_str().ok());
    let forwarding_signature = headers
        .get("x-ryeos-forwarding-signature")
        .and_then(|v| v.to_str().ok());

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
    if auth_key.principal_class == AuthorizedKeyPrincipalClass::RemoteNode {
        let configured_operator = NodeIdentity::load(&state.config.operator_signing_key_path)
            .map_err(|error| format!("load configured operator identity: {error}"))?;
        if configured_operator.fingerprint() == fingerprint {
            return Err(
                "configured operator key cannot authenticate through a remote_node grant"
                    .to_string(),
            );
        }
    }

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

    let forwarding_headers_present = forwarding_key_id.is_some()
        || forwarding_site_id.is_some()
        || forwarding_signature.is_some();
    let authenticated_site_id = match auth_key.principal_class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            if forwarding_headers_present {
                return Err("local_client request cannot carry forwarding proof".to_string());
            }
            None
        }
        AuthorizedKeyPrincipalClass::RemoteNode => {
            if forwarding_headers_present {
                return Err(
                    "remote_node request cannot carry operator forwarding proof".to_string()
                );
            }
            auth_key.configured_origin_site_id.clone()
        }
        AuthorizedKeyPrincipalClass::RemoteOperator => {
            let (forwarding_key_id, forwarding_site_id, forwarding_signature) =
                match (forwarding_key_id, forwarding_site_id, forwarding_signature) {
                    (Some(key_id), Some(site_id), Some(signature))
                        if !key_id.is_empty() && !site_id.is_empty() && !signature.is_empty() =>
                    {
                        (key_id, site_id, signature)
                    }
                    _ => {
                        return Err(
                        "remote_operator request requires a complete source-node forwarding proof"
                            .to_string(),
                    );
                    }
                };
            ryeos_app::identity::validate_canonical_site_id(forwarding_site_id)
                .map_err(|error| error.to_string())?;
            if auth_key.configured_origin_site_id.as_deref() != Some(forwarding_site_id) {
                return Err(
                    "source-node forwarding site does not match the remote_operator grant"
                        .to_string(),
                );
            }
            let forwarding_fingerprint = forwarding_key_id
                .strip_prefix("fp:")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "invalid forwarding key ID format".to_string())?;
            let forwarding_key = load_authorized_key(
                forwarding_fingerprint,
                &state.config.authorized_keys_dir,
                &state.identity,
            )
            .map_err(|error| format!("untrusted forwarding node: {error}"))?;
            if forwarding_key.principal_class != AuthorizedKeyPrincipalClass::RemoteNode {
                return Err(
                    "forwarding proof must be signed by an admitted remote_node".to_string()
                );
            }
            if !forwarding_key
                .scopes
                .iter()
                .any(|scope| scope == ryeos_app::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE)
            {
                return Err(
                    "forwarding-node grant lacks forwarded-operator attestation authority"
                        .to_string(),
                );
            }
            if forwarding_key.configured_origin_site_id.as_deref() != Some(forwarding_site_id) {
                return Err(
                    "forwarding-node grant does not match the asserted source site".to_string(),
                );
            }
            let forwarding_sig_bytes = base64::engine::general_purpose::STANDARD
                .decode(forwarding_signature)
                .map_err(|_| "invalid forwarding signature encoding".to_string())?;
            let forwarding_sig = Signature::from_slice(&forwarding_sig_bytes)
                .map_err(|_| "invalid forwarding signature".to_string())?;
            let forwarding_content_hash = forwarding_request_content_hash(
                method,
                &canon,
                &body_hash,
                timestamp,
                nonce,
                &audience,
                key_id,
                signature,
                forwarding_key_id,
                forwarding_site_id,
            );
            forwarding_key
                .public_key
                .verify(forwarding_content_hash.as_bytes(), &forwarding_sig)
                .map_err(|_| "invalid forwarding signature".to_string())?;
            Some(forwarding_site_id.to_string())
        }
    };

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
        principal_class: auth_key.principal_class,
        authenticated_site_id,
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
    use ryeos_app::identity::NodeIdentity;

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
        let attacker_key_b64 =
            base64::engine::general_purpose::STANDARD.encode(attacker_vk.as_bytes());

        let toml_body = format!(
            "schema_version = 2\nprincipal_class = \"local_client\"\nfingerprint = \"{real_fp}\"\npublic_key = \"ed25519:{attacker_key_b64}\"\nscopes = [\"*\"]\nlabel = \"evil\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n"
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

    #[test]
    fn load_authorized_key_rejects_unversioned_grant() {
        let subject = SigningKey::from_bytes(&[4u8; 32]);
        let node_signer = SigningKey::from_bytes(&[5u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let body = format!(
            "fingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\nlabel = \"old\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        std::fs::write(auth_dir.join(format!("{fp}.toml")), signed).unwrap();

        let error = load_authorized_key(&fp, &auth_dir, &node_identity).unwrap_err();
        assert!(format!("{error:#}").contains("schema_version"));
    }

    /// Round-trip: write via canonical writer → load via auth loader → verify.
    #[test]
    fn canonical_writer_auth_loader_round_trip() {
        let client_key = SigningKey::from_bytes(&[42u8; 32]);
        let node_signer = SigningKey::from_bytes(&[99u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let client_key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());

        // Write via canonical writer using REAL handler-required caps
        // (long-form `ryeos.execute.service.<subject>`). Short-form
        // scopes like "remote.admin" would load fine but never
        // authorize a real handler — see authorizer.rs.
        let scopes = vec![
            "ryeos.execute.service.remote/admin".to_string(),
            "ryeos.execute.service.bundle/install".to_string(),
        ];
        let _path = ryeos_app::identity::write_authorized_key_toml(
            &auth_dir,
            &client_fp,
            &client_key_b64,
            &scopes,
            "test-round-trip",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &node_signer,
            ryeos_app::identity::WildcardPolicy::Reject,
        )
        .unwrap();

        // Load via auth loader
        let loaded = load_authorized_key(&client_fp, &auth_dir, &node_identity)
            .expect("auth loader should accept canonical writer output");

        // Verify contents
        assert_eq!(
            lillux::signature::compute_fingerprint(&loaded.public_key),
            client_fp,
            "loaded public key fingerprint must match"
        );
        assert_eq!(loaded.scopes, scopes, "loaded scopes must match");
        assert_eq!(loaded.owner, "test-round-trip");

        // Defense in depth: loaded scopes must actually satisfy a real
        // handler's required cap when fed to the authorizer.
        let authorizer = ryeos_runtime::authorizer::Authorizer::new();
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(
            "ryeos.execute.service.bundle/install",
        );
        authorizer
            .authorize(&loaded.scopes, &policy)
            .expect("real handler cap must be satisfied by loaded scopes");
    }

    #[test]
    fn remote_node_grant_round_trip_carries_authenticated_site() {
        let client_key = SigningKey::from_bytes(&[43u8; 32]);
        let node_signer = SigningKey::from_bytes(&[98u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());

        ryeos_app::identity::write_authorized_remote_node_key_toml(
            &auth_dir,
            &client_fp,
            &key_b64,
            &["ryeos.execute.graph.arc/noop".to_string()],
            "remote",
            "admission:test",
            "2026-01-01T00:00:00Z",
            "site:origin",
            &node_signer,
        )
        .unwrap();

        let loaded = load_authorized_key(&client_fp, &auth_dir, &node_identity).unwrap();
        assert_eq!(
            loaded.principal_class,
            AuthorizedKeyPrincipalClass::RemoteNode
        );
        assert_eq!(
            loaded.configured_origin_site_id.as_deref(),
            Some("site:origin")
        );
    }

    #[test]
    fn remote_operator_grant_rejects_missing_or_noncanonical_origin() {
        let client_key = SigningKey::from_bytes(&[44u8; 32]);
        let node_signer = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());
        let path = auth_dir.join(format!("{client_fp}.toml"));

        for origin_line in ["", "origin_site_id = \"source\"\n"] {
            let body = format!(
                "schema_version = 2\nprincipal_class = \"remote_operator\"\n{origin_line}fingerprint = \"{client_fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.remote/run\"]\nlabel = \"remote operator\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n"
            );
            let signed = lillux::signature::sign_content_at(
                &body,
                &node_signer,
                "#",
                None,
                "2026-01-01T00:00:00Z",
            );
            std::fs::write(&path, signed).unwrap();
            assert!(load_authorized_key(&client_fp, &auth_dir, &node_identity).is_err());
        }
    }

    #[test]
    fn directory_validation_rejects_unexpected_entries() {
        let node_signer = SigningKey::from_bytes(&[97u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("README"), "not a grant").unwrap();

        let error = validate_authorized_key_directory(&auth_dir, &node_identity).unwrap_err();
        assert!(error.to_string().contains("unexpected entry"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_validation_rejects_symlinked_grants() {
        use std::os::unix::fs::symlink;

        let node_signer = SigningKey::from_bytes(&[96u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "not trusted").unwrap();
        symlink(&outside, auth_dir.join("fp:linked.toml")).unwrap();

        let error = validate_authorized_key_directory(&auth_dir, &node_identity).unwrap_err();
        assert!(error.to_string().contains("non-regular entry"));
    }

    /// Regression: a legitimately node-signed TOML that uses
    /// short-form scopes (`bundle.install` instead of
    /// `ryeos.execute.service.bundle/install`) must be REJECTED at
    /// load time, not silently loaded with useless scopes.
    ///
    /// Without this guard the request authenticates, then every
    /// handler 403s with a misleading "missing capability" message —
    /// the operator never sees that their TOML is broken.
    #[test]
    fn load_authorized_key_rejects_short_form_scopes() {
        let subject = SigningKey::from_bytes(&[7u8; 32]);
        let node_signer = SigningKey::from_bytes(&[8u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        // Hand-craft a TOML with short-form scopes that bypass the
        // canonical writer (which would reject them at write time).
        let body = format!(
            "schema_version = 2\nprincipal_class = \"local_client\"\nfingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"bundle.install\", \"remote.admin\"]\nlabel = \"old-short-form\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        let file_path = auth_dir.join(format!("{fp}.toml"));
        std::fs::write(&file_path, signed).unwrap();

        // Loader must REFUSE to load this file — not load with empty
        // or short-form scopes and silently fail every authorization.
        let err = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect_err("short-form scope must be rejected at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonical") || msg.contains("ryeos."),
            "rejection message must mention canonical form, got: {msg}"
        );
        assert!(
            msg.contains("bundle.install") || msg.contains("not canonical"),
            "rejection message must point at the offending scope, got: {msg}"
        );
    }

    /// Canonical-form scopes load fine (positive control for the
    /// short-form regression above).
    #[test]
    fn load_authorized_key_accepts_canonical_scopes() {
        let subject = SigningKey::from_bytes(&[9u8; 32]);
        let node_signer = SigningKey::from_bytes(&[10u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let body = format!(
            "schema_version = 2\nprincipal_class = \"local_client\"\nfingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\nlabel = \"ok\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        std::fs::write(auth_dir.join(format!("{fp}.toml")), signed).unwrap();

        let loaded = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect("canonical scopes must load");
        assert_eq!(
            loaded.scopes,
            vec!["ryeos.execute.service.vault/list".to_string()]
        );
    }

    /// A legitimately node-signed key file whose BODY is modified after
    /// signing (here: an extra scope spliced into the array) must be
    /// rejected by the content-hash check — the original signature
    /// header no longer matches the body.
    #[test]
    fn authorized_key_rejects_tampered_body() {
        let subject = SigningKey::from_bytes(&[11u8; 32]);
        let node_signer = SigningKey::from_bytes(&[12u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        // Write a valid node-signed key file via the canonical writer.
        let file_path = ryeos_app::identity::write_authorized_key_toml(
            &auth_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "victim",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &node_signer,
            ryeos_app::identity::WildcardPolicy::Reject,
        )
        .unwrap();

        // Sanity: the untampered file loads.
        load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect("untampered canonical file must load");

        // Tamper: keep the original signature header, but escalate the
        // scopes in the body.
        let raw = std::fs::read_to_string(&file_path).unwrap();
        let (sig_line, body) = raw.split_once('\n').unwrap();
        let tampered_body = body.replace(
            "\"ryeos.execute.service.vault/list\"",
            "\"ryeos.execute.service.vault/list\", \"ryeos.execute.service.bundle/install\"",
        );
        assert_ne!(body, tampered_body, "tamper must actually change the body");
        std::fs::write(&file_path, format!("{sig_line}\n{tampered_body}")).unwrap();

        let err = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect_err("body tampered after signing must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tampered"),
            "error message should mention tampering, got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // verify_request: full request-verification path
    // ------------------------------------------------------------------

    /// Build a minimal AppState for verify_request tests. Mirrors the
    /// build_test_state helper in routes/invokers/none_invocation.rs.
    fn build_test_state() -> (TempDir, ryeos_app::state::AppState) {
        use std::sync::Arc;

        let tmpdir = TempDir::new().unwrap();
        let runtime_state_dir = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = ryeos_app::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            app_root: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: tmpdir.path().join("user-key.pem"),
            require_auth: false,
            authorized_keys_dir: tmpdir.path().join("auth"),
            tool_env_passthrough: Vec::new(),
            accounting_issue_acceptance_window_ms: 60_000,
        };
        let identity = ryeos_app::identity::NodeIdentity::create(&key_path).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
        let state_store = Arc::new(
            ryeos_app::state_store::StateStore::new_with_head_trust(
                tmpdir.path().to_path_buf(),
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier.clone(),
                Arc::new(head_trust),
            )
            .unwrap(),
        );
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ));
        let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
        let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
            state_store.clone(),
        ));
        let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
        let threads = Arc::new(
            ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
                "site:testhost",
            )
            .expect("valid test site identity"),
        );
        let commands = Arc::new(ryeos_app::command_service::CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));
        let snapshot = ryeos_app::node_config::NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            commands: vec![],
            hosted_node_policies: vec![],
            command_registration_policy: Default::default(),
            external_content_import_policy: None,
            persistent_session_policy: None,
        };
        let test_command_registry = Arc::new(
            ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap(),
        );
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());
        let state = ryeos_app::state::AppState {
            config: Arc::new(config),
            daemon_build: ryeos_app::build_info::get(),
            isolation: Arc::new(ryeos_engine::isolation::IsolationRuntime::default()),
            state_store,
            engine,
            resolution_cache: std::sync::Arc::new(ryeos_app::resolution_cache::ResolutionCache::new(128)),
            engine_cache: ryeos_app::engine_cache::EngineCache::new(
                ryeos_app::engine_cache::EngineCacheConfig::default(),
            ),
            identity: Arc::new(identity),
            threads,
            live_input: Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new()),
            events,
            event_streams,
            commands,
            callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
            thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
            extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
            write_barrier: Arc::new(write_barrier),
            started_at: std::time::Instant::now(),
            started_at_iso: String::new(),
            catalog_health: ryeos_app::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: Arc::new(crate::registry::build_service_registry()),
            service_descriptors: crate::handlers::ALL,
            node_config: Arc::new(snapshot.clone()),
            node_history_policy: Arc::new(
                ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::durable_without_config(),
            ),
            vault: Arc::new(ryeos_app::vault::EmptyVault),
            command_registry: test_command_registry,
            authorizer: test_auth,
            scheduler_db: Arc::new(ryeos_scheduler::db::SchedulerDb::new_in_memory().unwrap()),
            scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
            scheduler_reload_tx: None,
            ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
            vault_fingerprint: None,
            accounting: None,
            persistent_sessions: Arc::new(
                ryeos_app::persistent_session::PersistentSessionPool::new(),
            ),
        };
        (tmpdir, state)
    }

    /// Register `client_key` as an authorized principal via the
    /// canonical writer (signed by the node's own identity). Returns the
    /// client fingerprint.
    fn register_client_key(state: &ryeos_app::state::AppState, client_key: &SigningKey) -> String {
        let vk = client_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_app::identity::write_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "test-client",
            "test-granter",
            "2026-01-01T00:00:00Z",
            state.identity.signing_key(),
            ryeos_app::identity::WildcardPolicy::Reject,
        )
        .unwrap();
        fp
    }

    fn register_remote_operator_key(
        state: &ryeos_app::state::AppState,
        operator_key: &SigningKey,
        site_id: &str,
    ) -> String {
        let vk = operator_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_app::identity::reconcile_authorized_key_toml_scopes(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "test-remote-operator",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &state.identity,
            ryeos_app::identity::WildcardPolicy::Reject,
            false,
            Some(site_id),
            false,
        )
        .unwrap();
        fp
    }

    fn register_remote_node_key(
        state: &ryeos_app::state::AppState,
        node_key: &SigningKey,
        site_id: &str,
    ) -> String {
        let vk = node_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_app::identity::write_authorized_remote_node_key_toml(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &[ryeos_app::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_string()],
            "test-forwarding-node",
            "test-granter",
            "2026-01-01T00:00:00Z",
            site_id,
            state.identity.signing_key(),
        )
        .unwrap();
        fp
    }

    /// Client-side signing mirroring remote/client.rs `sign_request`:
    /// Signature = Ed25519(sk, sha256(canonical_string).as_bytes()).
    /// The canonical string is computed over `signed_uri`, which may
    /// differ from the URI later presented to verify_request.
    #[allow(clippy::too_many_arguments)]
    fn signed_headers(
        client_key: &SigningKey,
        fingerprint: &str,
        method: &str,
        signed_uri: &axum::http::Uri,
        body: &[u8],
        timestamp: u64,
        nonce: &str,
        audience: &str,
    ) -> axum::http::HeaderMap {
        let body_hash = lillux::cas::sha256_hex(body);
        let canon = canonical_path(signed_uri);
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
        let sig = lillux::crypto::Signer::sign(client_key, content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-ryeos-key-id",
            format!("fp:{fingerprint}").parse().unwrap(),
        );
        headers.insert("x-ryeos-timestamp", timestamp.to_string().parse().unwrap());
        headers.insert("x-ryeos-nonce", nonce.parse().unwrap());
        headers.insert("x-ryeos-signature", sig_b64.parse().unwrap());
        headers
    }

    #[allow(clippy::too_many_arguments)]
    fn add_forwarding_proof(
        headers: &mut axum::http::HeaderMap,
        forwarding_key: &SigningKey,
        forwarding_fingerprint: &str,
        forwarding_site_id: &str,
        method: &str,
        uri: &axum::http::Uri,
        body: &[u8],
        audience: &str,
    ) {
        let header = |name: &str| headers.get(name).unwrap().to_str().unwrap().to_string();
        let primary_key_id = header("x-ryeos-key-id");
        let timestamp = header("x-ryeos-timestamp");
        let nonce = header("x-ryeos-nonce");
        let primary_signature = header("x-ryeos-signature");
        let forwarding_key_id = format!("fp:{forwarding_fingerprint}");
        let content_hash = forwarding_request_content_hash(
            method,
            &canonical_path(uri),
            &lillux::cas::sha256_hex(body),
            &timestamp,
            &nonce,
            audience,
            &primary_key_id,
            &primary_signature,
            &forwarding_key_id,
            forwarding_site_id,
        );
        let signature = lillux::crypto::Signer::sign(forwarding_key, content_hash.as_bytes());
        headers.insert(
            "x-ryeos-forwarding-key-id",
            forwarding_key_id.parse().unwrap(),
        );
        headers.insert(
            "x-ryeos-forwarding-site-id",
            forwarding_site_id.parse().unwrap(),
        );
        headers.insert(
            "x-ryeos-forwarding-signature",
            base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes())
                .parse()
                .unwrap(),
        );
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A request whose timestamp is older than TIMESTAMP_MAX_AGE_SECS
    /// is rejected as expired; an otherwise-identical fresh request
    /// passes (positive control).
    #[test]
    fn replay_guard_rejects_stale_timestamp() {
        let (_tmp, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[21u8; 32]);
        let fp = register_client_key(&state, &client_key);
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/api/v1/status".parse().unwrap();

        // Stale: just past the freshness window.
        let stale_ts = unix_now() - (TIMESTAMP_MAX_AGE_SECS + 5);
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &uri,
            b"",
            stale_ts,
            "nonce-stale-timestamp-test",
            &audience,
        );
        let err = verify_request(&state, "GET", &uri, &headers, b"")
            .expect_err("stale timestamp must be rejected");
        assert!(
            err.contains("expired"),
            "stale-timestamp rejection should say expired, got: {err}"
        );

        // Fresh positive control: same key, same URI, current timestamp.
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &uri,
            b"",
            unix_now(),
            "nonce-fresh-timestamp-test",
            &audience,
        );
        let principal =
            verify_request(&state, "GET", &uri, &headers, b"").expect("fresh request must verify");
        assert_eq!(principal.fingerprint, fp);
    }

    /// Canonicalization sorts query params, so a signature computed
    /// over the sorted form verifies a request whose params arrive in
    /// a different textual order — while changing an actual param
    /// VALUE breaks the signature.
    #[test]
    fn signature_binds_query_param_order() {
        let (_tmp, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[22u8; 32]);
        let fp = register_client_key(&state, &client_key);
        let audience = state.identity.principal_id();

        let signed_uri: axum::http::Uri = "/api/v1/items?alpha=1&beta=2".parse().unwrap();
        let reordered_uri: axum::http::Uri = "/api/v1/items?beta=2&alpha=1".parse().unwrap();

        // Both spellings canonicalize identically.
        assert_eq!(canonical_path(&signed_uri), canonical_path(&reordered_uri));

        // Sign over the sorted form, present the reordered form: passes.
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &signed_uri,
            b"",
            unix_now(),
            "nonce-query-order-pass",
            &audience,
        );
        let principal = verify_request(&state, "GET", &reordered_uri, &headers, b"")
            .expect("reordered query params must still verify");
        assert_eq!(principal.fingerprint, fp);

        // Same signature, but an actually different param value: fails.
        let altered_uri: axum::http::Uri = "/api/v1/items?beta=3&alpha=1".parse().unwrap();
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &signed_uri,
            b"",
            unix_now(),
            "nonce-query-order-fail",
            &audience,
        );
        let err = verify_request(&state, "GET", &altered_uri, &headers, b"")
            .expect_err("altered query param value must fail verification");
        assert!(
            err.contains("invalid signature"),
            "value change should break the signature, got: {err}"
        );
    }

    #[test]
    fn remote_operator_requires_and_verifies_source_node_cosignature() {
        let (_tmp, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[23u8; 32]);
        let forwarding_key = SigningKey::from_bytes(&[24u8; 32]);
        let site_id = "site:source";
        let operator_fp = register_remote_operator_key(&state, &operator_key, site_id);
        let forwarding_fp = register_remote_node_key(&state, &forwarding_key, site_id);
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let body = br#"{"item_ref":"test"}"#;

        let headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-operator-missing-proof",
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, body)
            .expect_err("remote operator without a source-node proof must fail");
        assert!(error.contains("source-node forwarding proof"));

        let mut headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-operator-valid-proof",
            &audience,
        );
        add_forwarding_proof(
            &mut headers,
            &forwarding_key,
            &forwarding_fp,
            site_id,
            "POST",
            &uri,
            body,
            &audience,
        );
        let principal = verify_request(&state, "POST", &uri, &headers, body)
            .expect("co-signed remote operator request must verify");
        assert_eq!(
            principal.principal_class,
            AuthorizedKeyPrincipalClass::RemoteOperator
        );
        assert_eq!(principal.authenticated_site_id.as_deref(), Some(site_id));
    }

    #[test]
    fn remote_operator_rejects_forwarding_proof_from_wrong_site() {
        let (_tmp, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[25u8; 32]);
        let forwarding_key = SigningKey::from_bytes(&[26u8; 32]);
        let operator_fp =
            register_remote_operator_key(&state, &operator_key, "site:expected-source");
        let forwarding_fp =
            register_remote_node_key(&state, &forwarding_key, "site:different-source");
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let body = br#"{"item_ref":"test"}"#;
        let mut headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-operator-wrong-site",
            &audience,
        );
        add_forwarding_proof(
            &mut headers,
            &forwarding_key,
            &forwarding_fp,
            "site:different-source",
            "POST",
            &uri,
            body,
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, body)
            .expect_err("a different admitted site must not satisfy the operator grant");
        assert!(error.contains("does not match the remote_operator grant"));
    }

    #[test]
    fn configured_operator_key_is_never_accepted_as_remote_node() {
        let (_tmp, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[27u8; 32]);
        let pem = operator_key.to_pkcs8_pem(Default::default()).unwrap();
        std::fs::write(&state.config.operator_signing_key_path, pem.as_bytes()).unwrap();
        let operator_fp = register_remote_node_key(&state, &operator_key, "site:source");
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            b"{}",
            unix_now(),
            "nonce-configured-operator-remote-node",
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, b"{}")
            .expect_err("configured operator remote_node confusion must fail");
        assert!(error.contains("cannot authenticate through a remote_node grant"));
    }
}
