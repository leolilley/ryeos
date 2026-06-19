//! Authorize an HTTP client to call the daemon's authenticated endpoints.
//!
//! Writes a node-signed authorized-key TOML to
//! `<app_root>/.ai/node/auth/authorized_keys/<fp>.toml`.
//!
//! The daemon's auth loader reads these files at startup (and on hot-reload).
//! Each file must be signed by the node identity key.
//!
//! Delegates to the canonical `ryeos_app::identity::write_authorized_key_toml`
//! so there is exactly one TOML emitter.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use rand::RngCore;

use crate::actions::hosted_policy::load_hosted_policy;

/// Parameters for the authorize-client action.
pub struct AuthorizeClientParams {
    /// App root directory (contains `.ai/node/identity/`).
    pub app_root: PathBuf,
    /// Client public key as raw 32-byte Ed25519 verifying key.
    pub public_key: VerifyingKey,
    /// Scopes to grant (e.g. `["remote.admin", "bundle.install"]`).
    /// Pass `["*"]` only with `allow_wildcard: true`.
    pub scopes: Vec<String>,
    /// Human-readable label for the key file.
    pub label: String,
    /// Allow wildcard `"*"` in scopes. Should only be `true` for
    /// operator bootstrap.
    pub allow_wildcard: bool,
    /// When `true`, union `scopes` with any scopes already present in the
    /// existing authorized-key file for this fingerprint instead of
    /// replacing them. Mirrors the `--merge-scopes` CLI flag. Without it,
    /// the write replaces the scope set (and any dropped scope is reported
    /// in `AuthorizeClientResult::dropped_scopes`).
    pub merge: bool,
}

/// Result of a successful authorize-client run.
pub struct AuthorizeClientResult {
    /// Fingerprint of the authorized key.
    pub fingerprint: String,
    /// Path of the written TOML file.
    pub path: PathBuf,
    /// Scopes that existed on the prior authorized-key file but are NOT in
    /// the scope set just written. Empty when the file was new or when
    /// `merge` preserved everything. The caller should warn loudly if this
    /// is non-empty — it means an existing grant was narrowed.
    pub dropped_scopes: Vec<String>,
    /// Whether existing scopes were merged into the written set.
    pub merged: bool,
}

/// Reconcile a requested scope set against the scopes already on disk.
///
/// Returns `(final_scopes, dropped_scopes)`. With `merge`, the result is
/// `existing ∪ requested` (order-preserving) and nothing is dropped. Without
/// `merge`, the result is exactly `requested` and `dropped` lists the existing
/// scopes that are not being re-granted.
fn reconcile_scopes(
    existing: &[String],
    requested: &[String],
    merge: bool,
) -> (Vec<String>, Vec<String>) {
    if merge {
        let mut final_scopes = existing.to_vec();
        for s in requested {
            if !final_scopes.contains(s) {
                final_scopes.push(s.clone());
            }
        }
        (final_scopes, Vec::new())
    } else {
        let dropped = existing
            .iter()
            .filter(|s| !requested.contains(s))
            .cloned()
            .collect();
        (requested.to_vec(), dropped)
    }
}

/// Read the `scopes` array from an existing signed authorized-key TOML.
/// The signature line is a `#`-prefixed comment, so TOML parsing skips it.
/// Returns an empty vec if the file is absent or unparseable.
fn read_existing_scopes(entry_path: &Path) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Existing {
        #[serde(default)]
        scopes: Vec<String>,
    }
    std::fs::read_to_string(entry_path)
        .ok()
        .and_then(|c| toml::from_str::<Existing>(&c).ok())
        .map(|e| e.scopes)
        .unwrap_or_default()
}

pub struct MintAdmissionTokenParams {
    /// App root directory for the target node.
    pub app_root: PathBuf,
    /// Capabilities this one-time token is allowed to grant.
    pub scopes: Vec<String>,
    /// Optional default label for the eventual authorized-key entry.
    pub label: Option<String>,
    /// Token lifetime in seconds.
    pub ttl_secs: u64,
}

#[derive(serde::Serialize)]
pub struct MintAdmissionTokenResult {
    /// One-time bearer token. Show once to the local node being admitted.
    pub token: String,
    /// SHA-256 hash of `token`, used as the token file name.
    pub token_hash: String,
    /// Path of the target-node-local token file.
    pub path: PathBuf,
    /// Unix timestamp when the token was minted.
    pub issued_at_unix: u64,
    /// Original requested token lifetime in seconds.
    pub ttl_secs: u64,
    /// Unix expiry timestamp.
    pub expires_at_unix: u64,
    /// Scopes this token may grant.
    pub scopes: Vec<String>,
    /// Optional default label stored in the token file.
    pub label: Option<String>,
}

#[derive(serde::Serialize)]
struct AdmissionTokenFile<'a> {
    version: u32,
    token_hash: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    scopes: &'a [String],
    issued_at_unix: u64,
    ttl_secs: u64,
    expires_at_unix: u64,
}

/// Authorize a client by writing a node-signed authorized-key TOML.
///
/// Idempotent: if the file already exists with the same fingerprint,
/// it is overwritten with the new scopes/label.
///
/// Delegates to the canonical writer in `ryeos_app::identity` so the
/// TOML format is identical to what the daemon's own handler produces.
pub fn run_authorize_client(params: AuthorizeClientParams) -> Result<AuthorizeClientResult> {
    let node_key_path = params
        .app_root
        .join(".ai")
        .join("node")
        .join("identity")
        .join("private_key.pem");

    if !node_key_path.exists() {
        bail!(
            "node identity key not found at {} — run `ryeos init` first",
            node_key_path.display()
        );
    }

    let node_key = load_node_key(&node_key_path)?;

    let fp = lillux::crypto::fingerprint(&params.public_key);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(params.public_key.as_bytes());

    let auth_dir = params
        .app_root
        .join(".ai")
        .join("node")
        .join("auth")
        .join("authorized_keys");

    let now = lillux::time::iso8601_now();

    let wildcard = if params.allow_wildcard {
        ryeos_app::identity::WildcardPolicy::AllowBootstrap
    } else {
        ryeos_app::identity::WildcardPolicy::Reject
    };

    // `authorize-client` writes one file per fingerprint and replaces it
    // wholesale. To avoid silently narrowing an existing grant (the
    // operator-bootstrap footgun), reconcile against the scopes already on
    // disk: union them when `merge`, otherwise report what is being dropped.
    let entry_path = auth_dir.join(format!("{fp}.toml"));
    let existing_scopes = read_existing_scopes(&entry_path);
    let (final_scopes, dropped_scopes) =
        reconcile_scopes(&existing_scopes, &params.scopes, params.merge);

    let path = ryeos_app::identity::write_authorized_key_toml(
        &auth_dir,
        &fp,
        &key_b64,
        &final_scopes,
        &params.label,
        "cli-authorize-key",
        &now,
        &node_key,
        wildcard,
    )
    .context("failed to write authorized-key TOML")?;

    Ok(AuthorizeClientResult {
        fingerprint: fp,
        path,
        dropped_scopes,
        merged: params.merge,
    })
}

pub fn run_mint_admission_token(
    params: MintAdmissionTokenParams,
) -> Result<MintAdmissionTokenResult> {
    if params.ttl_secs == 0 {
        bail!("ttl_secs must be greater than zero");
    }
    if let Some(policy) = load_hosted_policy(&params.app_root)? {
        if params.ttl_secs > policy.admission.token_ttl_secs {
            bail!(
                "ttl_secs {} exceeds hosted-node policy maximum {} from {}",
                params.ttl_secs,
                policy.admission.token_ttl_secs,
                policy.source_file.display()
            );
        }
    }

    let mut scopes = params
        .scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        bail!("scopes must not be empty");
    }
    if scopes.iter().any(|scope| scope.contains('*')) {
        bail!("wildcard scopes are not allowed in admission tokens");
    }
    for scope in &scopes {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(|e| anyhow::anyhow!("invalid scope: {e}"))?;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at_unix = now
        .checked_add(params.ttl_secs)
        .ok_or_else(|| anyhow::anyhow!("ttl_secs overflows unix timestamp"))?;

    let mut token_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = lillux::cas::sha256_hex(token.as_bytes());
    let label = params.label.clone();
    let token_dir = params
        .app_root
        .join(".ai")
        .join("node")
        .join("admission")
        .join("tokens");
    std::fs::create_dir_all(&token_dir).with_context(|| {
        format!(
            "failed to create admission token dir {}",
            token_dir.display()
        )
    })?;
    let path = token_dir.join(format!("{token_hash}.toml"));
    let tmp = path.with_extension("tmp");

    let doc = toml::to_string(&AdmissionTokenFile {
        version: 1,
        token_hash: &token_hash,
        label: label.as_deref(),
        scopes: &scopes,
        issued_at_unix: now,
        ttl_secs: params.ttl_secs,
        expires_at_unix,
    })?;
    std::fs::write(&tmp, doc).with_context(|| {
        format!(
            "failed to write admission token temp file {}",
            tmp.display()
        )
    })?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to install admission token file {}", path.display()))?;

    Ok(MintAdmissionTokenResult {
        token,
        token_hash,
        path,
        issued_at_unix: now,
        ttl_secs: params.ttl_secs,
        expires_at_unix,
        scopes,
        label,
    })
}

/// Load the node signing key from a PKCS#8 PEM file.
fn load_node_key(path: &std::path::Path) -> Result<lillux::crypto::SigningKey> {
    lillux::crypto::load_signing_key(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct HostedPolicyFixture {
        _env_guard: MutexGuard<'static, ()>,
        _user: std::path::PathBuf,
        key: lillux::crypto::SigningKey,
    }

    impl HostedPolicyFixture {
        fn new(root: &std::path::Path) -> Self {
            let env_guard = ENV_MUTEX.lock().unwrap();
            let user = root.join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            std::env::set_var("RYEOS_APP_ROOT", &user);
            write_node_bootstrap(root, &trust_dir, &key);
            Self {
                _env_guard: env_guard,
                _user: user,
                key,
            }
        }
    }

    impl Drop for HostedPolicyFixture {
        fn drop(&mut self) {
            std::env::remove_var("RYEOS_APP_ROOT");
        }
    }

    fn write_hosted_policy(
        app_root: &std::path::Path,
        token_ttl_secs: u64,
        key: &lillux::crypto::SigningKey,
    ) {
        let path = app_root.join(".ai/node/hosted/policy.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!(
            r#"
version: "0.1.0"
schema_version: "1.0.0"
description: "test hosted policy"
transport:
  public_https_required: true
  loopback_http_allowed: true
admission:
  mode: "one_time_token"
  token_ttl_secs: {token_ttl_secs}
  reject_wildcard_scopes: true
  token_delivery: "out_of_band"
descriptor:
  require_live_identity_match: true
  advertised_capabilities: []
authorization:
  authority: "target_node_authorized_keys"
  central_bearer_tokens_allowed: false
  implicit_cross_node_authority_allowed: false
operations:
  audit_admission_events: true
  audit_grant_changes: true
  prefer_isolated_node_per_principal: true
  shared_daemon_multitenancy_enabled: false
"#
        );
        std::fs::write(path, lillux::signature::sign_content(&body, key, "#", None)).unwrap();
    }

    fn write_node_bootstrap(
        app_root: &std::path::Path,
        trust_dir: &std::path::Path,
        fallback_key: &lillux::crypto::SigningKey,
    ) {
        let app_trust_dir = app_root.join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&app_trust_dir).unwrap();
        ryeos_engine::trust::pin_key(&fallback_key.verifying_key(), "test", &app_trust_dir, None)
            .unwrap();

        let identity_dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let identity_path = identity_dir.join("private_key.pem");
        let node_identity = if identity_path.exists() {
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        } else {
            std::fs::write(
                &identity_path,
                fallback_key
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        };
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", trust_dir, None)
            .unwrap();
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", &app_trust_dir, None)
            .unwrap();

        let policy_dir = app_root.join(".ai/node/command_registration");
        std::fs::create_dir_all(&policy_dir).unwrap();
        let policy = r#"claim_rules:
  - claim:
      kind: command.root
      value: execute
    required_caps: []
system_source_caps:
  - ryeos.register.command.root.execute
"#;
        std::fs::write(
            policy_dir.join("default.yaml"),
            lillux::signature::sign_content(policy, node_identity.signing_key(), "#", None),
        )
        .unwrap();
    }

    #[test]
    fn reconcile_scopes_replace_reports_dropped() {
        let existing = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let requested = vec!["a".to_string(), "d".to_string()];
        let (final_scopes, dropped) = reconcile_scopes(&existing, &requested, false);
        assert_eq!(final_scopes, vec!["a".to_string(), "d".to_string()]);
        // b and c existed but were not re-granted.
        assert_eq!(dropped, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn reconcile_scopes_merge_unions_and_drops_nothing() {
        let existing = vec!["a".to_string(), "b".to_string()];
        let requested = vec!["b".to_string(), "c".to_string()];
        let (final_scopes, dropped) = reconcile_scopes(&existing, &requested, true);
        // existing order preserved, new appended, no duplicates.
        assert_eq!(
            final_scopes,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn reconcile_scopes_new_file_no_drops() {
        let (final_scopes, dropped) = reconcile_scopes(&[], &["x".to_string()], false);
        assert_eq!(final_scopes, vec!["x".to_string()]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn mint_admission_token_rejects_ttl_above_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), 60, &fixture.key);

        let err = match run_mint_admission_token(MintAdmissionTokenParams {
            app_root: tmp.path().to_path_buf(),
            scopes: vec!["ryeos.execute.service.threads".into()],
            label: None,
            ttl_secs: 600,
        }) {
            Ok(_) => panic!("minting should reject TTL above hosted policy"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("hosted-node policy maximum"),
            "got: {err:#}"
        );
    }
}
