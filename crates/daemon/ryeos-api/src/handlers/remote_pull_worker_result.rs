//! Explicitly pull one terminal hosted-worker candidate back to its configured
//! owner and apply it to the exact clean-base source workspace.
//!
//! The target supplies node-signed testimony reconstructed from its existing
//! chain, exact command completion, candidate capture, and fresh closure/base
//! verification. Frozen/retained does not assert evaluator qualification or
//! change the target worker's lifecycle (which may already be completed).
//! This source operation retains that testimony in the ordinary sync-job
//! ledger and delegates transport/apply to the existing remote pull pipeline.
//! It never publishes or discards the target candidate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config::{self, ProjectSyncScope, RemoteConfig};
use ryeos_app::hosted_candidate_result::{
    HostedCandidateResultRequest, HostedCandidateResultResponse,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use ryeos_state::{
    FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt, SyncJobAttemptState, SyncJobRecord,
    SyncJobState, SyncJobUpdate,
};

const OPERATION_TYPE: &str = "remote_hosted_worker_result_pull";
const OPERATION_SCHEMA: &str = "ryeos.remote_hosted_worker_result_pull_operation.v2";
const INTENT_SCHEMA: &str = "ryeos.remote_hosted_worker_result_pull_intent.v1";
const RESPONSE_SCHEMA: &str = "ryeos.remote_hosted_worker_result_pull_result.v2";
const TARGET_SERVICE: &str = "service:worker-executions/candidate-result";

fn default_remote() -> String {
    "default".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    pub project: PathBuf,
    pub chain_root_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullIntent {
    schema: String,
    remote: String,
    remote_url: String,
    source_site_id: String,
    target_site_id: String,
    target_principal_id: String,
    target_signing_key: String,
    local_project_path: String,
    target_project_path: String,
    operator_fingerprint: String,
    operator_authority_digest: String,
    chain_root_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullOperation {
    operation_type: String,
    schema: String,
    intent: PullIntent,
    candidate_result: HostedCandidateResultResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    schema: String,
    job_id: String,
    remote: String,
    local_project_path: String,
    candidate_result: HostedCandidateResultResponse,
    cas_objects_fetched: usize,
    files_updated: usize,
    files_deleted: usize,
    applied: bool,
    published: bool,
    idempotent: bool,
}

impl Response {
    fn from_value(value: Value) -> Result<Self> {
        if value.get("schema").and_then(Value::as_str) != Some(RESPONSE_SCHEMA) {
            bail!("hosted worker-result pull result schema is not current");
        }
        serde_json::from_value(value).context("parse retained hosted worker-result pull result")
    }
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

impl PullIntent {
    fn validate(&self) -> Result<()> {
        if self.schema != INTENT_SCHEMA {
            bail!("hosted worker-result pull intent schema is not current");
        }
        for (label, value) in [
            ("remote name", self.remote.as_str()),
            ("remote URL", self.remote_url.as_str()),
            ("local project path", self.local_project_path.as_str()),
            ("target project path", self.target_project_path.as_str()),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                bail!("hosted worker-result pull {label} is invalid");
            }
        }
        if !Path::new(&self.local_project_path).is_absolute() {
            bail!("hosted worker-result pull local project path is not absolute");
        }
        if config::normalize_url(&self.remote_url)? != self.remote_url {
            bail!("hosted worker-result pull remote URL is not canonical");
        }
        config::validate_remote_project_path(&self.target_project_path)?;
        ryeos_app::identity::validate_canonical_site_id(&self.source_site_id)?;
        ryeos_app::identity::validate_canonical_site_id(&self.target_site_id)?;
        ryeos_runtime::validate_runtime_thread_id(&self.chain_root_id)
            .map_err(|error| anyhow::anyhow!(error))?;
        validate_hash("operator fingerprint", &self.operator_fingerprint)?;
        validate_hash("operator authority digest", &self.operator_authority_digest)?;
        let target = self
            .target_principal_id
            .strip_prefix("fp:")
            .context("hosted worker-result pull target principal is not canonical")?;
        validate_hash("target principal fingerprint", target)?;
        let target_key = config::decode_signing_key(&self.target_signing_key)?;
        if lillux::crypto::fingerprint(&target_key) != target {
            bail!("hosted worker-result pull target key differs from target principal");
        }
        Ok(())
    }

    fn job_id(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "remote-worker-result:{}",
            ryeos_state::objects::canonical_value_digest(&serde_json::to_value(self)?)?
        ))
    }

    fn owner_principal(&self) -> String {
        format!("fp:{}", self.operator_fingerprint)
    }
}

impl PullOperation {
    fn new(intent: PullIntent, candidate_result: HostedCandidateResultResponse) -> Result<Self> {
        let value = Self {
            operation_type: OPERATION_TYPE.to_owned(),
            schema: OPERATION_SCHEMA.to_owned(),
            intent,
            candidate_result,
        };
        value.validate_portable()?;
        Ok(value)
    }

    fn validate_portable(&self) -> Result<()> {
        if self.operation_type != OPERATION_TYPE || self.schema != OPERATION_SCHEMA {
            bail!("hosted worker-result pull operation schema or type is not current");
        }
        self.intent.validate()?;
        self.candidate_result.evidence.validate()?;
        let evidence = &self.candidate_result.evidence;
        if evidence.owner_principal != self.intent.owner_principal()
            || evidence.source_site_id != self.intent.source_site_id
            || evidence.target_site_id != self.intent.target_site_id
            || evidence.chain_root_id != self.intent.chain_root_id
            || evidence.target_project_path != self.intent.target_project_path
        {
            bail!("hosted worker-result testimony differs from retained pull intent");
        }
        Ok(())
    }

    fn from_value(value: Value) -> Result<Self> {
        if value.get("schema").and_then(Value::as_str) != Some(OPERATION_SCHEMA)
            || value.get("operation_type").and_then(Value::as_str) != Some(OPERATION_TYPE)
        {
            bail!("hosted worker-result pull operation schema or type is not current");
        }
        let operation: Self = serde_json::from_value(value)
            .context("parse retained hosted worker-result pull operation")?;
        operation.validate_portable()?;
        Ok(operation)
    }

    fn to_value(&self) -> Result<Value> {
        self.validate_portable()?;
        Ok(serde_json::to_value(self)?)
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_runtime::validate_runtime_thread_id(&req.chain_root_id)
        .map_err(|error| anyhow::anyhow!(error))
        .context("hosted worker-result chain root is not canonical")?;
    let operator_fingerprint =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    let operator_authority_digest =
        ryeos_app::operator_authority::admitted_operator_authority_digest(
            &state,
            &operator_fingerprint,
        )?;
    let (local_project_path, remote, target_project_path) =
        resolve_initial_route(&state, &req.remote, &req.project)?;
    let intent = PullIntent {
        schema: INTENT_SCHEMA.to_owned(),
        remote: req.remote,
        remote_url: remote.url.clone(),
        source_site_id: state.threads.site_id().to_owned(),
        target_site_id: remote.site_id.clone(),
        target_principal_id: remote.principal_id.clone(),
        target_signing_key: remote.signing_key.clone(),
        local_project_path,
        target_project_path,
        operator_fingerprint,
        operator_authority_digest,
        chain_root_id: req.chain_root_id,
    };
    intent.validate()?;
    let job_id = intent.job_id()?;
    let initial_result = if state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))?
        .is_none()
    {
        Some(query_candidate_result(&state, &remote, &intent, &ctx).await?)
    } else {
        None
    };
    let response = execute_operation(state, intent, initial_result, true).await?;
    Ok(serde_json::to_value(response)?)
}

async fn execute_operation(
    state: Arc<AppState>,
    proposed_intent: PullIntent,
    initial_result: Option<HostedCandidateResultResponse>,
    create_if_absent: bool,
) -> Result<Response> {
    let job_id = proposed_intent.job_id()?;
    let job = match state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))?
    {
        Some(job) => job,
        None if create_if_absent => {
            let candidate_result =
                initial_result.context("new hosted worker-result pull has no target testimony")?;
            let operation = PullOperation::new(proposed_intent.clone(), candidate_result)?;
            resolve_current_route(&state, &proposed_intent)?;
            validate_target_result(&operation)?;
            let roots = sorted_hashes([
                operation
                    .candidate_result
                    .evidence
                    .base_snapshot_hash
                    .clone(),
                operation
                    .candidate_result
                    .evidence
                    .candidate_snapshot_hash
                    .clone(),
            ]);
            match state.state_store.with_state_db(|db| {
                db.create_sync_job(&NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: OPERATION_TYPE.to_owned(),
                    operation: operation.to_value()?,
                    peer: Some(operation.intent.remote.clone()),
                    roots,
                    heads: Vec::new(),
                    max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
                })
            }) {
                Ok(job) => job,
                Err(create_error) => state
                    .state_store
                    .with_state_db(|db| db.get_sync_job(&job_id))?
                    .ok_or(create_error)?,
            }
        }
        None => bail!("durable hosted worker-result pull job disappeared"),
    };
    let operation = PullOperation::from_value(job.operation.clone())?;
    if operation.intent != proposed_intent || operation.intent.job_id()? != job_id {
        bail!("hosted worker-result pull job is bound to another request");
    }
    validate_job_binding(&job, &operation, &job_id)?;
    if job.state == SyncJobState::Completed {
        validate_target_result(&operation)?;
        let mut response = Response::from_value(
            job.result
                .clone()
                .context("completed hosted worker-result pull has no result")?,
        )?;
        validate_response(&response, &operation, &job_id)?;
        response.idempotent = true;
        return Ok(response);
    }
    if matches!(job.state, SyncJobState::Failed | SyncJobState::Cancelled) {
        bail!(
            "hosted worker-result pull job {job_id} is terminal in state {}: {}",
            job.state.as_str(),
            job.last_error
                .as_deref()
                .unwrap_or("no retained diagnostic")
        );
    }
    if job.state == SyncJobState::Running {
        bail!("hosted worker-result pull job {job_id} already has an active attempt");
    }

    let remote = match resolve_current_route(&state, &operation.intent) {
        Ok(remote) => remote,
        Err(error) => {
            terminalize_without_attempt(&state, &job, "authority_changed", &error)?;
            return Err(error);
        }
    };
    if let Err(error) = validate_target_result(&operation) {
        terminalize_without_attempt(&state, &job, "testimony_invalid", &error)?;
        return Err(error);
    }
    let attempt_id = format!("remote-worker-result-attempt:{}", uuid::Uuid::new_v4());
    state.state_store.with_state_db(|db| {
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("remote-worker-result-pull".to_owned()),
            phase: "candidate_revalidation".to_owned(),
        })
    })?;

    let result = run_attempt(Arc::clone(&state), &operation, &remote).await;
    match result {
        Ok(mut response) => {
            let value = serde_json::to_value(&response)?;
            settle_attempt(
                &state,
                &job,
                &attempt_id,
                SyncJobAttemptState::Completed,
                SyncJobState::Completed,
                "completed",
                None,
                Some(value),
                vec![
                    operation
                        .candidate_result
                        .evidence
                        .candidate_snapshot_hash
                        .clone(),
                ],
            )?;
            response.idempotent = false;
            Ok(response)
        }
        Err(failure) => {
            let message = bounded_error(&format!("{:#}", failure.error));
            settle_attempt(
                &state,
                &job,
                &attempt_id,
                SyncJobAttemptState::Failed,
                if failure.retryable {
                    SyncJobState::Retryable
                } else {
                    SyncJobState::Failed
                },
                if failure.retryable {
                    "retryable"
                } else {
                    "failed"
                },
                Some(message),
                None,
                Vec::new(),
            )?;
            Err(failure.error)
        }
    }
}

async fn run_attempt(
    state: Arc<AppState>,
    operation: &PullOperation,
    remote: &RemoteConfig,
) -> std::result::Result<Response, AttemptFailure> {
    // The retained target signature is the immutable admission testimony.
    // Recovery must not depend on the target still exposing mutable session
    // projection after an explicit later disposition; content transport is
    // independently verified by its exact hashes.
    let (authority, base_tree) =
        validate_local_base(&state, operation).map_err(AttemptFailure::permanent)?;
    let client = RemoteClient::from_remote_cfg_as_retained_configured_operator(
        &state,
        remote,
        &operation.intent.operator_fingerprint,
        &operation.intent.operator_authority_digest,
    )
    .map_err(AttemptFailure::permanent)?;
    let evidence = &operation.candidate_result.evidence;
    let pull = crate::remote::pull::pull_results(
        &client,
        &authority,
        &evidence.base_snapshot_hash,
        &evidence.candidate_snapshot_hash,
        Some(Path::new(&operation.intent.local_project_path)),
        &base_tree,
    )
    .await
    .map_err(classify_pull_error)?;
    if pull.snapshot_hash != evidence.candidate_snapshot_hash {
        return Err(AttemptFailure::permanent(anyhow::anyhow!(
            "remote pull returned another candidate generation"
        )));
    }
    let response = Response {
        schema: RESPONSE_SCHEMA.to_owned(),
        job_id: operation
            .intent
            .job_id()
            .map_err(AttemptFailure::permanent)?,
        remote: operation.intent.remote.clone(),
        local_project_path: operation.intent.local_project_path.clone(),
        candidate_result: operation.candidate_result.clone(),
        cas_objects_fetched: pull.cas_objects_fetched,
        files_updated: pull.files_updated,
        files_deleted: pull.files_deleted,
        applied: true,
        published: false,
        idempotent: false,
    };
    validate_response(&response, operation, &response.job_id).map_err(AttemptFailure::permanent)?;
    Ok(response)
}

async fn query_candidate_result(
    state: &AppState,
    remote: &RemoteConfig,
    intent: &PullIntent,
    ctx: &HandlerContext,
) -> Result<HostedCandidateResultResponse> {
    let client = RemoteClient::from_remote_cfg_as_configured_operator(state, remote, ctx)?;
    query_candidate_result_with_client(
        &client,
        &intent.chain_root_id,
        &intent.source_site_id,
        &intent.owner_principal(),
        &intent.target_site_id,
        &remote.pinned_signing_key()?,
    )
    .await
}

/// Query and verify the target-owned candidate testimony without applying or
/// publishing it. Remote workflow completion reuses this boundary so the
/// candidate-result service remains the sole owner of hosted candidate proof.
pub(crate) async fn query_candidate_result_with_client(
    client: &RemoteClient,
    chain_root_id: &str,
    source_site_id: &str,
    expected_owner_principal: &str,
    expected_target_site_id: &str,
    target_signing_key: &lillux::crypto::VerifyingKey,
) -> Result<HostedCandidateResultResponse> {
    let request = HostedCandidateResultRequest {
        chain_root_id: chain_root_id.to_owned(),
        source_site_id: source_site_id.to_owned(),
    };
    let value = client
        .execute_service_result_with_total_timeout(
            TARGET_SERVICE,
            &BTreeMap::new(),
            None,
            &serde_json::to_value(&request)?,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            lillux::time::Duration::from_secs(30),
        )
        .await
        .context("query exact terminal hosted candidate")?;
    let response = HostedCandidateResultResponse::from_value(value)
        .context("decode hosted candidate testimony")?;
    response.validate_against(
        &request,
        expected_owner_principal,
        expected_target_site_id,
        target_signing_key,
    )?;
    Ok(response)
}

fn validate_target_result(operation: &PullOperation) -> Result<()> {
    operation.validate_portable()?;
    let request = HostedCandidateResultRequest {
        chain_root_id: operation.intent.chain_root_id.clone(),
        source_site_id: operation.intent.source_site_id.clone(),
    };
    operation.candidate_result.validate_against(
        &request,
        &operation.intent.owner_principal(),
        &operation.intent.target_site_id,
        &config::decode_signing_key(&operation.intent.target_signing_key)?,
    )?;
    let expected_identity = ryeos_app::launch_metadata::StableProjectIdentity::from_path(
        Path::new(&operation.intent.target_project_path),
        &operation.intent.target_site_id,
    )?;
    if operation.candidate_result.evidence.stable_project_identity
        != expected_identity.normalized_logical_key
    {
        bail!("target testimony belongs to another stable project identity");
    }
    Ok(())
}

fn validate_local_base(
    state: &AppState,
    operation: &PullOperation,
) -> Result<(ryeos_state::PinnedStateAuthority, ProjectTree)> {
    let evidence = &operation.candidate_result.evidence;
    let owner_principal = operation.intent.owner_principal();
    let principal_key = ryeos_state::refs::principal_storage_key(&owner_principal)?;
    let project_hash = lillux::sha256_hex(operation.intent.local_project_path.as_bytes());
    let current = state
        .state_store
        .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
    if current.as_deref() != Some(evidence.base_snapshot_hash.as_str()) {
        bail!(
            "local project HEAD differs from hosted execution base: expected {}, current is {:?}",
            evidence.base_snapshot_hash,
            current
        );
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [evidence.base_snapshot_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    if !closure.is_complete() || !closure.large_object_hashes.is_empty() {
        bail!("hosted execution base has no complete local full-project closure");
    }
    let snapshot_value = cas
        .get_object(&evidence.base_snapshot_hash)?
        .context("hosted execution base snapshot is absent locally")?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_value)?;
    let tree = ProjectTree::from_value(
        &cas.get_object(&snapshot.project_tree_hash)?
            .context("hosted execution base tree is absent locally")?,
    )?;
    let policy = ProjectSnapshotPolicy::from_value(
        &cas.get_object(&snapshot.effective_policy_hash)?
            .context("hosted execution base policy is absent locally")?,
    )?;
    if policy.sync_scope != ProjectSyncScope::FullProject {
        bail!("hosted worker-result pull requires a full_project admitted base");
    }
    ryeos_state::project_sync::validate_project_tree_paths(&tree, &policy)?;
    ryeos_state::project_sync::validate_captured_policy_source(&cas, &tree, &policy)?;
    drop(guard);
    Ok((authority, tree))
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
        bail!("hosted worker-result pull requires a full_project remote binding");
    }
    Ok((
        local_project_path,
        loaded.config,
        binding.remote_project_path,
    ))
}

fn resolve_current_route(state: &AppState, intent: &PullIntent) -> Result<RemoteConfig> {
    intent.validate()?;
    if state.threads.site_id() != intent.source_site_id {
        bail!("durable hosted worker-result pull belongs to another source site");
    }
    let current_grant = ryeos_app::operator_authority::admitted_operator_authority_digest(
        state,
        &intent.operator_fingerprint,
    )?;
    if current_grant != intent.operator_authority_digest {
        bail!("durable hosted worker-result pull operator grant changed");
    }
    let project = PathBuf::from(&intent.local_project_path);
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&project))?;
    let loaded = config::get_loaded_remote(&report.remotes, &intent.remote)?;
    if loaded.config.url != intent.remote_url
        || loaded.config.site_id != intent.target_site_id
        || loaded.config.principal_id != intent.target_principal_id
        || loaded.config.signing_key != intent.target_signing_key
    {
        bail!("durable hosted worker-result pull remote authority changed");
    }
    let binding = config::resolve_loaded_project_binding(&loaded, &project)?;
    if config::local_project_identity(&binding.local_project_path)?
        != intent.local_project_path.as_str()
        || binding.sync_scope != ProjectSyncScope::FullProject
        || binding.remote_project_path != intent.target_project_path
    {
        bail!("durable hosted worker-result pull project route changed");
    }
    Ok(loaded.config)
}

fn validate_job_binding(
    job: &SyncJobRecord,
    operation: &PullOperation,
    job_id: &str,
) -> Result<()> {
    let evidence = &operation.candidate_result.evidence;
    let expected_roots = sorted_hashes([
        evidence.base_snapshot_hash.clone(),
        evidence.candidate_snapshot_hash.clone(),
    ]);
    if job.job_id != job_id
        || job.operation_type != OPERATION_TYPE
        || job.peer.as_deref() != Some(operation.intent.remote.as_str())
        || job.max_attempts != ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS
        || !job.attempt_count_is_valid()
        || job.roots != expected_roots
        || (!job.heads.is_empty() && job.heads != vec![evidence.candidate_snapshot_hash.clone()])
    {
        bail!("hosted worker-result pull job changed its retained authority binding");
    }
    Ok(())
}

fn validate_response(response: &Response, operation: &PullOperation, job_id: &str) -> Result<()> {
    if response.schema != RESPONSE_SCHEMA
        || response.job_id != job_id
        || response.remote != operation.intent.remote
        || response.local_project_path != operation.intent.local_project_path
        || response.candidate_result != operation.candidate_result
        || !response.applied
        || response.published
        || response.idempotent
    {
        bail!("completed hosted worker-result pull contradicts its retained operation");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn settle_attempt(
    state: &AppState,
    job: &SyncJobRecord,
    attempt_id: &str,
    attempt_state: SyncJobAttemptState,
    job_state: SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
    heads: Vec<String>,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &FinishSyncJobAttempt {
                state: attempt_state,
                phase: phase.to_owned(),
                error: error.clone(),
                result: result.clone(),
            },
            &job.job_id,
            &SyncJobUpdate {
                state: job_state,
                phase: phase.to_owned(),
                roots: None,
                heads: Some(heads),
                uploaded_hashes: job.uploaded_hashes.clone(),
                fetched_hashes: job.fetched_hashes.clone(),
                last_error: error,
                result,
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

fn classify_pull_error(error: crate::remote::pull::PullResultsError) -> AttemptFailure {
    use crate::remote::pull::PullResultsError;
    match error {
        failure @ (PullResultsError::InvalidRemoteSnapshot(_)
        | PullResultsError::RollbackIncomplete(_)
        | PullResultsError::UnrelatedSnapshot { .. }
        | PullResultsError::MissingSnapshotHash) => AttemptFailure::permanent(failure),
        failure @ (PullResultsError::LocalConflict(_)
        | PullResultsError::RecoveryRequired(_)
        | PullResultsError::Other(_)) => AttemptFailure::retryable(failure),
    }
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
        result = "hosted worker-result pull failed".to_owned();
    }
    result
}

pub async fn recover_durable_worker_result_pulls(state: &AppState) -> Result<usize> {
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
            let operation = match PullOperation::from_value(job.operation.clone()) {
                Ok(operation) => operation,
                Err(error) => {
                    terminalize_without_attempt(state, &job, "operation_invalid", &error)?;
                    continue;
                }
            };
            match execute_operation(
                Arc::new(state.clone()),
                operation.intent.clone(),
                None,
                false,
            )
            .await
            {
                Ok(_) => recovered += 1,
                Err(error) => tracing::warn!(
                    job_id = %job.job_id,
                    %error,
                    "durable hosted worker-result pull recovery did not complete"
                ),
            }
        }
        after = Some(next);
    }
    Ok(recovered)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/pull-worker-result",
    endpoint: "remote.pull-worker-result",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/pull-worker-result"],
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
    use base64::Engine as _;

    fn intent() -> PullIntent {
        let key = lillux::crypto::SigningKey::from_bytes(&[17_u8; 32]).verifying_key();
        let fingerprint = lillux::crypto::fingerprint(&key);
        PullIntent {
            schema: INTENT_SCHEMA.to_owned(),
            remote: "worker".to_owned(),
            remote_url: "https://worker.example".to_owned(),
            source_site_id: "site:source".to_owned(),
            target_site_id: "site:target".to_owned(),
            target_principal_id: format!("fp:{fingerprint}"),
            target_signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
            ),
            local_project_path: "/source/project".to_owned(),
            target_project_path: "/target/project".to_owned(),
            operator_fingerprint: "a".repeat(64),
            operator_authority_digest: "b".repeat(64),
            chain_root_id: "T-root".to_owned(),
        }
    }

    #[test]
    fn durable_identity_is_the_complete_owner_route_and_chain() {
        let baseline = intent();
        baseline.validate().unwrap();
        let job_id = baseline.job_id().unwrap();
        for changed in [
            PullIntent {
                chain_root_id: "T-other".to_owned(),
                ..baseline.clone()
            },
            PullIntent {
                target_project_path: "/target/other".to_owned(),
                ..baseline.clone()
            },
            PullIntent {
                operator_authority_digest: "c".repeat(64),
                ..baseline.clone()
            },
        ] {
            assert_ne!(changed.job_id().unwrap(), job_id);
        }
    }

    #[test]
    fn predecessor_pull_envelopes_are_classified_before_nested_result_decode() {
        let operation = serde_json::json!({
            "schema":"ryeos.remote_hosted_worker_result_pull_operation.v1",
            "operation_type":OPERATION_TYPE,
            "candidate_result":"not a current result",
        });
        let error = PullOperation::from_value(operation)
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema or type is not current"));
        let response = serde_json::json!({
            "schema":"ryeos.remote_hosted_worker_result_pull_result.v1",
            "candidate_result":"not a current result",
        });
        let error = Response::from_value(response).unwrap_err().to_string();
        assert!(error.contains("result schema is not current"));
    }
}
