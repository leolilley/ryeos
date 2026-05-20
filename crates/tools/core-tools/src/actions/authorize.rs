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

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;

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

/// Load the node signing key from a PKCS#8 PEM file.
fn load_node_key(path: &std::path::Path) -> Result<lillux::crypto::SigningKey> {
    lillux::crypto::load_signing_key(path)
}
