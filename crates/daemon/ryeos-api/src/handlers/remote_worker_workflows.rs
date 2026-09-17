//! Source-owned orchestration for one bounded remote worker workflow.
//!
//! The recorded start-service root is the audit identity for the initiating
//! command. A deterministic runtime-class work ID names the durable workflow
//! across status and resume calls. The sync-job row is its crash-recovery
//! projection and root inventory. Content liveness comes from the recorded
//! invocation's pinned project authority and the existing active-job GC fence,
//! not from `sync_jobs.roots_json` acting as a CAS pin.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config::{self, ProjectSyncScope, RemoteConfig};
use crate::remote::push::push_snapshot_generation;
use ryeos_app::hosted_candidate_result::{
    HostedCandidateResultRequest, HostedCandidateResultResponse,
};
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::config_loading::{ConfigLoadContext, ConfigSpec, ResolveMode};
use ryeos_engine::contracts::SubjectResolutionAuthority;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};
use ryeos_engine::protocols::PersistentSessionCleanupAuthority;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::{NewSyncJob, SyncJobRecord, SyncJobState, SyncJobUpdate};

const OPERATION_TYPE: &str = "remote_worker_workflow_start";
const OPERATION_SCHEMA: &str = "ryeos.remote_worker_workflow_operation.v6";
const HISTORICAL_OPERATION_SCHEMA: &str = "ryeos.remote_worker_workflow_operation.v5";
const PROGRESS_SCHEMA: &str = "ryeos.remote_worker_workflow_progress.v5";
const HISTORICAL_PROGRESS_SCHEMA: &str = "ryeos.remote_worker_workflow_progress.v4";
const RESPONSE_SCHEMA: &str = "ryeos.remote_worker_workflow_receipt.v5";
const HISTORICAL_RESPONSE_SCHEMA: &str = "ryeos.remote_worker_workflow_receipt.v4";
const WORKFLOW_SCHEMA: &str = "ryeos.remote_worker_workflow.v3";
const HISTORICAL_WORKFLOW_SCHEMA: &str = "ryeos.remote_worker_workflow.v2";
const DRIVE_INTENT_EVENT: &str = "remote_worker_workflow.drive_intent";
const DRIVE_SETTLED_EVENT: &str = "remote_worker_workflow.drive_settled";
const LAUNCH_ACCEPTED_EVENT: &str = "remote_worker_workflow.launch_accepted";
const DRIVE_FACT_SCHEMA: &str = "ryeos.remote_worker_workflow_drive_fact.v1";
const LAUNCH_ACCEPTANCE_SCHEMA: &str = "ryeos.remote_worker_workflow_launch_acceptance.v3";
const HISTORICAL_LAUNCH_ACCEPTANCE_SCHEMA: &str =
    "ryeos.remote_worker_workflow_launch_acceptance.v2";
const MAX_TASK_BYTES: usize = 64 * 1024;
const STATUS_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(30);
const LAUNCH_CONTACT_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("target launch contact is accepted but not yet bound")]
struct TargetLaunchNotBound;

#[derive(Debug, thiserror::Error)]
#[error("target launch settled terminally: {0}")]
struct TargetLaunchTerminal(String);

#[derive(Debug, thiserror::Error)]
#[error("target workflow terminal authority is invalid: {0}")]
struct TargetWorkflowTerminalInvalid(String);

fn default_remote() -> String {
    "default".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    #[serde(default = "default_remote")]
    remote: String,
    workflow_ref: String,
    credential_profile_id: String,
    task: Value,
    #[serde(default)]
    source_snapshot_hash: Option<String>,
    target_product_selections:
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    source_work_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    source_work_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    operation_type: String,
    schema: String,
    source_work_id: String,
    source_invocation_id: String,
    source_site_id: String,
    admitted_start_request: Value,
    admitted_start_request_digest: String,
    operator_fingerprint: String,
    operator_authority_digest: String,
    remote: String,
    remote_url: String,
    target_site_id: String,
    target_principal_id: String,
    target_signing_key: String,
    local_project_path: String,
    target_project_path: String,
    admitted_source_snapshot_hash: String,
    source_snapshot_hash: String,
    workflow_ref: String,
    credential_profile_id: String,
    task: Value,
    task_digest: String,
    target_product_selections:
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
    target_product_selections_digest: String,
    target_launch_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    schema: String,
    pushed: bool,
    workflow_digest: Option<String>,
    target_request_digest: Option<String>,
    target_readiness: Option<TargetReadinessEvidence>,
    target_chain_root_id: Option<String>,
    target_admission: Option<TargetAdmissionEvidence>,
    launch_acceptance_drive_root_id: Option<String>,
    drive_root_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    allowed_next_action: String,
    source_work_id: String,
    remote: String,
    source_snapshot_hash: String,
    workflow_ref: String,
    workflow_digest: String,
    target_launch_id: String,
    target_readiness: TargetReadinessEvidence,
    launch_acceptance_drive_root_id: String,
    target_chain_root_id: String,
    target_admission: TargetAdmissionEvidence,
    target_workflow_terminal_thread_id: String,
    target_workflow_result_digest: String,
    candidate_terminal_thread_id: String,
    candidate_result: HostedCandidateResultResponse,
    target_product_selections_digest: String,
    settlement_drive_root_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetAdmissionEvidence {
    admitted_capsule_hash: String,
    exact_program_hash: String,
    effective_definition_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchAcceptance {
    schema: String,
    source_work_id: String,
    operation_digest: String,
    workflow_digest: String,
    target_request_digest: String,
    target_launch_id: String,
    target_readiness: TargetReadinessEvidence,
    target_chain_root_id: String,
    target_admission: TargetAdmissionEvidence,
    launch_acceptance_drive_root_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowGraphResult {
    schema: String,
    candidate_terminal_thread_id: String,
}

const WORKFLOW_GRAPH_RESULT_SCHEMA: &str = "ryeos.remote_worker_workflow_graph_result.v1";
const MAX_TARGET_CHAIN_SEGMENTS: usize = 1024;

enum DriveOutcome {
    LaunchAccepted,
    CompletionPending,
    Completed(Receipt),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowConfig {
    category: String,
    schema: String,
    driver: String,
    target_requirements: TargetRuntimeRequirements,
    #[serde(default)]
    ref_bindings: BTreeMap<String, String>,
    /// Signed mapping from generic workflow inputs to the driver's exact
    /// parameter shape. Core renders it but never names provider routes.
    parameters: Value,
}

struct CompiledWorkflow {
    driver: String,
    ref_bindings: BTreeMap<String, String>,
    parameters: Value,
    target_requirements: TargetRuntimeRequirements,
    digest: String,
    effective_definition_digest: String,
    source_definition_derived: HashMap<String, Value>,
}

/// Exact target runtime semantics selected by the signed workflow Config.
/// These are checked remotely before project transfer or a new launch, then
/// the target executor independently performs final admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetRuntimeRequirements {
    process_control: TargetProcessControl,
    cleanup_authority: PersistentSessionCleanupAuthority,
    filesystem_mode: ryeos_engine::isolation::IsolationMode,
    network_mode: ryeos_engine::isolation::IsolationNetworkMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TargetProcessControl {
    OrdinarySubprocess,
    PooledRequests,
    ExclusiveSession,
}

impl TargetProcessControl {
    const fn status_field(self) -> &'static str {
        match self {
            Self::OrdinarySubprocess => "ordinary_subprocess",
            Self::PooledRequests => "pooled_requests",
            Self::ExclusiveSession => "exclusive_session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetReadinessEvidence {
    requirements: TargetRuntimeRequirements,
    daemon_revision: String,
    isolation_policy_digest: String,
    process_scope_authority_digest: Option<String>,
    process_control_reason: String,
}

impl Progress {
    fn new() -> Self {
        Self {
            schema: PROGRESS_SCHEMA.to_owned(),
            ..Self::default()
        }
    }

    fn from_job(job: &SyncJobRecord, family: RecoveryFamily) -> Result<Self> {
        let mut value = job.result.clone().unwrap_or_else(|| {
            serde_json::to_value(Self::new()).expect("progress serialization is infallible")
        });
        upgrade_historical_evidence(
            &mut value,
            HISTORICAL_PROGRESS_SCHEMA,
            PROGRESS_SCHEMA,
            family,
        )?;
        let progress: Self = serde_json::from_value(value)
            .context("parse retained remote-worker workflow progress")?;
        if progress.schema != PROGRESS_SCHEMA {
            bail!("retained remote-worker workflow progress schema is not current");
        }
        Ok(progress)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryFamily {
    PreCleanupField,
    ExplicitLocalCleanupBridge,
    Current,
}

fn recovery_family(operation: &Operation, retained: &Value) -> Result<RecoveryFamily> {
    match operation.schema.as_str() {
        OPERATION_SCHEMA => Ok(RecoveryFamily::Current),
        HISTORICAL_OPERATION_SCHEMA => match retained.get("schema").and_then(Value::as_str) {
            Some(HISTORICAL_PROGRESS_SCHEMA | HISTORICAL_RESPONSE_SCHEMA) => {
                Ok(RecoveryFamily::PreCleanupField)
            }
            Some(PROGRESS_SCHEMA | RESPONSE_SCHEMA) => {
                Ok(RecoveryFamily::ExplicitLocalCleanupBridge)
            }
            _ => bail!("historical remote-worker operation has no coherent evidence family"),
        },
        _ => bail!("remote-worker workflow operation schema is unsupported"),
    }
}

fn historical_cleanup_authority(process_control: &str) -> Result<&'static str> {
    match process_control {
        "ordinary_subprocess" | "pooled_requests" => Ok("not_required"),
        "exclusive_session" => Ok("local_process_scope"),
        _ => bail!("historical workflow has an unknown process-control mode"),
    }
}

fn insert_historical_cleanup_authority(requirements: &mut Value) -> Result<()> {
    let requirements = requirements
        .as_object_mut()
        .context("historical target requirements are not an object")?;
    if requirements.contains_key("cleanup_authority") {
        bail!("historical target requirements unexpectedly declare cleanup authority");
    }
    let process_control = requirements
        .get("process_control")
        .and_then(Value::as_str)
        .context("historical target requirements omit process control")?
        .to_owned();
    requirements.insert(
        "cleanup_authority".to_owned(),
        Value::String(historical_cleanup_authority(&process_control)?.to_owned()),
    );
    Ok(())
}

fn upgrade_historical_evidence(
    value: &mut Value,
    historical_schema: &str,
    current_schema: &str,
    family: RecoveryFamily,
) -> Result<()> {
    let schema = value.get("schema").and_then(Value::as_str);
    match family {
        RecoveryFamily::PreCleanupField if schema == Some(historical_schema) => {}
        RecoveryFamily::ExplicitLocalCleanupBridge | RecoveryFamily::Current
            if schema == Some(current_schema) =>
        {
            return Ok(());
        }
        _ => bail!("retained remote-worker evidence contradicts its recovery family"),
    }
    let object = value
        .as_object_mut()
        .context("historical remote-worker evidence is not an object")?;
    object.insert(
        "schema".to_owned(),
        Value::String(current_schema.to_owned()),
    );
    if let Some(readiness) = object.get_mut("target_readiness")
        && !readiness.is_null()
    {
        let requirements = readiness
            .get_mut("requirements")
            .context("historical target readiness omits requirements")?;
        insert_historical_cleanup_authority(requirements)?;
    }
    Ok(())
}

fn decode_receipt(mut value: Value, family: RecoveryFamily) -> Result<Receipt> {
    upgrade_historical_evidence(
        &mut value,
        HISTORICAL_RESPONSE_SCHEMA,
        RESPONSE_SCHEMA,
        family,
    )?;
    serde_json::from_value(value).context("parse retained remote-worker workflow receipt")
}

fn decode_workflow_config(mut value: Value, family: RecoveryFamily) -> Result<WorkflowConfig> {
    let schema = value.get("schema").and_then(Value::as_str);
    if family == RecoveryFamily::PreCleanupField && schema == Some(HISTORICAL_WORKFLOW_SCHEMA) {
        let object = value
            .as_object_mut()
            .context("historical remote-worker workflow Config is not an object")?;
        object.insert(
            "schema".to_owned(),
            Value::String(WORKFLOW_SCHEMA.to_owned()),
        );
        insert_historical_cleanup_authority(
            object
                .get_mut("target_requirements")
                .context("historical workflow Config omits target requirements")?,
        )?;
    } else if schema != Some(WORKFLOW_SCHEMA) || family == RecoveryFamily::PreCleanupField {
        bail!("remote-worker workflow Config contradicts its recovery family");
    }
    let config: WorkflowConfig =
        serde_json::from_value(value).context("parse remote-worker workflow Config")?;
    if config.schema != WORKFLOW_SCHEMA {
        bail!("remote-worker workflow Config schema is not current");
    }
    validate_target_runtime_requirements(&config.target_requirements)?;
    Ok(config)
}

fn retained_evidence_value<T: Serialize>(
    evidence: &T,
    family: RecoveryFamily,
    historical_schema: &str,
) -> Result<Value> {
    let mut value = serde_json::to_value(evidence)?;
    if family != RecoveryFamily::PreCleanupField {
        return Ok(value);
    }
    let object = value
        .as_object_mut()
        .context("remote-worker retained evidence is not an object")?;
    object.insert(
        "schema".to_owned(),
        Value::String(historical_schema.to_owned()),
    );
    if let Some(readiness) = object.get_mut("target_readiness")
        && !readiness.is_null()
    {
        readiness
            .get_mut("requirements")
            .and_then(Value::as_object_mut)
            .context("remote-worker retained readiness omits requirements")?
            .remove("cleanup_authority");
    }
    Ok(value)
}

fn validate_target_runtime_requirements(requirements: &TargetRuntimeRequirements) -> Result<()> {
    match (requirements.process_control, requirements.cleanup_authority) {
        (
            TargetProcessControl::OrdinarySubprocess | TargetProcessControl::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        )
        | (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::LocalProcessScope,
        )
        | (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        ) => Ok(()),
        (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
        ) => bail!(
            "external placement-incarnation cleanup is not activated without a protected lifecycle binding and closed-tool qualification"
        ),
        _ => bail!("target runtime requirements pair incompatible process and cleanup authority"),
    }
}

pub async fn start(
    req: StartRequest,
    admitted_start_request: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let invocation_root = ctx
        .recorded_service_root_id()
        .context("remote-worker workflow start requires a recorded service root")?
        .to_owned();
    ryeos_executor::executor::validate_service_invocation_id(&invocation_root)?;
    let operator_fingerprint =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    validate_recorded_invocation(
        &state,
        &invocation_root,
        &operator_fingerprint,
        "service:remote-worker-workflows/start",
    )?;
    validate_profile_id(&req.credential_profile_id)?;
    if !req.task.is_object() || serde_json::to_vec(&req.task)?.len() > MAX_TASK_BYTES {
        bail!("remote-worker workflow task must be an object within the 64 KiB limit");
    }
    let target_product_selections =
        validate_target_product_selections(req.target_product_selections.clone())?;
    let workflow = CanonicalRef::parse(&req.workflow_ref)?;
    if workflow.kind != "config" || workflow.suffix.is_some() {
        bail!("remote-worker workflow_ref must be an unsuffixed config reference");
    }
    let (local_project_path, admitted_source_snapshot_hash) =
        recorded_pinned_project(&state, &invocation_root)?;
    let source_snapshot_hash = select_source_snapshot(
        &state,
        &admitted_source_snapshot_hash,
        req.source_snapshot_hash.as_deref(),
    )?;
    let (resolved_local_path, remote, target_project_path) =
        resolve_route(&state, &req.remote, Path::new(&local_project_path))?;
    if resolved_local_path != local_project_path {
        bail!("recorded workflow project identity changed during route resolution");
    }
    let source_work_id = derive_source_work_id(&invocation_root)?;
    let operation = Operation {
        operation_type: OPERATION_TYPE.to_owned(),
        schema: OPERATION_SCHEMA.to_owned(),
        source_work_id: source_work_id.clone(),
        source_invocation_id: invocation_root.clone(),
        source_site_id: state.threads.site_id().to_owned(),
        admitted_start_request_digest: ryeos_state::objects::canonical_value_digest(
            &admitted_start_request,
        )?,
        admitted_start_request,
        operator_fingerprint: operator_fingerprint.clone(),
        operator_authority_digest:
            ryeos_app::operator_authority::admitted_operator_authority_digest(
                &state,
                &operator_fingerprint,
            )?,
        remote: req.remote,
        remote_url: remote.url,
        target_site_id: remote.site_id,
        target_principal_id: remote.principal_id,
        target_signing_key: remote.signing_key,
        local_project_path,
        target_project_path,
        admitted_source_snapshot_hash,
        source_snapshot_hash,
        workflow_ref: req.workflow_ref,
        credential_profile_id: req.credential_profile_id,
        task_digest: ryeos_state::objects::canonical_value_digest(&req.task)?,
        task: req.task,
        target_product_selections_digest: target_product_selections_digest(
            &target_product_selections,
        )?,
        target_product_selections,
        target_launch_id: derive_target_launch_id(&source_work_id)?,
    };
    drive_operation(state, operation, true, &invocation_root).await
}

pub async fn resume(
    req: ResumeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let invocation_root = ctx
        .recorded_service_root_id()
        .context("remote-worker workflow resume requires a recorded service root")?;
    let operator = ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    validate_recorded_invocation(
        &state,
        invocation_root,
        &operator,
        "service:remote-worker-workflows/resume",
    )?;
    ryeos_runtime::validate_runtime_thread_id(&req.source_work_id).map_err(anyhow::Error::msg)?;
    let retained = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id(&req.source_work_id)))?
        .context("remote-worker workflow recovery projection is absent")?;
    let operation: Operation = serde_json::from_value(retained.operation)?;
    if operation.operator_fingerprint != operator {
        bail!("remote-worker workflow not found");
    }
    drive_operation(state, operation, false, invocation_root).await
}

async fn drive_operation(
    state: Arc<AppState>,
    operation: Operation,
    create_if_absent: bool,
    drive_root_id: &str,
) -> Result<Value> {
    let source_work_id = operation.source_work_id.clone();
    validate_operation(&operation)?;
    validate_drive_invocation(&state, drive_root_id, &operation)?;
    let job_id = job_id(&source_work_id);
    let initial_progress = serde_json::to_value(Progress::new())?;
    let job = match state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))?
    {
        Some(job) => job,
        None if create_if_absent => state.state_store.with_state_db(|db| {
            db.create_sync_job_with_initial_progress(
                &NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type: OPERATION_TYPE.to_owned(),
                    operation: serde_json::to_value(&operation)?,
                    peer: Some(operation.remote.clone()),
                    roots: vec![operation.source_snapshot_hash.clone()],
                    heads: Vec::new(),
                    max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
                },
                SyncJobState::Running,
                "reserved",
                Some(&initial_progress),
            )
        })?,
        None => bail!("remote-worker workflow recovery projection disappeared"),
    };
    if job.operation != serde_json::to_value(&operation)? {
        bail!("remote-worker workflow recovery projection contradicts its recorded request");
    }
    validate_recorded_owner(&state, &operation)?;
    let operation_digest = operation_digest(&operation)?;
    let retained = job
        .result
        .as_ref()
        .context("remote-worker workflow projection omitted retained evidence")?;
    let recovery_family = recovery_family(&operation, retained)?;
    if job.state == SyncJobState::Completed {
        let receipt = decode_receipt(
            job.result
                .context("completed workflow projection omitted receipt")?,
            recovery_family,
        )?;
        authoritative_receipt(
            &state,
            &operation,
            &receipt,
            &operation_digest,
            recovery_family,
        )?;
        return Ok(serde_json::to_value(receipt)?);
    }
    let mut progress = Progress::from_job(&job, recovery_family)?;
    let Some(mut attempt) = WorkflowAttempt::begin(state.clone(), &job_id)? else {
        return Ok(in_progress_response(&operation, &job, &progress));
    };
    let drive_result: Result<DriveOutcome> = async {
        let prior_drive_root_id = progress.drive_root_id.clone();
        if let Some(prior_drive_root_id) = prior_drive_root_id.as_deref() {
            validate_drive_intent(&state, prior_drive_root_id, &operation, &operation_digest)?;
            if let Some(receipt) = read_settlement_fact(
                &state,
                prior_drive_root_id,
                &operation,
                &operation_digest,
                recovery_family,
            )? {
                return Ok(DriveOutcome::Completed(receipt));
            }
        }
        append_drive_intent(&state, drive_root_id, &operation, &operation_digest)?;
        let remote = validate_current_route(&state, &operation)?;
        let client = RemoteClient::from_remote_cfg_as_retained_configured_operator(
            &state,
            &remote,
            &operation.operator_fingerprint,
            &operation.operator_authority_digest,
        )?;

        let snapshot_hash = operation.source_snapshot_hash.as_str();
        let compiled = compile_workflow(
            &state,
            &operation.workflow_ref,
            snapshot_hash,
            Path::new(&operation.local_project_path),
            &operation.task,
            &operation.credential_profile_id,
            &operation.target_product_selections,
            recovery_family,
        )?;
        let had_launch_contact = progress.target_request_digest.is_some();
        if progress
            .workflow_digest
            .as_deref()
            .is_some_and(|digest| digest != compiled.digest)
        {
            bail!("remote-worker workflow recompilation differs from retained launch contact");
        }
        progress.workflow_digest = Some(compiled.digest.clone());
        let parameters = compiled.parameters.clone();
        let execution_policy = exact_snapshot_execution_policy(snapshot_hash)?;
        let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "driver": compiled.driver,
            "ref_bindings": compiled.ref_bindings,
            "parameters": parameters,
            "target_project_path": operation.target_project_path,
            "target_launch_id": operation.target_launch_id,
            "source_snapshot_hash": snapshot_hash,
            "execution_policy": execution_policy,
            "outer_product_selections": [],
            "target_product_selections": operation.target_product_selections,
            "target_product_selections_digest": operation.target_product_selections_digest,
        }))?;
        if progress
            .target_request_digest
            .as_deref()
            .is_some_and(|digest| digest != request_digest)
        {
            bail!("remote-worker workflow recompilation differs from retained launch contact");
        }
        progress.target_request_digest = Some(request_digest.clone());
        reconcile_launch_acceptance(
            &state,
            &operation,
            &operation_digest,
            &compiled.digest,
            &request_digest,
            &compiled.target_requirements,
            &mut progress,
            recovery_family,
        )?;
        progress.drive_root_id = Some(drive_root_id.to_owned());
        update_progress(
            &state,
            &job_id,
            "reserved",
            &progress,
            recovery_family,
            vec![operation.source_snapshot_hash.clone()],
        )?;

        match (
            progress.target_chain_root_id.as_deref(),
            progress.target_admission.as_ref(),
        ) {
            (Some(target_chain_root_id), Some(target_admission)) => {
                update_progress(
                    &state,
                    &job_id,
                    "completion_observing",
                    &progress,
                    recovery_family,
                    vec![snapshot_hash.to_owned()],
                )?;
                let Some(receipt) = observe_target_completion(
                    &client,
                    &operation,
                    &compiled,
                    target_chain_root_id,
                    target_admission,
                    progress
                        .target_readiness
                        .as_ref()
                        .context("accepted target launch omitted readiness evidence")?,
                    progress
                        .launch_acceptance_drive_root_id
                        .as_deref()
                        .context("launch acceptance omitted its drive root")?,
                    &remote.pinned_signing_key()?,
                    drive_root_id,
                )
                .await?
                else {
                    update_progress(
                        &state,
                        &job_id,
                        "completion_pending",
                        &progress,
                        recovery_family,
                        vec![snapshot_hash.to_owned()],
                    )?;
                    return Ok(DriveOutcome::CompletionPending);
                };
                append_settlement_fact(
                    &state,
                    drive_root_id,
                    &operation,
                    &operation_digest,
                    &receipt,
                    recovery_family,
                )?;
                return Ok(DriveOutcome::Completed(receipt));
            }
            (None, None) => {}
            _ => bail!("remote-worker workflow launch acceptance is incomplete"),
        }

        // An interrupted contact must be reconciled before inspecting current
        // readiness. A launch already accepted under the retained request is
        // authoritative even if target capabilities later change.
        let adopted = if had_launch_contact {
            adopt_contacted_launch(&client, &operation).await?
        } else {
            None
        };

        if adopted.is_none() {
            let readiness =
                inspect_target_readiness(&client, &compiled.target_requirements).await?;
            if progress
                .target_readiness
                .as_ref()
                .is_some_and(|retained| retained != &readiness)
            {
                bail!("target runtime readiness changed before launch acceptance");
            }
            progress.target_readiness = Some(readiness);
            update_progress(
                &state,
                &job_id,
                "target_ready",
                &progress,
                recovery_family,
                vec![snapshot_hash.to_owned()],
            )?;
        }

        if adopted.is_none() && !progress.pushed {
            update_progress(
                &state,
                &job_id,
                "push_contacting",
                &progress,
                recovery_family,
                vec![operation.source_snapshot_hash.clone()],
            )?;
            push_snapshot_generation(
                &client,
                &state.state_store.pinned_state_authority()?,
                &operation.source_snapshot_hash,
                &operation.target_project_path,
            )
            .await?;
            progress.pushed = true;
            update_progress(
                &state,
                &job_id,
                "pushed",
                &progress,
                recovery_family,
                vec![operation.source_snapshot_hash.clone()],
            )?;
        }

        update_progress(
            &state,
            &job_id,
            "launch_contacting",
            &progress,
            recovery_family,
            vec![snapshot_hash.to_owned()],
        )?;
        let target_chain_root_id = if let Some(thread_id) = adopted {
            thread_id
        } else {
            let response = client
                .execute_accepted_with_total_timeout(
                    &compiled.driver,
                    &compiled.ref_bindings,
                    &[],
                    Some(&operation.target_project_path),
                    &parameters,
                    &execution_policy,
                    &operation.target_launch_id,
                    LAUNCH_CONTACT_TIMEOUT,
                )
                .await?;
            response
                .get("thread_id")
                .and_then(Value::as_str)
                .context("accepted target launch omitted thread_id")?
                .to_owned()
        };
        let target_admission = verify_target_launch(
            &client,
            &operation,
            &compiled,
            &parameters,
            &execution_policy,
            &target_chain_root_id,
        )
        .await?;
        let acceptance = LaunchAcceptance {
            schema: LAUNCH_ACCEPTANCE_SCHEMA.to_owned(),
            source_work_id: operation.source_work_id.clone(),
            operation_digest: operation_digest.clone(),
            workflow_digest: compiled.digest.clone(),
            target_request_digest: request_digest,
            target_launch_id: operation.target_launch_id.clone(),
            target_readiness: progress
                .target_readiness
                .clone()
                .context("target launch omitted retained readiness evidence")?,
            launch_acceptance_drive_root_id: drive_root_id.to_owned(),
            target_chain_root_id: target_chain_root_id.clone(),
            target_admission: target_admission.clone(),
        };
        append_launch_acceptance(
            &state,
            &operation,
            drive_root_id,
            &acceptance,
            recovery_family,
        )?;
        progress.launch_acceptance_drive_root_id = Some(drive_root_id.to_owned());
        progress.target_chain_root_id = Some(target_chain_root_id.clone());
        progress.target_admission = Some(target_admission);
        update_progress(
            &state,
            &job_id,
            "launch_accepted",
            &progress,
            recovery_family,
            vec![snapshot_hash.to_owned()],
        )?;
        Ok(DriveOutcome::LaunchAccepted)
    }
    .await;
    match drive_result {
        Ok(DriveOutcome::LaunchAccepted) => {
            attempt.pause(&operation, &progress, "launch_accepted", recovery_family)?;
            Ok(launch_accepted_response(&operation, &progress))
        }
        Ok(DriveOutcome::CompletionPending) => {
            attempt.pause(&operation, &progress, "completion_pending", recovery_family)?;
            Ok(completion_pending_response(&operation, &progress))
        }
        Ok(DriveOutcome::Completed(receipt)) => {
            attempt.complete(&operation, &receipt, recovery_family)?;
            Ok(serde_json::to_value(receipt)?)
        }
        Err(error) => {
            attempt.fail(&error)?;
            Err(error)
        }
    }
}

pub async fn query(req: QueryRequest, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_runtime::validate_runtime_thread_id(&req.source_work_id).map_err(anyhow::Error::msg)?;
    let operator = ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id(&req.source_work_id)))?
        .context("remote-worker workflow not found")?;
    let operation: Operation = serde_json::from_value(job.operation.clone())?;
    validate_operation(&operation)?;
    if operation.operator_fingerprint != operator {
        bail!("remote-worker workflow not found");
    }
    validate_recorded_owner(&state, &operation)?;
    let retained = job
        .result
        .as_ref()
        .context("remote-worker workflow projection omitted retained evidence")?;
    let recovery_family = recovery_family(&operation, retained)?;
    if job.state == SyncJobState::Completed {
        let receipt = decode_receipt(
            job.result
                .context("completed remote-worker workflow omitted its receipt")?,
            recovery_family,
        )?;
        authoritative_receipt(
            &state,
            &operation,
            &receipt,
            &operation_digest(&operation)?,
            recovery_family,
        )?;
        return Ok(serde_json::json!({
            "source_work_id": operation.source_work_id,
            "state": "completed",
            "phase": job.phase,
            "remote": operation.remote,
            "source_snapshot_hash": receipt.source_snapshot_hash,
            "workflow_ref": receipt.workflow_ref,
            "workflow_digest": receipt.workflow_digest,
            "target_launch_id": receipt.target_launch_id,
            "target_chain_root_id": receipt.target_chain_root_id,
            "candidate_terminal_thread_id": receipt.candidate_terminal_thread_id,
            "allowed_next_action": "pull_result",
            "receipt": receipt,
        }));
    }
    let mut progress = Progress::from_job(&job, recovery_family)?;
    let digest = operation_digest(&operation)?;
    reconcile_launch_acceptance_for_query(
        &state,
        &operation,
        &digest,
        &mut progress,
        recovery_family,
    )?;
    if let Some(drive_root_id) = progress.drive_root_id.as_deref() {
        validate_drive_intent(&state, drive_root_id, &operation, &digest)?;
        if let Some(receipt) =
            read_settlement_fact(&state, drive_root_id, &operation, &digest, recovery_family)?
        {
            return Ok(serde_json::json!({
                "source_work_id": operation.source_work_id,
                "state": "completed",
                "phase": "completed",
                "remote": operation.remote,
                "source_snapshot_hash": receipt.source_snapshot_hash,
                "workflow_ref": receipt.workflow_ref,
                "workflow_digest": receipt.workflow_digest,
                "target_launch_id": receipt.target_launch_id,
                "target_chain_root_id": receipt.target_chain_root_id,
                "candidate_terminal_thread_id": receipt.candidate_terminal_thread_id,
                "allowed_next_action": "pull_result",
                "receipt": receipt,
            }));
        }
    }
    Ok(in_progress_response(&operation, &job, &progress))
}

fn compile_workflow(
    state: &AppState,
    workflow_ref: &str,
    snapshot_hash: &str,
    original_project_path: &Path,
    task: &Value,
    credential_profile_id: &str,
    target_product_selections: &ryeos_state::external_content::products::composition::ProductSelectionInputs,
    recovery_family: RecoveryFamily,
) -> Result<CompiledWorkflow> {
    let canonical = CanonicalRef::parse(workflow_ref)?;
    let context = ryeos_executor::execution::project_source::resolve_read_only_snapshot_context(
        state,
        snapshot_hash,
        original_project_path.to_owned(),
        &format!("workflow-config-{}", &snapshot_hash[..16]),
    )?;
    let authority = context
        .pinned_materialization
        .as_ref()
        .context("exact workflow Config context omitted project-content authority")?;
    let engine: &Engine = &context.request_engine;
    if engine.kinds.get(&canonical.kind).is_none() {
        bail!("remote-worker workflow Config kind is not registered");
    }
    let roots = engine.resolution_roots(Some(context.effective_path.clone()));
    let load_context = ConfigLoadContext {
        roots: &roots,
        parsers: &engine.parser_dispatcher,
        kinds: &engine.kinds,
        trust_store: &engine.trust_store,
        project_authority: Some((&context.effective_path, authority)),
    };
    let resolved = ryeos_engine::config_loading::resolve_config_spec(
        &ConfigSpec {
            path: format!("{}.yaml", canonical.bare_id),
            mode: ResolveMode::FirstMatch,
        },
        &load_context,
    )?;
    if resolved.layers.len() != 1
        || resolved.layers[0].space != ryeos_engine::contracts::ItemSpace::Project
    {
        bail!("remote-worker workflow Config must resolve exactly from project space");
    }
    let trusted = ryeos_engine::config_loading::load_and_verify_trusted_config_file(
        &resolved.layers[0].path,
        &load_context,
    )?;
    if trusted != resolved.value {
        bail!("trusted workflow Config bytes differ from the selected Config layer");
    }
    let config = decode_workflow_config(resolved.value.clone(), recovery_family)?;
    if config.category.trim().is_empty() || config.category.chars().any(char::is_control) {
        bail!("remote-worker workflow Config category is invalid");
    }
    let driver = CanonicalRef::parse(&config.driver)?;
    if driver.kind != "graph" || driver.suffix.is_some() {
        bail!("remote-worker workflow driver must be an unsuffixed graph reference");
    }
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(&config.ref_bindings)?;
    for value in config.ref_bindings.values() {
        let binding = CanonicalRef::parse(value)?;
        if engine.kinds.get(&binding.kind).is_none() {
            bail!("remote-worker workflow binding kind is not registered");
        }
    }
    let parameters = render_driver_parameters(
        &config.parameters,
        task,
        credential_profile_id,
        target_product_selections,
    )?;
    let effective = engine.effective_resolution_output(EffectiveItemRequest {
        item_ref: driver,
        expected_kind: Some("graph".to_owned()),
        project_root: Some(context.effective_path.clone()),
        subject_resolution_authority: SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.to_owned(),
        },
    })?;
    if !matches!(
        effective.effective_trust_class,
        ryeos_engine::resolution::TrustClass::TrustedBundle
            | ryeos_engine::resolution::TrustClass::TrustedProject
    ) {
        bail!("remote-worker workflow driver is not trusted");
    }
    let effective_definition_digest = effective
        .effective_definition_digest()
        .context("compute source Graph effective-definition identity")?
        .as_str()
        .to_owned();
    let source_definition_derived = effective.composed.derived.clone();
    Ok(CompiledWorkflow {
        driver: config.driver,
        ref_bindings: config.ref_bindings,
        parameters,
        target_requirements: config.target_requirements,
        digest: ryeos_state::objects::canonical_value_digest(&resolved.value)?,
        effective_definition_digest,
        source_definition_derived,
    })
}

fn render_driver_parameters(
    source: &Value,
    task: &Value,
    credential_profile_id: &str,
    target_product_selections: &ryeos_state::external_content::products::composition::ProductSelectionInputs,
) -> Result<Value> {
    let template = ryeos_runtime::CompiledJsonTemplate::compile(
        source,
        "remote-worker workflow driver parameters",
        &ryeos_runtime::CompilationLimits::default(),
    )?;
    if template.references().roots().any(|root| root != "inputs") {
        bail!("remote-worker workflow parameter template may reference only inputs");
    }
    let context = serde_json::json!({
        "inputs": {
            "task": task,
            "credential_profile_id": credential_profile_id,
            "target_product_selections": target_product_selections,
        }
    });
    let limits = ryeos_runtime::EvaluationLimits::default();
    let mut session = ryeos_runtime::EvaluationSession::new(&context, &limits);
    let rendered = template.render(&mut session)?;
    if !rendered.is_object() || serde_json::to_vec(&rendered)?.len() > MAX_TASK_BYTES {
        bail!("remote-worker workflow rendered parameters must be a bounded object");
    }
    Ok(rendered)
}

async fn inspect_target_readiness(
    client: &RemoteClient,
    requirements: &TargetRuntimeRequirements,
) -> Result<TargetReadinessEvidence> {
    let status = client
        .execute_service_result_with_total_timeout(
            "service:node/status",
            &BTreeMap::new(),
            None,
            &serde_json::json!({}),
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            STATUS_TIMEOUT,
        )
        .await
        .context("inspect target runtime readiness")?;
    target_readiness_evidence(&status, requirements)
}

fn target_readiness_evidence(
    status: &Value,
    requirements: &TargetRuntimeRequirements,
) -> Result<TargetReadinessEvidence> {
    let revision = status
        .get("revision")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("target node status omitted daemon revision")?;
    let isolation = status
        .get("isolation")
        .and_then(Value::as_object)
        .context("target node status omitted isolation readiness")?;
    let filesystem_mode: ryeos_engine::isolation::IsolationMode = serde_json::from_value(
        isolation
            .get("filesystem_mode")
            .cloned()
            .context("target node status omitted filesystem mode")?,
    )
    .context("target node status has invalid filesystem mode")?;
    let network_mode: ryeos_engine::isolation::IsolationNetworkMode = serde_json::from_value(
        isolation
            .get("network_mode")
            .cloned()
            .context("target node status omitted network mode")?,
    )
    .context("target node status has invalid network mode")?;
    if filesystem_mode != requirements.filesystem_mode {
        bail!(
            "target filesystem mode is not workflow-ready: required {:?}, observed {:?}",
            requirements.filesystem_mode,
            filesystem_mode
        );
    }
    if network_mode != requirements.network_mode {
        bail!(
            "target network mode is not workflow-ready: required {:?}, observed {:?}",
            requirements.network_mode,
            network_mode
        );
    }
    let process_scopes = isolation
        .get("process_scopes")
        .and_then(Value::as_object)
        .context("target node status omitted process-control readiness")?;
    let policy_digest = isolation
        .get("policy_digest")
        .and_then(Value::as_str)
        .filter(|value| lillux::valid_hash(value.strip_prefix("sha256:").unwrap_or(value)))
        .context("target node status omitted a valid isolation policy identity")?;
    let mut process_scope_authority_digest = None;
    let (ready, reason) = match (requirements.process_control, requirements.cleanup_authority) {
        (
            TargetProcessControl::OrdinarySubprocess | TargetProcessControl::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        )
        | (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::LocalProcessScope,
        )
        | (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        ) => {
            let selected = process_scopes
                .get(
                    if requirements.cleanup_authority
                        == PersistentSessionCleanupAuthority::TrustedProcessGroup
                    {
                        "trusted_exclusive_session"
                    } else {
                        requirements.process_control.status_field()
                    },
                )
                .and_then(Value::as_object)
                .context("target node status omitted selected process-control mode")?;
            if requirements.cleanup_authority
                == PersistentSessionCleanupAuthority::LocalProcessScope
            {
                process_scope_authority_digest = read_optional_digest(
                    process_scopes.get("authority_digest"),
                    "process-scope authority",
                )?;
                if process_scope_authority_digest.is_none() {
                    bail!(
                        "exclusive target readiness omitted protected process-scope authority identity"
                    );
                }
            }
            (
                selected
                    .get("ready")
                    .and_then(Value::as_bool)
                    .context("target node status omitted selected process-control readiness")?,
                selected
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .context("target node status omitted process-control reason")?,
            )
        }
        (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
        ) => {
            // A target cannot attest its own future death, enclosing-service
            // replacement, or safe slot reuse. Accept this mode only after a
            // source-side lifecycle adapter can contribute an independently
            // protected placement receipt; node/status is intentionally not
            // such an authority.
            bail!(
                "external placement-incarnation cleanup requires an independent source-side lifecycle receipt"
            )
        }
        _ => bail!("target runtime requirements pair incompatible process and cleanup authority"),
    };
    if !ready {
        bail!("target process cleanup is not workflow-ready: {reason}");
    }
    let evidence = TargetReadinessEvidence {
        requirements: requirements.clone(),
        daemon_revision: revision.to_owned(),
        isolation_policy_digest: policy_digest.to_owned(),
        process_scope_authority_digest,
        process_control_reason: reason.to_owned(),
    };
    // The status observation is the pre-contact admission boundary. Apply the
    // same closed evidence contract used by durable receipt validation here,
    // before project transfer or worker launch can occur.
    validate_target_readiness_evidence(&evidence)?;
    Ok(evidence)
}

fn read_optional_digest(value: Option<&Value>, label: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            value
                .as_str()
                .filter(|value| lillux::valid_hash(value.strip_prefix("sha256:").unwrap_or(value)))
                .map(str::to_owned)
                .with_context(|| format!("target node status has an invalid {label} identity"))
        })
        .transpose()
}

/// Resolve an interrupted target contact without replaying an accepted launch.
/// `None` is returned only when the authenticated owner-bound status proves
/// that no reservation exists, in which case the identical launch may be sent.
async fn adopt_contacted_launch(
    client: &RemoteClient,
    operation: &Operation,
) -> Result<Option<String>> {
    let status = match client
        .execute_service_result_with_total_timeout(
            "service:launch/status",
            &BTreeMap::new(),
            None,
            &serde_json::json!({"launch_id": operation.target_launch_id}),
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            STATUS_TIMEOUT,
        )
        .await
    {
        Ok(status) => status,
        Err(error)
            if error
                .downcast_ref::<crate::remote::client::RemoteHttpError>()
                .is_some_and(|remote| remote.status == reqwest::StatusCode::NOT_FOUND) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.context("query interrupted target launch contact")),
    };
    if status.get("launch_id").and_then(Value::as_str) != Some(operation.target_launch_id.as_str())
    {
        bail!("target launch status returned another launch coordinate");
    }
    match status.get("status").and_then(Value::as_str) {
        Some("bound") => {}
        Some("planning") | Some("qualified") => {
            return Err(TargetLaunchNotBound.into());
        }
        Some("failed") | Some("cancelled") | Some("expired") => {
            return Err(TargetLaunchTerminal(
                status
                    .get("status")
                    .and_then(Value::as_str)
                    .expect("matched terminal status")
                    .to_owned(),
            )
            .into());
        }
        _ => bail!("target launch status is not a current closed state"),
    }
    let thread_id = status
        .get("thread_id")
        .and_then(Value::as_str)
        .context("bound target launch omitted thread_id")?
        .to_owned();
    Ok(Some(thread_id))
}

async fn verify_target_launch(
    client: &RemoteClient,
    operation: &Operation,
    compiled: &CompiledWorkflow,
    parameters: &Value,
    execution_policy: &ryeos_app::execution_policy::ExecutionPolicy,
    expected_thread_id: &str,
) -> Result<TargetAdmissionEvidence> {
    ryeos_runtime::validate_runtime_thread_id(expected_thread_id).map_err(anyhow::Error::msg)?;
    let status = client
        .execute_service_result_with_total_timeout(
            "service:launch/status",
            &BTreeMap::new(),
            None,
            &serde_json::json!({"launch_id": operation.target_launch_id}),
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            STATUS_TIMEOUT,
        )
        .await
        .context("verify accepted target launch coordinate")?;
    if status.get("launch_id").and_then(Value::as_str) != Some(operation.target_launch_id.as_str())
        || status.get("status").and_then(Value::as_str) != Some("bound")
        || status.get("thread_id").and_then(Value::as_str) != Some(expected_thread_id)
    {
        bail!("accepted target launch is not exactly owner-bound to its returned thread");
    }
    let thread_id = expected_thread_id;
    let thread = client.threads_get(&thread_id).await?;
    if thread.pointer("/thread/item_ref").and_then(Value::as_str) != Some(compiled.driver.as_str())
        || thread
            .pointer("/thread/project_authority/base_snapshot_hash")
            .and_then(Value::as_str)
            != Some(operation.source_snapshot_hash.as_str())
    {
        bail!("bound target launch differs from retained workflow authority");
    }
    let capsule_hash = thread
        .pointer("/thread/admitted_launch_capsule_hash")
        .and_then(Value::as_str)
        .context("bound target launch omitted admitted capsule identity")?;
    if !lillux::valid_hash(capsule_hash) {
        bail!("bound target launch capsule identity is invalid");
    }
    let fetched = client
        .objects_get_with_total_timeout(&[capsule_hash.to_owned()], &[], STATUS_TIMEOUT)
        .await?;
    let capsule_value = fetched
        .entries
        .into_iter()
        .find(|entry| entry.hash == capsule_hash && entry.kind == "object")
        .and_then(|entry| entry.value)
        .context("bound target launch capsule is unavailable")?;
    let capsule = decode_target_capsule(capsule_hash, capsule_value)?;
    let sealed: ryeos_app::thread_lifecycle::SealedRootExecutionRequest =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &capsule,
        )?;
    let expected_project_identity = ryeos_app::launch_metadata::StableProjectIdentity::from_path(
        Path::new(&operation.target_project_path),
        &operation.target_site_id,
    )?
    .normalized_logical_key;
    let expected_project_authority_id = lillux::sha256_hex(
        format!(
            "live-project\0{}\0{}",
            expected_project_identity, operation.target_project_path
        )
        .as_bytes(),
    );
    let expected_environment = ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
        project_authority_id: expected_project_authority_id,
        source_identity: format!(
            "dotenv:{}",
            Path::new(&operation.target_project_path)
                .join(".env")
                .display()
        ),
        include_operator_vault: true,
        name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
    };
    let project_semantics_match = matches!(
        sealed.project_authority(),
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path: Some(display_path),
            base_snapshot_hash,
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            workspace_outputs: None,
            environment,
            capability_ceiling: _,
            child_policy: ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit,
        } if stable_project_identity == &expected_project_identity
            && display_path == Path::new(&operation.target_project_path)
            && base_snapshot_hash == &operation.source_snapshot_hash
            && snapshot_hash == &operation.source_snapshot_hash
            && environment == &expected_environment
    );
    let item_matches = sealed.item_ref() == compiled.driver;
    // Target admission finalizes receiver-local derived execution state (most
    // notably its effective hook plan) before sealing the capsule. Compare
    // the complete admitted resolution after projecting that derived layer
    // back to the source-owned definition captured from the pinned project.
    // The capsule decoder above independently verifies the unmodified target
    // effective-definition digest, so this normalization cannot bless a
    // forged target program.
    let mut source_definition_projection = sealed.admitted_effective_resolution()?.clone();
    source_definition_projection.composed.derived = compiled.source_definition_derived.clone();
    let projected_definition_digest = source_definition_projection
        .effective_definition_digest()?
        .as_str()
        .to_owned();
    let definition_matches = projected_definition_digest == compiled.effective_definition_digest;
    let parameters_match = sealed.admitted_parameters_digest()?
        == ryeos_state::objects::canonical_value_digest(parameters)?;
    let snapshot_matches = sealed.project_authority().subject_base_snapshot_hash()
        == Some(operation.source_snapshot_hash.as_str());
    let bindings_match = sealed.ref_bindings() == &compiled.ref_bindings;
    let selections_match = sealed.product_selections().is_empty();
    let lifecycle_matches = capsule.lifecycle_authority == execution_policy.lifecycle_authority();
    if !(item_matches
        && definition_matches
        && parameters_match
        && snapshot_matches
        && bindings_match
        && selections_match
        && lifecycle_matches
        && project_semantics_match)
    {
        bail!(
            "bound target launch capsule differs from retained workflow request \
             (item={item_matches}, definition={definition_matches}, \
             parameters={parameters_match}, snapshot={snapshot_matches}, \
             bindings={bindings_match}, selections={selections_match}, \
             lifecycle={lifecycle_matches}, project={project_semantics_match}; \
             admitted_definition={}, projected_definition={}, expected_definition={})",
            sealed.effective_definition_digest().as_str(),
            projected_definition_digest,
            compiled.effective_definition_digest,
        );
    }
    Ok(TargetAdmissionEvidence {
        admitted_capsule_hash: capsule_hash.to_owned(),
        exact_program_hash: capsule.exact_program_hash.clone(),
        effective_definition_digest: sealed.effective_definition_digest().as_str().to_owned(),
    })
}

async fn observe_target_completion(
    client: &RemoteClient,
    operation: &Operation,
    compiled: &CompiledWorkflow,
    target_chain_root_id: &str,
    target_admission: &TargetAdmissionEvidence,
    target_readiness: &TargetReadinessEvidence,
    launch_acceptance_drive_root_id: &str,
    target_signing_key: &lillux::crypto::VerifyingKey,
    settlement_drive_root_id: &str,
) -> Result<Option<Receipt>> {
    let Some((terminal_thread_id, graph_result, graph_result_digest)) =
        observe_exact_graph_terminal(client, target_chain_root_id, compiled, target_admission)
            .await?
    else {
        return Ok(None);
    };
    let returned: WorkflowGraphResult = serde_json::from_value(
        graph_result
            .result
            .clone()
            .context("target Graph terminal omitted its signed workflow return")?,
    )
    .map_err(|error| TargetWorkflowTerminalInvalid(error.to_string()))?;
    if returned.schema != WORKFLOW_GRAPH_RESULT_SCHEMA {
        return Err(TargetWorkflowTerminalInvalid(
            "target Graph returned another workflow-result schema".to_owned(),
        )
        .into());
    }
    ryeos_runtime::validate_runtime_thread_id(&returned.candidate_terminal_thread_id)
        .map_err(|error| TargetWorkflowTerminalInvalid(error.to_string()))?;

    let candidate_result = super::remote_pull_worker_result::query_candidate_result_with_client(
        client,
        &returned.candidate_terminal_thread_id,
        &operation.source_site_id,
        &format!("fp:{}", operation.operator_fingerprint),
        &operation.target_site_id,
        target_signing_key,
    )
    .await?;
    let evidence = &candidate_result.evidence;
    // v1 deliberately supports only the bounded single-placement seam. The
    // Graph return is a terminal thread coordinate, not a general chain-root
    // resolver; continued or moved worker children therefore fail closed.
    if evidence.chain_root_id != returned.candidate_terminal_thread_id
        || evidence.base_snapshot_hash != operation.source_snapshot_hash
        || evidence.target_project_path != operation.target_project_path
    {
        return Err(TargetWorkflowTerminalInvalid(
            "candidate testimony differs from the exact bounded Graph return".to_owned(),
        )
        .into());
    }
    let fetched = client
        .objects_get_with_total_timeout(
            &[evidence.admitted_launch_capsule_hash.clone()],
            &[],
            STATUS_TIMEOUT,
        )
        .await?;
    let candidate_capsule_value = fetched
        .entries
        .into_iter()
        .find(|entry| entry.hash == evidence.admitted_launch_capsule_hash && entry.kind == "object")
        .and_then(|entry| entry.value)
        .context("candidate launch capsule is unavailable")?;
    let candidate_capsule = decode_target_capsule(
        &evidence.admitted_launch_capsule_hash,
        candidate_capsule_value,
    )?;
    let candidate_sealed: ryeos_app::thread_lifecycle::SealedRootExecutionRequest =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &candidate_capsule,
        )?;
    if candidate_sealed.product_selections() != &operation.target_product_selections {
        return Err(TargetWorkflowTerminalInvalid(
            "candidate launch used different target product selections".to_owned(),
        )
        .into());
    }

    Ok(Some(Receipt {
        schema: RESPONSE_SCHEMA.to_owned(),
        allowed_next_action: "pull_result".to_owned(),
        source_work_id: operation.source_work_id.clone(),
        remote: operation.remote.clone(),
        source_snapshot_hash: operation.source_snapshot_hash.clone(),
        workflow_ref: operation.workflow_ref.clone(),
        workflow_digest: compiled.digest.clone(),
        target_launch_id: operation.target_launch_id.clone(),
        target_readiness: target_readiness.clone(),
        target_chain_root_id: target_chain_root_id.to_owned(),
        target_admission: target_admission.clone(),
        launch_acceptance_drive_root_id: launch_acceptance_drive_root_id.to_owned(),
        target_workflow_terminal_thread_id: terminal_thread_id,
        target_workflow_result_digest: graph_result_digest,
        candidate_terminal_thread_id: returned.candidate_terminal_thread_id,
        candidate_result,
        target_product_selections_digest: operation.target_product_selections_digest.clone(),
        settlement_drive_root_id: settlement_drive_root_id.to_owned(),
    }))
}

async fn observe_exact_graph_terminal(
    client: &RemoteClient,
    target_chain_root_id: &str,
    compiled: &CompiledWorkflow,
    target_admission: &TargetAdmissionEvidence,
) -> Result<Option<(String, ryeos_graph_definition::GraphResult, String)>> {
    ryeos_runtime::validate_runtime_thread_id(target_chain_root_id).map_err(anyhow::Error::msg)?;
    let mut current = target_chain_root_id.to_owned();
    for _ in 0..MAX_TARGET_CHAIN_SEGMENTS {
        let response = client.threads_get(&current).await?;
        if response.is_null() {
            bail!("target Graph thread is not observable by its admitted owner");
        }
        if response
            .pointer("/thread/thread_id")
            .and_then(Value::as_str)
            != Some(current.as_str())
            || response
                .pointer("/thread/chain_root_id")
                .and_then(Value::as_str)
                != Some(target_chain_root_id)
            || response.pointer("/thread/item_ref").and_then(Value::as_str)
                != Some(compiled.driver.as_str())
        {
            return Err(TargetWorkflowTerminalInvalid(
                "target Graph continuation differs from admitted workflow identity".to_owned(),
            )
            .into());
        }
        if let Some(successor) = response
            .pointer("/thread/successor_thread_id")
            .and_then(Value::as_str)
        {
            ryeos_runtime::validate_runtime_thread_id(successor)
                .map_err(|error| TargetWorkflowTerminalInvalid(error.to_string()))?;
            current = successor.to_owned();
            continue;
        }
        if !target_graph_tip_is_terminal(
            response.pointer("/thread/status").and_then(Value::as_str),
        )? {
            return Ok(None);
        }
        let value = response
            .pointer("/result/result")
            .cloned()
            .context("completed target Graph omitted its result")?;
        let digest = ryeos_state::objects::canonical_value_digest(&value)?;
        let result: ryeos_graph_definition::GraphResult = serde_json::from_value(value)
            .map_err(|error| TargetWorkflowTerminalInvalid(error.to_string()))?;
        if !result.success
            || result.status != ryeos_graph_definition::GraphRunStatus::Completed
            || result.definition_ref != compiled.driver
            || result.effective_definition_digest != target_admission.effective_definition_digest
        {
            return Err(TargetWorkflowTerminalInvalid(
                "target Graph result contradicts admitted effective definition".to_owned(),
            )
            .into());
        }
        return Ok(Some((current, result, digest)));
    }
    Err(TargetWorkflowTerminalInvalid("target Graph continuation exceeds bound".to_owned()).into())
}

fn target_graph_tip_is_terminal(status: Option<&str>) -> Result<bool> {
    match status {
        Some("created" | "running" | "queued" | "continued") => Ok(false),
        Some("completed") => Ok(true),
        Some(status) => Err(TargetLaunchTerminal(status.to_owned()).into()),
        None => Err(
            TargetWorkflowTerminalInvalid("target Graph thread omitted status".to_owned()).into(),
        ),
    }
}

fn decode_target_capsule(
    expected_hash: &str,
    value: Value,
) -> Result<ryeos_state::objects::AdmittedLaunchCapsule> {
    if ryeos_state::objects::canonical_value_digest(&value)? != expected_hash {
        bail!("bound target launch capsule bytes differ from their CAS identity");
    }
    serde_json::from_value(value).context("decode bound target launch capsule")
}

fn resolve_route(
    state: &AppState,
    remote_name: &str,
    project: &Path,
) -> Result<(String, RemoteConfig, String)> {
    let canonical = config::canonical_local_project_path(project)?;
    let local = config::local_project_identity(&canonical)?.to_owned();
    let report = config::load_remotes_layered_report(&state.config.app_root, Some(&canonical))?;
    let loaded = config::get_loaded_remote(&report.remotes, remote_name)?;
    let binding = config::resolve_loaded_project_binding(&loaded, &canonical)?;
    if binding.sync_scope != ProjectSyncScope::FullProject {
        bail!("remote-worker workflow requires a full_project remote binding");
    }
    Ok((local, loaded.config, binding.remote_project_path))
}

fn recorded_pinned_project(state: &AppState, source_work_id: &str) -> Result<(String, String)> {
    let root = state
        .state_store
        .authoritative_thread_subjects(&[source_work_id])?
        .into_iter()
        .next()
        .flatten()
        .context("recorded remote-worker workflow root is absent")?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        display_path,
        snapshot_hash,
        ..
    } = &root.project_authority
    else {
        bail!("remote-worker workflow requires exact pinned-generation root authority");
    };
    let path = display_path
        .as_ref()
        .context("pinned remote-worker workflow root omitted its canonical project path")?;
    if !path.is_absolute() {
        bail!("pinned remote-worker workflow project path is not absolute");
    }
    if !lillux::valid_hash(snapshot_hash) {
        bail!("pinned remote-worker workflow snapshot identity is invalid");
    }
    Ok((
        path.to_str()
            .context("pinned remote-worker workflow project path is not Unicode")?
            .to_owned(),
        snapshot_hash.clone(),
    ))
}

fn validate_current_route(state: &AppState, operation: &Operation) -> Result<RemoteConfig> {
    validate_operation(operation)?;
    if state.threads.site_id() != operation.source_site_id {
        bail!("remote-worker workflow belongs to another source site");
    }
    if ryeos_app::operator_authority::admitted_operator_authority_digest(
        state,
        &operation.operator_fingerprint,
    )? != operation.operator_authority_digest
    {
        bail!("remote-worker workflow operator grant changed");
    }
    let (_, remote, target) = resolve_route(
        state,
        &operation.remote,
        Path::new(&operation.local_project_path),
    )?;
    if remote.url != operation.remote_url
        || remote.site_id != operation.target_site_id
        || remote.principal_id != operation.target_principal_id
        || remote.signing_key != operation.target_signing_key
        || target != operation.target_project_path
    {
        bail!("remote-worker workflow route authority changed");
    }
    Ok(remote)
}

fn validate_operation(operation: &Operation) -> Result<()> {
    if operation.operation_type != OPERATION_TYPE
        || !matches!(
            operation.schema.as_str(),
            OPERATION_SCHEMA | HISTORICAL_OPERATION_SCHEMA
        )
    {
        bail!("remote-worker workflow operation schema or type is not current");
    }
    ryeos_runtime::validate_runtime_thread_id(&operation.source_work_id)
        .map_err(anyhow::Error::msg)?;
    ryeos_executor::executor::validate_service_invocation_id(&operation.source_invocation_id)?;
    if operation.source_work_id != derive_source_work_id(&operation.source_invocation_id)? {
        bail!("remote-worker workflow work identity differs from its recorded invocation");
    }
    if ryeos_state::objects::canonical_value_digest(&operation.admitted_start_request)?
        != operation.admitted_start_request_digest
    {
        bail!("remote-worker workflow retained request differs from its digest");
    }
    let request: StartRequest = serde_json::from_value(operation.admitted_start_request.clone())
        .context("reparse retained remote-worker workflow start request")?;
    if request.remote != operation.remote
        || request.workflow_ref != operation.workflow_ref
        || request.credential_profile_id != operation.credential_profile_id
        || request.task != operation.task
        || request
            .source_snapshot_hash
            .as_deref()
            .is_some_and(|hash| hash != operation.source_snapshot_hash)
        || request.target_product_selections != operation.target_product_selections
    {
        bail!("remote-worker workflow operation differs from its admitted start request");
    }
    if operation.target_launch_id != derive_target_launch_id(&operation.source_work_id)? {
        bail!("remote-worker workflow target launch coordinate is invalid");
    }
    if !operation.task.is_object()
        || serde_json::to_vec(&operation.task)?.len() > MAX_TASK_BYTES
        || ryeos_state::objects::canonical_value_digest(&operation.task)? != operation.task_digest
    {
        bail!("remote-worker workflow task differs from its retained digest");
    }
    let canonical =
        ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
            operation.target_product_selections.clone(),
        )?;
    if canonical != operation.target_product_selections
        || target_product_selections_digest(&canonical)?
            != operation.target_product_selections_digest
    {
        bail!("remote-worker workflow target product selections differ from retained authority");
    }
    if !lillux::valid_hash(&operation.operator_fingerprint)
        || !lillux::valid_hash(&operation.operator_authority_digest)
        || !lillux::valid_hash(&operation.admitted_start_request_digest)
        || !lillux::valid_hash(&operation.admitted_source_snapshot_hash)
        || !lillux::valid_hash(&operation.source_snapshot_hash)
    {
        bail!("remote-worker workflow operator authority is invalid");
    }
    ryeos_app::identity::validate_canonical_site_id(&operation.source_site_id)?;
    ryeos_app::identity::validate_canonical_site_id(&operation.target_site_id)?;
    if !Path::new(&operation.local_project_path).is_absolute() {
        bail!("remote-worker workflow local project path is not absolute");
    }
    config::validate_remote_project_path(&operation.target_project_path)?;
    let target_fingerprint = operation
        .target_principal_id
        .strip_prefix("fp:")
        .context("remote-worker target principal is not canonical")?;
    if !lillux::valid_hash(target_fingerprint) {
        bail!("remote-worker target principal fingerprint is invalid");
    }
    let target_key = config::decode_signing_key(&operation.target_signing_key)?;
    if lillux::crypto::fingerprint(&target_key) != target_fingerprint {
        bail!("remote-worker target key differs from target principal");
    }
    let workflow = CanonicalRef::parse(&operation.workflow_ref)?;
    if workflow.kind != "config" || workflow.suffix.is_some() {
        bail!("remote-worker workflow reference is invalid");
    }
    validate_profile_id(&operation.credential_profile_id)?;
    Ok(())
}

fn validate_receipt(receipt: &Receipt, operation: &Operation) -> Result<()> {
    if receipt.schema != RESPONSE_SCHEMA
        || receipt.allowed_next_action != "pull_result"
        || receipt.source_work_id != operation.source_work_id
        || receipt.remote != operation.remote
        || receipt.workflow_ref != operation.workflow_ref
        || receipt.target_launch_id != operation.target_launch_id
        || receipt.target_product_selections_digest != operation.target_product_selections_digest
        || receipt.source_snapshot_hash != operation.source_snapshot_hash
        || !lillux::valid_hash(&receipt.workflow_digest)
        || !lillux::valid_hash(&receipt.target_admission.admitted_capsule_hash)
        || !lillux::valid_hash(&receipt.target_admission.exact_program_hash)
        || !lillux::valid_hash(&receipt.target_workflow_result_digest)
        || validate_target_readiness_evidence(&receipt.target_readiness).is_err()
        || ryeos_engine::resolution::EffectiveDefinitionDigest::parse(
            receipt.target_admission.effective_definition_digest.clone(),
        )
        .is_err()
    {
        bail!("remote-worker workflow receipt contradicts retained operation authority");
    }
    ryeos_runtime::validate_runtime_thread_id(&receipt.target_chain_root_id)
        .map_err(|error| anyhow::anyhow!(error))?;
    ryeos_executor::executor::validate_service_invocation_id(
        &receipt.launch_acceptance_drive_root_id,
    )?;
    ryeos_runtime::validate_runtime_thread_id(&receipt.target_workflow_terminal_thread_id)
        .map_err(|error| anyhow::anyhow!(error))?;
    ryeos_runtime::validate_runtime_thread_id(&receipt.candidate_terminal_thread_id)
        .map_err(|error| anyhow::anyhow!(error))?;
    let candidate_request = HostedCandidateResultRequest {
        chain_root_id: receipt.candidate_terminal_thread_id.clone(),
        source_site_id: operation.source_site_id.clone(),
    };
    receipt.candidate_result.validate_against(
        &candidate_request,
        &format!("fp:{}", operation.operator_fingerprint),
        &operation.target_site_id,
        &config::decode_signing_key(&operation.target_signing_key)?,
    )?;
    if receipt.candidate_result.evidence.chain_root_id != receipt.candidate_terminal_thread_id
        || receipt.candidate_result.evidence.source_site_id != operation.source_site_id
        || receipt.candidate_result.evidence.target_site_id != operation.target_site_id
        || receipt.candidate_result.evidence.owner_principal
            != format!("fp:{}", operation.operator_fingerprint)
        || receipt.candidate_result.evidence.base_snapshot_hash != operation.source_snapshot_hash
        || receipt.candidate_result.evidence.target_project_path != operation.target_project_path
    {
        bail!("remote-worker workflow candidate receipt contradicts retained authority");
    }
    Ok(())
}

fn validate_target_readiness_evidence(evidence: &TargetReadinessEvidence) -> Result<()> {
    let expected_reason = match evidence.requirements.process_control {
        TargetProcessControl::ExclusiveSession
            if evidence.requirements.cleanup_authority
                == PersistentSessionCleanupAuthority::TrustedProcessGroup =>
        {
            "trusted_process_group"
        }
        TargetProcessControl::ExclusiveSession => "ready",
        TargetProcessControl::OrdinarySubprocess | TargetProcessControl::PooledRequests => {
            "not_required"
        }
    };
    let valid_optional_digest = |digest: Option<&str>| {
        digest.is_none_or(|digest| {
            lillux::valid_hash(digest.strip_prefix("sha256:").unwrap_or(digest))
        })
    };
    let authority_shape_valid = match (
        evidence.requirements.process_control,
        evidence.requirements.cleanup_authority,
    ) {
        (
            TargetProcessControl::OrdinarySubprocess | TargetProcessControl::PooledRequests,
            PersistentSessionCleanupAuthority::NotRequired,
        ) => evidence.process_scope_authority_digest.is_none(),
        (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::LocalProcessScope,
        ) => evidence.process_scope_authority_digest.is_some(),
        (
            TargetProcessControl::ExclusiveSession,
            PersistentSessionCleanupAuthority::TrustedProcessGroup,
        ) => evidence.process_scope_authority_digest.is_none(),
        _ => false,
    };
    if evidence.daemon_revision.is_empty()
        || !lillux::valid_hash(
            evidence
                .isolation_policy_digest
                .strip_prefix("sha256:")
                .unwrap_or(&evidence.isolation_policy_digest),
        )
        || evidence.process_control_reason != expected_reason
        || !authority_shape_valid
        || !valid_optional_digest(evidence.process_scope_authority_digest.as_deref())
    {
        bail!("remote-worker target readiness evidence is invalid");
    }
    Ok(())
}

fn validate_recorded_owner(state: &AppState, operation: &Operation) -> Result<()> {
    let operator_principal = format!("fp:{}", operation.operator_fingerprint);
    let root = state
        .state_store
        .authoritative_thread_subjects(&[&operation.source_invocation_id])?
        .into_iter()
        .next()
        .flatten()
        .context("recorded remote-worker workflow root is absent")?;
    if root.thread_id != operation.source_invocation_id
        || root.chain_root_id != operation.source_invocation_id
        || root.item_ref != "service:remote-worker-workflows/start"
        || root.requested_by.as_deref() != Some(operator_principal.as_str())
    {
        bail!("remote-worker workflow recovery projection differs from recorded root authority");
    }
    if state
        .threads
        .recorded_service_parameters_digest(&operation.source_invocation_id)?
        != operation.admitted_start_request_digest
    {
        bail!("remote-worker workflow recovery projection differs from admitted request authority");
    }
    let (project_path, admitted_snapshot_hash) =
        recorded_pinned_project(state, &operation.source_invocation_id)?;
    if project_path != operation.local_project_path
        || admitted_snapshot_hash != operation.admitted_source_snapshot_hash
    {
        bail!("remote-worker workflow recovery projection differs from recorded project authority");
    }
    if select_source_snapshot(
        state,
        &admitted_snapshot_hash,
        Some(&operation.source_snapshot_hash),
    )? != operation.source_snapshot_hash
    {
        bail!("remote-worker workflow selected source generation changed");
    }
    Ok(())
}

fn validate_recorded_invocation(
    state: &AppState,
    invocation_root: &str,
    operator_fingerprint: &str,
    service_ref: &str,
) -> Result<()> {
    let operator_principal = format!("fp:{operator_fingerprint}");
    let root = state
        .state_store
        .authoritative_thread_subjects(&[invocation_root])?
        .into_iter()
        .next()
        .flatten()
        .context("recorded remote-worker invocation root is absent")?;
    if root.thread_id != invocation_root
        || root.chain_root_id != invocation_root
        || root.item_ref != service_ref
        || root.requested_by.as_deref() != Some(operator_principal.as_str())
    {
        bail!("remote-worker workflow invocation differs from recorded root authority");
    }
    Ok(())
}

fn operation_digest(operation: &Operation) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::to_value(operation)?)
}

fn launch_acceptance_operation_id(source_work_id: &str) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema": "ryeos.remote_worker_workflow_launch_acceptance_operation.v1",
        "source_work_id": source_work_id,
    }))
}

fn append_launch_acceptance(
    state: &AppState,
    operation: &Operation,
    drive_root_id: &str,
    acceptance: &LaunchAcceptance,
    recovery_family: RecoveryFamily,
) -> Result<()> {
    validate_launch_acceptance(acceptance, operation, &operation_digest(operation)?)?;
    if acceptance.launch_acceptance_drive_root_id != drive_root_id {
        bail!("remote-worker workflow launch acceptance names another drive root");
    }
    ryeos_app::authoritative_root_fact::append_once(
        state,
        drive_root_id,
        LAUNCH_ACCEPTED_EVENT,
        &launch_acceptance_operation_id(&operation.source_work_id)?,
        retained_evidence_value(
            acceptance,
            recovery_family,
            HISTORICAL_LAUNCH_ACCEPTANCE_SCHEMA,
        )?,
    )
}

fn read_launch_acceptance(
    state: &AppState,
    operation: &Operation,
    operation_digest: &str,
    drive_root_id: &str,
    recovery_family: RecoveryFamily,
) -> Result<Option<LaunchAcceptance>> {
    let operation_id = launch_acceptance_operation_id(&operation.source_work_id)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        drive_root_id,
        LAUNCH_ACCEPTED_EVENT,
        &operation_id,
    )?;
    if fact.count == 0 {
        return Ok(None);
    }
    if fact.count != 1 {
        bail!("remote-worker workflow launch acceptance fact is duplicated");
    }
    let value = strip_fact_operation_id(
        fact.payload
            .context("remote-worker workflow launch acceptance fact is unavailable")?,
        &operation_id,
    )?;
    decode_launch_acceptance(value, operation, operation_digest, recovery_family).map(Some)
}

fn decode_launch_acceptance(
    mut value: Value,
    operation: &Operation,
    operation_digest: &str,
    recovery_family: RecoveryFamily,
) -> Result<LaunchAcceptance> {
    upgrade_historical_evidence(
        &mut value,
        HISTORICAL_LAUNCH_ACCEPTANCE_SCHEMA,
        LAUNCH_ACCEPTANCE_SCHEMA,
        recovery_family,
    )?;
    let acceptance: LaunchAcceptance =
        serde_json::from_value(value).context("parse retained remote-worker launch acceptance")?;
    validate_launch_acceptance(&acceptance, operation, operation_digest)?;
    Ok(acceptance)
}

fn strip_fact_operation_id(mut payload: Value, expected: &str) -> Result<Value> {
    let operation_id = payload
        .as_object_mut()
        .context("authoritative workflow fact payload is not an object")?
        .remove("operation_id")
        .and_then(|value| value.as_str().map(str::to_owned));
    if operation_id.as_deref() != Some(expected) {
        bail!("authoritative workflow fact carries another operation identity");
    }
    Ok(payload)
}

fn validate_launch_acceptance(
    acceptance: &LaunchAcceptance,
    operation: &Operation,
    operation_digest: &str,
) -> Result<()> {
    if acceptance.schema != LAUNCH_ACCEPTANCE_SCHEMA
        || acceptance.source_work_id != operation.source_work_id
        || acceptance.operation_digest != operation_digest
        || acceptance.target_launch_id != operation.target_launch_id
        || acceptance.launch_acceptance_drive_root_id.trim().is_empty()
        || !lillux::valid_hash(&acceptance.workflow_digest)
        || !lillux::valid_hash(&acceptance.target_request_digest)
        || !lillux::valid_hash(&acceptance.target_admission.admitted_capsule_hash)
        || !lillux::valid_hash(&acceptance.target_admission.exact_program_hash)
        || validate_target_readiness_evidence(&acceptance.target_readiness).is_err()
        || ryeos_engine::resolution::EffectiveDefinitionDigest::parse(
            acceptance
                .target_admission
                .effective_definition_digest
                .clone(),
        )
        .is_err()
    {
        bail!("remote-worker workflow launch acceptance contradicts retained authority");
    }
    ryeos_runtime::validate_runtime_thread_id(&acceptance.target_chain_root_id)
        .map_err(anyhow::Error::msg)?;
    ryeos_executor::executor::validate_service_invocation_id(
        &acceptance.launch_acceptance_drive_root_id,
    )
}

fn reconcile_launch_acceptance(
    state: &AppState,
    operation: &Operation,
    operation_digest: &str,
    workflow_digest: &str,
    target_request_digest: &str,
    target_requirements: &TargetRuntimeRequirements,
    progress: &mut Progress,
    recovery_family: RecoveryFamily,
) -> Result<()> {
    let acceptance_root = progress
        .launch_acceptance_drive_root_id
        .as_deref()
        .or(progress.drive_root_id.as_deref());
    let acceptance = match acceptance_root {
        Some(root) => {
            read_launch_acceptance(state, operation, operation_digest, root, recovery_family)?
        }
        None => None,
    };
    let Some(acceptance) = acceptance else {
        if progress.target_chain_root_id.is_some() || progress.target_admission.is_some() {
            bail!("remote-worker workflow projection claims unproved launch acceptance");
        }
        return Ok(());
    };
    if acceptance.workflow_digest != workflow_digest
        || acceptance.target_request_digest != target_request_digest
        || &acceptance.target_readiness.requirements != target_requirements
        || acceptance_root != Some(acceptance.launch_acceptance_drive_root_id.as_str())
        || progress
            .target_readiness
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_readiness)
        || progress
            .target_chain_root_id
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_chain_root_id)
        || progress
            .target_admission
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_admission)
    {
        bail!("remote-worker workflow projection differs from launch acceptance fact");
    }
    progress.workflow_digest = Some(acceptance.workflow_digest);
    progress.target_request_digest = Some(acceptance.target_request_digest);
    progress.target_readiness = Some(acceptance.target_readiness);
    progress.launch_acceptance_drive_root_id = Some(acceptance.launch_acceptance_drive_root_id);
    progress.target_chain_root_id = Some(acceptance.target_chain_root_id);
    progress.target_admission = Some(acceptance.target_admission);
    Ok(())
}

fn reconcile_launch_acceptance_for_query(
    state: &AppState,
    operation: &Operation,
    operation_digest: &str,
    progress: &mut Progress,
    recovery_family: RecoveryFamily,
) -> Result<()> {
    let acceptance_root = progress
        .launch_acceptance_drive_root_id
        .as_deref()
        .or(progress.drive_root_id.as_deref());
    let acceptance = match acceptance_root {
        Some(root) => {
            read_launch_acceptance(state, operation, operation_digest, root, recovery_family)?
        }
        None => None,
    };
    let Some(acceptance) = acceptance else {
        if progress.target_chain_root_id.is_some() || progress.target_admission.is_some() {
            bail!("remote-worker workflow projection claims unproved launch acceptance");
        }
        return Ok(());
    };
    if progress
        .workflow_digest
        .as_ref()
        .is_some_and(|value| value != &acceptance.workflow_digest)
        || progress
            .target_readiness
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_readiness)
        || progress
            .target_request_digest
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_request_digest)
        || progress
            .target_chain_root_id
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_chain_root_id)
        || progress
            .target_admission
            .as_ref()
            .is_some_and(|value| value != &acceptance.target_admission)
        || acceptance_root != Some(acceptance.launch_acceptance_drive_root_id.as_str())
    {
        bail!("remote-worker workflow projection differs from launch acceptance fact");
    }
    progress.workflow_digest = Some(acceptance.workflow_digest);
    progress.target_request_digest = Some(acceptance.target_request_digest);
    progress.target_readiness = Some(acceptance.target_readiness);
    progress.launch_acceptance_drive_root_id = Some(acceptance.launch_acceptance_drive_root_id);
    progress.target_chain_root_id = Some(acceptance.target_chain_root_id);
    progress.target_admission = Some(acceptance.target_admission);
    Ok(())
}

fn drive_fact_operation_id(
    source_work_id: &str,
    drive_root_id: &str,
    kind: &str,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema": "ryeos.remote_worker_workflow_drive_fact_operation.v1",
        "source_work_id": source_work_id,
        "drive_root_id": drive_root_id,
        "kind": kind,
    }))
}

fn validate_drive_invocation(
    state: &AppState,
    drive_root_id: &str,
    operation: &Operation,
) -> Result<()> {
    let service_ref = if drive_root_id == operation.source_invocation_id {
        "service:remote-worker-workflows/start"
    } else {
        "service:remote-worker-workflows/resume"
    };
    validate_recorded_invocation(
        state,
        drive_root_id,
        &operation.operator_fingerprint,
        service_ref,
    )?;
    if drive_root_id != operation.source_invocation_id {
        let expected = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "source_work_id": operation.source_work_id,
        }))?;
        if state
            .threads
            .recorded_service_parameters_digest(drive_root_id)?
            != expected
        {
            bail!("remote-worker workflow resume root names another request");
        }
    }
    Ok(())
}

fn append_drive_intent(
    state: &AppState,
    drive_root_id: &str,
    operation: &Operation,
    operation_digest: &str,
) -> Result<()> {
    ryeos_app::authoritative_root_fact::append_once(
        state,
        drive_root_id,
        DRIVE_INTENT_EVENT,
        &drive_fact_operation_id(&operation.source_work_id, drive_root_id, "intent")?,
        serde_json::json!({
            "schema": DRIVE_FACT_SCHEMA,
            "source_work_id": operation.source_work_id,
            "operation_digest": operation_digest,
        }),
    )
}

fn validate_drive_intent(
    state: &AppState,
    drive_root_id: &str,
    operation: &Operation,
    operation_digest: &str,
) -> Result<()> {
    validate_drive_invocation(state, drive_root_id, operation)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        drive_root_id,
        DRIVE_INTENT_EVENT,
        &drive_fact_operation_id(&operation.source_work_id, drive_root_id, "intent")?,
    )?;
    if fact.count != 1
        || fact
            .payload
            .as_ref()
            .and_then(|value| value.get("schema"))
            .and_then(Value::as_str)
            != Some(DRIVE_FACT_SCHEMA)
        || fact
            .payload
            .as_ref()
            .and_then(|value| value.get("source_work_id"))
            .and_then(Value::as_str)
            != Some(operation.source_work_id.as_str())
        || fact
            .payload
            .as_ref()
            .and_then(|value| value.get("operation_digest"))
            .and_then(Value::as_str)
            != Some(operation_digest)
    {
        bail!("remote-worker workflow drive intent is absent or contradictory");
    }
    Ok(())
}

fn append_settlement_fact(
    state: &AppState,
    drive_root_id: &str,
    operation: &Operation,
    operation_digest: &str,
    receipt: &Receipt,
    recovery_family: RecoveryFamily,
) -> Result<()> {
    let retained_receipt =
        retained_evidence_value(receipt, recovery_family, HISTORICAL_RESPONSE_SCHEMA)?;
    ryeos_app::authoritative_root_fact::append_once(
        state,
        drive_root_id,
        DRIVE_SETTLED_EVENT,
        &drive_fact_operation_id(&operation.source_work_id, drive_root_id, "settled")?,
        serde_json::json!({
            "schema": DRIVE_FACT_SCHEMA,
            "source_work_id": operation.source_work_id,
            "operation_digest": operation_digest,
            "receipt": retained_receipt,
        }),
    )
}

fn read_settlement_fact(
    state: &AppState,
    drive_root_id: &str,
    operation: &Operation,
    operation_digest: &str,
    recovery_family: RecoveryFamily,
) -> Result<Option<Receipt>> {
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        drive_root_id,
        DRIVE_SETTLED_EVENT,
        &drive_fact_operation_id(&operation.source_work_id, drive_root_id, "settled")?,
    )?;
    if fact.count == 0 {
        return Ok(None);
    }
    if fact.count != 1 {
        bail!("remote-worker workflow settlement fact is duplicated");
    }
    let payload = fact
        .payload
        .context("remote-worker workflow settlement fact is unavailable")?;
    decode_settlement_payload(
        payload,
        drive_root_id,
        operation,
        operation_digest,
        recovery_family,
    )
    .map(Some)
}

fn decode_settlement_payload(
    payload: Value,
    drive_root_id: &str,
    operation: &Operation,
    operation_digest: &str,
    recovery_family: RecoveryFamily,
) -> Result<Receipt> {
    if payload.get("schema").and_then(Value::as_str) != Some(DRIVE_FACT_SCHEMA)
        || payload.get("source_work_id").and_then(Value::as_str)
            != Some(operation.source_work_id.as_str())
        || payload.get("operation_digest").and_then(Value::as_str) != Some(operation_digest)
    {
        bail!("remote-worker workflow settlement fact is contradictory");
    }
    let receipt = decode_receipt(
        payload
            .get("receipt")
            .cloned()
            .context("workflow settlement omitted receipt")?,
        recovery_family,
    )?;
    validate_receipt(&receipt, operation)?;
    if receipt.settlement_drive_root_id != drive_root_id {
        bail!("remote-worker workflow settlement names another drive occurrence");
    }
    Ok(receipt)
}

fn authoritative_receipt(
    state: &AppState,
    operation: &Operation,
    receipt: &Receipt,
    operation_digest: &str,
    recovery_family: RecoveryFamily,
) -> Result<()> {
    validate_receipt(receipt, operation)?;
    let acceptance = read_launch_acceptance(
        state,
        operation,
        operation_digest,
        &receipt.launch_acceptance_drive_root_id,
        recovery_family,
    )?
    .context("completed workflow lacks authoritative launch acceptance")?;
    if acceptance.workflow_digest != receipt.workflow_digest
        || acceptance.target_chain_root_id != receipt.target_chain_root_id
        || acceptance.target_admission != receipt.target_admission
        || acceptance.target_readiness != receipt.target_readiness
    {
        bail!("completed workflow receipt contradicts launch acceptance");
    }
    validate_drive_intent(
        state,
        &receipt.settlement_drive_root_id,
        operation,
        operation_digest,
    )?;
    let settled = read_settlement_fact(
        state,
        &receipt.settlement_drive_root_id,
        operation,
        operation_digest,
        recovery_family,
    )?
    .context("completed workflow projection lacks authoritative settlement")?;
    if settled != *receipt {
        bail!("completed workflow projection contradicts authoritative settlement");
    }
    Ok(())
}

struct WorkflowAttempt {
    state: Arc<AppState>,
    job_id: String,
    attempt_id: String,
    active: bool,
}

impl WorkflowAttempt {
    fn begin(state: Arc<AppState>, job_id: &str) -> Result<Option<Self>> {
        let attempt_id = format!("remote-worker-workflow-attempt:{}", uuid::Uuid::new_v4());
        let claimed = claim_workflow_attempt(&state.state_store, job_id, &attempt_id)?;
        Ok(claimed.then_some(Self {
            state,
            job_id: job_id.to_owned(),
            attempt_id,
            active: true,
        }))
    }

    fn complete(
        &mut self,
        operation: &Operation,
        receipt: &Receipt,
        recovery_family: RecoveryFamily,
    ) -> Result<()> {
        let retained_receipt =
            retained_evidence_value(receipt, recovery_family, HISTORICAL_RESPONSE_SCHEMA)?;
        self.state.state_store.with_state_db(|db| {
            let latest = db
                .get_sync_job(&self.job_id)?
                .context("remote-worker workflow disappeared before completion")?;
            db.finish_sync_job_attempt_and_update_job(
                &self.attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: ryeos_state::SyncJobAttemptState::Completed,
                    phase: "completed".to_owned(),
                    error: None,
                    result: Some(retained_receipt.clone()),
                },
                &self.job_id,
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "completed".to_owned(),
                    roots: Some(vec![operation.source_snapshot_hash.clone()]),
                    heads: Some(vec![operation.source_snapshot_hash.clone()]),
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: latest.fetched_hashes,
                    last_error: None,
                    result: Some(retained_receipt),
                },
            )
        })?;
        self.active = false;
        Ok(())
    }

    fn pause(
        &mut self,
        operation: &Operation,
        progress: &Progress,
        phase: &str,
        recovery_family: RecoveryFamily,
    ) -> Result<()> {
        let retained_progress =
            retained_evidence_value(progress, recovery_family, HISTORICAL_PROGRESS_SCHEMA)?;
        self.state.state_store.with_state_db(|db| {
            let latest = db
                .get_sync_job(&self.job_id)?
                .context("remote-worker workflow disappeared after launch acceptance")?;
            db.finish_sync_job_attempt_and_update_job(
                &self.attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: ryeos_state::SyncJobAttemptState::Completed,
                    phase: phase.to_owned(),
                    error: None,
                    result: Some(retained_progress.clone()),
                },
                &self.job_id,
                &SyncJobUpdate {
                    state: SyncJobState::Running,
                    phase: phase.to_owned(),
                    roots: Some(vec![operation.source_snapshot_hash.clone()]),
                    heads: Some(Vec::new()),
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: latest.fetched_hashes,
                    last_error: None,
                    result: Some(retained_progress),
                },
            )
        })?;
        self.active = false;
        Ok(())
    }

    fn fail(&mut self, error: &anyhow::Error) -> Result<()> {
        let detail = bounded_error(&format!("{error:#}"));
        let retryable = workflow_error_is_retryable(error);
        self.state.state_store.with_state_db(|db| {
            let latest = db
                .get_sync_job(&self.job_id)?
                .context("remote-worker workflow disappeared before failure settlement")?;
            db.finish_sync_job_attempt_and_update_job(
                &self.attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: ryeos_state::SyncJobAttemptState::Failed,
                    phase: if retryable { "retryable" } else { "failed" }.to_owned(),
                    error: Some(detail.clone()),
                    result: None,
                },
                &self.job_id,
                &SyncJobUpdate {
                    state: if retryable {
                        SyncJobState::Retryable
                    } else {
                        SyncJobState::Failed
                    },
                    phase: if retryable {
                        latest.phase
                    } else {
                        "failed".to_owned()
                    },
                    roots: None,
                    heads: None,
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: latest.fetched_hashes,
                    last_error: Some(detail),
                    result: latest.result,
                },
            )
        })?;
        self.active = false;
        Ok(())
    }
}

fn claim_workflow_attempt(
    state_store: &ryeos_app::state_store::StateStore,
    job_id: &str,
    attempt_id: &str,
) -> Result<bool> {
    state_store.with_state_db(|db| {
        let job = db
            .get_sync_job(job_id)?
            .context("remote-worker workflow disappeared before attempt claim")?;
        if db
            .list_sync_job_attempts(job_id)?
            .iter()
            .any(|attempt| attempt.state == ryeos_state::SyncJobAttemptState::Running)
        {
            return Ok(false);
        }
        if !matches!(
            job.state,
            SyncJobState::Running | SyncJobState::Retryable | SyncJobState::Planned
        ) {
            bail!("remote-worker workflow is not retryable");
        }
        db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
            attempt_id: attempt_id.to_owned(),
            job_id: job_id.to_owned(),
            worker_id: Some("remote-worker-workflow".to_owned()),
            phase: "reserved".to_owned(),
        })?;
        Ok(true)
    })
}

impl Drop for WorkflowAttempt {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let error = "remote-worker workflow attempt returned before settlement".to_owned();
        if let Err(settle_error) = self.state.state_store.with_state_db(|db| {
            let latest = db
                .get_sync_job(&self.job_id)?
                .context("remote-worker workflow disappeared during attempt settlement")?;
            db.finish_sync_job_attempt_and_update_job(
                &self.attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: ryeos_state::SyncJobAttemptState::Failed,
                    phase: "retryable".to_owned(),
                    error: Some(error.clone()),
                    result: None,
                },
                &self.job_id,
                &SyncJobUpdate {
                    state: SyncJobState::Retryable,
                    phase: latest.phase,
                    roots: None,
                    heads: None,
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: latest.fetched_hashes,
                    last_error: Some(error),
                    result: latest.result,
                },
            )
        }) {
            tracing::error!(job_id = %self.job_id, error = %settle_error, "failed to settle remote-worker workflow attempt");
        }
    }
}

fn in_progress_response(operation: &Operation, job: &SyncJobRecord, progress: &Progress) -> Value {
    let allowed_next_action = allowed_next_action_for_state(job.state);
    serde_json::json!({
        "source_work_id": operation.source_work_id,
        "state": job.state.as_str(),
        "phase": job.phase,
        "remote": operation.remote,
        "source_snapshot_hash": operation.source_snapshot_hash,
        "workflow_ref": operation.workflow_ref,
        "workflow_digest": progress.workflow_digest,
        "target_launch_id": operation.target_launch_id,
        "target_chain_root_id": progress.target_chain_root_id,
        "target_admission": progress.target_admission,
        "target_readiness": progress.target_readiness,
        "allowed_next_action": allowed_next_action,
        "error": job.last_error,
        "receipt": Value::Null,
    })
}

fn allowed_next_action_for_state(state: SyncJobState) -> Value {
    if matches!(
        state,
        SyncJobState::Running | SyncJobState::Retryable | SyncJobState::Planned
    ) {
        Value::String("resume".to_owned())
    } else {
        Value::Null
    }
}

fn launch_accepted_response(operation: &Operation, progress: &Progress) -> Value {
    serde_json::json!({
        "source_work_id": operation.source_work_id,
        "state": "running",
        "phase": "launch_accepted",
        "remote": operation.remote,
        "source_snapshot_hash": operation.source_snapshot_hash,
        "workflow_ref": operation.workflow_ref,
        "workflow_digest": progress.workflow_digest,
        "target_launch_id": operation.target_launch_id,
        "target_chain_root_id": progress.target_chain_root_id,
        "target_admission": progress.target_admission,
        "target_readiness": progress.target_readiness,
        "allowed_next_action": "resume",
        "receipt": Value::Null,
    })
}

fn completion_pending_response(operation: &Operation, progress: &Progress) -> Value {
    serde_json::json!({
        "source_work_id": operation.source_work_id,
        "state": "running",
        "phase": "completion_pending",
        "remote": operation.remote,
        "source_snapshot_hash": operation.source_snapshot_hash,
        "workflow_ref": operation.workflow_ref,
        "workflow_digest": progress.workflow_digest,
        "target_launch_id": operation.target_launch_id,
        "target_chain_root_id": progress.target_chain_root_id,
        "target_admission": progress.target_admission,
        "target_readiness": progress.target_readiness,
        "allowed_next_action": "resume",
        "receipt": Value::Null,
    })
}

fn workflow_error_is_retryable(error: &anyhow::Error) -> bool {
    !error.chain().any(|cause| {
        cause.downcast_ref::<TargetLaunchTerminal>().is_some()
            || cause
                .downcast_ref::<TargetWorkflowTerminalInvalid>()
                .is_some()
    })
}

fn bounded_error(value: &str) -> String {
    const MAX_ERROR_BYTES: usize = 4 * 1024;
    if value.len() <= MAX_ERROR_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_ERROR_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &value[..end])
}

fn update_progress(
    state: &AppState,
    job_id: &str,
    phase: &str,
    progress: &Progress,
    recovery_family: RecoveryFamily,
    roots: Vec<String>,
) -> Result<()> {
    let retained_progress =
        retained_evidence_value(progress, recovery_family, HISTORICAL_PROGRESS_SCHEMA)?;
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: phase.to_owned(),
                roots: Some(roots),
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(retained_progress),
            },
        )
    })
}

fn derive_target_launch_id(source_work_id: &str) -> Result<String> {
    let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema": "ryeos.remote_worker_workflow_target_launch.v1",
        "source_work_id": source_work_id,
    }))?;
    Ok(format!("L-{}", &digest[..32]))
}

fn derive_source_work_id(source_invocation_id: &str) -> Result<String> {
    ryeos_executor::executor::validate_service_invocation_id(source_invocation_id)?;
    let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema": "ryeos.remote_worker_workflow_source_work_id.v1",
        "source_invocation_id": source_invocation_id,
    }))?;
    Ok(format!(
        "T-{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32],
    ))
}

fn exact_snapshot_execution_policy(
    snapshot_hash: &str,
) -> Result<ryeos_app::execution_policy::ExecutionPolicy> {
    let mut policy = ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
        ryeos_app::execution_policy::ExecutionResponse::Accepted,
    );
    let ryeos_app::execution_policy::ProjectExecutionPolicy::Pinned { source, .. } =
        &mut policy.project
    else {
        unreachable!("local pinned-capture constructor is pinned")
    };
    *source = ryeos_app::execution_policy::PinnedSource::Snapshot {
        hash: snapshot_hash.to_owned(),
    };
    policy.validate()?;
    Ok(policy)
}

fn job_id(source_work_id: &str) -> String {
    format!("remote-worker-workflow:{source_work_id}")
}

fn validate_profile_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("credential profile id is invalid");
    }
    Ok(())
}

fn select_source_snapshot(
    state: &AppState,
    admitted_snapshot_hash: &str,
    requested_snapshot_hash: Option<&str>,
) -> Result<String> {
    let Some(requested_snapshot_hash) = requested_snapshot_hash else {
        return Ok(admitted_snapshot_hash.to_owned());
    };
    if !lillux::valid_hash(requested_snapshot_hash) {
        bail!("source_snapshot_hash is not a valid CAS hash");
    }
    if requested_snapshot_hash == admitted_snapshot_hash {
        return Ok(admitted_snapshot_hash.to_owned());
    }

    let cas_read = state.acquire_cas_read()?;
    let cas = cas_read.cas();
    let admitted = cas
        .get_object(admitted_snapshot_hash)?
        .with_context(|| format!("admitted project snapshot {admitted_snapshot_hash} is absent"))?;
    let admitted = ryeos_state::objects::ProjectSnapshot::from_value(&admitted)?;
    let requested = cas
        .get_object(requested_snapshot_hash)?
        .with_context(|| format!("selected source snapshot {requested_snapshot_hash} is absent"))?;
    let requested = ryeos_state::objects::ProjectSnapshot::from_value(&requested)?;
    if requested.project_tree_hash != admitted.project_tree_hash
        || requested.effective_policy_hash != admitted.effective_policy_hash
    {
        bail!(
            "source_snapshot_hash does not contain the project tree and policy captured at admission"
        );
    }
    Ok(requested_snapshot_hash.to_owned())
}

fn validate_target_product_selections(
    inputs: ryeos_state::external_content::products::composition::ProductSelectionInputs,
) -> Result<ryeos_state::external_content::products::composition::ProductSelectionInputs> {
    let canonical =
        ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
            inputs.clone(),
        )?;
    if canonical != inputs {
        bail!("target_product_selections must be in canonical order");
    }
    Ok(canonical)
}

fn target_product_selections_digest(
    inputs: &ryeos_state::external_content::products::composition::ProductSelectionInputs,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::to_value(inputs)?)
}

pub const START_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote-worker-workflows/start",
    endpoint: "remote-worker-workflows.start",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: StartRequest = crate::handler_error::parse_request(params.clone())?;
            start(req, params, ctx, state).await
        })
    },
};

pub const QUERY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote-worker-workflows/query",
    endpoint: "remote-worker-workflows.query",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: QueryRequest = crate::handler_error::parse_request(params)?;
            query(req, ctx, state).await
        })
    },
};

pub const RESUME_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote-worker-workflows/resume",
    endpoint: "remote-worker-workflows.resume",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ResumeRequest = crate::handler_error::parse_request(params)?;
            resume(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_state::signer::Signer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_state_store(root: &Path) -> ryeos_app::state_store::StateStore {
        let identity =
            ryeos_app::identity::NodeIdentity::create(&root.join("identity/node-key.pem")).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        ryeos_app::state_store::StateStore::new_with_head_trust(
            root.to_owned(),
            root.join(".ai/state"),
            root.join("runtime.sqlite3"),
            signer,
            ryeos_app::write_barrier::WriteBarrier::new(),
            Arc::new(trust),
        )
        .unwrap()
    }

    #[test]
    fn high_level_request_has_no_target_coordinate_or_project_path() {
        let request = serde_json::json!({
            "remote": "qualification",
            "workflow_ref": "config:development/remote-worker",
            "credential_profile_id": "personal",
            "task": {"objective": "change one file"},
            "target_product_selections": [],
        });
        serde_json::from_value::<StartRequest>(request.clone()).unwrap();
        let mut missing = request.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("target_product_selections");
        assert!(serde_json::from_value::<StartRequest>(missing).is_err());
        for field in ["launch_id", "project", "target_project_path"] {
            let mut invalid = request.clone();
            invalid[field] = Value::String("caller-owned".into());
            assert!(serde_json::from_value::<StartRequest>(invalid).is_err());
        }
    }

    fn product_selection(
        declaration_id: &str,
    ) -> ryeos_state::external_content::products::composition::ProductSelectionInput {
        serde_json::from_value(serde_json::json!({
            "target": {"kind": "root"},
            "selection": {
                "declaration_id": declaration_id,
                "witness_hash": "a".repeat(64),
                "witness_source": {"kind": "local_capture"},
                "qualification_hash": null,
            }
        }))
        .unwrap()
    }

    #[test]
    fn target_product_selections_use_the_existing_bounded_canonical_contract() {
        let first = product_selection("a");
        let second = product_selection("b");
        assert!(validate_target_product_selections(vec![first.clone(), second.clone()]).is_ok());
        assert!(validate_target_product_selections(vec![second, first.clone()]).is_err());
        assert!(validate_target_product_selections(vec![first.clone(), first.clone()]).is_err());
        assert!(
            validate_target_product_selections(vec![
                first;
                ryeos_state::external_content::products::composition::MAX_PRODUCT_SELECTIONS
                    + 1
            ])
            .is_err()
        );
    }

    #[test]
    fn target_launch_is_deterministic_and_separate() {
        let source = "T-0025cdcf-c920-0783-ff3a-95250dd8f8d2";
        let first = derive_target_launch_id(source).unwrap();
        assert_eq!(first, derive_target_launch_id(source).unwrap());
        assert!(ryeos_app::state_store::is_canonical_launch_id(&first));
        assert_ne!(first, source);
    }

    #[test]
    fn workflow_config_is_closed_and_provider_neutral() {
        let config = decode_workflow_config(serde_json::json!({
            "category": "provider",
            "schema": WORKFLOW_SCHEMA,
            "driver": "graph:provider/bounded-task",
            "target_requirements": {
                "process_control": "exclusive_session",
                "cleanup_authority": "local_process_scope",
                "filesystem_mode": "enforce",
                "network_mode": "host",
            },
            "ref_bindings": {"environment": "config:development/environment"},
            "parameters": {"request": "${inputs.task}", "profile": "${inputs.credential_profile_id}"},
        }), RecoveryFamily::Current)
        .unwrap();
        assert_eq!(config.driver, "graph:provider/bounded-task");
        assert_eq!(
            render_driver_parameters(
                &config.parameters,
                &serde_json::json!({"objective": "edit"}),
                "personal",
                &Vec::new(),
            )
            .unwrap(),
            serde_json::json!({"request": {"objective": "edit"}, "profile": "personal"})
        );
        assert!(
            decode_workflow_config(
                serde_json::json!({
                    "category": "provider",
                    "schema": WORKFLOW_SCHEMA,
                    "driver": "graph:provider/bounded-task",
                    "target_requirements": {
                        "process_control": "exclusive_session",
                        "cleanup_authority": "local_process_scope",
                        "filesystem_mode": "enforce",
                        "network_mode": "host",
                    },
                    "parameters": {},
                    "target_project_path": "/wrong-owner",
                }),
                RecoveryFamily::Current
            )
            .is_err()
        );
        assert!(
            decode_workflow_config(
                serde_json::json!({
                    "category": "provider",
                    "schema": WORKFLOW_SCHEMA,
                    "driver": "graph:provider/bounded-task",
                    "target_requirements": {
                        "process_control": "pooled_requests",
                        "filesystem_mode": "disabled",
                        "network_mode": "host",
                    },
                    "parameters": {},
                }),
                RecoveryFamily::Current
            )
            .is_err()
        );
        assert!(
            decode_workflow_config(
                serde_json::json!({
                    "category": "provider",
                    "schema": WORKFLOW_SCHEMA,
                    "driver": "graph:provider/bounded-task",
                    "target_requirements": {
                        "process_control": "pooled_requests",
                        "cleanup_authority": "local_process_scope",
                        "filesystem_mode": "disabled",
                        "network_mode": "host",
                    },
                    "parameters": {},
                }),
                RecoveryFamily::Current
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_requirement_pairs_are_validated_before_contact() {
        let requirement = |process_control, cleanup_authority| TargetRuntimeRequirements {
            process_control,
            cleanup_authority,
            filesystem_mode: ryeos_engine::isolation::IsolationMode::Disabled,
            network_mode: ryeos_engine::isolation::IsolationNetworkMode::Host,
        };
        for process_control in [
            TargetProcessControl::OrdinarySubprocess,
            TargetProcessControl::PooledRequests,
        ] {
            assert!(
                validate_target_runtime_requirements(&requirement(
                    process_control,
                    PersistentSessionCleanupAuthority::NotRequired,
                ))
                .is_ok()
            );
            assert!(
                validate_target_runtime_requirements(&requirement(
                    process_control,
                    PersistentSessionCleanupAuthority::LocalProcessScope,
                ))
                .is_err()
            );
        }
        assert!(
            validate_target_runtime_requirements(&requirement(
                TargetProcessControl::ExclusiveSession,
                PersistentSessionCleanupAuthority::LocalProcessScope,
            ))
            .is_ok()
        );
        assert!(
            validate_target_runtime_requirements(&requirement(
                TargetProcessControl::ExclusiveSession,
                PersistentSessionCleanupAuthority::TrustedProcessGroup,
            ))
            .is_ok()
        );
        for cleanup_authority in [
            PersistentSessionCleanupAuthority::NotRequired,
            PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
        ] {
            assert!(
                validate_target_runtime_requirements(&requirement(
                    TargetProcessControl::ExclusiveSession,
                    cleanup_authority,
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn historical_workflow_schema_has_a_narrow_read_only_upgrade() {
        let historical = serde_json::json!({
            "category": "provider",
            "schema": HISTORICAL_WORKFLOW_SCHEMA,
            "driver": "graph:provider/bounded-task",
            "target_requirements": {
                "process_control": "exclusive_session",
                "filesystem_mode": "enforce",
                "network_mode": "host",
            },
            "parameters": {},
        });
        assert!(decode_workflow_config(historical.clone(), RecoveryFamily::Current).is_err());
        let upgraded = decode_workflow_config(historical, RecoveryFamily::PreCleanupField).unwrap();
        assert_eq!(upgraded.schema, WORKFLOW_SCHEMA);
        assert_eq!(
            upgraded.target_requirements.cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );
    }

    #[test]
    fn historical_progress_upgrade_does_not_create_external_authority() {
        let mut historical = serde_json::json!({
            "schema": HISTORICAL_PROGRESS_SCHEMA,
            "pushed": false,
            "workflow_digest": null,
            "target_request_digest": null,
            "target_readiness": {
                "requirements": {
                    "process_control": "pooled_requests",
                    "filesystem_mode": "disabled",
                    "network_mode": "host"
                },
                "daemon_revision": "old",
                "isolation_policy_digest": "a".repeat(64),
                "process_scope_authority_digest": null,
                "process_control_reason": "not_required"
            },
            "target_chain_root_id": null,
            "target_admission": null,
            "launch_acceptance_drive_root_id": null,
            "drive_root_id": null
        });
        upgrade_historical_evidence(
            &mut historical,
            HISTORICAL_PROGRESS_SCHEMA,
            PROGRESS_SCHEMA,
            RecoveryFamily::PreCleanupField,
        )
        .unwrap();
        let progress: Progress = serde_json::from_value(historical).unwrap();
        assert_eq!(
            progress
                .target_readiness
                .unwrap()
                .requirements
                .cleanup_authority,
            PersistentSessionCleanupAuthority::NotRequired
        );
    }

    #[test]
    fn operation_version_selects_one_exact_recovery_family() {
        let mut operation = operation_fixture();
        let current_progress = serde_json::json!({"schema": PROGRESS_SCHEMA});
        assert_eq!(
            recovery_family(&operation, &current_progress).unwrap(),
            RecoveryFamily::Current
        );

        operation.schema = HISTORICAL_OPERATION_SCHEMA.to_owned();
        assert_eq!(
            recovery_family(
                &operation,
                &serde_json::json!({"schema": HISTORICAL_PROGRESS_SCHEMA})
            )
            .unwrap(),
            RecoveryFamily::PreCleanupField
        );
        assert_eq!(
            recovery_family(&operation, &current_progress).unwrap(),
            RecoveryFamily::ExplicitLocalCleanupBridge
        );
        assert!(recovery_family(&operation, &serde_json::json!({"schema": "unknown"})).is_err());

        let mut historical_receipt = serde_json::to_value(receipt_fixture(&operation)).unwrap();
        historical_receipt["schema"] = Value::String(HISTORICAL_RESPONSE_SCHEMA.to_owned());
        historical_receipt["target_readiness"]["requirements"]
            .as_object_mut()
            .unwrap()
            .remove("cleanup_authority");
        let decoded =
            decode_receipt(historical_receipt.clone(), RecoveryFamily::PreCleanupField).unwrap();
        assert_eq!(
            decoded.target_readiness.requirements.cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );
        assert!(
            decode_receipt(
                historical_receipt.clone(),
                RecoveryFamily::ExplicitLocalCleanupBridge
            )
            .is_err()
        );
        historical_receipt["target_readiness"]["requirements"]["cleanup_authority"] =
            Value::String("external_placement_incarnation".to_owned());
        assert!(decode_receipt(historical_receipt, RecoveryFamily::PreCleanupField).is_err());
    }

    #[test]
    fn resumed_pre_cleanup_operation_keeps_its_durable_evidence_family() {
        let mut operation = operation_fixture();
        operation.schema = HISTORICAL_OPERATION_SCHEMA.to_owned();

        let mut progress = Progress::new();
        progress.target_readiness = Some(readiness_fixture());
        let retained_progress = retained_evidence_value(
            &progress,
            RecoveryFamily::PreCleanupField,
            HISTORICAL_PROGRESS_SCHEMA,
        )
        .unwrap();
        assert_eq!(
            recovery_family(&operation, &retained_progress).unwrap(),
            RecoveryFamily::PreCleanupField
        );
        assert_eq!(
            retained_progress.get("schema").and_then(Value::as_str),
            Some(HISTORICAL_PROGRESS_SCHEMA)
        );
        assert!(
            retained_progress
                .pointer("/target_readiness/requirements/cleanup_authority")
                .is_none()
        );
        let decoded_progress = {
            let job = SyncJobRecord {
                job_id: "remote-worker-workflow:historical".into(),
                operation_type: OPERATION_TYPE.into(),
                operation: serde_json::to_value(&operation).unwrap(),
                peer: Some("default".into()),
                state: SyncJobState::Running,
                phase: "target_ready".into(),
                roots: Vec::new(),
                heads: Vec::new(),
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                attempt_count: 1,
                max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
                last_error: None,
                result: Some(retained_progress),
                created_at: "2026-09-16T00:00:00Z".into(),
                updated_at: "2026-09-16T00:00:01Z".into(),
                finished_at: None,
            };
            Progress::from_job(&job, RecoveryFamily::PreCleanupField).unwrap()
        };
        assert_eq!(decoded_progress, progress);

        let receipt = receipt_fixture(&operation);
        let retained_receipt = retained_evidence_value(
            &receipt,
            RecoveryFamily::PreCleanupField,
            HISTORICAL_RESPONSE_SCHEMA,
        )
        .unwrap();
        assert_eq!(
            recovery_family(&operation, &retained_receipt).unwrap(),
            RecoveryFamily::PreCleanupField
        );
        assert_eq!(
            decode_receipt(retained_receipt, RecoveryFamily::PreCleanupField).unwrap(),
            receipt
        );
    }

    fn target_status(
        process_ready: bool,
        process_reason: &str,
        filesystem_mode: &str,
        network_mode: &str,
    ) -> Value {
        let mut status = serde_json::json!({
            "revision": "target-revision",
            "isolation": {
                "filesystem_mode": filesystem_mode,
                "network_mode": network_mode,
                "policy_digest": "a".repeat(64),
                "process_scopes": {
                    "authority": if process_ready { "qualified" } else { "absent" },
                    "authority_digest": format!("sha256:{}", "b".repeat(64)),
                    "ordinary_subprocess": {"ready": true, "reason": "not_required"},
                    "pooled_requests": {"ready": true, "reason": "not_required"},
                    "exclusive_session": {
                        "ready": process_ready,
                        "reason": process_reason,
                    },
                    "trusted_exclusive_session": {
                        "ready": false,
                        "reason": "policy_unconfigured",
                    },
                },
            },
        });
        if !process_ready {
            status["isolation"]["process_scopes"]
                .as_object_mut()
                .unwrap()
                .remove("authority_digest");
        }
        status
    }

    #[test]
    fn target_readiness_requires_the_exact_signed_runtime_contract() {
        let requirements = readiness_fixture().requirements;
        let evidence = target_readiness_evidence(
            &target_status(true, "ready", "enforce", "host"),
            &requirements,
        )
        .unwrap();
        assert_eq!(evidence.requirements, requirements);
        assert_eq!(evidence.process_control_reason, "ready");
        assert!(evidence.process_scope_authority_digest.is_some());

        let error = target_readiness_evidence(
            &target_status(false, "protected_authority_absent", "enforce", "host"),
            &requirements,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("protected_authority_absent"));

        let error = target_readiness_evidence(
            &target_status(true, "ready", "disabled", "host"),
            &requirements,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("filesystem mode"));

        let error = target_readiness_evidence(
            &target_status(true, "unexpected", "enforce", "host"),
            &requirements,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("readiness evidence is invalid"));
    }

    #[test]
    fn pooled_request_readiness_does_not_claim_exclusive_scope_authority() {
        let requirements = TargetRuntimeRequirements {
            process_control: TargetProcessControl::PooledRequests,
            cleanup_authority: PersistentSessionCleanupAuthority::NotRequired,
            filesystem_mode: ryeos_engine::isolation::IsolationMode::Disabled,
            network_mode: ryeos_engine::isolation::IsolationNetworkMode::Host,
        };
        let evidence = target_readiness_evidence(
            &target_status(false, "policy_unconfigured", "disabled", "host"),
            &requirements,
        )
        .unwrap();
        assert_eq!(evidence.process_control_reason, "not_required");
        assert!(evidence.process_scope_authority_digest.is_none());

        let mut malformed = target_status(false, "policy_unconfigured", "disabled", "host");
        malformed["isolation"]["process_scopes"]["pooled_requests"]["reason"] =
            Value::String("ready".to_string());
        let error = target_readiness_evidence(&malformed, &requirements).unwrap_err();
        assert!(format!("{error:#}").contains("readiness evidence is invalid"));
    }

    #[test]
    fn target_cannot_self_attest_external_placement_cleanup() {
        let requirements = TargetRuntimeRequirements {
            process_control: TargetProcessControl::ExclusiveSession,
            cleanup_authority: PersistentSessionCleanupAuthority::ExternalPlacementIncarnation,
            filesystem_mode: ryeos_engine::isolation::IsolationMode::Disabled,
            network_mode: ryeos_engine::isolation::IsolationNetworkMode::Host,
        };
        let mut status = target_status(false, "policy_unconfigured", "disabled", "host");
        status["external_placement_incarnation"] = serde_json::json!({
            "ready": true,
            "reason": "ready",
            "authority_digest": format!("sha256:{}", "c".repeat(64)),
            "incarnation_digest": format!("sha256:{}", "d".repeat(64)),
            "controller_authority_digest": format!("sha256:{}", "e".repeat(64)),
        });
        let error = target_readiness_evidence(&status, &requirements).unwrap_err();
        assert!(format!("{error:#}").contains("independent source-side lifecycle receipt"));
    }

    #[test]
    fn trusted_process_group_readiness_requires_the_signed_target_opt_in() {
        let requirements = TargetRuntimeRequirements {
            process_control: TargetProcessControl::ExclusiveSession,
            cleanup_authority: PersistentSessionCleanupAuthority::TrustedProcessGroup,
            filesystem_mode: ryeos_engine::isolation::IsolationMode::Disabled,
            network_mode: ryeos_engine::isolation::IsolationNetworkMode::Host,
        };
        let mut status = target_status(false, "policy_unconfigured", "disabled", "host");
        let error = target_readiness_evidence(&status, &requirements).unwrap_err();
        assert!(format!("{error:#}").contains("policy_unconfigured"));

        status["isolation"]["process_scopes"]["trusted_exclusive_session"] =
            serde_json::json!({"ready": true, "reason": "trusted_process_group"});
        let evidence = target_readiness_evidence(&status, &requirements).unwrap();
        assert_eq!(evidence.process_control_reason, "trusted_process_group");
        assert!(evidence.process_scope_authority_digest.is_none());
    }

    #[test]
    fn ryeos_project_workflow_uses_the_exact_signed_driver_and_environment() {
        let root = ryeos_engine::test_support::workspace_root();
        let config_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(root.join(".ai/config/development/ryeos/remote-worker.yaml"))
                .unwrap(),
        )
        .unwrap();
        let config: WorkflowConfig = serde_json::from_value(config_value).unwrap();
        assert_eq!(config.schema, WORKFLOW_SCHEMA);
        assert_eq!(config.driver, "graph:ryeos/development/remote-worker");
        assert_eq!(
            config.target_requirements.process_control,
            TargetProcessControl::ExclusiveSession
        );
        assert!(config.ref_bindings.is_empty());

        let trusted_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(
                root.join(".ai/config/development/ryeos/trusted-remote-worker.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let trusted: WorkflowConfig = serde_json::from_value(trusted_value).unwrap();
        assert_eq!(
            trusted.target_requirements.cleanup_authority,
            PersistentSessionCleanupAuthority::TrustedProcessGroup
        );
        assert!(trusted.ref_bindings.is_empty());
        assert_eq!(
            trusted.driver,
            "graph:ryeos/development/trusted-remote-worker"
        );

        let graph_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(root.join(".ai/graphs/ryeos/development/remote-worker.yaml"))
                .unwrap(),
        )
        .unwrap();
        let graph: ryeos_graph_definition::GraphFile =
            serde_json::from_value(graph_value.clone()).unwrap();
        ryeos_graph_definition::validate_graph_file(&graph).unwrap();
        assert_eq!(
            graph_value.pointer("/config/nodes/run/action/item_id"),
            Some(&Value::String(
                "worker_execution:codex/bounded-turn".to_owned()
            ))
        );
        assert_eq!(
            graph_value.pointer("/config/nodes/run/action/ref_bindings/environment"),
            Some(&Value::String(
                "config:development/ryeos/worker-environment".to_owned()
            ))
        );
        let declared = graph_value
            .pointer("/requires/capabilities/declared")
            .and_then(Value::as_array)
            .unwrap();
        for capability in [
            "ryeos.runtime.dedicated_session.start",
            "ryeos.runtime.dedicated_session.command",
            "ryeos.runtime.dedicated_session.terminate",
        ] {
            assert!(declared.contains(&Value::String(capability.to_owned())));
        }
        assert_eq!(
            graph_value.pointer("/config/nodes/done/output/schema"),
            Some(&Value::String(WORKFLOW_GRAPH_RESULT_SCHEMA.to_owned()))
        );
        assert_eq!(
            graph_value.pointer("/config/nodes/run/assign/candidate_terminal_thread_id"),
            Some(&Value::String("${dispatch.child_thread_id}".to_owned()))
        );

        let trusted_graph_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(
                root.join(".ai/graphs/ryeos/development/trusted-remote-worker.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let trusted_graph: ryeos_graph_definition::GraphFile =
            serde_json::from_value(trusted_graph_value.clone()).unwrap();
        ryeos_graph_definition::validate_graph_file(&trusted_graph).unwrap();
        assert_eq!(
            trusted_graph_value.pointer("/config/nodes/run/action/ref_bindings/environment"),
            Some(&Value::String(
                "config:development/ryeos/trusted-worker-environment".to_owned()
            ))
        );
    }

    #[test]
    fn ryeos_recovery_qualification_workflow_selects_only_the_recovery_profile() {
        let root = ryeos_engine::test_support::workspace_root();
        let config_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(
                root.join(".ai/config/development/ryeos/remote-worker-recovery.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let config: WorkflowConfig = serde_json::from_value(config_value).unwrap();
        assert_eq!(config.schema, WORKFLOW_SCHEMA);
        assert_eq!(
            config.driver,
            "graph:ryeos/development/remote-worker-recovery"
        );
        assert_eq!(
            config.target_requirements.process_control,
            TargetProcessControl::ExclusiveSession
        );

        let graph_value: Value = serde_yaml::from_str(
            &std::fs::read_to_string(
                root.join(".ai/graphs/ryeos/development/remote-worker-recovery.yaml"),
            )
            .unwrap(),
        )
        .unwrap();
        let graph: ryeos_graph_definition::GraphFile =
            serde_json::from_value(graph_value.clone()).unwrap();
        ryeos_graph_definition::validate_graph_file(&graph).unwrap();
        assert_eq!(
            graph_value.pointer("/config/nodes/run/action/item_id"),
            Some(&Value::String(
                "worker_execution:codex/bounded-turn-recovery".to_owned()
            ))
        );
        assert_eq!(
            graph_value.pointer("/config/nodes/run/action/ref_bindings/environment"),
            Some(&Value::String(
                "config:development/ryeos/worker-environment".to_owned()
            ))
        );
    }

    #[test]
    fn resume_contract_accepts_only_source_work_id() {
        let request = serde_json::json!({
            "source_work_id": "T-0025cdcf-c920-0783-ff3a-95250dd8f8d2",
        });
        serde_json::from_value::<ResumeRequest>(request.clone()).unwrap();
        let mut invalid = request;
        invalid["remote"] = Value::String("other".into());
        assert!(serde_json::from_value::<ResumeRequest>(invalid).is_err());
    }

    #[test]
    fn authoritative_fact_envelope_is_removed_before_strict_payload_decode() {
        let operation_id = "a".repeat(64);
        let payload = serde_json::json!({
            "operation_id": operation_id,
            "schema": "fixture.v1",
        });
        assert_eq!(
            strip_fact_operation_id(payload, &operation_id).unwrap(),
            serde_json::json!({"schema": "fixture.v1"})
        );
        assert!(
            strip_fact_operation_id(
                serde_json::json!({"operation_id": "b".repeat(64)}),
                &operation_id,
            )
            .is_err()
        );
    }

    #[test]
    fn receipt_must_name_the_operations_exact_snapshot() {
        let mut operation = operation_fixture();
        let mut receipt = receipt_fixture(&operation);
        receipt.source_snapshot_hash = "f".repeat(64);
        assert!(validate_receipt(&receipt, &operation).is_err());
        operation.source_snapshot_hash = receipt.source_snapshot_hash.clone();
        let receipt = receipt_fixture(&operation);
        assert!(validate_receipt(&receipt, &operation).is_ok());

        let mut receipt = receipt_fixture(&operation);
        receipt.target_admission.admitted_capsule_hash = "invalid".into();
        assert!(validate_receipt(&receipt, &operation).is_err());

        let mut receipt = receipt_fixture(&operation);
        receipt.target_admission.exact_program_hash = "invalid".into();
        assert!(validate_receipt(&receipt, &operation).is_err());

        let mut receipt = receipt_fixture(&operation);
        receipt.target_admission.effective_definition_digest = "invalid".into();
        assert!(validate_receipt(&receipt, &operation).is_err());

        let mut receipt = receipt_fixture(&operation);
        receipt.candidate_terminal_thread_id = "T-another-candidate".into();
        assert!(validate_receipt(&receipt, &operation).is_err());

        let mut receipt = receipt_fixture(&operation);
        receipt.candidate_result.attestation.signature = "invalid".to_owned();
        assert!(validate_receipt(&receipt, &operation).is_err());
    }

    #[test]
    fn only_drivable_workflow_states_offer_resume() {
        for state in [
            SyncJobState::Planned,
            SyncJobState::Running,
            SyncJobState::Retryable,
        ] {
            assert_eq!(allowed_next_action_for_state(state), "resume");
        }
        for state in [
            SyncJobState::Completed,
            SyncJobState::Failed,
            SyncJobState::Cancelled,
        ] {
            assert_eq!(allowed_next_action_for_state(state), Value::Null);
        }
    }

    #[test]
    fn every_nonterminal_response_exposes_retained_target_readiness() {
        let operation = operation_fixture();
        let readiness = readiness_fixture();
        let mut progress = Progress::new();
        progress.target_readiness = Some(readiness.clone());
        let job = SyncJobRecord {
            job_id: "remote-worker-workflow:fixture".into(),
            operation_type: OPERATION_TYPE.into(),
            operation: serde_json::json!({}),
            peer: Some("default".into()),
            state: SyncJobState::Running,
            phase: "target_ready".into(),
            roots: Vec::new(),
            heads: Vec::new(),
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 1,
            max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
            last_error: None,
            result: None,
            created_at: "2026-09-16T00:00:00Z".into(),
            updated_at: "2026-09-16T00:00:01Z".into(),
            finished_at: None,
        };
        let expected = serde_json::to_value(readiness).unwrap();
        assert_eq!(
            in_progress_response(&operation, &job, &progress)["target_readiness"],
            expected
        );
        assert_eq!(
            launch_accepted_response(&operation, &progress)["target_readiness"],
            expected
        );
        assert_eq!(
            completion_pending_response(&operation, &progress)["target_readiness"],
            expected
        );
    }

    #[test]
    fn workflow_graph_return_is_closed_and_names_a_terminal_coordinate() {
        let returned: WorkflowGraphResult = serde_json::from_value(serde_json::json!({
            "schema": WORKFLOW_GRAPH_RESULT_SCHEMA,
            "candidate_terminal_thread_id": "T-target-candidate",
        }))
        .unwrap();
        ryeos_runtime::validate_runtime_thread_id(&returned.candidate_terminal_thread_id).unwrap();
        assert!(
            serde_json::from_value::<WorkflowGraphResult>(serde_json::json!({
                "schema": WORKFLOW_GRAPH_RESULT_SCHEMA,
                "candidate_terminal_thread_id": "T-target-candidate",
                "chain_root_id": "T-untrusted-alternate-authority",
            }))
            .is_err()
        );
    }

    #[test]
    fn followed_graph_without_resume_successor_is_completion_pending() {
        assert!(!target_graph_tip_is_terminal(Some("continued")).unwrap());
        assert!(!target_graph_tip_is_terminal(Some("running")).unwrap());
        assert!(target_graph_tip_is_terminal(Some("completed")).unwrap());
        assert!(target_graph_tip_is_terminal(Some("failed")).is_err());
    }

    #[test]
    fn launch_acceptance_binds_operation_request_and_drive_root() {
        let operation = operation_fixture();
        let digest = operation_digest(&operation).unwrap();
        let mut acceptance = LaunchAcceptance {
            schema: LAUNCH_ACCEPTANCE_SCHEMA.into(),
            source_work_id: operation.source_work_id.clone(),
            operation_digest: digest.clone(),
            workflow_digest: "1".repeat(64),
            target_request_digest: "2".repeat(64),
            target_launch_id: operation.target_launch_id.clone(),
            target_readiness: readiness_fixture(),
            launch_acceptance_drive_root_id: operation.source_invocation_id.clone(),
            target_chain_root_id: "T-target-graph".into(),
            target_admission: TargetAdmissionEvidence {
                admitted_capsule_hash: "3".repeat(64),
                exact_program_hash: "4".repeat(64),
                effective_definition_digest: "5".repeat(64),
            },
        };
        validate_launch_acceptance(&acceptance, &operation, &digest).unwrap();

        let mut changed = operation.clone();
        changed.target_product_selections = vec![product_selection("authoring-tools")];
        changed.target_product_selections_digest =
            target_product_selections_digest(&changed.target_product_selections).unwrap();
        changed.admitted_start_request["target_product_selections"] =
            serde_json::to_value(&changed.target_product_selections).unwrap();
        changed.admitted_start_request_digest =
            ryeos_state::objects::canonical_value_digest(&changed.admitted_start_request).unwrap();
        validate_operation(&changed).unwrap();
        let changed_digest = operation_digest(&changed).unwrap();
        assert_ne!(changed_digest, digest);
        assert!(validate_launch_acceptance(&acceptance, &changed, &changed_digest).is_err());

        acceptance.target_launch_id = derive_target_launch_id("T-another-source").unwrap();
        assert!(validate_launch_acceptance(&acceptance, &operation, &digest).is_err());
    }

    #[test]
    fn historical_launch_acceptance_preserves_identity_and_rejects_mixed_family() {
        let mut operation = operation_fixture();
        operation.schema = HISTORICAL_OPERATION_SCHEMA.to_owned();
        let digest = operation_digest(&operation).unwrap();
        let acceptance = LaunchAcceptance {
            schema: LAUNCH_ACCEPTANCE_SCHEMA.into(),
            source_work_id: operation.source_work_id.clone(),
            operation_digest: digest.clone(),
            workflow_digest: "1".repeat(64),
            target_request_digest: "2".repeat(64),
            target_launch_id: operation.target_launch_id.clone(),
            target_readiness: readiness_fixture(),
            launch_acceptance_drive_root_id: operation.source_invocation_id.clone(),
            target_chain_root_id: "T-target-graph".into(),
            target_admission: TargetAdmissionEvidence {
                admitted_capsule_hash: "3".repeat(64),
                exact_program_hash: "4".repeat(64),
                effective_definition_digest: "5".repeat(64),
            },
        };
        let mut historical = serde_json::to_value(&acceptance).unwrap();
        historical["schema"] = Value::String(HISTORICAL_LAUNCH_ACCEPTANCE_SCHEMA.to_owned());
        historical["target_readiness"]["requirements"]
            .as_object_mut()
            .unwrap()
            .remove("cleanup_authority");

        let decoded = decode_launch_acceptance(
            historical.clone(),
            &operation,
            &digest,
            RecoveryFamily::PreCleanupField,
        )
        .unwrap();
        assert_eq!(decoded.source_work_id, operation.source_work_id);
        assert_eq!(decoded.operation_digest, digest);
        assert_eq!(decoded.target_launch_id, operation.target_launch_id);
        assert_eq!(
            decoded.target_readiness.requirements.cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );
        assert!(
            decode_launch_acceptance(
                historical,
                &operation,
                &digest,
                RecoveryFamily::ExplicitLocalCleanupBridge,
            )
            .is_err()
        );
    }

    #[test]
    fn target_capsule_bytes_must_match_the_status_identity() {
        let value = serde_json::json!({"kind": "not-the-requested-capsule"});
        let another_hash = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "kind": "another-capsule"
        }))
        .unwrap();
        assert!(decode_target_capsule(&another_hash, value).is_err());
    }

    #[test]
    fn settlement_fact_payload_repairs_only_with_exact_receipt() {
        let operation = operation_fixture();
        let receipt = receipt_fixture(&operation);
        let digest = operation_digest(&operation).unwrap();
        let payload = serde_json::json!({
            "schema": DRIVE_FACT_SCHEMA,
            "source_work_id": operation.source_work_id,
            "operation_digest": digest,
            "receipt": receipt,
        });
        assert_eq!(
            decode_settlement_payload(
                payload.clone(),
                &operation.source_invocation_id,
                &operation,
                &operation_digest(&operation).unwrap(),
                RecoveryFamily::Current,
            )
            .unwrap(),
            receipt_fixture(&operation)
        );
        let mut forged = payload;
        forged["receipt"]["target_admission"]["exact_program_hash"] =
            Value::String("invalid".into());
        assert!(
            decode_settlement_payload(
                forged,
                &operation.source_invocation_id,
                &operation,
                &operation_digest(&operation).unwrap(),
                RecoveryFamily::Current,
            )
            .is_err()
        );
    }

    #[test]
    fn historical_settlement_folds_only_its_exact_receipt_family() {
        let mut operation = operation_fixture();
        operation.schema = HISTORICAL_OPERATION_SCHEMA.to_owned();
        let digest = operation_digest(&operation).unwrap();
        let mut historical_receipt = serde_json::to_value(receipt_fixture(&operation)).unwrap();
        historical_receipt["schema"] = Value::String(HISTORICAL_RESPONSE_SCHEMA.to_owned());
        historical_receipt["target_readiness"]["requirements"]
            .as_object_mut()
            .unwrap()
            .remove("cleanup_authority");
        let payload = serde_json::json!({
            "schema": DRIVE_FACT_SCHEMA,
            "source_work_id": operation.source_work_id,
            "operation_digest": digest,
            "receipt": historical_receipt,
        });
        let decoded = decode_settlement_payload(
            payload.clone(),
            &operation.source_invocation_id,
            &operation,
            &operation_digest(&operation).unwrap(),
            RecoveryFamily::PreCleanupField,
        )
        .unwrap();
        assert_eq!(decoded.source_work_id, operation.source_work_id);
        assert_eq!(decoded.target_launch_id, operation.target_launch_id);
        assert_eq!(
            decoded.target_readiness.requirements.cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );
        assert!(
            decode_settlement_payload(
                payload,
                &operation.source_invocation_id,
                &operation,
                &operation_digest(&operation).unwrap(),
                RecoveryFamily::ExplicitLocalCleanupBridge,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn lost_launch_ack_adopts_bound_coordinate_without_second_execute() {
        #[derive(Clone)]
        struct Fixture {
            calls: Arc<AtomicUsize>,
            launch_id: String,
        }
        async fn status(
            axum::extract::State(fixture): axum::extract::State<Fixture>,
        ) -> axum::Json<Value> {
            fixture.calls.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({
                "thread": {
                    "status": "completed",
                    "thread_id": "svc-1789074790258-1427d4e2",
                },
                "result": {
                    "launch_id": fixture.launch_id,
                    "status": "bound",
                    "thread_id": "T-12345678-1234-1234-1234-123456789abc",
                }
            }))
        }

        let operation = operation_fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/execute", axum::routing::post(status))
            .with_state(Fixture {
                calls: calls.clone(),
                launch_id: operation.target_launch_id.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let directory = tempfile::tempdir().unwrap();
        let identity = Arc::new(
            ryeos_app::identity::NodeIdentity::create(&directory.path().join("operator.pem"))
                .unwrap(),
        );
        let client = RemoteClient::new(&format!("http://{address}"), "fp:peer", identity);

        assert_eq!(
            adopt_contacted_launch(&client, &operation).await.unwrap(),
            Some("T-12345678-1234-1234-1234-123456789abc".into())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn operation_cannot_diverge_from_admitted_request_digest() {
        let mut operation = operation_fixture();
        assert!(validate_operation(&operation).is_ok());
        operation.task["objective"] = Value::String("different".into());
        assert!(validate_operation(&operation).is_err());

        let mut operation = operation_fixture();
        operation.target_product_selections_digest = "f".repeat(64);
        assert!(validate_operation(&operation).is_err());
    }

    #[test]
    fn retained_errors_are_bounded_without_losing_short_causes() {
        assert_eq!(bounded_error("network unavailable"), "network unavailable");
        let large = "x".repeat(8 * 1024);
        let bounded = bounded_error(&large);
        assert!(bounded.len() < large.len());
        assert!(bounded.ends_with("… [truncated]"));
    }

    #[test]
    fn durable_attempt_excludes_a_concurrent_driver() {
        let root = tempfile::tempdir().unwrap();
        let store = test_state_store(root.path());
        let job_id = "remote-worker-workflow:T-attempt";
        store
            .with_state_db(|db| {
                db.create_sync_job_with_initial_progress(
                    &NewSyncJob {
                        job_id: job_id.into(),
                        operation_type: OPERATION_TYPE.into(),
                        operation: serde_json::json!({
                            "operation_type": OPERATION_TYPE,
                            "fixture": true,
                        }),
                        peer: Some("default".into()),
                        roots: vec!["d".repeat(64)],
                        heads: Vec::new(),
                        max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
                    },
                    SyncJobState::Running,
                    "reserved",
                    Some(&serde_json::to_value(Progress::new()).unwrap()),
                )
            })
            .unwrap();
        assert!(claim_workflow_attempt(&store, job_id, "attempt:one").unwrap());
        assert!(!claim_workflow_attempt(&store, job_id, "attempt:two").unwrap());
        let attempts = store
            .with_state_db(|db| db.list_sync_job_attempts(job_id))
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].state, ryeos_state::SyncJobAttemptState::Running);
    }

    fn operation_fixture() -> Operation {
        let target_key = lillux::crypto::SigningKey::from_bytes(&[29_u8; 32]).verifying_key();
        let request = serde_json::json!({
            "workflow_ref": "config:development/remote-worker",
            "credential_profile_id": "personal",
            "task": {"objective": "edit"},
            "target_product_selections": [],
        });
        let source_invocation_id = "svc-1789074790258-1427d4e2".to_owned();
        let source_work_id = derive_source_work_id(&source_invocation_id).unwrap();
        Operation {
            operation_type: OPERATION_TYPE.into(),
            schema: OPERATION_SCHEMA.into(),
            source_work_id: source_work_id.clone(),
            source_invocation_id,
            source_site_id: "site:source".into(),
            admitted_start_request_digest: ryeos_state::objects::canonical_value_digest(&request)
                .unwrap(),
            admitted_start_request: request,
            operator_fingerprint: "a".repeat(64),
            operator_authority_digest: "b".repeat(64),
            remote: "default".into(),
            remote_url: "https://example.invalid".into(),
            target_site_id: "site:target".into(),
            target_principal_id: format!("fp:{}", lillux::crypto::fingerprint(&target_key)),
            target_signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(target_key.as_bytes())
            ),
            local_project_path: "/source".into(),
            target_project_path: "/target".into(),
            admitted_source_snapshot_hash: "d".repeat(64),
            source_snapshot_hash: "d".repeat(64),
            workflow_ref: "config:development/remote-worker".into(),
            credential_profile_id: "personal".into(),
            task_digest: ryeos_state::objects::canonical_value_digest(
                &serde_json::json!({"objective":"edit"}),
            )
            .unwrap(),
            task: serde_json::json!({"objective":"edit"}),
            target_product_selections: Vec::new(),
            target_product_selections_digest: target_product_selections_digest(&Vec::new())
                .unwrap(),
            target_launch_id: derive_target_launch_id(&source_work_id).unwrap(),
        }
    }

    fn receipt_fixture(operation: &Operation) -> Receipt {
        let candidate_result = candidate_result_fixture(operation);
        Receipt {
            schema: RESPONSE_SCHEMA.into(),
            allowed_next_action: "pull_result".into(),
            source_work_id: operation.source_work_id.clone(),
            remote: operation.remote.clone(),
            source_snapshot_hash: operation.source_snapshot_hash.clone(),
            workflow_ref: operation.workflow_ref.clone(),
            workflow_digest: "e".repeat(64),
            target_launch_id: operation.target_launch_id.clone(),
            target_readiness: readiness_fixture(),
            target_chain_root_id: "T-target".into(),
            target_admission: TargetAdmissionEvidence {
                admitted_capsule_hash: "f".repeat(64),
                exact_program_hash: "1".repeat(64),
                effective_definition_digest: "2".repeat(64),
            },
            launch_acceptance_drive_root_id: operation.source_invocation_id.clone(),
            target_workflow_terminal_thread_id: "T-target-graph-terminal".into(),
            target_workflow_result_digest: "3".repeat(64),
            candidate_terminal_thread_id: "T-target-candidate".into(),
            candidate_result,
            target_product_selections_digest: operation.target_product_selections_digest.clone(),
            settlement_drive_root_id: operation.source_invocation_id.clone(),
        }
    }

    fn readiness_fixture() -> TargetReadinessEvidence {
        TargetReadinessEvidence {
            requirements: TargetRuntimeRequirements {
                process_control: TargetProcessControl::ExclusiveSession,
                cleanup_authority: PersistentSessionCleanupAuthority::LocalProcessScope,
                filesystem_mode: ryeos_engine::isolation::IsolationMode::Enforce,
                network_mode: ryeos_engine::isolation::IsolationNetworkMode::Host,
            },
            daemon_revision: "fixture-revision".into(),
            isolation_policy_digest: format!("sha256:{}", "8".repeat(64)),
            process_scope_authority_digest: Some(format!("sha256:{}", "9".repeat(64))),
            process_control_reason: "ready".into(),
        }
    }

    fn candidate_result_fixture(operation: &Operation) -> HostedCandidateResultResponse {
        struct TestSigner {
            key: lillux::crypto::SigningKey,
            fingerprint: String,
        }
        impl Signer for TestSigner {
            fn sign(&self, data: &[u8]) -> Vec<u8> {
                use lillux::crypto::Signer as _;
                self.key.sign(data).to_bytes().to_vec()
            }
            fn fingerprint(&self) -> &str {
                &self.fingerprint
            }
            fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
                self.key.verifying_key()
            }
        }
        let key = lillux::crypto::SigningKey::from_bytes(&[29_u8; 32]);
        let signer = TestSigner {
            fingerprint: lillux::crypto::fingerprint(&key.verifying_key()),
            key,
        };
        let candidate = "6".repeat(64);
        let validation =
            ryeos_app::thread_lifecycle::candidate_validation_identity(&candidate).unwrap();
        let closure_validation =
            ryeos_app::hosted_candidate_result::HostedCandidateClosureValidation {
                schema: "ryeos.hosted_candidate_closure_and_base_validation.v2".into(),
                checks: ryeos_app::hosted_candidate_result::HostedCandidateClosureChecks {
                    canonical_snapshot_closure: true,
                    base_ancestry: true,
                },
                candidate_snapshot_hash: candidate.clone(),
                candidate_validation_hash: validation.clone(),
                base_snapshot_hash: operation.source_snapshot_hash.clone(),
                candidate_tree_hash: "7".repeat(64),
                base_tree_hash: "8".repeat(64),
                candidate_policy_hash: "9".repeat(64),
                changed_path_count: 1,
                changed_paths_digest: "0".repeat(64),
                object_count: 4,
                blob_count: 1,
            };
        let evidence = ryeos_app::hosted_candidate_result::HostedCandidateResultEvidence {
            schema: ryeos_app::hosted_candidate_result::HOSTED_CANDIDATE_RESULT_SCHEMA.into(),
            owner_principal: format!("fp:{}", operation.operator_fingerprint),
            source_site_id: operation.source_site_id.clone(),
            target_site_id: operation.target_site_id.clone(),
            chain_root_id: "T-target-candidate".into(),
            placement_thread_id: "T-target-candidate".into(),
            chain_head_hash: "a".repeat(64),
            last_event_hash: "b".repeat(64),
            admitted_launch_capsule_hash: "c".repeat(64),
            admitted_session_capsule_hash: "d".repeat(64),
            stable_project_identity: ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                Path::new(&operation.target_project_path),
                &operation.target_site_id,
            )
            .unwrap()
            .normalized_logical_key,
            target_project_path: operation.target_project_path.clone(),
            base_snapshot_hash: operation.source_snapshot_hash.clone(),
            candidate_snapshot_hash: candidate,
            candidate_validation_hash: validation,
            completion_fence: ryeos_app::dedicated_session_service::HostedCommandCompletionFence {
                placement_thread_id: "T-target-candidate".into(),
                admitted_capsule_hash: "d".repeat(64),
                worker_boot_epoch: 1,
                command_sequence: 2,
                request_digest: "e".repeat(64),
                turn_id: "turn-1".into(),
                completion_operation_id: "f".repeat(64),
            },
            command_response_digest: "1".repeat(64),
            candidate_capture_operation_id: "2".repeat(64),
            closure_validation_hash: closure_validation.content_hash().unwrap(),
            closure_validation,
            candidate_state: ryeos_app::hosted_candidate_result::HostedCandidateState::Frozen,
            disposition: ryeos_app::hosted_candidate_result::HostedCandidateDisposition::Retained,
        };
        HostedCandidateResultResponse::new(evidence, &signer).unwrap()
    }
}
