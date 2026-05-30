//! `remote/import-admitted-root` — verified remote CAS import.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::{ObjectsClosureRequestOptions, RemoteClient};
use crate::remote::config;
use crate::remote::import::{self, VerifiedRemoteImportRequest};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_POLICY: &str = "local-node-v1";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default)]
    pub project: Option<PathBuf>,
    pub subject_hash: String,
    #[serde(default = "default_policy")]
    pub policy: String,
    /// Optional override for the configured pinned remote Ed25519 signing key.
    /// When omitted, the remote config's pinned `signing_key` is used.
    #[serde(default)]
    pub expected_signing_key: Option<String>,
    #[serde(default)]
    pub max_objects: Option<usize>,
    #[serde(default)]
    pub max_blobs: Option<usize>,
    #[serde(default)]
    pub max_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_total_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_total_blob_bytes: Option<u64>,
    #[serde(default)]
    pub max_response_bytes: Option<u64>,
    #[serde(default)]
    pub max_links_per_object: Option<usize>,
}

fn default_remote() -> String {
    "default".to_string()
}

fn default_policy() -> String {
    DEFAULT_POLICY.to_string()
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let remote_cfg = {
        let remotes =
            config::load_remotes_layered(&state.config.system_space_dir, req.project.as_deref())?;
        config::get_remote(&remotes, &req.remote)?
    };
    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg);
    let expected_key = remote_cfg.pinned_signing_key().with_context(|| {
        format!(
            "remote '{}' has no valid pinned signing_key; run `ryeos remote configure --remote {}`",
            remote_cfg.name, remote_cfg.name,
        )
    })?;
    if let Some(input) = req.expected_signing_key.as_deref() {
        let asserted_key = decode_expected_signing_key(input)?;
        if asserted_key.as_bytes() != expected_key.as_bytes() {
            anyhow::bail!("expected_signing_key does not match pinned remote signing_key");
        }
    }
    let expected_issuer = format!("fp:{}", lillux::crypto::fingerprint(&expected_key));

    let result = import::import_admitted_root_with_job(
        &state,
        &client,
        VerifiedRemoteImportRequest {
            subject_hash: req.subject_hash,
            policy: req.policy,
            expected_issuer,
            expected_key,
            expected_attestation_hash: None,
            source_peer: Some(remote_cfg.name.clone()),
            job_id: None,
            closure_options: ObjectsClosureRequestOptions {
                max_objects: req.max_objects,
                max_blobs: req.max_blobs,
                max_object_bytes: req.max_object_bytes,
                max_total_object_bytes: req.max_total_object_bytes,
                max_blob_bytes: req.max_blob_bytes,
                max_total_blob_bytes: req.max_total_blob_bytes,
                max_response_bytes: req.max_response_bytes,
                max_links_per_object: req.max_links_per_object,
                allow_incomplete: false,
            },
        },
    )
    .await?;

    Ok(serde_json::json!({
        "job_id": result.job_id,
        "attempt_id": result.attempt_id,
        "subject_hash": result.import.subject_hash,
        "policy": result.import.policy,
        "attestation_hash": result.import.attestation_hash,
        "imported": result.import.imported,
        "already_present": result.import.already_present,
        "bytes_written": result.import.bytes_written,
        "mirrored_objects": result.import.mirrored_objects,
        "mirrored_blobs": result.import.mirrored_blobs,
    }))
}

fn decode_expected_signing_key(input: &str) -> Result<lillux::crypto::VerifyingKey> {
    config::decode_signing_key(input).context("invalid expected_signing_key")
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/import-admitted-root",
    endpoint: "remote.import-admitted-root",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.import-admitted-root"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn decode_expected_signing_key_requires_ed25519_prefix() {
        assert!(decode_expected_signing_key("bad").is_err());
    }

    #[test]
    fn decode_expected_signing_key_accepts_ed25519_base64() {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[9_u8; 32]);
        let encoded = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD
                .encode(signing_key.verifying_key().as_bytes())
        );
        let decoded = decode_expected_signing_key(&encoded).unwrap();
        assert_eq!(decoded.as_bytes(), signing_key.verifying_key().as_bytes());
    }
}
