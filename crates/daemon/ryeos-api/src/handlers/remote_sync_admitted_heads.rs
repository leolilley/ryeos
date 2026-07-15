//! `remote/sync-admitted-heads` — mirror all discovered remote admission heads.

use std::collections::BTreeSet;
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
const DEFAULT_MAX_IMPORTS: usize = 50;
const MAX_IMPORTS: usize = 500;

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
    if matches!(req.max_imports, Some(max) if max > MAX_IMPORTS) {
        anyhow::bail!("max_imports must be less than or equal to {MAX_IMPORTS}");
    }

    let remote_cfg = {
        let remotes = config::load_remotes_layered(&state.config.app_root, req.project.as_deref())?;
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
    let ids = create_batch_job(&state, &remote_cfg.name)?;
    let mut progress = BatchProgress::new();

    let result: Result<Value> = async {
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

        let mirrored_targets = state.state_store.with_state_db(|db| {
            let mut mirrored = BTreeSet::new();
            for (head, _) in &candidates {
                if db
                    .get_cas_entry(CasEntryKind::Object, &head.target_hash)?
                    .is_some_and(|entry| entry.state == CasEntryState::Mirrored)
                {
                    mirrored.insert(head.target_hash.clone());
                }
            }
            Ok(mirrored)
        })?;
        let missing = {
            let cas_read = state.acquire_cas_read()?;
            let mut missing = Vec::new();
            for (head, subject_hash) in &candidates {
                let complete_locally = if mirrored_targets.contains(&head.target_hash) {
                    let report =
                        ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                            cas_read.cas(),
                            [head.target_hash.clone()],
                            ryeos_state::object_closure::ObjectClosureLimits::unbounded_for_local_maintenance(),
                        )?;
                    report.is_complete()
                } else {
                    false
                };
                if !complete_locally {
                    missing.push((*head, subject_hash.clone()));
                }
            }
            // The durable Mirrored rows are GC roots. The read capability is
            // scoped to the closure decision and cannot cross remote I/O or
            // staged import work.
            missing
        };
        let already_mirrored = candidates.len().saturating_sub(missing.len());
        let missing_count = missing.len();
        let max_imports = req.max_imports.unwrap_or(DEFAULT_MAX_IMPORTS);

        let to_import = missing.into_iter().take(max_imports).collect::<Vec<_>>();
        let deferred = missing_count.saturating_sub(to_import.len());
        progress.record_discovery(candidates.len(), already_mirrored, deferred);
        update_batch_job_scope(
            &state,
            &ids,
            to_import
                .iter()
                .map(|(_, subject)| subject.clone())
                .collect::<Vec<_>>(),
            to_import
                .iter()
                .map(|(head, _)| head.target_hash.clone())
                .collect::<Vec<_>>(),
            "importing",
        )?;

        for (head, subject_hash) in &to_import {
            let import = import::import_admitted_root(
                &state,
                &client,
                VerifiedRemoteImportRequest {
                    subject_hash: subject_hash.clone(),
                    policy: req.policy.clone(),
                    expected_issuer: expected_issuer.clone(),
                    expected_key,
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
            progress.record_import(head, import);
        }

        Ok::<_, anyhow::Error>(progress.to_json(&ids, &remote_cfg.name, &req.policy, false, None))
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
                progress.fetched_hashes.clone(),
            )?;
            Ok(value)
        }
        Err(err) => {
            let message = format!("{err:#}");
            let result =
                progress.to_json(&ids, &remote_cfg.name, &req.policy, true, Some(&message));
            let _ = finish_batch_job(
                &state,
                &ids,
                SyncJobState::Failed,
                "failed",
                Some(message.clone()),
                Some(result),
                progress.fetched_hashes.clone(),
            );
            Err(err)
        }
    }
}

fn create_batch_job(state: &Arc<AppState>, peer: &str) -> Result<SyncJobIds> {
    let job_id = format!("remote-sync-admissions:{}", uuid::Uuid::new_v4());
    let attempt_id = format!("remote-sync-admissions-attempt:{}", uuid::Uuid::new_v4());
    state.state_store.with_state_db(|db| {
        db.create_sync_job(&NewSyncJob {
            job_id: job_id.clone(),
            operation_type: "remote_sync_admitted_heads".to_string(),
            peer: Some(peer.to_string()),
            roots: Vec::new(),
            heads: Vec::new(),
            max_attempts: 1,
        })?;
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-sync-admissions".to_string()),
            phase: "listing_heads".to_string(),
        })?;
        db.update_sync_job(
            &job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "listing_heads".to_string(),
                roots: Some(Vec::new()),
                heads: Some(Vec::new()),
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

fn update_batch_job_scope(
    state: &Arc<AppState>,
    ids: &SyncJobIds,
    roots: Vec<String>,
    heads: Vec<String>,
    phase: &str,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            &ids.job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: phase.to_string(),
                roots: Some(roots),
                heads: Some(heads),
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: None,
            },
        )
    })
}

#[derive(Debug, Default)]
struct BatchProgress {
    listed: usize,
    already_mirrored: usize,
    deferred: usize,
    imported: Vec<Value>,
    total_imported: usize,
    total_already_present: usize,
    total_bytes_written: usize,
    total_mirrored_objects: usize,
    total_mirrored_blobs: usize,
    fetched_hashes: Vec<String>,
}

impl BatchProgress {
    fn new() -> Self {
        Self::default()
    }

    fn record_discovery(&mut self, listed: usize, already_mirrored: usize, deferred: usize) {
        self.listed = listed;
        self.already_mirrored = already_mirrored;
        self.deferred = deferred;
    }

    fn record_import(
        &mut self,
        head: &FederationHeadRemoteRecord,
        import: import::VerifiedRemoteImportResult,
    ) {
        self.total_imported = self.total_imported.saturating_add(import.imported);
        self.total_already_present = self
            .total_already_present
            .saturating_add(import.already_present);
        self.total_bytes_written = self
            .total_bytes_written
            .saturating_add(import.bytes_written);
        self.total_mirrored_objects = self
            .total_mirrored_objects
            .saturating_add(import.mirrored_objects);
        self.total_mirrored_blobs = self
            .total_mirrored_blobs
            .saturating_add(import.mirrored_blobs);
        self.fetched_hashes.push(import.subject_hash.clone());
        self.fetched_hashes.push(import.attestation_hash.clone());
        self.imported.push(serde_json::json!({
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

    fn to_json(
        &self,
        ids: &SyncJobIds,
        remote: &str,
        policy: &str,
        partial: bool,
        error: Option<&str>,
    ) -> Value {
        serde_json::json!({
            "job_id": ids.job_id,
            "attempt_id": ids.attempt_id,
            "remote": remote,
            "policy": policy,
            "partial": partial,
            "error": error,
            "listed": self.listed,
            "already_mirrored": self.already_mirrored,
            "deferred": self.deferred,
            "skipped": self.already_mirrored.saturating_add(self.deferred),
            "imported_heads": self.imported.len(),
            "imported": self.imported,
            "totals": {
                "imported": self.total_imported,
                "already_present": self.total_already_present,
                "bytes_written": self.total_bytes_written,
                "mirrored_objects": self.total_mirrored_objects,
                "mirrored_blobs": self.total_mirrored_blobs,
            },
            "fetched_hashes": self.fetched_hashes,
        })
    }
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
    required_caps: &["ryeos.execute.service.remote/sync-admitted-heads"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
