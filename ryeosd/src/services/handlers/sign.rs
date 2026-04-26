//! `sign` — sign (or re-sign) a file in place using the daemon's node key.
//!
//! Mirrors `ryeos_tools::actions::sign::run_sign` but uses the already-loaded
//! `state.identity` signing key instead of reading a key from disk. This is
//! the node-key signing path; user-key signing remains an offline CLI utility
//! in `ryeos_tools::actions::sign`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct Request {
    /// Absolute path to the file to sign.
    pub path: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct SignatureReport {
    pub file: String,
    pub signer_fingerprint: String,
    pub signature_line: String,
    pub updated_at: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = tokio::task::spawn_blocking(move || -> Result<SignatureReport> {
        sign_file_with_node_key(&req.path, &state)
    })
    .await??;
    serde_json::to_value(report).map_err(Into::into)
}

fn sign_file_with_node_key(path: &std::path::Path, state: &AppState) -> Result<SignatureReport> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;

    let signing_key = state.identity.signing_key();
    let fingerprint = state.identity.fingerprint().to_string();

    let (prefix, suffix) = determine_envelope(path);

    let stripped = lillux::signature::strip_signature_lines(&body);
    let signed = lillux::signature::sign_content(
        &stripped,
        signing_key,
        &prefix,
        suffix.as_deref(),
    );

    // Atomic write next to the file.
    let tmp = path.with_extension(format!("signed.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &signed)
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;

    let needle = format!("{prefix} rye:signed:");
    let signature_line = signed
        .lines()
        .find(|l| l.starts_with(&needle))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "signature applied".to_string());

    Ok(SignatureReport {
        file: path.display().to_string(),
        signer_fingerprint: fingerprint,
        signature_line,
        updated_at: lillux::time::iso8601_now(),
    })
}

fn determine_envelope(path: &std::path::Path) -> (String, Option<String>) {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => ("#".to_string(), None),
        Some("json") => ("//".to_string(), None),
        Some("md") => ("<!--".to_string(), Some("-->".to_string())),
        Some("py") | Some("rb") | Some("sh") => ("#".to_string(), None),
        _ => ("#".to_string(), None),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:sign",
    endpoint: "sign",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .map_err(|e| anyhow::anyhow!("invalid sign params: {e}"))?;
            handle(req, state).await
        })
    },
};
