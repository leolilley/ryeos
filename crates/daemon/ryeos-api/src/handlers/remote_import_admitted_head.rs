//! `remote/import-admitted-head` — discover and import a verified remote admission head.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::{
    FederationHeadRemoteRecord, ObjectsClosureRequestOptions, RemoteClient,
};
use crate::remote::config;
use crate::remote::import::{self, VerifiedRemoteImportRequest};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DEFAULT_POLICY: &str = "local-node-v1";
const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default = "default_policy")]
    pub policy: String,
    #[serde(default)]
    pub subject_hash: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
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

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.policy.is_empty() {
        anyhow::bail!("admission policy must not be empty");
    }
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
    let expected_signer = lillux::crypto::fingerprint(&expected_key);
    let expected_issuer = format!("fp:{expected_signer}");
    if let Some(subject_hash) = req.subject_hash.as_deref() {
        validate_subject_hash(subject_hash)?;
    }
    let prefix = match req.subject_hash.as_deref() {
        Some(subject_hash) => format!("admissions/{}/{subject_hash}", req.policy),
        None => format!("admissions/{}", req.policy),
    };

    let heads = client
        .federation_heads_list(
            &prefix,
            Some(req.limit.min(500)),
            &expected_signer,
            &expected_key,
        )
        .await?;
    if heads.truncated && req.subject_hash.is_none() {
        anyhow::bail!(
            "remote returned a truncated admission head list for policy '{}'; pass subject_hash",
            req.policy
        );
    }
    let selected = select_admission_head(&heads.heads, &req.policy, req.subject_hash.as_deref())?;
    let selected_subject = admission_subject_from_head(selected, &req.policy)?;

    let result = import::import_admitted_root_with_job(
        &state,
        &client,
        VerifiedRemoteImportRequest {
            subject_hash: selected_subject.clone(),
            policy: req.policy.clone(),
            expected_issuer,
            expected_key,
            expected_attestation_hash: Some(selected.target_hash.clone()),
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
        "remote": remote_cfg.name,
        "head_ref": selected.ref_path,
        "head_attestation_hash": selected.target_hash,
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

fn select_admission_head<'a>(
    heads: &'a [FederationHeadRemoteRecord],
    policy: &str,
    subject_hash: Option<&str>,
) -> Result<&'a FederationHeadRemoteRecord> {
    let matches = heads
        .iter()
        .filter(|head| match admission_subject_from_head(head, policy) {
            Ok(subject) => subject_hash.map_or(true, |requested| requested == subject),
            Err(_) => false,
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [head] => Ok(*head),
        [] => match subject_hash {
            Some(subject) => anyhow::bail!(
                "remote has no admission head for policy '{}' subject '{}'",
                policy,
                subject,
            ),
            None => anyhow::bail!("remote has no admission heads for policy '{}'", policy),
        },
        _ => anyhow::bail!(
            "remote returned {} admission heads for policy '{}'; pass subject_hash to choose one",
            matches.len(),
            policy,
        ),
    }
}

fn admission_subject_from_head(head: &FederationHeadRemoteRecord, policy: &str) -> Result<String> {
    let expected_prefix = format!("admissions/{policy}/");
    let suffix = head
        .ref_path
        .strip_prefix(&expected_prefix)
        .and_then(|rest| rest.strip_suffix("/head"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "head {} is not an admission head for policy {}",
                head.ref_path,
                policy
            )
        })?;
    validate_subject_hash(suffix)
        .with_context(|| format!("admission head {} has invalid subject hash", head.ref_path))?;
    Ok(suffix.to_string())
}

fn validate_subject_hash(subject_hash: &str) -> Result<()> {
    if !lillux::valid_hash(subject_hash) || subject_hash.bytes().any(|b| b.is_ascii_uppercase()) {
        anyhow::bail!("invalid admission subject hash: {subject_hash}");
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/import-admitted-head",
    endpoint: "remote.import-admitted-head",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.import-admitted-head"],
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
    use crate::remote::client::RemoteSignedRef;

    fn head(subject: &str) -> FederationHeadRemoteRecord {
        let ref_path = format!("admissions/local-node-v1/{subject}/head");
        FederationHeadRemoteRecord {
            namespace: "admissions".into(),
            name: format!("local-node-v1/{subject}"),
            ref_path: ref_path.clone(),
            target_hash: "11".repeat(32),
            signer: "signer".into(),
            updated_at: "2026-05-30T00:00:00Z".into(),
            signed_ref: RemoteSignedRef {
                schema: 1,
                kind: "signed_ref".into(),
                ref_path,
                target_hash: "11".repeat(32),
                updated_at: "2026-05-30T00:00:00Z".into(),
                signer: "signer".into(),
                signature: "sig".into(),
            },
        }
    }

    #[test]
    fn select_admission_head_requires_subject_when_ambiguous() {
        let heads = vec![head(&"22".repeat(32)), head(&"33".repeat(32))];
        let err = select_admission_head(&heads, "local-node-v1", None).unwrap_err();
        assert!(err.to_string().contains("pass subject_hash"));
    }

    #[test]
    fn select_admission_head_filters_by_subject() {
        let wanted = "33".repeat(32);
        let heads = vec![head(&"22".repeat(32)), head(&wanted)];
        let selected = select_admission_head(&heads, "local-node-v1", Some(&wanted)).unwrap();
        assert_eq!(
            admission_subject_from_head(selected, "local-node-v1").unwrap(),
            wanted
        );
    }
}
