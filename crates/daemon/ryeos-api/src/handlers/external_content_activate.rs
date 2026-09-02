//! Configured-operator activation of trusted external-content acquisition recipes.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Seek as _, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::managed_external_content::{
    ManagedActivationSource, ManagedMemberDisposition, ResolvedManagedExternalContentActivation,
};
use ryeos_app::managed_external_content_operation::{
    AcquisitionMode, MANAGED_ACTIVATION_OPERATION, ManagedActivationJobOperation,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const CACHE_ENTRY_LIMIT: usize = 4096;
const CACHE_RECONCILIATION_ENTRY_LIMIT: usize = 65536;
const ERROR_LIMIT: usize = 2048;
const MAX_MANAGED_TAR_EXTENSION_BYTES: u64 = ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES
    as u64
    + ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES
    + 512;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub activation_ref: String,
    pub mode: AcquisitionMode,
    #[serde(default)]
    pub offline_archive_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub job_id: String,
    pub activation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_hash: Option<String>,
    pub consumer_ref: String,
    pub state: String,
    pub idempotent: bool,
}

struct ActivationDirectories {
    cache: lillux::PinnedDirectory,
    staging: lillux::PinnedDirectory,
    job: lillux::PinnedDirectory,
    job_name: String,
    _lock: lillux::PinnedDirectoryLock,
}

#[derive(Debug)]
struct ImportedComponent {
    component_id: String,
    import: ryeos_app::operator_external_content::ImportResponse,
}

#[derive(Clone, Copy)]
struct ActivationReceiptAuthority<'a> {
    state_store: &'a ryeos_app::state_store::StateStore,
    write_barrier: &'a ryeos_app::write_barrier::WriteBarrier,
    node_fingerprint: &'a str,
}

impl<'a> ActivationReceiptAuthority<'a> {
    fn from_state(state: &'a AppState) -> Self {
        Self {
            state_store: &state.state_store,
            write_barrier: &state.write_barrier,
            node_fingerprint: state.identity.fingerprint(),
        }
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let operator = ryeos_app::operator_external_content::require_configured_operator(&state, &ctx)?;
    let activation = ryeos_app::managed_external_content::resolve_activation(
        &state,
        &req.activation_ref,
        req.mode,
    )?;
    let operator_authority_digest =
        ryeos_app::operator_external_content::configured_operator_authority_digest(
            &state, &operator,
        )?;
    let operation = ManagedActivationJobOperation::new(
        &activation,
        operator,
        operator_authority_digest,
        external_content_policy(&state)?,
        req.mode,
        req.offline_archive_root,
    )?;
    Ok(serde_json::to_value(submit_operation(
        state, activation, operation,
    )?)?)
}

struct SubmittedActivation {
    job: ryeos_state::SyncJobRecord,
    attempt_id: Option<String>,
    reused: bool,
}

fn reserve_activation_job(
    state_store: &ryeos_app::state_store::StateStore,
    job_id: &str,
    operation: &Value,
    max_attempts: u64,
) -> Result<(ryeos_state::SyncJobRecord, bool)> {
    state_store.with_state_db(|db| {
        let (job, reused) = match db.get_sync_job(job_id)? {
            Some(existing) => (existing, true),
            None => (
                db.create_sync_job(&ryeos_state::NewSyncJob {
                    job_id: job_id.to_owned(),
                    operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
                    operation: operation.clone(),
                    peer: None,
                    roots: Vec::new(),
                    heads: Vec::new(),
                    max_attempts,
                })?,
                false,
            ),
        };
        if job.operation != *operation {
            bail!("managed activation job id is retained for another canonical operation");
        }
        Ok((job, reused))
    })
}

fn claim_activation_attempt(
    state_store: &ryeos_app::state_store::StateStore,
    job_id: &str,
    operation: &Value,
    reused: bool,
) -> Result<SubmittedActivation> {
    state_store.with_state_db(|db| {
        let mut job = db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow::anyhow!("managed activation job disappeared before claim"))?;
        if job.operation != *operation {
            bail!("managed activation job changed before attempt claim");
        }
        let has_running_attempt = db
            .list_sync_job_attempts(job_id)?
            .iter()
            .any(|attempt| attempt.state == ryeos_state::SyncJobAttemptState::Running);
        let attempt_id = if matches!(
            job.state,
            ryeos_state::SyncJobState::Planned
                | ryeos_state::SyncJobState::Running
                | ryeos_state::SyncJobState::Retryable
        ) && !has_running_attempt
        {
            if job.attempts_exhausted() {
                db.update_sync_job(
                    job_id,
                    &ryeos_state::SyncJobUpdate {
                        state: ryeos_state::SyncJobState::Failed,
                        phase: "attempts_exhausted".to_owned(),
                        roots: None,
                        heads: None,
                        uploaded_hashes: job.uploaded_hashes.clone(),
                        fetched_hashes: job.fetched_hashes.clone(),
                        last_error: Some(
                            "managed activation exhausted its admitted attempts".to_owned(),
                        ),
                        result: job.result.clone(),
                    },
                )?;
                job = db
                    .get_sync_job(job_id)?
                    .ok_or_else(|| anyhow::anyhow!("terminalized activation disappeared"))?;
                None
            } else {
                let attempt_id = format!(
                    "external-content-activation-attempt:{}",
                    uuid::Uuid::new_v4()
                );
                db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
                    attempt_id: attempt_id.clone(),
                    job_id: job_id.to_owned(),
                    worker_id: Some("managed-external-content".to_owned()),
                    phase: "acquiring".to_owned(),
                })?;
                job = db
                    .get_sync_job(job_id)?
                    .ok_or_else(|| anyhow::anyhow!("claimed activation disappeared"))?;
                Some(attempt_id)
            }
        } else {
            None
        };
        Ok(SubmittedActivation {
            job,
            attempt_id,
            reused,
        })
    })
}

fn submit_operation(
    state: Arc<AppState>,
    activation: ResolvedManagedExternalContentActivation,
    operation: ManagedActivationJobOperation,
) -> Result<Response> {
    Ok(submit_operation_with_status(state, activation, operation)?.response)
}

struct ActivationSubmission {
    response: Response,
    attempt_started: bool,
}

/// Proof that the retained invocation still agrees with the node's current
/// configured-operator and external-content policy authority. Receipt folding
/// accepts this token instead of independently supplied activation/operation
/// values so no recovery caller can complete an old active job before the
/// revocation check.
struct CurrentActivationAuthority<'a> {
    activation: &'a ResolvedManagedExternalContentActivation,
    operation: &'a ManagedActivationJobOperation,
}

impl<'a> CurrentActivationAuthority<'a> {
    fn validate(
        state: &AppState,
        activation: &'a ResolvedManagedExternalContentActivation,
        operation: &'a ManagedActivationJobOperation,
    ) -> Result<Self> {
        let operator_authority_digest =
            ryeos_app::operator_external_content::configured_operator_authority_digest(
                state,
                &operation.operator_fingerprint,
            )?;
        operation.validate_current(
            activation,
            external_content_policy(state)?,
            &operator_authority_digest,
        )?;
        Ok(Self {
            activation,
            operation,
        })
    }
}

fn submit_operation_with_status(
    state: Arc<AppState>,
    activation: ResolvedManagedExternalContentActivation,
    operation: ManagedActivationJobOperation,
) -> Result<ActivationSubmission> {
    let operation_value = operation.to_value()?;
    let operation_digest = ryeos_state::objects::canonical_value_digest(&operation_value)?;
    let job_id = activation_job_id(&operation_digest);
    let max_attempts = managed_policy(&state)?.max_attempts;
    let (existing, reused) =
        reserve_activation_job(&state.state_store, &job_id, &operation_value, max_attempts)?;

    if existing.state == ryeos_state::SyncJobState::Completed {
        let mut response: Response = serde_json::from_value(
            existing
                .result
                .ok_or_else(|| anyhow::anyhow!("completed activation job has no result"))?,
        )?;
        validate_completed_activation(&state, &activation, &operation, &job_id, &response)?;
        response.idempotent = true;
        return Ok(ActivationSubmission {
            response,
            attempt_started: false,
        });
    }
    if matches!(
        existing.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        bail!(
            "managed activation job {} is terminal in state {}: {}",
            job_id,
            existing.state.as_str(),
            existing
                .last_error
                .as_deref()
                .unwrap_or("no retained diagnostic")
        );
    }

    let has_running_attempt = state.state_store.with_state_db(|db| {
        Ok(db
            .list_sync_job_attempts(&job_id)?
            .iter()
            .any(|attempt| attempt.state == ryeos_state::SyncJobAttemptState::Running))
    })?;
    if !has_running_attempt {
        let current = match CurrentActivationAuthority::validate(&state, &activation, &operation) {
            Ok(current) => current,
            Err(error) => {
                let detail = bounded_error(&format!(
                    "retained managed activation no longer matches current signed or node authority: {error:#}"
                ));
                state.state_store.with_state_db(|db| {
                    let latest = db.get_sync_job(&job_id)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "managed activation job disappeared before authority terminalization"
                        )
                    })?;
                    if latest.operation != operation_value {
                        bail!(
                            "managed activation operation changed before authority terminalization"
                        );
                    }
                    db.update_sync_job(
                        &job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Failed,
                            phase: "authority_changed".to_owned(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: latest.uploaded_hashes,
                            fetched_hashes: latest.fetched_hashes,
                            last_error: Some(detail),
                            result: latest.result,
                        },
                    )
                })?;
                return Err(error);
            }
        };
        if let Some(mut response) = complete_job_from_current_receipt(
            ActivationReceiptAuthority::from_state(&state),
            current,
            &job_id,
            &existing,
        )? {
            response.idempotent = true;
            return Ok(ActivationSubmission {
                response,
                attempt_started: false,
            });
        }
    }

    let submitted =
        claim_activation_attempt(&state.state_store, &job_id, &operation_value, reused)?;

    if submitted.job.state == ryeos_state::SyncJobState::Completed {
        let mut response: Response = serde_json::from_value(
            submitted
                .job
                .result
                .ok_or_else(|| anyhow::anyhow!("completed activation job has no result"))?,
        )?;
        validate_completed_activation(&state, &activation, &operation, &job_id, &response)?;
        response.idempotent = true;
        return Ok(ActivationSubmission {
            response,
            attempt_started: false,
        });
    }
    if matches!(
        submitted.job.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        bail!(
            "managed activation job {} is terminal in state {}: {}",
            job_id,
            submitted.job.state.as_str(),
            submitted
                .job
                .last_error
                .as_deref()
                .unwrap_or("no retained diagnostic")
        );
    }

    let response = Response {
        job_id: job_id.clone(),
        activation_id: operation.activation_id.clone(),
        receipt_hash: None,
        consumer_ref: operation.consumer_ref.clone(),
        state: submitted.job.state.as_str().to_owned(),
        idempotent: submitted.reused,
    };
    let attempt_started = submitted.attempt_id.is_some();
    if let Some(attempt_id) = submitted.attempt_id {
        let task_state = Arc::clone(&state);
        let task_job_id = job_id.clone();
        tokio::spawn(async move {
            if let Err(error) =
                execute_claimed_operation(task_state, activation, operation, attempt_id).await
            {
                tracing::warn!(
                    job_id = %task_job_id,
                    %error,
                    "submitted managed activation attempt did not complete"
                );
            }
        });
    }
    Ok(ActivationSubmission {
        response,
        attempt_started,
    })
}

async fn execute_claimed_operation(
    state: Arc<AppState>,
    activation: ResolvedManagedExternalContentActivation,
    operation: ManagedActivationJobOperation,
    attempt_id: String,
) -> Result<Response> {
    let operation_value = operation.to_value()?;
    let operation_digest = ryeos_state::objects::canonical_value_digest(&operation_value)?;
    let job_id = activation_job_id(&operation_digest);
    let existing = state.state_store.with_state_db(|db| {
        let job = db
            .get_sync_job(&job_id)?
            .ok_or_else(|| anyhow::anyhow!("claimed managed activation job disappeared"))?;
        if job.operation != operation_value {
            bail!("claimed managed activation operation changed");
        }
        let attempt = db
            .get_sync_job_attempt(&attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("claimed managed activation attempt disappeared"))?;
        if attempt.job_id != job_id
            || attempt.state != ryeos_state::SyncJobAttemptState::Running
            || job.state != ryeos_state::SyncJobState::Running
        {
            bail!("managed activation task no longer owns its exact running attempt");
        }
        Ok(job)
    })?;

    let current = match CurrentActivationAuthority::validate(&state, &activation, &operation) {
        Ok(current) => current,
        Err(error) => {
            let detail = bounded_error(&format!(
                "retained managed activation no longer matches current signed or node authority: {error:#}"
            ));
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Failed,
                "authority_changed",
                None,
                Some(detail),
                existing.result.clone(),
            )?;
            cleanup_retained_terminal_staging(&state, &job_id, &operation_value).await;
            return Err(error);
        }
    };

    // The realization head is durable authority independent of the local
    // attempt ledger. Fold an exact current receipt into this already-claimed
    // attempt before any acquisition contact. This closes the crash window
    // between head publication and attempt settlement; the submission path
    // performs the same fold before consuming an otherwise exhausted retry.
    if let Some(response) = complete_claimed_attempt_from_current_receipt(
        ActivationReceiptAuthority::from_state(&state),
        current,
        &job_id,
        &attempt_id,
        &existing,
    )? {
        cleanup_retained_terminal_staging(&state, &job_id, &operation_value).await;
        return Ok(response);
    }

    let directories = match tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || open_activation_directories(&state, &operation_digest)
    })
    .await
    .context("managed activation directory task panicked")?
    {
        Ok(directories) => directories,
        Err(error) => {
            let terminal = settle_retryable_attempt_failure(
                &state,
                &job_id,
                &attempt_id,
                bounded_error(&format!("{error:#}")),
            )?;
            if terminal {
                cleanup_retained_terminal_staging(&state, &job_id, &operation_value).await;
            }
            return Err(error);
        }
    };

    let run = run_attempt(Arc::clone(&state), &activation, &operation, &directories).await;
    match run {
        Ok(publication) => {
            let response = Response {
                job_id: job_id.clone(),
                activation_id: publication.activation_id,
                receipt_hash: Some(publication.receipt_hash.clone()),
                consumer_ref: activation.document.consumer_ref,
                state: "completed".to_owned(),
                idempotent: publication.idempotent,
            };
            let result = serde_json::to_value(&response)?;
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Completed,
                "completed",
                Some(publication.receipt_hash),
                None,
                Some(result),
            )?;
            // The receipt is authoritative before cleanup. A crash or bounded
            // cleanup failure leaves only rebuildable staging; the recovery
            // sweep removes terminal/orphaned job trees by durable job ID.
            cleanup_terminal_staging(directories, &job_id).await;
            Ok(response)
        }
        Err(error) => {
            let detail = bounded_error(&format!("{error:#}"));
            let terminal = settle_retryable_attempt_failure(&state, &job_id, &attempt_id, detail)?;
            if terminal {
                cleanup_terminal_staging(directories, &job_id).await;
            }
            Err(error)
        }
    }
}

fn activation_job_id(operation_digest: &str) -> String {
    format!("external-activation:{operation_digest}")
}

fn validate_completed_activation(
    state: &AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    job_id: &str,
    response: &Response,
) -> Result<()> {
    validate_completed_activation_with_authority(
        ActivationReceiptAuthority::from_state(state),
        activation,
        operation,
        job_id,
        response,
    )
}

fn validate_completed_activation_with_authority(
    authority: ActivationReceiptAuthority<'_>,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    job_id: &str,
    response: &Response,
) -> Result<()> {
    let Some(receipt_hash) = response.receipt_hash.as_deref() else {
        bail!("completed managed activation result has no receipt hash");
    };
    if response.job_id != job_id
        || response.activation_id != operation.activation_id
        || response.consumer_ref != activation.document.consumer_ref
        || response.state != "completed"
        || !lillux::valid_hash(receipt_hash)
    {
        bail!("completed managed activation result contradicts its durable operation");
    }
    let state_authority = authority.state_store.pinned_state_authority()?;
    let guard = state_authority.acquire_shared_guard()?;
    let _permit = authority
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot verify completed activation under write barrier: {error}")
        })?;
    let cas = state_authority.cas_store()?;
    let head = authority
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE,
                &operation.activation_id,
            )
        })?
        .ok_or_else(|| anyhow::anyhow!("completed managed activation head is absent"))?;
    if head.target_hash != receipt_hash {
        bail!("completed managed activation result is not the current receipt head");
    }
    let receipt_value = cas
        .get_object(&head.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("completed managed activation receipt is absent"))?;
    let receipt =
        ryeos_state::objects::ExternalContentActivationReceipt::from_value(&receipt_value)?;
    if receipt.activation_id != operation.activation_id
        || receipt.activation_ref != activation.activation_ref
        || receipt.activation_program_digest != activation.activation_program_digest
        || receipt.consumer_ref != activation.document.consumer_ref
        || receipt.publisher_fingerprint != activation.publisher_fingerprint
        || receipt.node_fingerprint != authority.node_fingerprint
    {
        bail!("completed managed activation receipt contradicts current exact authority");
    }
    let receipt_components = receipt
        .components
        .iter()
        .map(|component| component.id.as_str())
        .collect::<BTreeSet<_>>();
    let expected_components = activation
        .components
        .iter()
        .map(|component| component.recipe.id.as_str())
        .collect::<BTreeSet<_>>();
    if receipt_components != expected_components {
        bail!("completed managed activation receipt has a different component set");
    }
    for component in &activation.components {
        let binding = ryeos_app::operator_external_content::require_active_binding_from_store(
            authority.state_store,
            &cas,
            &component.expected_manifest_hash,
            &activation.document.consumer_ref,
            &activation.publisher_fingerprint,
        )
        .with_context(|| {
            format!(
                "completed managed activation component `{}` is no longer active",
                component.recipe.id
            )
        })?;
        let receipt_component = receipt
            .components
            .iter()
            .find(|candidate| candidate.id == component.recipe.id)
            .expect("component set equality checked above");
        let binding_hash = ryeos_state::objects::canonical_value_digest(&binding.to_value()?)?;
        if binding_hash != receipt_component.binding_hash
            || binding.manifest_kind != component.expected_manifest_kind
        {
            bail!(
                "completed managed activation component `{}` contradicts its receipt or consumer storage grant",
                component.recipe.id
            );
        }
    }
    state_authority.ensure_guard(&guard)?;
    Ok(())
}

fn complete_job_from_current_receipt(
    authority: ActivationReceiptAuthority<'_>,
    current: CurrentActivationAuthority<'_>,
    job_id: &str,
    existing: &ryeos_state::SyncJobRecord,
) -> Result<Option<Response>> {
    let activation = current.activation;
    let operation = current.operation;
    let Some(head) = authority.state_store.with_state_db(|db| {
        db.read_generic_head_ref(
            ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE,
            &operation.activation_id,
        )
    })?
    else {
        return Ok(None);
    };
    let response = Response {
        job_id: job_id.to_owned(),
        activation_id: operation.activation_id.clone(),
        receipt_hash: Some(head.target_hash.clone()),
        consumer_ref: activation.document.consumer_ref.clone(),
        state: "completed".to_owned(),
        idempotent: true,
    };
    validate_completed_activation_with_authority(
        authority, activation, operation, job_id, &response,
    )?;
    let result = serde_json::to_value(&response)?;
    authority.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Completed,
                phase: "completed_from_authoritative_receipt".to_owned(),
                roots: Some(vec![head.target_hash]),
                heads: None,
                uploaded_hashes: existing.uploaded_hashes.clone(),
                fetched_hashes: existing.fetched_hashes.clone(),
                last_error: None,
                result: Some(result),
            },
        )
    })?;
    Ok(Some(response))
}

fn complete_claimed_attempt_from_current_receipt(
    authority: ActivationReceiptAuthority<'_>,
    current: CurrentActivationAuthority<'_>,
    job_id: &str,
    attempt_id: &str,
    existing: &ryeos_state::SyncJobRecord,
) -> Result<Option<Response>> {
    let activation = current.activation;
    let operation = current.operation;
    let Some(head) = authority.state_store.with_state_db(|db| {
        db.read_generic_head_ref(
            ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE,
            &operation.activation_id,
        )
    })?
    else {
        return Ok(None);
    };
    let response = Response {
        job_id: job_id.to_owned(),
        activation_id: operation.activation_id.clone(),
        receipt_hash: Some(head.target_hash.clone()),
        consumer_ref: activation.document.consumer_ref.clone(),
        state: "completed".to_owned(),
        idempotent: true,
    };
    validate_completed_activation_with_authority(
        authority, activation, operation, job_id, &response,
    )?;
    let result = serde_json::to_value(&response)?;
    authority.state_store.with_state_db(|db| {
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &ryeos_state::FinishSyncJobAttempt {
                state: ryeos_state::SyncJobAttemptState::Completed,
                phase: "completed_from_authoritative_receipt".to_owned(),
                error: None,
                result: Some(result.clone()),
            },
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Completed,
                phase: "completed_from_authoritative_receipt".to_owned(),
                roots: Some(vec![head.target_hash]),
                heads: None,
                uploaded_hashes: existing.uploaded_hashes.clone(),
                fetched_hashes: existing.fetched_hashes.clone(),
                last_error: None,
                result: Some(result),
            },
        )
    })?;
    Ok(Some(response))
}

async fn run_attempt(
    state: Arc<AppState>,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    directories: &ActivationDirectories,
) -> Result<ryeos_app::managed_external_content_operation::ManagedActivationPublication> {
    let imported = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let activation = activation.clone();
        let operation = operation.clone();
        let cache = directories.cache.try_clone()?;
        let job = directories.job.try_clone()?;
        move || acquire_and_import(&state, &activation, &operation, &cache, &job)
    })
    .await
    .context("managed acquisition task panicked")??;

    let mut receipts = Vec::with_capacity(imported.len());
    for imported in imported {
        let binding = ryeos_app::operator_external_content::bind_managed_activation_component(
            Arc::clone(&state),
            operation.operator_fingerprint.clone(),
            activation,
            ryeos_app::operator_external_content::BindRequest {
                staging_id: imported.import.staging_id,
                request_digest: imported.import.request_digest,
                manifest_hash: imported.import.manifest_hash,
                consumer_ref: activation.document.consumer_ref.clone(),
            },
        )
        .await?;
        receipts.push(
            ryeos_state::objects::ExternalContentActivationComponentReceipt {
                id: imported.component_id,
                binding_hash: binding.binding_hash,
            },
        );
    }
    tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let activation = activation.clone();
        let operation = operation.clone();
        move || {
            ryeos_app::managed_external_content_operation::publish_activation_receipt(
                &state,
                &activation,
                &operation,
                receipts,
            )
        }
    })
    .await
    .context("managed activation publication task panicked")?
}

fn acquire_and_import(
    state: &AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    cache: &lillux::PinnedDirectory,
    staging: &lillux::PinnedDirectory,
) -> Result<Vec<ImportedComponent>> {
    let operator_authority_digest =
        ryeos_app::operator_external_content::configured_operator_authority_digest(
            state,
            &operation.operator_fingerprint,
        )?;
    operation.validate_current(
        activation,
        external_content_policy(state)?,
        &operator_authority_digest,
    )?;
    reset_activation_staging(staging)?;
    reconcile_activation_cache(cache)?;
    let offline_archive_root = open_offline_archive_root(state, operation)?;
    let mut archives = Vec::with_capacity(activation.document.sources.len());
    for source in &activation.document.sources {
        archives.push(obtain_archive(
            cache,
            source,
            operation.acquisition_mode,
            managed_policy(state)?,
            offline_archive_root.as_ref(),
        )?);
    }
    require_staging_capacity(staging, activation, managed_policy(state)?)?;
    for (source, archive) in activation.document.sources.iter().zip(archives) {
        extract_archive(archive, source, activation, staging, managed_policy(state)?)?;
    }
    let mut imported = Vec::with_capacity(activation.components.len());
    for component in &activation.components {
        let response = ryeos_app::operator_external_content::import_managed_activation_component(
            state,
            &operation.operator_fingerprint,
            activation,
            component,
            staging,
            &component.recipe.id,
        )?;
        imported.push(ImportedComponent {
            component_id: component.recipe.id.clone(),
            import: response,
        });
    }
    Ok(imported)
}

fn open_offline_archive_root(
    state: &AppState,
    operation: &ManagedActivationJobOperation,
) -> Result<Option<lillux::PinnedDirectory>> {
    let Some(root_name) = operation.offline_archive_root.as_deref() else {
        return Ok(None);
    };
    if operation.acquisition_mode != AcquisitionMode::Offline {
        bail!("online managed activation cannot open an offline archive root");
    }
    let root_policy = external_content_policy(state)?
        .roots
        .get(root_name)
        .ok_or_else(|| {
            anyhow::anyhow!("offline managed activation archive root is not admitted")
        })?;
    let root = lillux::PinnedDirectory::open(&root_policy.path)?
        .ok_or_else(|| anyhow::anyhow!("offline managed activation archive root is unavailable"))?;
    let (device, inode) = root.device_inode()?;
    if device != root_policy.containing_device || inode != root_policy.root_inode {
        bail!("offline managed activation archive root filesystem identity changed");
    }
    root.ensure_path_binding()?;
    Ok(Some(root))
}

fn reconcile_activation_cache(cache: &lillux::PinnedDirectory) -> Result<()> {
    for entry in cache.regular_files_bounded(CACHE_RECONCILIATION_ENTRY_LIMIT)? {
        let Some(name) = entry.name().to_str() else {
            bail!("managed activation cache contains a non-UTF8 entry");
        };
        if lillux::valid_hash(name) {
            continue;
        }
        if !name.starts_with(".secure.tmp.") {
            bail!("managed activation cache contains an unexpected entry");
        }
        cache
            .remove_pinned_regular_if_same(&entry)
            .context("remove managed activation cache crash orphan")?;
    }
    cache.ensure_path_binding()?;
    Ok(())
}

fn reset_activation_staging(staging: &lillux::PinnedDirectory) -> Result<()> {
    staging.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
        ryeos_app::managed_external_content::MAX_MANAGED_ACTIVATION_STAGING_ENTRIES,
        ryeos_state::external_content::MAX_CAPTURE_DEPTH,
    ))?;
    staging.ensure_path_binding()?;
    Ok(())
}

fn require_staging_capacity(
    staging: &lillux::PinnedDirectory,
    activation: &ResolvedManagedExternalContentActivation,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<()> {
    let component_ceiling = activation
        .components
        .iter()
        .try_fold(0u64, |total, component| {
            total
                .checked_add(component.capture_bounds.maximum_total_bytes)
                .ok_or_else(|| anyhow::anyhow!("managed activation staging byte ceiling overflow"))
        })?;
    let archive_ceiling = activation
        .document
        .sources
        .iter()
        .try_fold(0u64, |total, source| {
            total
                .checked_add(source.maximum_expanded_bytes)
                .ok_or_else(|| anyhow::anyhow!("managed activation expanded byte ceiling overflow"))
        })?;
    // The staged bytes are bounded independently both by the resolved consumer
    // capture contracts and by the aggregate expanded archive contract.
    let maximum_staged_bytes = component_ceiling.min(archive_ceiling);
    let maximum_staged_entries = activation.components.iter().try_fold(
        activation.components.len(),
        |total, component| {
            total
                .checked_add(component.capture_bounds.maximum_entries)
                .ok_or_else(|| anyhow::anyhow!("managed activation staging entry ceiling overflow"))
        },
    )?;
    staging.ensure_path_binding()?;
    let capacity = staging.filesystem_capacity()?;
    let allocation_overhead = u64::try_from(maximum_staged_entries)?
        .checked_mul(capacity.allocation_unit_bytes)
        .ok_or_else(|| anyhow::anyhow!("managed activation allocation reserve overflow"))?;
    let required_free = policy
        .minimum_free_bytes
        .checked_add(maximum_staged_bytes)
        .and_then(|total| total.checked_add(allocation_overhead))
        .ok_or_else(|| anyhow::anyhow!("managed activation staging reserve overflow"))?;
    if capacity.available_bytes < required_free {
        bail!(
            "managed activation staging requires {required_free} available bytes on its actual filesystem, observed {}",
            capacity.available_bytes
        );
    }
    if capacity.available_files < u64::try_from(maximum_staged_entries)? {
        bail!(
            "managed activation staging requires {maximum_staged_entries} available file identities on its actual filesystem, observed {}",
            capacity.available_files
        );
    }
    Ok(())
}

fn open_activation_directories(
    state: &AppState,
    operation_digest: &str,
) -> Result<ActivationDirectories> {
    if !lillux::valid_hash(operation_digest) {
        bail!("managed activation operation digest is not canonical");
    }
    let state_root = lillux::PinnedDirectory::open(&state.config.runtime_state_dir())?
        .ok_or_else(|| anyhow::anyhow!("runtime state root is unavailable"))?;
    let cache_root = state_root.open_or_create_child(OsStr::new("cache"), 0o700)?;
    let managed_cache =
        cache_root.open_or_create_child(OsStr::new("managed-external-content"), 0o700)?;
    let cache = managed_cache.open_or_create_child(OsStr::new("archives"), 0o700)?;
    let managed = state_root.open_or_create_child(OsStr::new("managed-external-content"), 0o700)?;
    let staging = managed.open_or_create_child(OsStr::new("staging"), 0o700)?;
    let runtime_root = state.config.runtime_root();
    if cache.path() != runtime_root.managed_external_content_cache()
        || staging.path() != runtime_root.managed_external_content_staging()
    {
        bail!("managed external-content layout differs from the typed runtime-root contract");
    }
    let lock = staging.lock_exclusive()?;
    let job_name = operation_digest.to_owned();
    let job = staging.open_or_create_child(OsStr::new(&job_name), 0o700)?;
    lock.ensure_protects(&staging)?;
    Ok(ActivationDirectories {
        cache,
        staging,
        job,
        job_name,
        _lock: lock,
    })
}

fn cleanup_staging(directories: ActivationDirectories) -> Result<()> {
    directories
        .job
        .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget {
            max_depth: ryeos_state::external_content::MAX_CAPTURE_DEPTH,
            max_entries:
                ryeos_app::managed_external_content::MAX_MANAGED_ACTIVATION_STAGING_ENTRIES,
        })?;
    if !directories
        .staging
        .remove_empty_child_if_same(OsStr::new(&directories.job_name), &directories.job)?
    {
        bail!("managed activation staging remained non-empty after bounded cleanup");
    }
    Ok(())
}

async fn cleanup_terminal_staging(directories: ActivationDirectories, job_id: &str) {
    match tokio::task::spawn_blocking(move || cleanup_staging(directories)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, %job_id, "terminal managed activation retained bounded staging")
        }
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation cleanup task panicked")
        }
    }
}

fn obtain_archive(
    cache: &lillux::PinnedDirectory,
    source: &ManagedActivationSource,
    mode: AcquisitionMode,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
    offline_archive_root: Option<&lillux::PinnedDirectory>,
) -> Result<lillux::PinnedRegularFile> {
    let name = OsStr::new(&source.sha256);
    if let Some(mut existing) = cache.open_pinned_regular(name, false)? {
        let verified = verify_open_file(
            &mut existing,
            source.maximum_compressed_bytes,
            &source.sha256,
            "cached managed activation archive",
        );
        match verified {
            Ok(_) => {
                cache.ensure_path_binding()?;
                return Ok(existing);
            }
            Err(error) => {
                cache
                    .remove_pinned_regular_if_same(&existing)
                    .context("remove invalid managed activation cache entry")?;
                if mode == AcquisitionMode::Offline {
                    return Err(error).context(
                        "offline managed activation cache entry failed exact verification",
                    );
                }
            }
        }
    }
    if mode == AcquisitionMode::Offline {
        let Some(root) = offline_archive_root else {
            bail!(
                "offline managed activation is missing cached archive {} and no admitted archive root was selected",
                source.sha256
            );
        };
        return import_offline_archive(cache, root, source, policy);
    }
    if !policy.allow_online {
        bail!("node policy does not permit online managed activation");
    }
    reserve_archive_cache(cache, source, policy, CACHE_ENTRY_LIMIT)?;
    let capacity = cache.filesystem_capacity()?;
    let required_free = policy
        .minimum_free_bytes
        .checked_add(source.maximum_compressed_bytes)
        .and_then(|total| total.checked_add(capacity.allocation_unit_bytes))
        .ok_or_else(|| anyhow::anyhow!("managed archive free-space requirement overflow"))?;
    if capacity.available_bytes < required_free || capacity.available_files == 0 {
        bail!("managed archive acquisition has insufficient node-private free space");
    }

    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .context("build managed archive HTTPS client")?;
    let mut current_url =
        reqwest::Url::parse(&source.url).context("parse admitted managed activation source URL")?;
    let mut redirect_count = 0usize;
    let response = loop {
        let response = client
            .get(current_url.clone())
            .header(
                reqwest::header::USER_AGENT,
                "RyeOS-managed-external-content/1",
            )
            .send()
            .map_err(reqwest::Error::without_url)
            .context("download managed activation archive")?;
        if !response.status().is_redirection() {
            break response
                .error_for_status()
                .map_err(reqwest::Error::without_url)
                .context("managed activation archive server refused the request")?;
        }
        if redirect_count >= policy.max_redirects {
            bail!("managed activation archive exceeds the node redirect ceiling");
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| anyhow::anyhow!("managed activation redirect has no Location"))?
            .to_str()
            .context("managed activation redirect Location is not UTF-8")?;
        let next_url = current_url
            .join(location)
            .context("managed activation redirect Location is invalid")?;
        admit_redirect_url(&next_url, policy)?;
        current_url = next_url;
        redirect_count += 1;
    };
    if response
        .content_length()
        .is_some_and(|length| length > source.maximum_compressed_bytes)
    {
        bail!("managed activation archive content length exceeds its signed bound");
    }
    let mut response = RedactedHttpBodyReader(response);
    let created = cache.atomic_create_pinned_regular_from_reader(
        name,
        &mut response,
        source.maximum_compressed_bytes,
        0o600,
    )?;
    let archive = match created {
        Some((archive, _)) => archive,
        None => cache
            .open_pinned_regular(name, false)?
            .ok_or_else(|| anyhow::anyhow!("managed archive publication winner disappeared"))?,
    };
    retain_verified_archive(
        cache,
        archive,
        source.maximum_compressed_bytes,
        &source.sha256,
        "downloaded managed activation archive",
    )
}

/// Reqwest body failures can retain the final request URL in their source
/// chain. Redirect URLs may legitimately carry short-lived query credentials,
/// so collapse every streaming failure to its I/O class before it can enter a
/// durable sync-job diagnostic, log line, or API error.
struct RedactedHttpBodyReader<R>(R);

impl<R: Read> Read for RedactedHttpBodyReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "managed activation archive body read failed ({:?})",
                    error.kind()
                ),
            )
        })
    }
}

fn import_offline_archive(
    cache: &lillux::PinnedDirectory,
    root: &lillux::PinnedDirectory,
    source: &ManagedActivationSource,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<lillux::PinnedRegularFile> {
    let archive_name = offline_archive_name(source)?;
    let archive_name = OsStr::new(&archive_name);
    let mut input = root
        .open_pinned_regular(archive_name, false)
        .context("open offline activation archive through admitted root")?
        .ok_or_else(|| {
            anyhow::anyhow!("offline activation archive is absent from the admitted root")
        })?;
    verify_open_file(
        &mut input,
        source.maximum_compressed_bytes,
        &source.sha256,
        "offline managed activation source archive",
    )?;
    input.seek(SeekFrom::Start(0))?;
    root.ensure_path_binding()?;

    reserve_archive_cache(cache, source, policy, CACHE_ENTRY_LIMIT)?;
    let capacity = cache.filesystem_capacity()?;
    let required_free = policy
        .minimum_free_bytes
        .checked_add(source.maximum_compressed_bytes)
        .and_then(|total| total.checked_add(capacity.allocation_unit_bytes))
        .ok_or_else(|| anyhow::anyhow!("managed archive free-space requirement overflow"))?;
    if capacity.available_bytes < required_free || capacity.available_files == 0 {
        bail!("managed archive acquisition has insufficient node-private free space");
    }

    let cache_name = OsStr::new(&source.sha256);
    let created = cache.atomic_create_pinned_regular_from_reader(
        cache_name,
        &mut input,
        source.maximum_compressed_bytes,
        0o600,
    )?;
    root.ensure_path_binding()?;
    let archive = match created {
        Some((archive, _)) => archive,
        None => cache
            .open_pinned_regular(cache_name, false)?
            .ok_or_else(|| anyhow::anyhow!("managed archive publication winner disappeared"))?,
    };
    retain_verified_archive(
        cache,
        archive,
        source.maximum_compressed_bytes,
        &source.sha256,
        "offline imported managed activation archive",
    )
}

fn offline_archive_name(source: &ManagedActivationSource) -> Result<String> {
    let url = reqwest::Url::parse(&source.url)
        .context("parse admitted managed activation source URL for offline archive")?;
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .ok_or_else(|| anyhow::anyhow!("managed activation source URL has no archive filename"))?;
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
    {
        bail!("managed activation source URL has no canonical offline archive filename");
    }
    Ok(name.to_owned())
}

fn reserve_archive_cache(
    cache: &lillux::PinnedDirectory,
    source: &ManagedActivationSource,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
    entry_limit: usize,
) -> Result<()> {
    if entry_limit == 0 {
        bail!("managed archive cache entry limit is zero");
    }
    let mut entries = cache.regular_files_bounded(CACHE_RECONCILIATION_ENTRY_LIMIT)?;
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    let mut retained = entries.iter().try_fold(0u64, |total, entry| {
        total
            .checked_add(entry.size()?)
            .ok_or_else(|| anyhow::anyhow!("managed archive cache byte count overflow"))
    })?;
    let mut retained_entries = entries.len();
    for entry in entries {
        let fits_entries = retained_entries < entry_limit;
        let fits_bytes = retained
            .checked_add(source.maximum_compressed_bytes)
            .is_some_and(|total| total <= policy.cache_budget_bytes);
        if fits_entries && fits_bytes {
            break;
        }
        let size = entry.size()?;
        cache
            .remove_pinned_regular_if_same(&entry)
            .context("evict rebuildable managed activation archive")?;
        retained = retained
            .checked_sub(size)
            .ok_or_else(|| anyhow::anyhow!("managed archive cache byte count underflow"))?;
        retained_entries = retained_entries
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("managed archive cache entry count underflow"))?;
    }
    if retained_entries >= entry_limit
        || retained
            .checked_add(source.maximum_compressed_bytes)
            .ok_or_else(|| anyhow::anyhow!("managed archive cache budget overflow"))?
            > policy.cache_budget_bytes
    {
        bail!("managed archive cache cannot reserve the admitted archive budget");
    }
    cache.ensure_path_binding()?;
    Ok(())
}

fn retain_verified_archive(
    cache: &lillux::PinnedDirectory,
    mut archive: lillux::PinnedRegularFile,
    maximum_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<lillux::PinnedRegularFile> {
    if let Err(error) = verify_open_file(&mut archive, maximum_bytes, expected_sha256, label) {
        cache
            .remove_pinned_regular_if_same(&archive)
            .context("remove invalid newly published managed activation archive")?;
        return Err(error);
    }
    cache.ensure_path_binding()?;
    Ok(archive)
}

fn admit_redirect_url(
    url: &reqwest::Url,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<()> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
    {
        bail!("managed activation redirect must be canonical HTTPS");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("managed activation redirect has no host"))?;
    if host != host.to_ascii_lowercase()
        || !policy
            .allowed_https_hosts
            .iter()
            .any(|allowed| allowed == host)
    {
        bail!("managed activation redirect host is not admitted by node policy");
    }
    Ok(())
}

fn extract_archive(
    mut archive_file: lillux::PinnedRegularFile,
    source: &ManagedActivationSource,
    activation: &ResolvedManagedExternalContentActivation,
    staging: &lillux::PinnedDirectory,
    policy: &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<()> {
    archive_file.seek(SeekFrom::Start(0))?;
    let decoder = flate2::read::MultiGzDecoder::new(archive_file);
    let bounded = decoder.take(source.maximum_expanded_bytes.saturating_add(1));
    let mut archive = tar::Archive::new(bounded);
    let selected = source
        .members
        .iter()
        .map(|member| (member.path.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    let imported = activation
        .components
        .iter()
        .flat_map(|component| {
            component
                .recipe
                .mapped_members()
                .unwrap_or_default()
                .iter()
                .filter(|mapping| mapping.source == source.id)
                .map(move |mapping| (mapping.member.as_str(), (component, mapping)))
        })
        .collect::<BTreeMap<_, _>>();
    let whole_tree = activation
        .components
        .iter()
        .filter_map(|component| {
            component
                .recipe
                .whole_archive_tree()
                .filter(|(whole_source, _, _)| *whole_source == source.id)
                .map(|(_, prefix, bounds)| (component, prefix, bounds))
        })
        .next();
    if whole_tree.is_some() && (!selected.is_empty() || !imported.is_empty()) {
        bail!("managed activation source mixes mapped and whole-tree extraction");
    }
    let whole_root = whole_tree
        .map(|(component, _, _)| {
            staging.open_or_create_child(OsStr::new(&component.recipe.id), 0o755)
        })
        .transpose()?;
    let mut seen_paths = BTreeSet::new();
    let mut archive_namespace = BTreeMap::new();
    let mut whole_namespace = BTreeMap::new();
    let mut whole_symlinks = BTreeMap::new();
    let mut seen_selected = BTreeSet::new();
    let raw_entry_ceiling = source
        .maximum_entries
        .min(policy.max_members)
        .checked_mul(3)
        .ok_or_else(|| anyhow::anyhow!("managed archive raw entry ceiling overflow"))?;
    let mut raw_entries = 0usize;
    let mut logical_entries = 0usize;
    let mut regular_bytes = 0u64;
    let mut whole_regular_bytes = 0u64;
    let mut saw_whole_prefix = false;
    let mut extensions = PendingTarExtensions::default();
    for entry in archive
        .entries()
        .context("read managed activation tar archive")?
        .raw(true)
    {
        let mut entry = entry.context("read managed activation tar member")?;
        raw_entries = raw_entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("managed archive entry count overflow"))?;
        if raw_entries > raw_entry_ceiling {
            bail!("managed activation archive exceeds its raw-header bound");
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_gnu_sparse() {
            bail!("managed activation archive contains a sparse entry");
        }
        if entry_type.is_pax_global_extensions() {
            bail!("managed activation archive contains unsupported global PAX authority");
        }
        if entry_type.is_gnu_longname() {
            let value = read_tar_extension(
                &mut entry,
                ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES as u64 + 1,
                "GNU long path",
            )?;
            set_tar_extension(
                &mut extensions.path,
                normalize_gnu_extension(value, "GNU long path")?,
                "path",
            )?;
            continue;
        }
        if entry_type.is_gnu_longlink() {
            let value = read_tar_extension(
                &mut entry,
                ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES + 1,
                "GNU long link",
            )?;
            set_tar_extension(
                &mut extensions.link,
                normalize_gnu_extension(value, "GNU long link")?,
                "link",
            )?;
            continue;
        }
        if entry_type.is_pax_local_extensions() {
            let value =
                read_tar_extension(&mut entry, MAX_MANAGED_TAR_EXTENSION_BYTES, "local PAX")?;
            apply_local_pax_extensions(&value, &mut extensions)?;
            continue;
        }
        logical_entries = logical_entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("managed archive logical entry count overflow"))?;
        if logical_entries > source.maximum_entries || logical_entries > policy.max_members {
            bail!("managed activation archive exceeds the node logical entry-count bound");
        }
        let namespace_kind =
            ArchiveNamespaceKind::from_entry_type(entry_type).ok_or_else(|| {
                anyhow::anyhow!("managed activation archive contains a hardlink or special entry")
            })?;
        if whole_tree.is_none() && namespace_kind == ArchiveNamespaceKind::Symlink {
            bail!("mapped managed activation archive contains a link or special entry");
        }
        let path_bytes = extensions
            .path
            .take()
            .unwrap_or_else(|| entry.path_bytes().into_owned());
        let raw = std::str::from_utf8(&path_bytes)
            .context("managed activation archive contains a non-UTF8 path")?;
        let path = if entry_type.is_dir() {
            raw.strip_suffix('/').unwrap_or(raw)
        } else {
            raw
        }
        .to_owned();
        ryeos_state::objects::validate_canonical_project_relative_path(&path)
            .context("managed activation archive path is not canonical")?;
        if path.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES {
            bail!("managed activation archive path exceeds the portable path bound");
        }
        if !seen_paths.insert(path.clone()) {
            bail!("managed activation archive repeats a canonical path");
        }
        insert_archive_namespace(&mut archive_namespace, &path, namespace_kind)?;
        if archive_namespace.len() > source.maximum_entries
            || archive_namespace.len() > policy.max_members
        {
            bail!("managed activation archive namespace exceeds its entry-count bound");
        }
        let size = entry.size();
        if namespace_kind != ArchiveNamespaceKind::File && size != 0 {
            bail!("managed activation archive directory or symlink carries body bytes");
        }
        let symlink_target = if namespace_kind == ArchiveNamespaceKind::Symlink {
            let target = extensions
                .link
                .take()
                .or_else(|| entry.link_name_bytes().map(|value| value.into_owned()))
                .ok_or_else(|| anyhow::anyhow!("managed activation symlink has no target"))?;
            ryeos_state::objects::validate_internal_symlink_target(&path, &target)?;
            Some(target)
        } else {
            if extensions.link.take().is_some() {
                bail!("managed activation archive applies link authority to a non-symlink");
            }
            None
        };
        if namespace_kind == ArchiveNamespaceKind::File {
            regular_bytes = regular_bytes
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("managed archive expanded byte count overflow"))?;
            if regular_bytes > source.maximum_expanded_bytes {
                bail!("managed activation archive exceeds its expanded-byte bound");
            }
        }

        if let Some((_, prefix, bounds)) = whole_tree {
            if path == prefix {
                if namespace_kind != ArchiveNamespaceKind::Directory {
                    bail!("whole-tree activation prefix is not an archive directory");
                }
                saw_whole_prefix = true;
                continue;
            }
            let Some(relative) = path
                .strip_prefix(prefix)
                .and_then(|suffix| suffix.strip_prefix('/'))
            else {
                continue;
            };
            ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
            if relative.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES {
                bail!("whole-tree activation path exceeds the portable path bound");
            }
            insert_archive_namespace(&mut whole_namespace, relative, namespace_kind)?;
            if whole_namespace.len() > bounds.maximum_entries {
                bail!("whole-tree activation exceeds its signed entry bound");
            }
            let required_depth = relative.split('/').count().saturating_add(usize::from(
                namespace_kind == ArchiveNamespaceKind::Directory,
            ));
            if required_depth > bounds.maximum_depth {
                bail!("whole-tree activation exceeds its signed depth bound");
            }
            if namespace_kind == ArchiveNamespaceKind::File {
                if size > bounds.maximum_file_bytes {
                    bail!("whole-tree activation file exceeds its signed bound");
                }
                whole_regular_bytes = whole_regular_bytes
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("whole-tree activation byte count overflow"))?;
                if whole_regular_bytes > bounds.maximum_total_bytes {
                    bail!("whole-tree activation exceeds its signed aggregate byte bound");
                }
            }
            if namespace_kind == ArchiveNamespaceKind::Symlink {
                let target = std::str::from_utf8(
                    symlink_target
                        .as_deref()
                        .expect("validated symlink entry retains a target"),
                )
                .context("whole-tree activation symlink target is not UTF-8")?;
                whole_symlinks.insert(relative.to_owned(), target.to_owned());
            }
            let portable_mode = if namespace_kind == ArchiveNamespaceKind::File
                && entry.header().mode()? & 0o111 != 0
            {
                0o755
            } else {
                0o644
            };
            let root = whole_root
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("whole-tree staging root is absent"))?;
            stage_whole_tree_entry(
                root,
                relative,
                namespace_kind,
                symlink_target.as_deref(),
                &mut entry,
                size,
                bounds.maximum_file_bytes,
                portable_mode,
            )?;
            continue;
        }

        if namespace_kind == ArchiveNamespaceKind::Directory {
            continue;
        }
        let Some(member) = selected.get(path.as_str()).copied() else {
            continue;
        };
        if size > member.maximum_bytes {
            bail!("selected managed activation member exceeds its signed bound");
        }
        let executable = entry.header().mode()? & 0o111 != 0;
        if executable != member.executable {
            bail!("selected managed activation member executable mode changed");
        }
        seen_selected.insert(path.clone());
        if member.disposition == ManagedMemberDisposition::VerifyOnly {
            verify_reader(
                &mut entry,
                member.maximum_bytes,
                &member.sha256,
                "managed activation verification member",
            )?;
            continue;
        }
        let (component, mapping) = imported.get(path.as_str()).copied().ok_or_else(|| {
            anyhow::anyhow!("imported managed activation member has no resolved consumer component")
        })?;
        let mode = if member.executable { 0o755 } else { 0o644 };
        let (destination, name) = match component.declaration_kind {
            ryeos_engine::external_content::ExternalContentKind::File => {
                if mapping.target.is_some() {
                    bail!("managed file component member unexpectedly has a target");
                }
                (staging.try_clone()?, component.recipe.id.as_str())
            }
            ryeos_engine::external_content::ExternalContentKind::Tree => {
                let target = mapping.target.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("managed tree component member has no target")
                })?;
                let parts = target.split('/').collect::<Vec<_>>();
                let name = parts
                    .last()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("managed tree target is empty"))?;
                let mut destination =
                    staging.open_or_create_child(OsStr::new(&component.recipe.id), 0o755)?;
                for part in parts.iter().take(parts.len().saturating_sub(1)) {
                    destination = destination.open_or_create_child(OsStr::new(part), 0o755)?;
                }
                (destination, name)
            }
        };
        let created = destination.atomic_create_regular_from_reader(
            OsStr::new(name),
            &mut entry,
            member.maximum_bytes,
            mode,
        )?;
        let mut file = match created {
            Some((file, _)) => file,
            None => destination
                .open_regular(OsStr::new(name), false)?
                .ok_or_else(|| anyhow::anyhow!("managed activation staged member disappeared"))?,
        };
        verify_open_file(
            &mut file,
            member.maximum_bytes,
            &member.sha256,
            "managed activation staged member",
        )?;
        if lillux::normalized_portable_regular_mode(&file.metadata()?)? != mode {
            bail!("managed activation staged member mode changed");
        }
        destination.ensure_path_binding()?;
    }
    if extensions.path.is_some() || extensions.link.is_some() {
        bail!("managed activation archive ends with an unattached extension header");
    }
    ryeos_state::objects::validate_internal_symlink_graph(
        whole_symlinks
            .iter()
            .map(|(path, target)| (path.as_str(), target.as_str())),
    )?;
    let mut bounded = archive.into_inner();
    let mut trailing = [0u8; 128 * 1024];
    loop {
        let read = bounded
            .read(&mut trailing)
            .context("finish validating managed activation compressed stream")?;
        if read == 0 {
            break;
        }
        if trailing[..read].iter().any(|byte| *byte != 0) {
            bail!("managed activation archive has non-zero data after its tar terminator");
        }
    }
    let expanded = source
        .maximum_expanded_bytes
        .saturating_add(1)
        .saturating_sub(bounded.limit());
    if expanded > source.maximum_expanded_bytes {
        bail!("managed activation archive exceeds its decompression bound");
    }
    let expected = selected
        .keys()
        .map(|path| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    if seen_selected != expected {
        bail!("managed activation archive is missing a selected member");
    }
    if let Some((_, _, bounds)) = whole_tree {
        if !saw_whole_prefix {
            bail!("managed activation archive is missing its whole-tree prefix");
        }
        if whole_namespace.is_empty() {
            bail!("whole-tree activation contains no realization entries");
        }
        if whole_namespace.len() > bounds.maximum_entries
            || whole_regular_bytes > bounds.maximum_total_bytes
        {
            bail!("whole-tree activation exceeds its signed capture bounds");
        }
    }
    staging.ensure_path_binding()?;
    Ok(())
}

#[derive(Default)]
struct PendingTarExtensions {
    path: Option<Vec<u8>>,
    link: Option<Vec<u8>>,
}

fn read_tar_extension<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    maximum_bytes: u64,
    label: &str,
) -> Result<Vec<u8>> {
    if entry.size() > maximum_bytes {
        bail!("managed activation {label} extension exceeds its byte bound");
    }
    let mut bounded = entry.take(maximum_bytes.saturating_add(1));
    let mut value = Vec::new();
    bounded
        .read_to_end(&mut value)
        .with_context(|| format!("read managed activation {label} extension"))?;
    if value.len() as u64 > maximum_bytes {
        bail!("managed activation {label} extension exceeds its byte bound");
    }
    Ok(value)
}

fn normalize_gnu_extension(mut value: Vec<u8>, label: &str) -> Result<Vec<u8>> {
    if value.last() == Some(&0) {
        value.pop();
    }
    if value.is_empty() || value.contains(&0) {
        bail!("managed activation {label} extension is not canonical");
    }
    Ok(value)
}

fn set_tar_extension(target: &mut Option<Vec<u8>>, value: Vec<u8>, label: &str) -> Result<()> {
    if target.replace(value).is_some() {
        bail!("managed activation archive repeats {label} extension authority");
    }
    Ok(())
}

fn apply_local_pax_extensions(value: &[u8], pending: &mut PendingTarExtensions) -> Result<()> {
    let mut observed = false;
    for extension in tar::PaxExtensions::new(value) {
        let extension = extension.context("parse managed activation local PAX extension")?;
        observed = true;
        match extension.key_bytes() {
            b"path" => {
                if extension.value_bytes().len()
                    > ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES
                {
                    bail!("managed activation local PAX path exceeds its byte bound");
                }
                set_tar_extension(&mut pending.path, extension.value_bytes().to_vec(), "path")?;
            }
            b"linkpath" => {
                if extension.value_bytes().len() as u64
                    > ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES
                {
                    bail!("managed activation local PAX link exceeds its byte bound");
                }
                set_tar_extension(&mut pending.link, extension.value_bytes().to_vec(), "link")?;
            }
            key if key.starts_with(b"GNU.sparse.") => {
                bail!("managed activation archive contains sparse PAX authority")
            }
            _ => bail!("managed activation archive contains unsupported local PAX authority"),
        }
    }
    if !observed {
        bail!("managed activation local PAX extension is empty");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveNamespaceKind {
    Directory,
    File,
    Symlink,
}

impl ArchiveNamespaceKind {
    fn from_entry_type(entry_type: tar::EntryType) -> Option<Self> {
        if entry_type.is_dir() {
            Some(Self::Directory)
        } else if entry_type.is_file() {
            Some(Self::File)
        } else if entry_type.is_symlink() {
            Some(Self::Symlink)
        } else {
            None
        }
    }
}

fn insert_archive_namespace(
    namespace: &mut BTreeMap<String, ArchiveNamespaceKind>,
    path: &str,
    kind: ArchiveNamespaceKind,
) -> Result<()> {
    let parts = path.split('/').collect::<Vec<_>>();
    let mut current = String::new();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        match namespace.get(&current) {
            Some(ArchiveNamespaceKind::Directory) => {}
            Some(_) => {
                bail!("managed activation archive path collides with a non-directory ancestor")
            }
            None => {
                namespace.insert(current.clone(), ArchiveNamespaceKind::Directory);
            }
        }
    }
    match namespace.get(path) {
        Some(ArchiveNamespaceKind::Directory) if kind == ArchiveNamespaceKind::Directory => Ok(()),
        Some(_) => bail!("managed activation archive contains a colliding path namespace"),
        None => {
            namespace.insert(path.to_owned(), kind);
            Ok(())
        }
    }
}

fn stage_whole_tree_entry(
    root: &lillux::PinnedDirectory,
    relative: &str,
    kind: ArchiveNamespaceKind,
    symlink_target: Option<&[u8]>,
    reader: &mut impl Read,
    expected_size: u64,
    maximum_file_bytes: u64,
    portable_mode: u32,
) -> Result<()> {
    let parts = relative.split('/').collect::<Vec<_>>();
    let name = parts
        .last()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("whole-tree activation path is empty"))?;
    let mut destination = root.try_clone()?;
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        destination = destination.open_or_create_child(OsStr::new(part), 0o755)?;
    }
    match kind {
        ArchiveNamespaceKind::Directory => {
            destination.open_or_create_child(OsStr::new(name), 0o755)?;
        }
        ArchiveNamespaceKind::Symlink => {
            let target = symlink_target
                .ok_or_else(|| anyhow::anyhow!("whole-tree symlink target is absent"))?;
            ryeos_state::objects::validate_internal_symlink_target(relative, target)?;
            if let Some(existing) = destination.read_symlink_target(
                OsStr::new(name),
                ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
            )? {
                if existing != target {
                    bail!("whole-tree activation staged symlink differs from the archive");
                }
            } else {
                destination.create_symlink(OsStr::new(name), target)?;
            }
        }
        ArchiveNamespaceKind::File => {
            let mut digesting = DigestingReader::new(reader);
            let existing = destination.open_regular(OsStr::new(name), false)?;
            let created = if existing.is_some() {
                std::io::copy(&mut digesting, &mut std::io::sink())?;
                None
            } else {
                destination.atomic_create_regular_from_reader(
                    OsStr::new(name),
                    &mut digesting,
                    maximum_file_bytes,
                    portable_mode,
                )?
            };
            let (observed_size, digest) = digesting.finish();
            if observed_size != expected_size {
                bail!("whole-tree activation file size differs from its archive header");
            }
            let mut file = match created {
                Some((file, copied)) if copied == expected_size => file,
                Some(_) => bail!("whole-tree activation staged an incomplete file"),
                None => match existing {
                    Some(file) => file,
                    None => destination
                        .open_regular(OsStr::new(name), false)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("whole-tree activation staged file disappeared")
                        })?,
                },
            };
            verify_open_file(
                &mut file,
                maximum_file_bytes,
                &digest,
                "whole-tree activation staged file",
            )?;
            if lillux::normalized_portable_regular_mode(&file.metadata()?)? != portable_mode {
                bail!("whole-tree activation staged file mode differs from the archive");
            }
        }
    }
    destination.ensure_path_binding()?;
    Ok(())
}

struct DigestingReader<'a, R> {
    reader: &'a mut R,
    digest: Sha256,
    observed: u64,
}

impl<'a, R> DigestingReader<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            digest: Sha256::new(),
            observed: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.observed, format!("{:x}", self.digest.finalize()))
    }
}

impl<R: Read> Read for DigestingReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.reader.read(buffer)?;
        self.observed = self
            .observed
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("whole-tree activation byte count overflow"))?;
        self.digest.update(&buffer[..read]);
        Ok(read)
    }
}

fn verify_open_file(
    file: &mut (impl std::io::Read + std::io::Seek),
    maximum_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let size = verify_reader(file, maximum_bytes, expected_sha256, label)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(size)
}

fn verify_reader(
    reader: &mut impl std::io::Read,
    maximum_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<u64> {
    let mut digest = Sha256::new();
    let mut observed = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(u64::try_from(read)?)
            .ok_or_else(|| anyhow::anyhow!("{label} byte count overflow"))?;
        if observed > maximum_bytes {
            bail!("{label} exceeds its signed byte bound");
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual != expected_sha256 {
        bail!("{label} digest changed: expected {expected_sha256}, observed {actual}");
    }
    Ok(observed)
}

fn settle_attempt(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    receipt_hash: Option<String>,
    error: Option<String>,
    result: Option<Value>,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        let latest = db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow::anyhow!("managed activation job disappeared"))?;
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &ryeos_state::FinishSyncJobAttempt {
                state: attempt_state,
                phase: phase.to_owned(),
                error: error.clone(),
                result: result.clone(),
            },
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: job_state,
                phase: phase.to_owned(),
                roots: receipt_hash.map(|hash| vec![hash]),
                heads: None,
                uploaded_hashes: latest.uploaded_hashes,
                fetched_hashes: latest.fetched_hashes,
                last_error: error,
                result,
            },
        )
    })
}

fn settle_retryable_attempt_failure(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    detail: String,
) -> Result<bool> {
    let latest = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("managed activation job disappeared before failure settlement")?;
    let terminal = latest.attempts_exhausted();
    settle_attempt(
        state,
        job_id,
        attempt_id,
        ryeos_state::SyncJobAttemptState::Failed,
        if terminal {
            ryeos_state::SyncJobState::Failed
        } else {
            ryeos_state::SyncJobState::Retryable
        },
        if terminal { "failed" } else { "retryable" },
        None,
        Some(detail),
        latest.result,
    )?;
    Ok(terminal)
}

fn managed_policy(
    state: &AppState,
) -> Result<
    &ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
> {
    state
        .node_policy
        .require::<ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?
        .managed_activation
        .require_enabled()
}

fn external_content_policy(
    state: &AppState,
) -> Result<&ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord>
{
    state
        .node_policy
        .require::<ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()
}

fn bounded_error(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    while output.len() > ERROR_LIMIT {
        output.pop();
    }
    let output = output.trim();
    if output.is_empty() {
        "managed external-content activation failed".to_owned()
    } else {
        output.to_owned()
    }
}

pub async fn recover_durable_activations(state: &AppState) -> Result<usize> {
    reconcile_retained_activation_staging_task(state).await?;
    let mut recovered = 0usize;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type_after(
                MANAGED_ACTIVATION_OPERATION,
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
            let operation = match ManagedActivationJobOperation::from_value(job.operation.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    let detail = bounded_error(&format!(
                        "retained managed activation operation is invalid: {error:#}"
                    ));
                    state.state_store.with_state_db(|db| {
                        db.update_sync_job(
                            &job.job_id,
                            &ryeos_state::SyncJobUpdate {
                                state: ryeos_state::SyncJobState::Failed,
                                phase: "operation_invalid".to_owned(),
                                roots: None,
                                heads: None,
                                uploaded_hashes: job.uploaded_hashes.clone(),
                                fetched_hashes: job.fetched_hashes.clone(),
                                last_error: Some(detail),
                                result: job.result.clone(),
                            },
                        )
                    })?;
                    tracing::error!(job_id = %job.job_id, %error, "invalid managed activation operation terminalized");
                    cleanup_retained_terminal_staging(state, &job.job_id, &job.operation).await;
                    continue;
                }
            };
            if let Err(error) = retained_activation_operation_digest(&job.job_id, &job.operation) {
                let detail = bounded_error(&format!(
                    "retained managed activation operation identity is invalid: {error:#}"
                ));
                state.state_store.with_state_db(|db| {
                    db.update_sync_job(
                        &job.job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Failed,
                            phase: "operation_identity_invalid".to_owned(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: job.uploaded_hashes.clone(),
                            fetched_hashes: job.fetched_hashes.clone(),
                            last_error: Some(detail),
                            result: job.result.clone(),
                        },
                    )
                })?;
                tracing::error!(job_id = %job.job_id, %error, "mismatched managed activation operation terminalized before recovery contact");
                continue;
            }
            let activation = match ryeos_app::managed_external_content::resolve_activation(
                state,
                &operation.activation_ref,
                operation.acquisition_mode,
            ) {
                Ok(activation) => activation,
                Err(error) => {
                    let detail = bounded_error(&format!(
                        "retained managed activation can no longer resolve its exact signed program: {error:#}"
                    ));
                    state.state_store.with_state_db(|db| {
                        db.update_sync_job(
                            &job.job_id,
                            &ryeos_state::SyncJobUpdate {
                                state: ryeos_state::SyncJobState::Failed,
                                phase: "program_unavailable".to_owned(),
                                roots: None,
                                heads: None,
                                uploaded_hashes: job.uploaded_hashes.clone(),
                                fetched_hashes: job.fetched_hashes.clone(),
                                last_error: Some(detail),
                                result: job.result.clone(),
                            },
                        )
                    })?;
                    cleanup_retained_terminal_staging(state, &job.job_id, &job.operation).await;
                    continue;
                }
            };
            match submit_operation_with_status(Arc::new(state.clone()), activation, operation) {
                Ok(submission) => {
                    if submission.attempt_started || submission.response.state == "completed" {
                        recovered += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(job_id = %job.job_id, %error, "managed activation recovery submission did not complete")
                }
            }
        }
        after = Some(next);
    }
    // A malformed retained row may have been terminalized during this pass.
    // Sweep again so its rebuildable tree does not wait for another daemon
    // restart; a crash before this sweep is repaired by the pre-pass above.
    reconcile_retained_activation_staging_task(state).await?;
    Ok(recovered)
}

async fn reconcile_retained_activation_staging_task(state: &AppState) -> Result<usize> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || reconcile_retained_activation_staging(&state))
        .await
        .context("managed activation staging recovery task panicked")?
}

fn reconcile_retained_activation_staging(state: &AppState) -> Result<usize> {
    let state_root = lillux::PinnedDirectory::open(&state.config.runtime_state_dir())?
        .ok_or_else(|| anyhow::anyhow!("runtime state root is unavailable"))?;
    let managed = state_root.open_or_create_child(OsStr::new("managed-external-content"), 0o700)?;
    let staging = managed.open_or_create_child(OsStr::new("staging"), 0o700)?;
    if staging.path()
        != state
            .config
            .runtime_root()
            .managed_external_content_staging()
    {
        bail!("managed external-content staging differs from the typed runtime-root contract");
    }
    reconcile_retained_staging_entries(&staging, |job_id| {
        Ok(state
            .state_store
            .with_state_db(|db| db.get_sync_job(job_id))?
            .map(|job| job.state))
    })
}

fn reconcile_retained_staging_entries<F>(
    staging: &lillux::PinnedDirectory,
    mut lookup: F,
) -> Result<usize>
where
    F: FnMut(&str) -> Result<Option<ryeos_state::SyncJobState>>,
{
    let lock = staging.lock_exclusive()?;
    lock.ensure_protects(staging)?;
    let entries = staging.entries_no_follow_bounded(CACHE_RECONCILIATION_ENTRY_LIMIT)?;
    let mut removed = 0usize;
    for entry in entries {
        let Some(operation_digest) = entry.name.to_str() else {
            bail!("managed activation staging contains a non-UTF8 entry");
        };
        if !lillux::valid_hash(operation_digest)
            || entry.entry_type != lillux::PinnedEntryType::Directory
        {
            bail!("managed activation staging contains an unexpected entry");
        }
        let job_id = activation_job_id(operation_digest);
        let rebuildable = matches!(
            lookup(&job_id)?,
            None | Some(
                ryeos_state::SyncJobState::Completed
                    | ryeos_state::SyncJobState::Failed
                    | ryeos_state::SyncJobState::Cancelled
            )
        );
        if !rebuildable {
            continue;
        }
        let child = staging
            .open_child_directory(&entry.name)?
            .ok_or_else(|| anyhow::anyhow!("managed activation staging child disappeared"))?;
        child.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
            ryeos_app::managed_external_content::MAX_MANAGED_ACTIVATION_STAGING_ENTRIES,
            ryeos_state::external_content::MAX_CAPTURE_DEPTH,
        ))?;
        if !staging.remove_empty_child_if_same(&entry.name, &child)? {
            bail!("managed activation terminal staging remained non-empty");
        }
        removed += 1;
    }
    staging.ensure_path_binding()?;
    Ok(removed)
}

fn retained_activation_operation_digest(job_id: &str, operation: &Value) -> Result<String> {
    let digest = ryeos_state::objects::canonical_value_digest(operation)?;
    if activation_job_id(&digest) != job_id {
        bail!("managed activation job id does not match its canonical operation");
    }
    Ok(digest)
}

async fn cleanup_retained_terminal_staging(state: &AppState, job_id: &str, operation: &Value) {
    let operation_digest = match retained_activation_operation_digest(job_id, operation) {
        Ok(digest) => digest,
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation operation cannot authorize staging cleanup");
            return;
        }
    };
    let state = state.clone();
    let directories = match tokio::task::spawn_blocking(move || {
        open_activation_directories(&state, &operation_digest)
    })
    .await
    {
        Ok(Ok(directories)) => directories,
        Ok(Err(error)) => {
            tracing::warn!(%error, %job_id, "terminal managed activation staging could not be opened for cleanup");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation staging cleanup task panicked");
            return;
        }
    };
    cleanup_terminal_staging(directories, job_id).await;
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/activate",
    endpoint: "external-content.activate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/activate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::managed_external_content::{
        ManagedActivationComponentBounds, ManagedActivationComponentMember,
        ManagedActivationComponentShape, ManagedActivationMember, ManagedComponentStorage,
        ResolvedManagedActivationComponent,
    };

    fn pinned_archive_fixture(path: &std::path::Path) -> lillux::PinnedRegularFile {
        lillux::open_pinned_regular_file_no_follow(path).unwrap()
    }

    fn test_policy()
    -> ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy
    {
        ryeos_app::node_policy::sections::external_content::ManagedExternalContentActivationPolicy {
            allow_online: false,
            allowed_https_hosts: vec!["releases.example.test".to_owned()],
            max_redirects: 0,
            max_archives: 1,
            max_compressed_bytes: 65536,
            max_expanded_bytes: 65536,
            max_members: 8,
            max_member_bytes: 1024,
            max_concurrent_activations: 1,
            cache_budget_bytes: 131072,
            store_budget_bytes: 131072,
            minimum_free_bytes: 1,
            max_attempts: 2,
        }
    }

    fn test_activation(
        source: ManagedActivationSource,
    ) -> ResolvedManagedExternalContentActivation {
        let expected_file_sha256 = source.members.first().map(|member| member.sha256.clone());
        let recipe = ryeos_app::managed_external_content::ManagedActivationComponent {
            id: "runtime".to_owned(),
            storage: ManagedComponentStorage::LargeContent,
            shape: ManagedActivationComponentShape::Mapped {
                members: vec![ManagedActivationComponentMember {
                    source: "package".to_owned(),
                    member: "bin/runtime".to_owned(),
                    target: None,
                }],
            },
        };
        ResolvedManagedExternalContentActivation {
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            document: ryeos_app::managed_external_content::ManagedExternalContentActivation {
                schema: ryeos_app::managed_external_content::MANAGED_ACTIVATION_SCHEMA.to_owned(),
                consumer_ref: "worker:fixture/hosted".to_owned(),
                sources: vec![source],
                components: vec![recipe.clone()],
            },
            components: vec![ResolvedManagedActivationComponent {
                recipe,
                expected_manifest_hash: "d".repeat(64),
                expected_manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                    .to_owned(),
                declaration_kind: ryeos_engine::external_content::ExternalContentKind::File,
                capture_bounds: ManagedActivationComponentBounds {
                    maximum_entries: 1,
                    maximum_depth: 1,
                    maximum_file_bytes: 64,
                    maximum_total_bytes: 64,
                },
                expected_file_sha256,
            }],
        }
    }

    #[test]
    fn redirect_admission_requires_https_and_a_node_allowed_host() {
        let mut policy = test_policy();
        policy.allowed_https_hosts = vec![
            "github.com".to_owned(),
            "release-assets.githubusercontent.com".to_owned(),
        ];
        assert!(
            admit_redirect_url(
                &reqwest::Url::parse(
                    "https://release-assets.githubusercontent.com/object?sig=opaque"
                )
                .unwrap(),
                &policy,
            )
            .is_ok()
        );
        for refused in [
            "http://release-assets.githubusercontent.com/object",
            "https://metadata.invalid/object",
            "https://user@release-assets.githubusercontent.com/object",
            "https://release-assets.githubusercontent.com:8443/object",
            "https://release-assets.githubusercontent.com/object#fragment",
        ] {
            assert!(
                admit_redirect_url(&reqwest::Url::parse(refused).unwrap(), &policy).is_err(),
                "redirect should be refused: {refused}"
            );
        }
    }

    #[test]
    fn redirect_body_failure_drops_credential_bearing_url_chain() {
        struct CredentialBearingFailure;

        impl Read for CredentialBearingFailure {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other(
                    "request failed for https://objects.example.test/archive?token=secret",
                ))
            }
        }

        let mut reader = RedactedHttpBodyReader(CredentialBearingFailure);
        let error = reader.read(&mut [0u8; 8]).unwrap_err();
        let retained = format!("{error:#}");
        assert!(retained.contains("managed activation archive body read failed"));
        assert!(!retained.contains("token=secret"));
        assert!(!retained.contains("objects.example.test"));
    }

    fn test_whole_activation(
        source: ManagedActivationSource,
        bounds: ManagedActivationComponentBounds,
    ) -> ResolvedManagedExternalContentActivation {
        let recipe = ryeos_app::managed_external_content::ManagedActivationComponent {
            id: "runtime".to_owned(),
            storage: ManagedComponentStorage::Content,
            shape: ManagedActivationComponentShape::WholeArchiveTree {
                source: "package".to_owned(),
                prefix: "runtime-root".to_owned(),
                bounds: bounds.clone(),
            },
        };
        ResolvedManagedExternalContentActivation {
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            document: ryeos_app::managed_external_content::ManagedExternalContentActivation {
                schema: ryeos_app::managed_external_content::MANAGED_ACTIVATION_SCHEMA.to_owned(),
                consumer_ref: "worker:fixture/hosted".to_owned(),
                sources: vec![source],
                components: vec![recipe.clone()],
            },
            components: vec![ResolvedManagedActivationComponent {
                recipe,
                expected_manifest_hash: "d".repeat(64),
                expected_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                    .to_owned(),
                declaration_kind: ryeos_engine::external_content::ExternalContentKind::Tree,
                capture_bounds: bounds,
                expected_file_sha256: None,
            }],
        }
    }

    fn whole_source() -> ManagedActivationSource {
        ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/runtime.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 65536,
            maximum_expanded_bytes: 65536,
            maximum_entries: 8,
            members: Vec::new(),
        }
    }

    fn whole_bounds() -> ManagedActivationComponentBounds {
        ManagedActivationComponentBounds {
            maximum_entries: 4,
            maximum_depth: 3,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 2048,
        }
    }

    fn test_state_store(root: &std::path::Path) -> ryeos_app::state_store::StateStore {
        let identity =
            ryeos_app::identity::NodeIdentity::create(&root.join("identity/node-key.pem")).unwrap();
        let signer = std::sync::Arc::new(
            ryeos_app::state_store::NodeIdentitySigner::from_identity(&identity),
        );
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        ryeos_app::state_store::StateStore::new_with_head_trust(
            root.to_path_buf(),
            root.join(".ai/state"),
            root.join("runtime.sqlite3"),
            signer,
            ryeos_app::write_barrier::WriteBarrier::new(),
            std::sync::Arc::new(trust),
        )
        .unwrap()
    }

    #[test]
    fn durable_attempt_is_the_submission_lease_and_restart_fence() {
        let root = tempfile::tempdir().unwrap();
        let state_store = test_state_store(root.path());
        let operation = serde_json::json!({
            "operation_type": MANAGED_ACTIVATION_OPERATION,
            "schema": "fixture",
        });
        let operation_digest = ryeos_state::objects::canonical_value_digest(&operation).unwrap();
        let job_id = activation_job_id(&operation_digest);

        let (planned, reused) =
            reserve_activation_job(&state_store, &job_id, &operation, 2).unwrap();
        assert!(!reused);
        assert_eq!(planned.state, ryeos_state::SyncJobState::Planned);

        let first = claim_activation_attempt(&state_store, &job_id, &operation, reused).unwrap();
        assert!(first.attempt_id.is_some());
        assert_eq!(first.job.state, ryeos_state::SyncJobState::Running);
        assert_eq!(first.job.attempt_count, 1);

        let concurrent = claim_activation_attempt(&state_store, &job_id, &operation, true).unwrap();
        assert!(concurrent.attempt_id.is_none());
        assert_eq!(concurrent.job.attempt_count, 1);
        assert_eq!(
            state_store
                .with_state_db(|db| db.list_sync_job_attempts(&job_id))
                .unwrap()
                .len(),
            1
        );

        assert_eq!(
            state_store
                .with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())
                .unwrap(),
            1
        );
        let retried = claim_activation_attempt(&state_store, &job_id, &operation, true).unwrap();
        assert!(retried.attempt_id.is_some());
        assert_eq!(retried.job.attempt_count, 2);

        assert_eq!(
            state_store
                .with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())
                .unwrap(),
            1
        );
        let exhausted = claim_activation_attempt(&state_store, &job_id, &operation, true).unwrap();
        assert!(exhausted.attempt_id.is_none());
        assert_eq!(exhausted.job.state, ryeos_state::SyncJobState::Failed);
        assert_eq!(exhausted.job.phase, "attempts_exhausted");
        assert_eq!(exhausted.job.attempt_count, 2);
    }

    #[test]
    fn durable_job_identity_includes_invocation_authority() {
        let program_digest = "b".repeat(64);
        let consumer_ref = "worker:fixture/hosted".to_owned();
        let publisher_fingerprint = "c".repeat(64);
        let activation_id =
            ryeos_state::objects::ExternalContentActivationReceipt::derive_activation_id(
                &program_digest,
                &consumer_ref,
                &publisher_fingerprint,
            )
            .unwrap();
        let mut operation = ManagedActivationJobOperation {
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            schema: "ryeos.external_content_activation_operation.v3".to_owned(),
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: program_digest,
            activation_id: activation_id.clone(),
            consumer_ref,
            publisher_fingerprint,
            operator_fingerprint: "d".repeat(64),
            operator_authority_digest: "4".repeat(64),
            policy_digest: "e".repeat(64),
            acquisition_mode: AcquisitionMode::Online,
            offline_archive_root: None,
            offline_archive_root_authority_digest: None,
        };
        let first_digest =
            ryeos_state::objects::canonical_value_digest(&operation.to_value().unwrap()).unwrap();
        let first = activation_job_id(&first_digest);
        assert_eq!(
            retained_activation_operation_digest(&first, &operation.to_value().unwrap()).unwrap(),
            first_digest
        );
        assert!(
            retained_activation_operation_digest(
                &format!("external-activation:{}", "0".repeat(64)),
                &operation.to_value().unwrap(),
            )
            .is_err()
        );
        operation.policy_digest = "f".repeat(64);
        let later_digest =
            ryeos_state::objects::canonical_value_digest(&operation.to_value().unwrap()).unwrap();
        let later = activation_job_id(&later_digest);

        assert_ne!(first, later);
        assert!(first.len() <= 128);
        assert!(first.ends_with(&first_digest));

        operation.offline_archive_root = Some("archives".to_owned());
        assert!(
            operation.validate().is_err(),
            "online acquisition must reject offline root authority"
        );
        operation.acquisition_mode = AcquisitionMode::Offline;
        operation.offline_archive_root_authority_digest = Some("a".repeat(64));
        operation.validate().unwrap();
        operation.offline_archive_root = Some("../ambient".to_owned());
        assert!(
            operation.validate().is_err(),
            "offline root name must use the node-policy namespace"
        );
    }

    #[test]
    fn exhausted_interrupted_job_folds_authoritative_receipt_without_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let runtime_state = root.path().join(".ai/state");
        let runtime_db = root.path().join("runtime.sqlite3");
        let identity =
            ryeos_app::identity::NodeIdentity::create(&root.path().join("identity/node-key.pem"))
                .unwrap();
        let signer = std::sync::Arc::new(
            ryeos_app::state_store::NodeIdentitySigner::from_identity(&identity),
        );
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
        let state_store = ryeos_app::state_store::StateStore::new_with_head_trust(
            root.path().to_path_buf(),
            runtime_state,
            runtime_db,
            signer,
            write_barrier.clone(),
            std::sync::Arc::new(trust),
        )
        .unwrap();

        let state_authority = state_store.pinned_state_authority().unwrap();
        let guard = state_authority.acquire_shared_guard().unwrap();
        let cas = state_authority.cas_store().unwrap();
        let manifest = ryeos_state::objects::ExternalContentManifestObject {
            schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
            kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            entries: Vec::new(),
            entry_count: 0,
            total_bytes: 0,
        };
        manifest.validate().unwrap();
        let manifest_hash = cas
            .put_object(&serde_json::to_value(manifest).unwrap())
            .unwrap()
            .hash;

        let mut activation = test_whole_activation(whole_source(), whole_bounds());
        activation.components[0].expected_manifest_hash = manifest_hash.clone();
        let operator_fingerprint = "e".repeat(64);
        let binding = ryeos_state::objects::ExternalContentBinding::active(
            manifest_hash,
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            activation.document.consumer_ref.clone(),
            activation.publisher_fingerprint.clone(),
            operator_fingerprint.clone(),
        )
        .unwrap();
        let binding_hash = cas.put_object(&binding.to_value().unwrap()).unwrap().hash;
        let head_signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&identity);
        state_store
            .with_state_db(|db| {
                db.advance_generic_head_ref(
                    ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                    &binding.binding_id,
                    &binding_hash,
                    None,
                    &head_signer,
                    &guard,
                )
            })
            .unwrap();

        let operation = ManagedActivationJobOperation {
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            schema: "ryeos.external_content_activation_operation.v3".to_owned(),
            activation_ref: activation.activation_ref.clone(),
            activation_program_digest: activation.activation_program_digest.clone(),
            activation_id:
                ryeos_state::objects::ExternalContentActivationReceipt::derive_activation_id(
                    &activation.activation_program_digest,
                    &activation.document.consumer_ref,
                    &activation.publisher_fingerprint,
                )
                .unwrap(),
            consumer_ref: activation.document.consumer_ref.clone(),
            publisher_fingerprint: activation.publisher_fingerprint.clone(),
            operator_fingerprint: operator_fingerprint.clone(),
            operator_authority_digest: "f".repeat(64),
            policy_digest: "1".repeat(64),
            acquisition_mode: AcquisitionMode::Online,
            offline_archive_root: None,
            offline_archive_root_authority_digest: None,
        };
        operation.validate().unwrap();
        let receipt = ryeos_state::objects::ExternalContentActivationReceipt::new(
            activation.activation_ref.clone(),
            activation.activation_program_digest.clone(),
            activation.document.consumer_ref.clone(),
            activation.publisher_fingerprint.clone(),
            identity.fingerprint().to_owned(),
            operation.policy_digest.clone(),
            vec![
                ryeos_state::objects::ExternalContentActivationComponentReceipt {
                    id: activation.components[0].recipe.id.clone(),
                    binding_hash,
                },
            ],
            operator_fingerprint,
        )
        .unwrap();
        let receipt_hash = cas.put_object(&receipt.to_value().unwrap()).unwrap().hash;
        state_store
            .with_state_db(|db| {
                db.advance_generic_head_ref(
                    ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE,
                    &operation.activation_id,
                    &receipt_hash,
                    None,
                    &head_signer,
                    &guard,
                )
            })
            .unwrap();
        drop(guard);

        let operation_digest =
            ryeos_state::objects::canonical_value_digest(&operation.to_value().unwrap()).unwrap();
        let job_id = activation_job_id(&operation_digest);
        let existing = state_store
            .with_state_db(|db| {
                db.create_sync_job(&ryeos_state::NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
                    operation: operation.to_value()?,
                    peer: None,
                    roots: Vec::new(),
                    heads: Vec::new(),
                    max_attempts: 1,
                })?;
                db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
                    attempt_id: "attempt:receipt-recovery".to_owned(),
                    job_id: job_id.clone(),
                    worker_id: None,
                    phase: "publishing".to_owned(),
                })?;
                assert_eq!(db.reconcile_interrupted_sync_job_attempts()?, 1);
                db.get_sync_job(&job_id)?
                    .ok_or_else(|| anyhow::anyhow!("job absent"))
            })
            .unwrap();
        assert_eq!(existing.state, ryeos_state::SyncJobState::Retryable);
        assert_eq!(existing.attempt_count, existing.max_attempts);

        let response = complete_job_from_current_receipt(
            ActivationReceiptAuthority {
                state_store: &state_store,
                write_barrier: &write_barrier,
                node_fingerprint: identity.fingerprint(),
            },
            CurrentActivationAuthority {
                activation: &activation,
                operation: &operation,
            },
            &job_id,
            &existing,
        )
        .unwrap()
        .expect("authoritative receipt must complete without acquisition");
        assert!(response.idempotent);
        assert_eq!(response.receipt_hash, Some(receipt_hash.clone()));
        let completed = state_store
            .with_state_db(|db| db.get_sync_job(&job_id))
            .unwrap()
            .unwrap();
        assert_eq!(completed.state, ryeos_state::SyncJobState::Completed);
        assert_eq!(completed.phase, "completed_from_authoritative_receipt");
        assert_eq!(completed.attempt_count, 1);
        assert_eq!(completed.roots, vec![receipt_hash]);
    }

    #[test]
    fn staging_preflight_preserves_the_node_free_space_floor() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let activation = test_whole_activation(whole_source(), whole_bounds());
        let mut policy = test_policy();

        staging
            .create_symlink(OsStr::new("partial-link"), b"internal-target")
            .unwrap();
        reset_activation_staging(&staging).unwrap();
        assert!(
            staging
                .read_symlink_target(OsStr::new("partial-link"), 128)
                .unwrap()
                .is_none()
        );

        require_staging_capacity(&staging, &activation, &policy).unwrap();

        policy.minimum_free_bytes = staging.filesystem_capacity().unwrap().available_bytes;
        let error = require_staging_capacity(&staging, &activation, &policy).unwrap_err();
        assert!(error.to_string().contains("staging requires"));
    }

    #[test]
    fn cache_reconciliation_removes_crash_orphans_before_retry_accounting() {
        let root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let orphan_name = OsStr::new(".secure.tmp.4242.7");
        let mut orphan = cache
            .open_regular_create(orphan_name, true, true, 0o600)
            .unwrap();
        std::io::Write::write_all(&mut orphan, b"partial archive").unwrap();
        orphan.sync_all().unwrap();

        reconcile_activation_cache(&cache).unwrap();

        assert!(cache.open_regular(orphan_name, false).unwrap().is_none());
    }

    #[test]
    fn cache_reservation_deterministically_evicts_rebuildable_archives() {
        let root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        for digest in [&first, &second] {
            let mut file = cache
                .open_regular_create(OsStr::new(digest), true, true, 0o600)
                .unwrap();
            std::io::Write::write_all(&mut file, b"four").unwrap();
            file.sync_all().unwrap();
        }
        let mut source = whole_source();
        source.sha256 = "c".repeat(64);
        source.maximum_compressed_bytes = 6;
        let mut policy = test_policy();
        policy.cache_budget_bytes = 10;

        reserve_archive_cache(&cache, &source, &policy, 2).unwrap();

        assert!(
            cache
                .open_regular(OsStr::new(&first), false)
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .open_regular(OsStr::new(&second), false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn newly_published_wrong_archive_is_not_retained() {
        let root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let digest = "a".repeat(64);
        let name = OsStr::new(&digest);
        let mut bytes = b"wrong archive bytes".as_slice();
        let archive = cache
            .atomic_create_pinned_regular_from_reader(name, &mut bytes, 1024, 0o600)
            .unwrap()
            .unwrap()
            .0;

        let error = retain_verified_archive(&cache, archive, 1024, &digest, "downloaded fixture")
            .unwrap_err();

        assert!(error.to_string().contains("digest changed"));
        assert!(cache.open_regular(name, false).unwrap().is_none());
    }

    #[test]
    fn recovery_uses_job_id_to_remove_terminal_staging_but_retains_active_staging() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let terminal_digest = "a".repeat(64);
        let active_digest = "b".repeat(64);
        for digest in [&terminal_digest, &active_digest] {
            let job = staging
                .open_or_create_child(OsStr::new(digest), 0o700)
                .unwrap();
            let mut retained = job
                .open_regular_create(OsStr::new("retained"), true, true, 0o600)
                .unwrap();
            std::io::Write::write_all(&mut retained, b"rebuildable staging").unwrap();
            retained.sync_all().unwrap();
        }

        let removed = reconcile_retained_staging_entries(&staging, |job_id| {
            if job_id == activation_job_id(&terminal_digest) {
                Ok(Some(ryeos_state::SyncJobState::Completed))
            } else if job_id == activation_job_id(&active_digest) {
                Ok(Some(ryeos_state::SyncJobState::Retryable))
            } else {
                unreachable!("unexpected staging job")
            }
        })
        .unwrap();

        assert_eq!(removed, 1);
        assert!(
            staging
                .open_child_directory(OsStr::new(&terminal_digest))
                .unwrap()
                .is_none()
        );
        assert!(
            staging
                .open_child_directory(OsStr::new(&active_digest))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn selected_archive_members_are_bounded_and_staged_by_consumer_id() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let archive_path = root.path().join("fixture.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest.clone(),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let activation = test_activation(source.clone());
        let policy = test_policy();
        let archive = pinned_archive_fixture(&archive_path);
        extract_archive(archive, &source, &activation, &staging, &policy).unwrap();
        let mut staged = staging
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(&mut staged, 64, &digest, "fixture").unwrap();
    }

    #[test]
    fn selected_archive_members_build_a_descriptor_rooted_tree() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let archive_file = root.path().join("tree.tar.gz");
        {
            let file = std::fs::File::create(&archive_file).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/tree.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest.clone(),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let mut activation = test_activation(source.clone());
        let ManagedActivationComponentShape::Mapped { members } =
            &mut activation.document.components[0].shape
        else {
            panic!("fixture must remain mapped");
        };
        members[0].target = Some("tools/runtime".to_owned());
        activation.components[0].recipe = activation.document.components[0].clone();
        activation.components[0].declaration_kind =
            ryeos_engine::external_content::ExternalContentKind::Tree;
        activation.components[0].capture_bounds.maximum_entries = 2;
        activation.components[0].capture_bounds.maximum_depth = 2;
        let policy = test_policy();
        extract_archive(
            pinned_archive_fixture(&archive_file),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .unwrap();
        let component = staging
            .open_child_directory(OsStr::new("runtime"))
            .unwrap()
            .unwrap();
        let tools = component
            .open_child_directory(OsStr::new("tools"))
            .unwrap()
            .unwrap();
        let mut staged = tools
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(&mut staged, 64, &digest, "tree fixture").unwrap();
    }

    #[test]
    fn selected_archive_refuses_links_and_executable_mode_drift() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest,
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let activation = test_activation(source.clone());
        let policy = test_policy();

        let link_archive = root.path().join("link.tar.gz");
        {
            let file = std::fs::File::create(&link_archive).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../outside").unwrap();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, std::io::empty()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&link_archive),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("managed activation must reject archive links");
        assert!(error.to_string().contains("link or special entry"));

        let mode_archive = root.path().join("mode.tar.gz");
        {
            let file = std::fs::File::create(&mode_archive).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&mode_archive),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("managed activation must reject selected-member mode drift");
        assert!(error.to_string().contains("executable mode changed"));
    }

    #[test]
    fn whole_archive_tree_stages_files_directories_and_internal_symlinks_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let archive_path = root.path().join("whole.tar.gz");
        let bytes = b"runtime";
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            for path in ["runtime-root/", "runtime-root/bin/"] {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o700);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            let mut file_header = tar::Header::new_gnu();
            file_header.set_path("runtime-root/bin/runtime").unwrap();
            file_header.set_size(bytes.len() as u64);
            file_header.set_mode(0o751);
            file_header.set_cksum();
            archive.append(&file_header, bytes.as_slice()).unwrap();
            let mut link_header = tar::Header::new_gnu();
            link_header.set_path("runtime-root/bin/current").unwrap();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_link_name("runtime").unwrap();
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_cksum();
            archive.append(&link_header, std::io::empty()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let policy = test_policy();
        for _ in 0..2 {
            extract_archive(
                pinned_archive_fixture(&archive_path),
                &source,
                &activation,
                &staging,
                &policy,
            )
            .unwrap();
        }
        let component = staging
            .open_child_directory(OsStr::new("runtime"))
            .unwrap()
            .unwrap();
        let bin = component
            .open_child_directory(OsStr::new("bin"))
            .unwrap()
            .unwrap();
        let mut runtime = bin
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(
            &mut runtime,
            1024,
            &lillux::sha256_hex(bytes),
            "whole-tree fixture",
        )
        .unwrap();
        assert_eq!(
            lillux::normalized_portable_regular_mode(&runtime.metadata().unwrap()).unwrap(),
            0o755
        );
        assert_eq!(
            bin.read_symlink_target(OsStr::new("current"), 64)
                .unwrap()
                .unwrap(),
            b"runtime"
        );
    }

    #[test]
    fn whole_archive_tree_refuses_escaping_symlinks_and_hardlinks() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let policy = test_policy();

        let escaping = root.path().join("escaping.tar.gz");
        {
            let file = std::fs::File::create(&escaping).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            for path in ["runtime-root/", "runtime-root/bin/"] {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            let mut header = tar::Header::new_gnu();
            header.set_path("runtime-root/bin/escape").unwrap();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../../outside").unwrap();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            archive.append(&header, std::io::empty()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&escaping),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("whole-tree symlink must remain inside the stripped prefix");
        assert!(format!("{error:#}").contains("escapes the realization root"));

        let hardlink = root.path().join("hardlink.tar.gz");
        {
            let file = std::fs::File::create(&hardlink).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let mut root_header = tar::Header::new_gnu();
            root_header.set_path("runtime-root/").unwrap();
            root_header.set_entry_type(tar::EntryType::Directory);
            root_header.set_size(0);
            root_header.set_mode(0o755);
            root_header.set_cksum();
            archive.append(&root_header, std::io::empty()).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("runtime-root/copy").unwrap();
            header.set_entry_type(tar::EntryType::Link);
            header.set_link_name("runtime-root/original").unwrap();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append(&header, std::io::empty()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&hardlink),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("whole-tree activation must reject hardlinks");
        assert!(error.to_string().contains("hardlink or special entry"));
    }

    #[test]
    fn whole_archive_tree_refuses_a_chained_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let archive_path = root.path().join("chained-symlink-escape.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            for path in ["runtime-root/", "runtime-root/dir/"] {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            for (path, target) in [
                ("runtime-root/a", "."),
                ("runtime-root/dir/b", "../a/../outside"),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name(target).unwrap();
                header.set_size(0);
                header.set_mode(0o777);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
        }

        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let error = extract_archive(
            pinned_archive_fixture(&archive_path),
            &source,
            &activation,
            &staging,
            &test_policy(),
        )
        .expect_err("the complete whole-tree symlink graph must remain below its root");
        assert!(
            format!("{error:#}").contains("escapes the realization root through the symlink graph"),
            "unexpected chained-symlink refusal: {error:#}"
        );
    }

    #[test]
    fn whole_archive_tree_refuses_namespace_collisions_and_missing_prefixes() {
        let mut namespace = BTreeMap::new();
        insert_archive_namespace(&mut namespace, "bin", ArchiveNamespaceKind::File).unwrap();
        assert!(
            insert_archive_namespace(&mut namespace, "bin/runtime", ArchiveNamespaceKind::File,)
                .is_err()
        );

        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let archive_path = root.path().join("missing-prefix.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let bytes = b"runtime";
            let mut header = tar::Header::new_gnu();
            header.set_path("other/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append(&header, bytes.as_slice()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let error = extract_archive(
            pinned_archive_fixture(&archive_path),
            &source,
            &activation,
            &staging,
            &test_policy(),
        )
        .expect_err("whole-tree prefix must be explicit and exact");
        assert!(error.to_string().contains("missing its whole-tree prefix"));
    }

    #[test]
    fn whole_archive_tree_refuses_sparse_entries_and_nonzero_trailing_streams() {
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let policy = test_policy();

        let sparse = root.path().join("sparse.tar.gz");
        {
            let file = std::fs::File::create(&sparse).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let mut root_header = tar::Header::new_gnu();
            root_header.set_path("runtime-root/").unwrap();
            root_header.set_entry_type(tar::EntryType::Directory);
            root_header.set_size(0);
            root_header.set_mode(0o755);
            root_header.set_cksum();
            archive.append(&root_header, std::io::empty()).unwrap();
            let mut sparse_header = tar::Header::new_gnu();
            sparse_header.set_path("runtime-root/sparse").unwrap();
            sparse_header.set_entry_type(tar::EntryType::GNUSparse);
            sparse_header.set_size(0);
            let sparse_fields = sparse_header.as_gnu_mut().unwrap();
            sparse_fields.set_real_size(0);
            sparse_fields.set_is_extended(true);
            sparse_header.set_mode(0o644);
            sparse_header.set_cksum();
            archive.append(&sparse_header, std::io::empty()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&sparse),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("whole-tree activation must reject GNU sparse entries");
        assert!(
            format!("{error:#}").contains("sparse entry"),
            "unexpected sparse refusal: {error:#}"
        );

        let trailing = root.path().join("trailing.tar.gz");
        {
            let file = std::fs::File::create(&trailing).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            for path in ["runtime-root/", "runtime-root/bin/"] {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&trailing)
                .unwrap();
            let mut extra = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            extra.write_all(b"hidden payload").unwrap();
            extra.finish().unwrap();
        }
        let error = extract_archive(
            pinned_archive_fixture(&trailing),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("whole-tree activation must reject a second non-zero gzip payload");
        assert!(error.to_string().contains("non-zero data"));
    }

    #[test]
    fn raw_archive_iteration_bounds_extension_payloads_before_preprocessing() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        let policy = test_policy();

        for (name, entry_type, bytes) in [
            (
                "oversized-pax",
                tar::EntryType::XHeader,
                MAX_MANAGED_TAR_EXTENSION_BYTES + 1,
            ),
            (
                "oversized-longname",
                tar::EntryType::GNULongName,
                ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES as u64 + 2,
            ),
        ] {
            let archive_path = root.path().join(format!("{name}.tar.gz"));
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("extension").unwrap();
            header.set_entry_type(entry_type);
            header.set_size(bytes);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append(&header, std::io::repeat(0).take(bytes))
                .unwrap();
            archive.into_inner().unwrap().finish().unwrap();

            let error = extract_archive(
                pinned_archive_fixture(&archive_path),
                &source,
                &activation,
                &staging,
                &policy,
            )
            .expect_err("raw extension payload must be refused before tar preprocessing");
            assert!(
                format!("{error:#}").contains("extension exceeds its byte bound"),
                "unexpected {name} refusal: {error:#}"
            );
        }
    }

    #[test]
    fn raw_archive_iteration_accepts_a_bounded_local_pax_path() {
        fn pax_record(key: &str, value: &str) -> Vec<u8> {
            let suffix = format!(" {key}={value}\n");
            let mut length = suffix.len() + 1;
            loop {
                let next = suffix.len() + length.to_string().len();
                if next == length {
                    return format!("{length}{suffix}").into_bytes();
                }
                length = next;
            }
        }

        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let archive_path = root.path().join("pax-path.tar.gz");
        let long_name = "x".repeat(110);
        let archive_path_value = format!("runtime-root/nested/{long_name}");
        let payload = pax_record("path", &archive_path_value);
        let bytes = b"pax runtime";
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);

            let mut root_header = tar::Header::new_gnu();
            root_header.set_path("runtime-root/").unwrap();
            root_header.set_entry_type(tar::EntryType::Directory);
            root_header.set_size(0);
            root_header.set_mode(0o755);
            root_header.set_cksum();
            archive.append(&root_header, std::io::empty()).unwrap();

            let mut pax_header = tar::Header::new_gnu();
            pax_header.set_path("pax-header").unwrap();
            pax_header.set_entry_type(tar::EntryType::XHeader);
            pax_header.set_size(payload.len() as u64);
            pax_header.set_mode(0o644);
            pax_header.set_cksum();
            archive.append(&pax_header, payload.as_slice()).unwrap();

            let mut file_header = tar::Header::new_gnu();
            file_header.set_path("placeholder").unwrap();
            file_header.set_size(bytes.len() as u64);
            file_header.set_mode(0o644);
            file_header.set_cksum();
            archive.append(&file_header, bytes.as_slice()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }

        let source = whole_source();
        let activation = test_whole_activation(source.clone(), whole_bounds());
        extract_archive(
            pinned_archive_fixture(&archive_path),
            &source,
            &activation,
            &staging,
            &test_policy(),
        )
        .unwrap();

        let component = staging
            .open_child_directory(OsStr::new("runtime"))
            .unwrap()
            .unwrap();
        let nested = component
            .open_child_directory(OsStr::new("nested"))
            .unwrap()
            .unwrap();
        let mut staged = nested
            .open_regular(OsStr::new(&long_name), false)
            .unwrap()
            .unwrap();
        verify_open_file(
            &mut staged,
            1024,
            &lillux::sha256_hex(bytes),
            "PAX path fixture",
        )
        .unwrap();
    }

    #[test]
    fn whole_archive_tree_enforces_file_depth_and_aggregate_bounds() {
        let root = tempfile::tempdir().unwrap();
        let archive_path = root.path().join("bounds.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let mut root_header = tar::Header::new_gnu();
            root_header.set_path("runtime-root/").unwrap();
            root_header.set_entry_type(tar::EntryType::Directory);
            root_header.set_size(0);
            root_header.set_mode(0o755);
            root_header.set_cksum();
            archive.append(&root_header, std::io::empty()).unwrap();
            let bytes = b"12345678";
            let mut file_header = tar::Header::new_gnu();
            file_header.set_path("runtime-root/a/b/file").unwrap();
            file_header.set_size(bytes.len() as u64);
            file_header.set_mode(0o644);
            file_header.set_cksum();
            archive.append(&file_header, bytes.as_slice()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let source = whole_source();
        for (bounds, expected) in [
            (
                ManagedActivationComponentBounds {
                    maximum_entries: 4,
                    maximum_depth: 2,
                    maximum_file_bytes: 8,
                    maximum_total_bytes: 8,
                },
                "depth bound",
            ),
            (
                ManagedActivationComponentBounds {
                    maximum_entries: 4,
                    maximum_depth: 4,
                    maximum_file_bytes: 7,
                    maximum_total_bytes: 8,
                },
                "file exceeds",
            ),
            (
                ManagedActivationComponentBounds {
                    maximum_entries: 4,
                    maximum_depth: 4,
                    maximum_file_bytes: 8,
                    maximum_total_bytes: 7,
                },
                "aggregate byte bound",
            ),
        ] {
            let staging_root = tempfile::tempdir().unwrap();
            let staging = lillux::PinnedDirectory::open(staging_root.path())
                .unwrap()
                .unwrap();
            let activation = test_whole_activation(source.clone(), bounds);
            let error = extract_archive(
                pinned_archive_fixture(&archive_path),
                &source,
                &activation,
                &staging,
                &test_policy(),
            )
            .expect_err("whole-tree capture must enforce every signed bound");
            assert!(
                error.to_string().contains(expected),
                "unexpected refusal for {expected}: {error:#}"
            );
        }
    }

    #[test]
    fn invalid_cached_archive_is_removed_and_offline_activation_refuses() {
        let root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = b"expected archive";
        let digest = lillux::sha256_hex(expected);
        std::fs::write(root.path().join(&digest), b"corrupt archive").unwrap();
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: digest.clone(),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: "a".repeat(64),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let policy = test_policy();

        let error = obtain_archive(&cache, &source, AcquisitionMode::Offline, &policy, None)
            .expect_err("offline activation must refuse corrupt cache content");
        assert!(format!("{error:#}").contains("failed exact verification"));
        assert!(
            cache
                .open_regular(OsStr::new(&digest), false)
                .unwrap()
                .is_none(),
            "invalid cache coordinate must be reusable by a later online retry"
        );
    }

    #[test]
    fn admitted_offline_root_populates_only_the_digest_keyed_private_cache() {
        let cache_root = tempfile::tempdir().unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(cache_root.path())
            .unwrap()
            .unwrap();
        let source_directory = lillux::PinnedDirectory::open(source_root.path())
            .unwrap()
            .unwrap();
        let bytes = b"exact offline archive";
        let digest = lillux::sha256_hex(bytes);
        std::fs::write(source_root.path().join("fixture.tar.gz"), bytes).unwrap();
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: digest.clone(),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: Vec::new(),
        };

        let imported = obtain_archive(
            &cache,
            &source,
            AcquisitionMode::Offline,
            &test_policy(),
            Some(&source_directory),
        )
        .unwrap();
        let observed = imported.read_bounded(4096).unwrap();
        assert_eq!(observed, bytes);
        assert!(
            cache
                .open_regular(OsStr::new(&digest), false)
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .open_regular(OsStr::new("fixture.tar.gz"), false)
                .unwrap()
                .is_none(),
            "publisher filename must not become private cache authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn admitted_offline_root_refuses_a_symlinked_archive() {
        use std::os::unix::fs::symlink;

        let cache_root = tempfile::tempdir().unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(cache_root.path())
            .unwrap()
            .unwrap();
        let source_directory = lillux::PinnedDirectory::open(source_root.path())
            .unwrap()
            .unwrap();
        let bytes = b"exact offline archive";
        let digest = lillux::sha256_hex(bytes);
        std::fs::write(source_root.path().join("target.tar.gz"), bytes).unwrap();
        symlink("target.tar.gz", source_root.path().join("fixture.tar.gz")).unwrap();
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: digest,
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            maximum_entries: 8,
            members: Vec::new(),
        };

        let error = obtain_archive(
            &cache,
            &source,
            AcquisitionMode::Offline,
            &test_policy(),
            Some(&source_directory),
        )
        .expect_err("offline archive symlink must not be followed");
        assert!(format!("{error:#}").contains("offline activation archive"));
    }

    #[test]
    fn offline_archive_filename_is_exactly_the_signed_canonical_url_leaf() {
        let mut source = whole_source();
        source.url = "https://releases.example.test/runtime-v1.tar.gz".to_owned();
        assert_eq!(offline_archive_name(&source).unwrap(), "runtime-v1.tar.gz");

        for invalid in [
            "https://releases.example.test/runtime/",
            "https://releases.example.test/runtime%20v1.tar.gz",
            "https://releases.example.test/",
        ] {
            source.url = invalid.to_owned();
            assert!(
                offline_archive_name(&source).is_err(),
                "offline acquisition accepted ambiguous URL leaf {invalid}"
            );
        }
    }
}
