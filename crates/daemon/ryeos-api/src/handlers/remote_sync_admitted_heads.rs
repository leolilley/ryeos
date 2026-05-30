//! `remote/sync-admitted-heads` — mirror all discovered remote admission heads.

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
use ryeos_state::{
    CasEntryKind, CasEntryState, FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt,
    SyncJobAttemptState, SyncJobState, SyncJobUpdate,
};

const DEFAULT_POLICY: &str = "local-node-v1";
const DEFAULT_LIMIT: usize = 500;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default)]
    pub project: Option<PathBuf>,
    #[serde(default = "default_policy")]
    pub policy: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub max_imports: Option<usize>,
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

#[derive(Debug)]
struct SyncJobIds {
    job_id: String,
    attempt_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.policy.is_empty() {
        anyhow::bail!("admission policy must not be empty");
    }
    if req.limit == 0 || req.limit > 500 {
        anyhow::bail!("limit must be between 1 and 500");
    }
    if matches!(req.max_imports, Some(0)) {
        anyhow::bail!("max_imports must be greater than zero when supplied");
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
            remote_cfg.name, remote_cfg.name
        )
    })?;
    let expected_signer = lillux::crypto::fingerprint(&expected_key);
    let expected_issuer = format!("fp:{expected_signer}");
    let prefix = format!("admissions/{}", req.policy);

    let heads = client
        .federation_heads_list(&prefix, Some(req.limit), &expected_signer, &expected_key)
        .await?;
    if heads.truncated {
        anyhow::bail!("remote returned a truncated admission head list for policy '{}'; increase limit or sync a narrower policy shard", req.policy);
    }

    let candidates = heads
        .heads
        .iter()
        .map(|head| {
            let subject_hash = admission_subject_from_head(head, &req.policy)?;
            Ok((head, subject_hash))
        })
        .collect::<Result<Vec<_>>>()?;

    let missing = state.state_store.with_state_db(|db| {
        candidates
            .iter()
            .filter_map(|(head, subject_hash)| {
                match db.get_cas_entry(CasEntryKind::Object, &head.target_hash) {
                    Ok(Some(entry)) if entry.state == CasEntryState::Mirrored => None,
                    Ok(_) => Some(Ok((*head, subject_hash.clone()))),
                    Err(err) => Some(Err(err)),
                }
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let already_mirrored = candidates.len().saturating_sub(missing.len());
    let missing_count = missing.len();

    let to_import = match req.max_imports {
        Some(max) => missing.into_iter().take(max).collect::<Vec<_>>(),
        None => missing,
    };
    let deferred = missing_count.saturating_sub(to_import.len());

    let ids = create_batch_job(
        &state,
        &remote_cfg.name,
        to_import
            .iter()
            .map(|(_, subject)| subject.clone())
            .collect::<Vec<_>>(),
        to_import
            .iter()
            .map(|(head, _)| head.target_hash.clone())
            .collect::<Vec<_>>(),
    )?;

    let result = async {
        let mut imported = Vec::new();
        let mut total_imported = 0usize;
        let mut total_already_present = 0usize;
        let mut total_bytes_written = 0usize;
        let mut total_mirrored_objects = 0usize;
        let mut total_mirrored_blobs = 0usize;
        let mut fetched_hashes = Vec::new();

        for (head, subject_hash) in &to_import {
            let import = import::import_admitted_root(
                &state,
                &client,
                VerifiedRemoteImportRequest {
                    subject_hash: subject_hash.clone(),
                    policy: req.policy.clone(),
                    expected_issuer: expected_issuer.clone(),
                    expected_key: expected_key.clone(),
                    expected_attestation_hash: Some(head.target_hash.clone()),
                    source_peer: Some(remote_cfg.name.clone()),
                    job_id: Some(ids.job_id.clone()),
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
            total_imported = total_imported.saturating_add(import.imported);
            total_already_present = total_already_present.saturating_add(import.already_present);
            total_bytes_written = total_bytes_written.saturating_add(import.bytes_written);
            total_mirrored_objects = total_mirrored_objects.saturating_add(import.mirrored_objects);
            total_mirrored_blobs = total_mirrored_blobs.saturating_add(import.mirrored_blobs);
            fetched_hashes.push(import.subject_hash.clone());
            fetched_hashes.push(import.attestation_hash.clone());
            imported.push(serde_json::json!({
                "subject_hash": import.subject_hash,
                "attestation_hash": import.attestation_hash,
                "head_ref": head.ref_path,
                "imported": import.imported,
                "already_present": import.already_present,
                "bytes_written": import.bytes_written,
                "mirrored_objects": import.mirrored_objects,
                "mirrored_blobs": import.mirrored_blobs,
            }));
        }

        Ok::<_, anyhow::Error>(serde_json::json!({
            "job_id": ids.job_id,
            "attempt_id": ids.attempt_id,
            "remote": remote_cfg.name,
            "policy": req.policy,
            "listed": candidates.len(),
            "already_mirrored": already_mirrored,
            "deferred": deferred,
            "skipped": already_mirrored.saturating_add(deferred),
            "imported_heads": imported.len(),
            "imported": imported,
            "totals": {
                "imported": total_imported,
                "already_present": total_already_present,
                "bytes_written": total_bytes_written,
                "mirrored_objects": total_mirrored_objects,
                "mirrored_blobs": total_mirrored_blobs,
            },
            "fetched_hashes": fetched_hashes,
        }))
    }
    .await;

    match result {
        Ok(value) => {
            finish_batch_job(
                &state,
                &ids,
                SyncJobState::Completed,
                "completed",
                None,
                Some(value.clone()),
                value
                    .get("fetched_hashes")
                    .and_then(Value::as_array)
                    .map(|hashes| {
                        hashes
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            )?;
            Ok(value)
        }
        Err(err) => {
            let message = format!("{err:#}");
            let _ = finish_batch_job(
                &state,
                &ids,
                SyncJobState::Failed,
                "failed",
                Some(message.clone()),
                None,
                Vec::new(),
            );
            Err(err)
        }
    }
}

fn create_batch_job(
    state: &Arc<AppState>,
    peer: &str,
    roots: Vec<String>,
    heads: Vec<String>,
) -> Result<SyncJobIds> {
    let job_id = format!("remote-sync-admissions:{}", uuid::Uuid::new_v4());
    let attempt_id = format!("remote-sync-admissions-attempt:{}", uuid::Uuid::new_v4());
    state.state_store.with_state_db(|db| {
        db.create_sync_job(&NewSyncJob {
            job_id: job_id.clone(),
            operation_type: "remote_sync_admitted_heads".to_string(),
            peer: Some(peer.to_string()),
            roots: roots.clone(),
            heads: heads.clone(),
            max_attempts: 1,
        })?;
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-sync-admissions".to_string()),
            phase: "importing".to_string(),
        })?;
        db.update_sync_job(
            &job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "importing".to_string(),
                roots: Some(roots),
                heads: Some(heads),
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: None,
            },
        )?;
        Ok(())
    })?;
    Ok(SyncJobIds { job_id, attempt_id })
}

fn finish_batch_job(
    state: &Arc<AppState>,
    ids: &SyncJobIds,
    job_state: SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
    fetched_hashes: Vec<String>,
) -> Result<()> {
    let attempt_state = match job_state {
        SyncJobState::Completed => SyncJobAttemptState::Completed,
        SyncJobState::Failed => SyncJobAttemptState::Failed,
        SyncJobState::Cancelled => SyncJobAttemptState::Cancelled,
        _ => SyncJobAttemptState::Failed,
    };
    state.state_store.with_state_db(|db| {
        db.finish_sync_job_attempt_and_update_job(
            &ids.attempt_id,
            &FinishSyncJobAttempt {
                state: attempt_state,
                phase: phase.to_string(),
                error: error.clone(),
                result: result.clone(),
            },
            &ids.job_id,
            &SyncJobUpdate {
                state: job_state,
                phase: phase.to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes,
                last_error: error,
                result,
            },
        )
    })
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
    service_ref: "service:remote/sync-admitted-heads",
    endpoint: "remote.sync-admitted-heads",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote.sync-admitted-heads"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
