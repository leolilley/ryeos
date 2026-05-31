//! Authorize an HTTP client to call the daemon's authenticated endpoints.
//!
//! Writes a node-signed authorized-key TOML to
//! `<system_space_dir>/.ai/node/auth/authorized_keys/<fp>.toml`.
//!
//! The daemon's auth loader reads these files at startup (and on hot-reload).
//! Each file must be signed by the node identity key.
//!
//! Delegates to the canonical `ryeos_app::identity::write_authorized_key_toml`
//! so there is exactly one TOML emitter.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use rand::RngCore;

/// Parameters for the authorize-client action.
pub struct AuthorizeClientParams {
    /// System space directory (contains `.ai/node/identity/`).
    pub system_space_dir: PathBuf,
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
}

/// Result of a successful authorize-client run.
pub struct AuthorizeClientResult {
    /// Fingerprint of the authorized key.
    pub fingerprint: String,
    /// Path of the written TOML file.
    pub path: PathBuf,
}

pub struct MintAdmissionTokenParams {
    /// System space directory for the target node.
    pub system_space_dir: PathBuf,
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
        .system_space_dir
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
        .system_space_dir
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

    let path = ryeos_app::identity::write_authorized_key_toml(
        &auth_dir,
        &fp,
        &key_b64,
        &params.scopes,
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
    })
}

pub fn run_mint_admission_token(
    params: MintAdmissionTokenParams,
) -> Result<MintAdmissionTokenResult> {
    if params.ttl_secs == 0 {
        bail!("ttl_secs must be greater than zero");
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
        .system_space_dir
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
        expires_at_unix,
        scopes,
        label,
    })
}

/// Load the node signing key from a PKCS#8 PEM file.
fn load_node_key(path: &std::path::Path) -> Result<lillux::crypto::SigningKey> {
    lillux::crypto::load_signing_key(path)
}
