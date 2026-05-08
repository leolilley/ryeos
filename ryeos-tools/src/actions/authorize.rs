//! Authorize an HTTP client to call the daemon's authenticated endpoints.
//!
//! Writes a node-signed authorized-key TOML to
//! `<system_space_dir>/.ai/node/auth/authorized_keys/<fp>.toml`.
//!
//! The daemon's auth loader reads these files at startup (and on hot-reload).
//! Each file must be signed by the node identity key.

use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;

/// Parameters for the authorize-client action.
pub struct AuthorizeClientParams {
    /// System space directory (contains `.ai/node/identity/`).
    pub system_space_dir: std::path::PathBuf,
    /// Client public key as raw 32-byte Ed25519 verifying key.
    pub public_key: VerifyingKey,
    /// Scopes to grant (e.g. `["*"]`).
    pub scopes: Vec<String>,
    /// Human-readable label for the key file.
    pub label: String,
}

/// Result of a successful authorize-client run.
pub struct AuthorizeClientResult {
    /// Fingerprint of the authorized key.
    pub fingerprint: String,
    /// Path of the written TOML file.
    pub path: std::path::PathBuf,
}

/// Authorize a client by writing a node-signed authorized-key TOML.
///
/// Idempotent: if the file already exists with the same fingerprint,
/// it is overwritten with the new scopes/label.
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

    let scopes_str = if params.scopes.len() == 1 {
        format!("\"{}\"", params.scopes[0])
    } else {
        let items: Vec<String> = params.scopes.iter().map(|s| format!("\"{s}\"")).collect();
        format!("[{}]", items.join(", "))
    };

    let toml_body = format!(
        r#"fingerprint = "{fp}"
public_key = "ed25519:{key_b64}"
scopes = [{scopes_str}]
label = "{label}"
"#,
        label = params.label,
    );

    let signed = lillux::signature::sign_content(&toml_body, &node_key, "#", None);

    let auth_dir = params
        .system_space_dir
        .join(".ai")
        .join("node")
        .join("auth")
        .join("authorized_keys");
    std::fs::create_dir_all(&auth_dir)
        .with_context(|| format!("create {}", auth_dir.display()))?;

    let target = auth_dir.join(format!("{fp}.toml"));
    let tmp = target.with_extension("tmp");
    std::fs::write(&tmp, signed.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;

    Ok(AuthorizeClientResult {
        fingerprint: fp,
        path: target,
    })
}

/// Load the node signing key from a PKCS#8 PEM file.
fn load_node_key(path: &Path) -> Result<lillux::crypto::SigningKey> {
    lillux::crypto::load_signing_key(path)
}
