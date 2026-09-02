//! Explicit convergence of two configured-operator project HEADs.
//!
//! A normal remote push preserves the destination's project history and may
//! therefore leave the source and destination at different, equally valid DAG
//! generations. Portable execution requires one exact base on both sites.
//! This service creates a two-parent merge generation with an explicitly
//! selected content winner, publishes it remote-first, and then advances the
//! local operator HEAD. A durable sync job closes the crash gap between those
//! two publications.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use crate::remote::client::{ObjectsClosureRequestOptions, RemoteClient};
use crate::remote::config::{self, ProjectSyncScope, RemoteConfig};
use crate::remote::push::push_descendant_snapshot_with_session;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use ryeos_state::{
    CasEntryKind, CasEntryState, FinishSyncJobAttempt, NewCasEntryAttribution, NewSyncJob,
    NewSyncJobAttempt, SyncJobAttemptState, SyncJobRecord, SyncJobState, SyncJobUpdate,
};

const OPERATION_TYPE: &str = "remote_project_head_reconciliation";
const OPERATION_SCHEMA: &str = "ryeos.remote_project_head_reconciliation_operation.v1";
const PROGRESS_SCHEMA: &str = "ryeos.remote_project_head_reconciliation_progress.v1";
const RESPONSE_SCHEMA: &str = "ryeos.remote_project_head_reconciliation_result.v1";
const MAX_ATTEMPTS: u64 = 8;

fn default_remote() -> String {
    "default".to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationWinner {
    Local,
    Remote,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    pub expected_local_head: String,
    pub expected_remote_head: String,
    pub winner: ReconciliationWinner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationOperation {
    operation_type: String,
    schema: String,
    remote: String,
    remote_url: String,
    source_site_id: String,
    remote_site_id: String,
    remote_principal_id: String,
    local_project_path: String,
    remote_project_path: String,
    operator_fingerprint: String,
    operator_authority_digest: String,
    requested_origin_site_id: Option<String>,
    expected_local_head: String,
    expected_remote_head: String,
    winner: ReconciliationWinner,
    merge_created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconciliationProgress {
    schema: String,
    merge_snapshot_hash: Option<String>,
    latest_remote_staging_id: Option<String>,
    remote_published: bool,
    local_published: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    schema: String,
    job_id: String,
    remote: String,
    local_project_path: String,
    remote_project_path: String,
    previous_local_head: String,
    previous_remote_head: String,
    reconciled_head: String,
    winner: ReconciliationWinner,
    remote_published: bool,
    local_published: bool,
    idempotent: bool,
}

#[derive(Debug)]
struct AttemptFailure {
    error: anyhow::Error,
    retryable: bool,
}

impl AttemptFailure {
    fn retryable(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            retryable: true,
        }
    }

    fn permanent(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            retryable: false,
        }
    }
}

impl ReconciliationProgress {
    fn new() -> Self {
        Self {
            schema: PROGRESS_SCHEMA.to_owned(),
            ..Self::default()
        }
    }

    fn from_job(job: &SyncJobRecord) -> Result<Self> {
        match job.result.clone() {
            Some(value) => {
                let progress: Self = serde_json::from_value(value)
                    .context("parse retained project-head reconciliation progress")?;
                if progress.schema != PROGRESS_SCHEMA {
                    bail!("retained project-head reconciliation progress schema is not current");
                }
                if let Some(hash) = progress.merge_snapshot_hash.as_deref() {
                    validate_hash("retained merge snapshot", hash)?;
                }
                if (progress.remote_published || progress.local_published)
                    && progress.merge_snapshot_hash.is_none()
                {
                    bail!("published reconciliation progress has no merge generation");
                }
                if progress.local_published && !progress.remote_published {
                    bail!("local reconciliation publication precedes remote publication");
                }
                Ok(progress)
            }
            None => Ok(Self::new()),
        }
    }
}

impl ReconciliationOperation {
    fn validate(&self) -> Result<()> {
        if self.operation_type != OPERATION_TYPE || self.schema != OPERATION_SCHEMA {
            bail!("project-head reconciliation operation schema or type is not current");
        }
        for (label, value) in [
            ("remote name", self.remote.as_str()),
            ("remote URL", self.remote_url.as_str()),
            ("local project path", self.local_project_path.as_str()),
            ("remote project path", self.remote_project_path.as_str()),
            ("remote principal", self.remote_principal_id.as_str()),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                bail!("project-head reconciliation {label} is invalid");
            }
        }
        ryeos_app::identity::validate_canonical_site_id(&self.source_site_id)?;
        ryeos_app::identity::validate_canonical_site_id(&self.remote_site_id)?;
        if let Some(origin) = self.requested_origin_site_id.as_deref() {
            ryeos_app::identity::validate_canonical_site_id(origin)?;
        }
        if !Path::new(&self.local_project_path).is_absolute() {
            bail!("project-head reconciliation local project path is not absolute");
        }
        config::validate_remote_project_path(&self.remote_project_path)?;
        let remote_fingerprint = self
            .remote_principal_id
            .strip_prefix("fp:")
            .context("project-head reconciliation remote principal is not canonical")?;
        validate_hash("remote principal fingerprint", remote_fingerprint)?;
        for (label, hash) in [
            ("operator fingerprint", self.operator_fingerprint.as_str()),
            (
                "operator authority digest",
                self.operator_authority_digest.as_str(),
            ),
            ("expected local HEAD", self.expected_local_head.as_str()),
            ("expected remote HEAD", self.expected_remote_head.as_str()),
        ] {
            validate_hash(label, hash)?;
        }
        ryeos_state::parse_canonical_timestamp(&self.merge_created_at)
            .context("project-head reconciliation merge timestamp is not canonical")?;
        Ok(())
    }

    fn from_value(value: Value) -> Result<Self> {
        let operation: Self = serde_json::from_value(value)
            .context("parse retained project-head reconciliation operation")?;
        operation.validate()?;
        Ok(operation)
    }

    fn to_value(&self) -> Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    fn intent_value(&self) -> Value {
        json!({
            "schema": "ryeos.remote_project_head_reconciliation_intent.v1",
            "remote": self.remote,
            "remote_url": self.remote_url,
            "source_site_id": self.source_site_id,
            "remote_site_id": self.remote_site_id,
            "remote_principal_id": self.remote_principal_id,
            "local_project_path": self.local_project_path,
            "remote_project_path": self.remote_project_path,
            "operator_fingerprint": self.operator_fingerprint,
            "operator_authority_digest": self.operator_authority_digest,
            "requested_origin_site_id": self.requested_origin_site_id,
            "expected_local_head": self.expected_local_head,
            "expected_remote_head": self.expected_remote_head,
            "winner": self.winner,
        })
    }

    fn job_id(&self) -> Result<String> {
        Ok(format!(
            "remote-project-reconcile:{}",
            ryeos_state::objects::canonical_value_digest(&self.intent_value())?
        ))
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    validate_hash("expected local HEAD", &req.expected_local_head)?;
    validate_hash("expected remote HEAD", &req.expected_remote_head)?;
    let operator_fingerprint =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    let operator_authority_digest =
        ryeos_app::operator_authority::admitted_operator_authority_digest(
            &state,
            &operator_fingerprint,
        )?;
    let (local_project_path, remote, remote_project_path) =
        resolve_initial_route(&state, &req.remote, &req.project)?;
    let operation = ReconciliationOperation {
        operation_type: OPERATION_TYPE.to_owned(),
        schema: OPERATION_SCHEMA.to_owned(),
        remote: req.remote,
        remote_url: remote.url,
        source_site_id: state.threads.site_id().to_owned(),
        remote_site_id: remote.site_id,
        remote_principal_id: remote.principal_id,
        local_project_path,
        remote_project_path,
        operator_fingerprint,
        operator_authority_digest,
        requested_origin_site_id: ctx.authenticated_origin_site_id,
        expected_local_head: req.expected_local_head,
        expected_remote_head: req.expected_remote_head,
        winner: req.winner,
        merge_created_at: lillux::time::iso8601_now(),
    };
    operation.validate()?;
    // A completed/retryable operation is addressed by its authority tuple,
    // not by the now-advanced mutable HEAD. Only a genuinely new operation
    // performs this admission-time comparison.
    if state
        .state_store
        .with_state_db(|db| db.get_sync_job(&operation.job_id()?))?
        .is_none()
    {
        let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)?;
        let project_hash = lillux::sha256_hex(operation.local_project_path.as_bytes());
        let local_head = state
            .state_store
            .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
        if local_head.as_deref() != Some(operation.expected_local_head.as_str()) {
            bail!(
                "local project HEAD mismatch: expected {}, current is {:?}",
                operation.expected_local_head,
                local_head
            );
        }
    }
    let response = execute_operation(state, operation, true).await?;
    Ok(serde_json::to_value(response)?)
}

async fn execute_operation(
    state: Arc<AppState>,
    proposed: ReconciliationOperation,
    create_if_absent: bool,
) -> Result<Response> {
    let job_id = proposed.job_id()?;
    let job = match state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))?
    {
        Some(job) => job,
        None if create_if_absent => {
            let roots = sorted_hashes([
                proposed.expected_local_head.clone(),
                proposed.expected_remote_head.clone(),
            ]);
            match state.state_store.with_state_db(|db| {
                db.create_sync_job(&NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: OPERATION_TYPE.to_owned(),
                    operation: proposed.to_value()?,
                    peer: Some(proposed.remote.clone()),
                    roots,
                    heads: Vec::new(),
                    max_attempts: MAX_ATTEMPTS,
                })
            }) {
                Ok(job) => job,
                Err(create_error) => state
                    .state_store
                    .with_state_db(|db| db.get_sync_job(&job_id))?
                    .ok_or(create_error)?,
            }
        }
        None => bail!("durable project-head reconciliation job disappeared"),
    };
    let operation = ReconciliationOperation::from_value(job.operation.clone())?;
    if operation.intent_value() != proposed.intent_value() || operation.job_id()? != job_id {
        bail!("project-head reconciliation job identity is bound to another operation");
    }
    validate_job_binding(&job, &operation, &job_id)?;
    if job.state == SyncJobState::Completed {
        let mut response: Response = serde_json::from_value(
            job.result
                .clone()
                .context("completed project-head reconciliation has no result")?,
        )?;
        validate_response(&response, &operation, &job_id)?;
        validate_merge_generation(&state, &operation, &response.reconciled_head)?;
        validate_job_recovery_coordinates(
            &job,
            &operation,
            Some(response.reconciled_head.as_str()),
            true,
        )?;
        let principal = format!("fp:{}", operation.operator_fingerprint);
        let principal_key = ryeos_state::refs::principal_storage_key(&principal)?;
        let project_hash = lillux::sha256_hex(operation.local_project_path.as_bytes());
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
        let current = current.context(
            "completed reconciliation lost the configured operator's local project HEAD",
        )?;
        validate_project_generation(&state, &current)?;
        let authority = state.state_store.pinned_state_authority()?;
        let _guard = authority.acquire_shared_guard()?;
        if !super::project_apply_snapshot::snapshot_history_contains(
            &authority.cas_store()?,
            &current,
            &response.reconciled_head,
        )? {
            bail!(
                "current local project HEAD {current} no longer descends from completed reconciliation {}",
                response.reconciled_head
            );
        }
        response.idempotent = true;
        return Ok(response);
    }
    let mut progress = ReconciliationProgress::from_job(&job)?;
    validate_job_recovery_coordinates(
        &job,
        &operation,
        progress.merge_snapshot_hash.as_deref(),
        false,
    )?;
    if matches!(job.state, SyncJobState::Failed | SyncJobState::Cancelled) {
        bail!(
            "project-head reconciliation job {job_id} is terminal in state {}: {}",
            job.state.as_str(),
            job.last_error
                .as_deref()
                .unwrap_or("no retained diagnostic")
        );
    }
    if job.state == SyncJobState::Running {
        bail!("project-head reconciliation job {job_id} already has an active attempt");
    }

    let remote = match validate_current_route_and_authority(&state, &operation) {
        Ok(remote) => remote,
        Err(error) => {
            terminalize_without_attempt(&state, &job, "authority_changed", &error)?;
            return Err(error);
        }
    };
    let attempt_id = format!("remote-project-reconcile-attempt:{}", uuid::Uuid::new_v4());
    if let Err(error) = state.state_store.with_state_db(|db| {
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-project-reconcile".to_owned()),
            phase: "verifying_heads".to_owned(),
        })
    }) {
        terminalize_if_exhausted(&state, &job_id)?;
        return Err(error);
    }

    let result = run_attempt(
        Arc::clone(&state),
        &operation,
        &remote,
        &job_id,
        &mut progress,
    )
    .await;
    match result {
        Ok(mut response) => {
            let value = serde_json::to_value(&response)?;
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                AttemptSettlement {
                    job_state: SyncJobState::Completed,
                    phase: "completed",
                    error: None,
                    result: value,
                    heads: progress.merge_snapshot_hash.clone().into_iter().collect(),
                },
            )?;
            response.idempotent = false;
            Ok(response)
        }
        Err(failure) => {
            let message = bounded_error(&format!("{:#}", failure.error));
            let latest = state
                .state_store
                .with_state_db(|db| db.get_sync_job(&job_id))?
                .context("project-head reconciliation job disappeared during settlement")?;
            let retryable = failure.retryable && !latest.attempts_exhausted();
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                AttemptSettlement {
                    job_state: if retryable {
                        SyncJobState::Retryable
                    } else {
                        SyncJobState::Failed
                    },
                    phase: if retryable { "retryable" } else { "failed" },
                    error: Some(message),
                    result: serde_json::to_value(&progress)?,
                    heads: progress.merge_snapshot_hash.clone().into_iter().collect(),
                },
            )?;
            Err(failure.error)
        }
    }
}

async fn run_attempt(
    state: Arc<AppState>,
    operation: &ReconciliationOperation,
    remote: &RemoteConfig,
    job_id: &str,
    progress: &mut ReconciliationProgress,
) -> std::result::Result<Response, AttemptFailure> {
    let operator_client = RemoteClient::from_remote_cfg_as_retained_configured_operator(
        &state,
        remote,
        &operation.operator_fingerprint,
        &operation.operator_authority_digest,
    )
    .map_err(AttemptFailure::permanent)?;
    let node_client = RemoteClient::from_remote_cfg(&state, remote);
    let upload_session = operator_client
        .objects_put(None, &operation.remote_project_path, &[], &[])
        .await
        .map_err(AttemptFailure::retryable)?;
    progress.latest_remote_staging_id = Some(upload_session.staging_id.clone());
    update_progress(
        &state,
        job_id,
        "remote_head_observed",
        progress,
        &[],
        &[],
        &[],
    )
    .map_err(AttemptFailure::retryable)?;

    let observed_remote = upload_session
        .expected_previous_hash
        .as_deref()
        .ok_or_else(|| {
            AttemptFailure::permanent(anyhow::anyhow!(
                "remote project has no configured-operator HEAD"
            ))
        })?;
    let retained_merge = progress.merge_snapshot_hash.as_deref();
    if observed_remote != operation.expected_remote_head && retained_merge != Some(observed_remote)
    {
        return Err(AttemptFailure::permanent(anyhow::anyhow!(
            "remote project HEAD mismatch: expected {} or retained merge {:?}, current is {}",
            operation.expected_remote_head,
            retained_merge,
            observed_remote
        )));
    }

    ensure_remote_generation_local(&state, &node_client, remote, operation, job_id, progress)
        .await?;
    let merge_hash = match progress.merge_snapshot_hash.clone() {
        Some(hash) => {
            validate_merge_generation(&state, operation, &hash)
                .map_err(AttemptFailure::permanent)?;
            hash
        }
        None => create_and_retain_merge_generation(&state, operation, job_id, progress)?,
    };
    if observed_remote != operation.expected_remote_head && observed_remote != merge_hash {
        return Err(AttemptFailure::permanent(anyhow::anyhow!(
            "remote project HEAD changed after the retained merge was selected"
        )));
    }

    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(AttemptFailure::retryable)?;
    push_descendant_snapshot_with_session(
        &operator_client,
        &authority,
        &merge_hash,
        &operation.remote_project_path,
        upload_session,
    )
    .await
    .map_err(AttemptFailure::retryable)?;
    progress.remote_published = true;
    update_progress(
        &state,
        job_id,
        "remote_head_published",
        progress,
        std::slice::from_ref(&merge_hash),
        std::slice::from_ref(&merge_hash),
        &[],
    )
    .map_err(AttemptFailure::retryable)?;

    publish_local_head(&state, operation, &merge_hash)
        .await
        .map_err(AttemptFailure::permanent)?;
    progress.local_published = true;
    update_progress(
        &state,
        job_id,
        "local_head_published",
        progress,
        std::slice::from_ref(&merge_hash),
        &[],
        &[],
    )
    .map_err(AttemptFailure::retryable)?;
    promote_reconciled_closure(&state, &merge_hash).map_err(AttemptFailure::retryable)?;

    let response = Response {
        schema: RESPONSE_SCHEMA.to_owned(),
        job_id: job_id.to_owned(),
        remote: operation.remote.clone(),
        local_project_path: operation.local_project_path.clone(),
        remote_project_path: operation.remote_project_path.clone(),
        previous_local_head: operation.expected_local_head.clone(),
        previous_remote_head: operation.expected_remote_head.clone(),
        reconciled_head: merge_hash,
        winner: operation.winner,
        remote_published: true,
        local_published: true,
        idempotent: false,
    };
    validate_response(&response, operation, job_id).map_err(AttemptFailure::permanent)?;
    Ok(response)
}

async fn ensure_remote_generation_local(
    state: &Arc<AppState>,
    node_client: &RemoteClient,
    remote: &RemoteConfig,
    operation: &ReconciliationOperation,
    job_id: &str,
    progress: &ReconciliationProgress,
) -> std::result::Result<(), AttemptFailure> {
    if validate_project_generation(state, &operation.expected_remote_head).is_ok() {
        return Ok(());
    }
    let closure = node_client
        .objects_closure_get(
            std::slice::from_ref(&operation.expected_remote_head),
            ObjectsClosureRequestOptions {
                max_objects: Some(100_000),
                max_blobs: Some(100_000),
                max_object_bytes: Some(32 * 1024 * 1024),
                max_total_object_bytes: Some(512 * 1024 * 1024),
                max_blob_bytes: Some(512 * 1024 * 1024),
                max_total_blob_bytes: Some(1024 * 1024 * 1024),
                max_response_bytes: Some(1024 * 1024 * 1024),
                max_links_per_object: Some(100_000),
                allow_incomplete: false,
                allow_untransported_large_objects: false,
            },
        )
        .await
        .map_err(AttemptFailure::retryable)?;
    let payload = crate::remote::import::closure_response_to_export_payload(
        &format!("project-head-reconcile:{job_id}"),
        &operation.expected_remote_head,
        &closure.entries,
    )
    .map_err(AttemptFailure::permanent)?;
    if !payload
        .entries
        .iter()
        .any(|entry| !entry.is_blob && entry.hash == operation.expected_remote_head)
    {
        return Err(AttemptFailure::permanent(anyhow::anyhow!(
            "remote project closure omitted its exact HEAD root"
        )));
    }
    state
        .state_store
        .stage_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(remote.principal_id.clone()),
                source_peer: Some(operation.remote.clone()),
                job_id: Some(job_id.to_owned()),
            },
            job_id,
            "remote_generation_imported",
            std::slice::from_ref(&operation.expected_remote_head),
            Some(serde_json::to_value(progress).map_err(AttemptFailure::permanent)?),
        )
        .map_err(AttemptFailure::retryable)?;
    validate_project_generation(state, &operation.expected_remote_head)
        .map_err(AttemptFailure::permanent)?;
    Ok(())
}

fn create_and_retain_merge_generation(
    state: &AppState,
    operation: &ReconciliationOperation,
    job_id: &str,
    progress: &mut ReconciliationProgress,
) -> std::result::Result<String, AttemptFailure> {
    let local = validate_project_generation(state, &operation.expected_local_head)
        .map_err(AttemptFailure::permanent)?;
    let remote = validate_project_generation(state, &operation.expected_remote_head)
        .map_err(AttemptFailure::permanent)?;
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(AttemptFailure::retryable)?;
    let guard = authority
        .acquire_shared_guard()
        .map_err(AttemptFailure::retryable)?;
    let hash = if operation.expected_local_head == operation.expected_remote_head {
        operation.expected_local_head.clone()
    } else {
        let snapshot = build_merge_snapshot(operation, &local, &remote);
        let value = snapshot.to_value();
        ProjectSnapshot::from_value(&value).map_err(AttemptFailure::permanent)?;
        let _permit = state
            .write_barrier
            .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| {
                AttemptFailure::retryable(anyhow::anyhow!(
                    "cannot acquire reconciliation write permit: {error}"
                ))
            })?;
        let stored = authority
            .cas_store()
            .and_then(|cas| cas.put_object(&value))
            .map_err(AttemptFailure::retryable)?;
        let expected = ryeos_state::objects::canonical_value_digest(&value)
            .map_err(AttemptFailure::permanent)?;
        if stored.hash != expected {
            return Err(AttemptFailure::permanent(anyhow::anyhow!(
                "stored reconciliation generation digest changed"
            )));
        }
        state
            .state_store
            .with_state_db(|db| {
                db.record_cas_entry(&NewCasEntryAttribution {
                    hash: stored.hash.clone(),
                    entry_kind: CasEntryKind::Object,
                    bytes: u64::try_from(lillux::canonical_json(&value)?.len())?,
                    source_principal: Some(format!("fp:{}", operation.operator_fingerprint)),
                    source_peer: Some(operation.remote.clone()),
                    job_id: Some(operation.job_id()?),
                    state: CasEntryState::Local,
                })
            })
            .map_err(AttemptFailure::retryable)?;
        stored.hash
    };
    progress.merge_snapshot_hash = Some(hash.clone());
    // Keep the shared state authority from the CAS write through the durable
    // job-root update. A crash before this update leaves no retained merge
    // coordinate and deterministically recreates it; after the update, GC can
    // no longer collect the generation needed by recovery.
    update_progress(
        state,
        job_id,
        "merge_generation_ready",
        progress,
        std::slice::from_ref(&hash),
        &[],
        &[],
    )
    .map_err(AttemptFailure::retryable)?;
    drop(guard);
    Ok(hash)
}

fn build_merge_snapshot(
    operation: &ReconciliationOperation,
    local: &ProjectSnapshot,
    remote: &ProjectSnapshot,
) -> ProjectSnapshot {
    let winner = match operation.winner {
        ReconciliationWinner::Local => local,
        ReconciliationWinner::Remote => remote,
    };
    ProjectSnapshot {
        project_tree_hash: winner.project_tree_hash.clone(),
        effective_policy_hash: winner.effective_policy_hash.clone(),
        message: None,
        parent_hashes: sorted_hashes([
            operation.expected_local_head.clone(),
            operation.expected_remote_head.clone(),
        ]),
        created_at: operation.merge_created_at.clone(),
        source: "remote_project_head_reconcile".to_owned(),
    }
}

fn validate_merge_generation(
    state: &AppState,
    operation: &ReconciliationOperation,
    merge_hash: &str,
) -> Result<()> {
    let merge = validate_project_generation(state, merge_hash)?;
    if operation.expected_local_head == operation.expected_remote_head {
        if merge_hash != operation.expected_local_head {
            bail!("identical project heads retained a synthetic merge generation");
        }
        return Ok(());
    }
    let expected_parents = sorted_hashes([
        operation.expected_local_head.clone(),
        operation.expected_remote_head.clone(),
    ]);
    if merge.parent_hashes != expected_parents
        || merge.created_at != operation.merge_created_at
        || merge.source != "remote_project_head_reconcile"
        || merge.message.is_some()
    {
        bail!("retained merge generation contradicts its reconciliation operation");
    }
    let winner = validate_project_generation(
        state,
        match operation.winner {
            ReconciliationWinner::Local => &operation.expected_local_head,
            ReconciliationWinner::Remote => &operation.expected_remote_head,
        },
    )?;
    if merge.project_tree_hash != winner.project_tree_hash
        || merge.effective_policy_hash != winner.effective_policy_hash
    {
        bail!("retained merge generation does not preserve the selected winner content");
    }
    Ok(())
}

fn validate_project_generation(state: &AppState, snapshot_hash: &str) -> Result<ProjectSnapshot> {
    validate_hash("project snapshot", snapshot_hash)?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [snapshot_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    if !report.is_complete() || !report.large_object_hashes.is_empty() {
        bail!("project snapshot {snapshot_hash} has an incomplete local closure");
    }
    let value = cas
        .get_object(snapshot_hash)?
        .with_context(|| format!("project snapshot {snapshot_hash} is absent"))?;
    let snapshot = ProjectSnapshot::from_value(&value)?;
    let tree_value = cas
        .get_object(&snapshot.project_tree_hash)?
        .context("project snapshot tree is absent")?;
    let tree = ProjectTree::from_value(&tree_value)?;
    let policy_value = cas
        .get_object(&snapshot.effective_policy_hash)?
        .context("project snapshot policy is absent")?;
    let policy = ProjectSnapshotPolicy::from_value(&policy_value)?;
    if policy.sync_scope != ProjectSyncScope::FullProject {
        bail!("project-head reconciliation requires full_project generations");
    }
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;
    Ok(snapshot)
}

async fn publish_local_head(
    state: &AppState,
    operation: &ReconciliationOperation,
    merge_hash: &str,
) -> Result<()> {
    validate_merge_generation(state, operation, merge_hash)?;
    let principal = format!("fp:{}", operation.operator_fingerprint);
    let principal_key = ryeos_state::refs::principal_storage_key(&principal)?;
    let project_hash = lillux::sha256_hex(operation.local_project_path.as_bytes());
    let project_lock = super::project_apply_snapshot::project_apply_lock(&project_hash);
    let _project_guard = project_lock.lock_owned().await;
    let authority = state.state_store.pinned_state_authority()?;
    let cas_guard = authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire reconciliation write permit: {error}"))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let publication = state.state_store.with_state_db(|db| {
        let current = db.read_project_head(principal_key, &project_hash)?;
        if current.as_deref() == Some(merge_hash) {
            return Ok(());
        }
        if current.as_deref() != Some(operation.expected_local_head.as_str()) {
            bail!(
                "local project HEAD changed after remote reconciliation publication: expected {}, current is {:?}",
                operation.expected_local_head,
                current
            );
        }
        db.advance_project_head_ref(
            principal_key,
            &project_hash,
            merge_hash,
            &operation.expected_local_head,
            &signer,
            &cas_guard,
        )
    });
    if let Err(error) = publication {
        // A signed-ref replace may become visible before a parent-directory
        // durability error is returned. Resolve that ambiguity from the exact
        // verified HEAD rather than reporting a false split-brain result.
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
        if current.as_deref() == Some(merge_hash) {
            tracing::warn!(
                %error,
                reconciled_head = %merge_hash,
                "local project HEAD publication reported an error after the exact target became visible"
            );
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

fn promote_reconciled_closure(state: &AppState, merge_hash: &str) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &authority.cas_store()?,
        [merge_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    if !report.is_complete() {
        bail!("reconciled project closure became incomplete before promotion");
    }
    state.state_store.with_state_db(|db| {
        for hash in report.object_hashes {
            if db.get_cas_entry(CasEntryKind::Object, &hash)?.is_some() {
                db.set_cas_entry_state(CasEntryKind::Object, &hash, CasEntryState::Mirrored)?;
            }
        }
        for hash in report.blob_hashes {
            if db.get_cas_entry(CasEntryKind::Blob, &hash)?.is_some() {
                db.set_cas_entry_state(CasEntryKind::Blob, &hash, CasEntryState::Mirrored)?;
            }
        }
        Ok(())
    })
}

fn resolve_initial_route(
    state: &AppState,
    remote_name: &str,
    project: &Path,
) -> Result<(String, RemoteConfig, String)> {
    let canonical = config::canonical_local_project_path(project)?;
    let local_project_path = config::local_project_identity(&canonical)?.to_owned();
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&canonical))?;
    let loaded = config::get_loaded_remote(&report.remotes, remote_name)?;
    let binding = config::resolve_loaded_project_binding(&loaded, &canonical)?;
    if binding.sync_scope != ProjectSyncScope::FullProject {
        bail!("project-head reconciliation requires a full_project remote binding");
    }
    Ok((
        local_project_path,
        loaded.config,
        binding.remote_project_path,
    ))
}

fn validate_current_route_and_authority(
    state: &AppState,
    operation: &ReconciliationOperation,
) -> Result<RemoteConfig> {
    operation.validate()?;
    if state.threads.site_id() != operation.source_site_id {
        bail!("durable project-head reconciliation belongs to another source site");
    }
    let authority_digest = ryeos_app::operator_authority::admitted_operator_authority_digest(
        state,
        &operation.operator_fingerprint,
    )?;
    if authority_digest != operation.operator_authority_digest {
        bail!("durable project-head reconciliation operator grant changed");
    }
    let project = PathBuf::from(&operation.local_project_path);
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&project))?;
    let loaded = config::get_loaded_remote(&report.remotes, &operation.remote)?;
    if loaded.config.site_id != operation.remote_site_id
        || loaded.config.principal_id != operation.remote_principal_id
        || loaded.config.url != operation.remote_url
    {
        bail!("durable project-head reconciliation remote identity or endpoint changed");
    }
    let binding = config::resolve_loaded_project_binding(&loaded, &project)?;
    if binding.sync_scope != ProjectSyncScope::FullProject
        || binding.remote_project_path != operation.remote_project_path
    {
        bail!("durable project-head reconciliation route changed");
    }
    Ok(loaded.config)
}

fn update_progress(
    state: &AppState,
    job_id: &str,
    phase: &str,
    progress: &ReconciliationProgress,
    additional_roots: &[String],
    uploaded: &[String],
    fetched: &[String],
) -> Result<()> {
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("project-head reconciliation job disappeared")?;
    let mut roots = job.roots;
    roots.extend(additional_roots.iter().cloned());
    roots.sort();
    roots.dedup();
    let mut uploaded_hashes = job.uploaded_hashes;
    uploaded_hashes.extend(uploaded.iter().cloned());
    uploaded_hashes.sort();
    uploaded_hashes.dedup();
    let mut fetched_hashes = job.fetched_hashes;
    fetched_hashes.extend(fetched.iter().cloned());
    fetched_hashes.sort();
    fetched_hashes.dedup();
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: phase.to_owned(),
                roots: Some(roots),
                heads: None,
                uploaded_hashes,
                fetched_hashes,
                last_error: None,
                result: Some(serde_json::to_value(progress)?),
            },
        )
    })
}

struct AttemptSettlement {
    job_state: SyncJobState,
    phase: &'static str,
    error: Option<String>,
    result: Value,
    heads: Vec<String>,
}

fn settle_attempt(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    settlement: AttemptSettlement,
) -> Result<()> {
    let AttemptSettlement {
        job_state,
        phase,
        error,
        result,
        heads,
    } = settlement;
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("project-head reconciliation job disappeared")?;
    state.state_store.with_state_db(|db| {
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &FinishSyncJobAttempt {
                state: if job_state == SyncJobState::Completed {
                    SyncJobAttemptState::Completed
                } else {
                    SyncJobAttemptState::Failed
                },
                phase: phase.to_owned(),
                error: error.clone(),
                result: Some(result.clone()),
            },
            job_id,
            &SyncJobUpdate {
                state: job_state,
                phase: phase.to_owned(),
                roots: None,
                heads: Some(heads),
                uploaded_hashes: job.uploaded_hashes,
                fetched_hashes: job.fetched_hashes,
                last_error: error,
                result: Some(result),
            },
        )
    })
}

fn terminalize_without_attempt(
    state: &AppState,
    job: &SyncJobRecord,
    phase: &str,
    error: &anyhow::Error,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            &job.job_id,
            &SyncJobUpdate {
                state: SyncJobState::Failed,
                phase: phase.to_owned(),
                roots: None,
                heads: None,
                uploaded_hashes: job.uploaded_hashes.clone(),
                fetched_hashes: job.fetched_hashes.clone(),
                last_error: Some(bounded_error(&format!("{error:#}"))),
                result: job.result.clone(),
            },
        )
    })
}

fn terminalize_if_exhausted(state: &AppState, job_id: &str) -> Result<()> {
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("project-head reconciliation job disappeared")?;
    if job.state == SyncJobState::Retryable && job.attempts_exhausted() {
        state.state_store.with_state_db(|db| {
            db.update_sync_job(
                job_id,
                &SyncJobUpdate {
                    state: SyncJobState::Failed,
                    phase: "attempts_exhausted".to_owned(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: job.uploaded_hashes,
                    fetched_hashes: job.fetched_hashes,
                    last_error: Some(
                        "project-head reconciliation exhausted its admitted attempts".to_owned(),
                    ),
                    result: job.result,
                },
            )
        })?;
    }
    Ok(())
}

fn validate_response(
    response: &Response,
    operation: &ReconciliationOperation,
    job_id: &str,
) -> Result<()> {
    if response.schema != RESPONSE_SCHEMA
        || response.job_id != job_id
        || response.remote != operation.remote
        || response.local_project_path != operation.local_project_path
        || response.remote_project_path != operation.remote_project_path
        || response.previous_local_head != operation.expected_local_head
        || response.previous_remote_head != operation.expected_remote_head
        || response.winner != operation.winner
        || !response.remote_published
        || !response.local_published
    {
        bail!("completed project-head reconciliation result contradicts its operation");
    }
    validate_hash("reconciled project HEAD", &response.reconciled_head)
}

fn validate_job_binding(
    job: &SyncJobRecord,
    operation: &ReconciliationOperation,
    job_id: &str,
) -> Result<()> {
    if job.job_id != job_id
        || job.operation_type != OPERATION_TYPE
        || job.peer.as_deref() != Some(operation.remote.as_str())
        || job.max_attempts != MAX_ATTEMPTS
        || !job.attempt_count_is_valid()
    {
        bail!("project-head reconciliation job changed its retained authority binding");
    }
    Ok(())
}

fn validate_job_recovery_coordinates(
    job: &SyncJobRecord,
    operation: &ReconciliationOperation,
    merge_snapshot_hash: Option<&str>,
    require_published_head: bool,
) -> Result<()> {
    let mut expected_roots = sorted_hashes([
        operation.expected_local_head.clone(),
        operation.expected_remote_head.clone(),
    ]);
    if let Some(hash) = merge_snapshot_hash {
        validate_hash("retained merge snapshot", hash)?;
        expected_roots.push(hash.to_owned());
        expected_roots.sort();
        expected_roots.dedup();
    }
    if job.roots != expected_roots {
        bail!("project-head reconciliation job changed its exact recovery roots");
    }

    let expected_heads = merge_snapshot_hash
        .map(|hash| vec![hash.to_owned()])
        .unwrap_or_default();
    if (!job.heads.is_empty() || require_published_head) && job.heads != expected_heads {
        bail!("project-head reconciliation job changed its exact published heads");
    }
    Ok(())
}

fn sorted_hashes<const N: usize>(values: [String; N]) -> Vec<String> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn validate_hash(label: &str, value: &str) -> Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} is not a canonical SHA-256 digest");
    }
    Ok(())
}

fn bounded_error(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(2048)
        .collect::<String>();
    if result.trim().is_empty() {
        result = "project-head reconciliation failed".to_owned();
    }
    result
}

pub async fn recover_durable_project_head_reconciliations(state: &AppState) -> Result<usize> {
    let mut recovered = 0usize;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type_after(
                OPERATION_TYPE,
                after
                    .as_ref()
                    .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
                64,
            )
        })?;
        let Some(last) = jobs.last() else {
            break;
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            if job.state == SyncJobState::Running {
                continue;
            }
            let operation = match ReconciliationOperation::from_value(job.operation.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    terminalize_without_attempt(state, &job, "operation_invalid", &error)?;
                    continue;
                }
            };
            match execute_operation(Arc::new(state.clone()), operation, false).await {
                Ok(_) => recovered += 1,
                Err(error) => tracing::warn!(
                    job_id = %job.job_id,
                    %error,
                    "durable project-head reconciliation recovery did not complete"
                ),
            }
        }
        after = Some(next);
    }
    Ok(recovered)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/reconcile-project-head",
    endpoint: "remote.reconcile-project-head",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/reconcile-project-head"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn operation() -> ReconciliationOperation {
        ReconciliationOperation {
            operation_type: OPERATION_TYPE.to_owned(),
            schema: OPERATION_SCHEMA.to_owned(),
            remote: "hosted".to_owned(),
            remote_url: "https://hosted.example".to_owned(),
            source_site_id: "site:source".to_owned(),
            remote_site_id: "site:target".to_owned(),
            remote_principal_id: format!("fp:{}", "3".repeat(64)),
            local_project_path: "/project".to_owned(),
            remote_project_path: "/remote/project".to_owned(),
            operator_fingerprint: "4".repeat(64),
            operator_authority_digest: "5".repeat(64),
            requested_origin_site_id: None,
            expected_local_head: "a".repeat(64),
            expected_remote_head: "b".repeat(64),
            winner: ReconciliationWinner::Remote,
            merge_created_at: "2026-08-29T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn durable_identity_excludes_only_the_first_attempt_timestamp() {
        let left = operation();
        let mut right = left.clone();
        right.merge_created_at = "2026-08-29T00:00:01Z".to_owned();
        assert_eq!(left.job_id().unwrap(), right.job_id().unwrap());
        assert_ne!(left.to_value().unwrap(), right.to_value().unwrap());
        right.remote_url = "https://replacement.example".to_owned();
        assert_ne!(left.job_id().unwrap(), right.job_id().unwrap());
    }

    #[test]
    fn merge_parents_are_sorted_and_deduplicated() {
        assert_eq!(
            sorted_hashes(["b".repeat(64), "a".repeat(64)]),
            vec!["a".repeat(64), "b".repeat(64)]
        );
        assert_eq!(
            sorted_hashes(["a".repeat(64), "a".repeat(64)]),
            vec!["a".repeat(64)]
        );
    }

    #[test]
    fn merge_uses_only_the_explicit_winners_content() {
        let operation = operation();
        let local = ProjectSnapshot {
            project_tree_hash: "1".repeat(64),
            effective_policy_hash: "2".repeat(64),
            message: Some("local history".to_owned()),
            parent_hashes: vec![],
            created_at: "2026-08-28T00:00:00Z".to_owned(),
            source: "local".to_owned(),
        };
        let remote = ProjectSnapshot {
            project_tree_hash: "6".repeat(64),
            effective_policy_hash: "7".repeat(64),
            message: Some("remote history".to_owned()),
            parent_hashes: vec![],
            created_at: "2026-08-28T01:00:00Z".to_owned(),
            source: "remote".to_owned(),
        };
        let merge = build_merge_snapshot(&operation, &local, &remote);
        assert_eq!(merge.project_tree_hash, remote.project_tree_hash);
        assert_eq!(merge.effective_policy_hash, remote.effective_policy_hash);
        assert_eq!(merge.parent_hashes, vec!["a".repeat(64), "b".repeat(64)]);
        assert_eq!(merge.created_at, operation.merge_created_at);
        assert!(merge.message.is_none());
        assert_eq!(merge.source, "remote_project_head_reconcile");
    }

    #[test]
    fn request_contract_forbids_unknown_or_missing_authority_fields() {
        assert!(
            serde_json::from_value::<Request>(json!({
                "remote":"hosted",
                "project":"/project",
                "expected_local_head":"a".repeat(64),
                "expected_remote_head":"b".repeat(64),
                "winner":"remote",
                "force":true,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Request>(json!({
                "remote":"hosted",
                "project":"/project",
                "expected_local_head":"a".repeat(64),
                "winner":"remote",
            }))
            .is_err()
        );
    }

    #[test]
    fn retained_job_cannot_change_peer_budget_or_recovery_roots() {
        let operation = operation();
        let job_id = operation.job_id().unwrap();
        let mut job = SyncJobRecord {
            job_id: job_id.clone(),
            operation_type: OPERATION_TYPE.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some(operation.remote.clone()),
            state: SyncJobState::Planned,
            phase: "planned".to_owned(),
            roots: sorted_hashes([
                operation.expected_local_head.clone(),
                operation.expected_remote_head.clone(),
            ]),
            heads: Vec::new(),
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 0,
            max_attempts: MAX_ATTEMPTS,
            last_error: None,
            result: None,
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:00Z".to_owned(),
            finished_at: None,
        };
        validate_job_binding(&job, &operation, &job_id).unwrap();
        validate_job_recovery_coordinates(&job, &operation, None, false).unwrap();

        job.peer = Some("other".to_owned());
        assert!(validate_job_binding(&job, &operation, &job_id).is_err());
        job.peer = Some(operation.remote.clone());
        job.roots.push("c".repeat(64));
        job.roots.sort();
        assert!(validate_job_recovery_coordinates(&job, &operation, None, false).is_err());
    }

    #[test]
    fn completed_job_requires_the_exact_merge_as_its_only_head() {
        let operation = operation();
        let merge = "c".repeat(64);
        let mut job = SyncJobRecord {
            job_id: operation.job_id().unwrap(),
            operation_type: OPERATION_TYPE.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some(operation.remote.clone()),
            state: SyncJobState::Completed,
            phase: "completed".to_owned(),
            roots: sorted_hashes([
                operation.expected_local_head.clone(),
                operation.expected_remote_head.clone(),
                merge.clone(),
            ]),
            heads: vec![merge.clone()],
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 1,
            max_attempts: MAX_ATTEMPTS,
            last_error: None,
            result: None,
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:01Z".to_owned(),
            finished_at: Some("2026-08-29T00:00:01Z".to_owned()),
        };
        validate_job_recovery_coordinates(&job, &operation, Some(&merge), true).unwrap();
        job.heads.clear();
        assert!(validate_job_recovery_coordinates(&job, &operation, Some(&merge), true).is_err());
    }
}
