//! Signing action — Ed25519 inline-comment signatures.
//!
//! Used by:
//! - `rye sign <path>` (direct subcommand)
//! - `rye dev resign <path>` (dev convenience)
//! - `rye dev build-bundle` (re-signs every YAML it touches)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::{DecodePrivateKey, SigningKey};

/// Sign (or re-sign) a file in place with an inline comment signature.
///
/// `key_path = None` → uses `RYE_SIGNING_KEY` env or
/// `~/.ai/config/keys/signing/private_key.pem` as fallback.
pub fn run_sign(input: &Path, key_path: Option<&Path>) -> Result<SignatureReport> {
    let body = fs::read_to_string(input)
        .with_context(|| format!("read {}", input.display()))?;

    let signing_key = load_signing_key(key_path)?;
    let fingerprint =
        lillux::sha256_hex(signing_key.verifying_key().as_bytes());

    let (prefix, suffix) = determine_envelope(input)?;

    // Strip any pre-existing signature lines — re-signing replaces.
    let stripped = lillux::signature::strip_signature_lines(&body);
    let signed = lillux::signature::sign_content(
        &stripped,
        &signing_key,
        &prefix,
        suffix.as_deref(),
    );

    // Atomic write
    let tmp = input.with_extension(format!(
        "signed.tmp.{}",
        std::process::id()
    ));
    fs::write(&tmp, &signed)
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, input)
        .with_context(|| format!("rename {} -> {}", tmp.display(), input.display()))?;

    let signature_line = extract_signature_line(&signed, &prefix)
        .unwrap_or_else(|| "signature applied".to_string());

    Ok(SignatureReport {
        file: input.display().to_string(),
        signer_fingerprint: fingerprint,
        signature_line,
        updated_at: lillux::time::iso8601_now(),
    })
}

#[derive(Debug, serde::Serialize)]
pub struct SignatureReport {
    pub file: String,
    pub signer_fingerprint: String,
    pub signature_line: String,
    pub updated_at: String,
}

fn load_signing_key(key_path: Option<&Path>) -> Result<SigningKey> {
    let path: PathBuf = match key_path {
        Some(p) => p.to_path_buf(),
        None => match std::env::var("RYE_SIGNING_KEY") {
            Ok(p) => PathBuf::from(p),
            Err(_) => {
                let home = std::env::var("HOME").context("HOME not set")?;
                Path::new(&home)
                    .join(".ai/config/keys/signing/private_key.pem")
            }
        },
    };

    let pem = fs::read_to_string(&path)
        .with_context(|| format!("read signing key from {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .context("parse signing key (Ed25519 PKCS8 PEM)")
}

fn determine_envelope(path: &Path) -> Result<(String, Option<String>)> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => Ok(("#".to_string(), None)),
        Some("json") => Ok(("//".to_string(), None)),
        Some("md") => Ok(("<!--".to_string(), Some("-->".to_string()))),
        Some("py") | Some("rb") | Some("sh") => {
            Ok(("#".to_string(), None))
        }
        _ => Ok(("#".to_string(), None)),
    }
}

fn extract_signature_line(content: &str, prefix: &str) -> Option<String> {
    let needle = format!("{prefix} rye:signed:");
    content
        .lines()
        .find(|l| l.starts_with(&needle))
        .map(|s| s.to_string())
}
