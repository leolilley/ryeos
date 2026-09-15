use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_app::accounting_db::{
    ExecutionResourceBudgetSnapshot, ExecutionResourceClaimOutcome, ExecutionResourceDimension,
};
use ryeos_app::callback_token::CallbackCapability;
use ryeos_app::runtime_db::{
    DedicatedCandidateDisposition, NewDedicatedSession, WorkspaceBinding, WorkspaceRecord,
    WorkspaceState,
};
use ryeos_app::state::AppState;
use ryeos_executor::execution::persistent_session::ExclusivePersistentSessionIdentity;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_runtime::callback::{
    DedicatedSessionBoundedBudgetDimension, DedicatedSessionBoundedOutcome,
    DedicatedSessionCommandObservationRequest, DedicatedSessionCommandRequest,
    DedicatedSessionStartRequest, HostedCommandCompletionFence,
};

const START_CAPABILITY: &str = "ryeos.runtime.dedicated_session.start";
const COMMAND_CAPABILITY: &str = "ryeos.runtime.dedicated_session.command";
const TERMINATE_CAPABILITY: &str = "ryeos.runtime.dedicated_session.terminate";

/// Normalize an internal diagnostic before it crosses the durable session
/// boundary. Worker stderr may contain newlines and can be much larger than
/// the database contract; neither property is valid for a terminal reason.
fn bounded_worker_failure_reason(prefix: &str, detail: &str) -> String {
    const MAX_BYTES: usize = 2_048;
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let available = MAX_BYTES.saturating_sub(prefix.len());
    let detail = normalized
        .chars()
        .scan(0usize, |bytes, character| {
            let next = *bytes + character.len_utf8();
            if next > available {
                None
            } else {
                *bytes = next;
                Some(character)
            }
        })
        .collect::<String>();
    format!("{prefix}{detail}").trim().to_owned()
}

/// Complete the explicit failed-start state machine without replacing the
/// initiating worker error. Any secondary failure leaves cleanup unproved and
/// therefore keeps the credential fence in place.
fn settle_failed_dedicated_worker_start(
    state: &AppState,
    placement_thread_id: &str,
    worker_instance_id: &str,
    boot_epoch: u64,
    reason: &str,
    cleanup_was_proved_before_attachment: bool,
) -> Result<()> {
    let mut failures = Vec::new();
    let mut cleanup_proved = cleanup_was_proved_before_attachment;
    match state.state_store.worker_process(worker_instance_id) {
        Ok(Some(worker)) => {
            match ryeos_app::dedicated_session_service::retire_worker_process(
                state,
                placement_thread_id,
                &worker,
            ) {
                Ok(cleanup_state) => {
                    cleanup_proved = cleanup_state == "reaped";
                    if let Err(error) = state.state_store.settle_worker_process(
                        worker_instance_id,
                        placement_thread_id,
                        boot_epoch,
                        cleanup_state,
                        reason,
                    ) {
                        cleanup_proved = false;
                        failures.push(format!("persist worker cleanup: {error:#}"));
                    }
                }
                Err(error) => {
                    cleanup_proved = false;
                    failures.push(format!("retire failed worker: {error:#}"));
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            cleanup_proved = false;
            failures.push(format!("read failed-worker identity: {error:#}"));
        }
    }
    if let Err(error) = state.state_store.fail_dedicated_session_start(
        placement_thread_id,
        worker_instance_id,
        boot_epoch,
        reason,
        cleanup_proved,
    ) {
        failures.push(format!(
            "persist dedicated-session start failure: {error:#}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn release_credential_after_unadmitted_start(
    state: &AppState,
    profile_id: &str,
    worker_instance_id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match state
        .state_store
        .release_credential_profile(profile_id, worker_instance_id)
    {
        Ok(()) => error,
        Err(release) => error.context(format!(
            "release credential profile after failed worker admission also failed: {release:#}"
        )),
    }
}

fn require_start_authority(state: &AppState, cap: &CallbackCapability) -> Result<()> {
    state
        .authorizer
        .authorize(
            &cap.effective_caps,
            &AuthorizationPolicy::require(START_CAPABILITY),
        )
        .map_err(|error| anyhow!(error.to_string()))
}

fn require_command_authority(state: &AppState, cap: &CallbackCapability) -> Result<()> {
    state
        .authorizer
        .authorize(
            &cap.effective_caps,
            &AuthorizationPolicy::require(COMMAND_CAPABILITY),
        )
        .map_err(|error| anyhow!(error.to_string()))
}

fn require_terminate_authority(state: &AppState, cap: &CallbackCapability) -> Result<()> {
    state
        .authorizer
        .authorize(
            &cap.effective_caps,
            &AuthorizationPolicy::require(TERMINATE_CAPABILITY),
        )
        .map_err(|error| anyhow!(error.to_string()))
}

fn require_callback_root(operation: &str, requested: &str, callback_root: &str) -> Result<()> {
    if requested != callback_root {
        bail!("dedicated-session {operation} is restricted to the callback root");
    }
    Ok(())
}

fn execution_resource_budget(
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Option<ExecutionResourceBudgetSnapshot>> {
    let Some(scope) = cap.accounting_scope.as_ref() else {
        return Ok(None);
    };
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("sealed accounting scope has no live accounting ledger"))?;
    let budget = accounting
        .execution_resource_budget_snapshot(&scope.execution_budget_id)?
        .ok_or_else(|| anyhow!("sealed execution scope has no aggregate budget authority"))?;
    Ok(Some(budget))
}

fn attach_execution_resource_budget(
    value: impl serde::Serialize,
    budget: Option<&ExecutionResourceBudgetSnapshot>,
) -> Result<Value> {
    let mut value = serde_json::to_value(value)?;
    if let Some(budget) = budget {
        value
            .as_object_mut()
            .ok_or_else(|| anyhow!("dedicated-session projection is not an object"))?
            .insert("execution_budget".to_owned(), serde_json::to_value(budget)?);
    }
    Ok(value)
}

fn attach_exact_pending_approval(
    state: &AppState,
    session: impl serde::Serialize,
    placement_thread_id: &str,
) -> Result<Value> {
    let mut value = serde_json::to_value(session)?;
    if value.get("candidate_disposition").and_then(Value::as_str) != Some("retained_for_review") {
        return Ok(value);
    }
    if let Some(approval) =
        ryeos_app::dedicated_session_service::exact_pending_approval(state, placement_thread_id)?
    {
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow!("dedicated-session projection is not an object"))?;
        let exact = object.get("placement_thread_id").and_then(Value::as_str)
            == Some(approval.placement_thread_id.as_str())
            && object.get("chain_root_id").and_then(Value::as_str)
                == Some(approval.chain_root_id.as_str())
            && object.get("admitted_capsule_hash").and_then(Value::as_str)
                == Some(approval.admitted_capsule_hash.as_str())
            && object.get("worker_boot_epoch").and_then(Value::as_u64)
                == Some(approval.worker_boot_epoch)
            && object.get("current_turn_id").and_then(Value::as_str)
                == Some(approval.turn_id.as_str());
        if !exact {
            bail!("pending approval changed across its session projection read");
        }
        object.insert(
            "pending_approval".to_owned(),
            serde_json::to_value(approval)?,
        );
    }
    Ok(value)
}

fn attach_session_start_authority(
    state: &AppState,
    session: impl serde::Serialize,
    budget: Option<&ExecutionResourceBudgetSnapshot>,
    placement_thread_id: &str,
) -> Result<Value> {
    let projection = attach_execution_resource_budget(session, budget)?;
    attach_exact_pending_approval(state, projection, placement_thread_id)
}

fn bounded_budget_outcome(reason: &str) -> Result<DedicatedSessionBoundedOutcome> {
    let dimension = match reason {
        "aggregate_worker_executions_exhausted" => {
            DedicatedSessionBoundedBudgetDimension::WorkerExecutions
        }
        "aggregate_provider_contacts_exhausted" => {
            DedicatedSessionBoundedBudgetDimension::ProviderContacts
        }
        "aggregate_duration_exhausted" => DedicatedSessionBoundedBudgetDimension::Duration,
        _ => bail!("aggregate worker refusal has an unknown budget reason"),
    };
    Ok(DedicatedSessionBoundedOutcome {
        kind: ryeos_runtime::callback::DedicatedSessionBoundedOutcomeKind::BudgetExhausted,
        dimension: Some(dimension),
        approval: None,
    })
}

fn admitted_session_capsule(
    state: &AppState,
    thread_id: &str,
    dependency_ref: &str,
) -> Result<(String, bool)> {
    let launch = state
        .state_store
        .admitted_launch_capsule(thread_id)?
        .ok_or_else(|| anyhow!("thread has no authoritative admitted launch capsule"))?;
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = launch.execution_closure
    else {
        bail!("dedicated session requires a managed runtime launch closure");
    };
    let prepared: ryeos_executor::execution::launch_preparation::PreparedRuntimeLaunch =
        serde_json::from_value(prepared_runtime_launch)
            .context("decode retained runtime launch authority")?;
    let mode = prepared
        .runtime_data
        .get("worker_execution")
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("worker execution has no admitted mode"))?;
    let bounded_turn = match mode {
        "session" => false,
        "bounded_turn" => true,
        _ => bail!("worker execution has an unknown admitted mode"),
    };
    let mut matches = prepared
        .execution_dependencies
        .iter()
        .filter(|(_, dependency)| dependency.canonical_ref == dependency_ref)
        .map(|(name, _)| name.as_str());
    let name = matches
        .next()
        .ok_or_else(|| anyhow!("requested dependency was not admitted by this launch"))?;
    if matches.next().is_some() {
        bail!("requested dependency ref is ambiguous in the admitted launch");
    }
    let capsule_hash = prepared
        .admitted_sessions
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("admitted dependency has no retained session capsule"))?;
    Ok((capsule_hash, bounded_turn))
}

fn require_structured_session_route_effect_contract(
    state: &AppState,
    capsule_hash: &str,
    route_set: &str,
    allowed_effect_classes: &[String],
    recover_upstream_session: bool,
) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(capsule_hash)?
        .ok_or_else(|| anyhow!("admitted session capsule disappeared"))?;
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != capsule_hash {
        bail!("admitted session capsule content hash changed");
    }
    let profile = capsule
        .structured_session_profile
        .ok_or_else(|| anyhow!("structured session capsule has no admitted protocol profile"))?;
    validate_structured_session_route_effect_contract(
        &profile.contract,
        route_set,
        allowed_effect_classes,
        recover_upstream_session,
    )
}

fn validate_structured_session_route_effect_contract(
    contract: &Value,
    route_set: &str,
    allowed_effect_classes: &[String],
    recover_upstream_session: bool,
) -> Result<()> {
    let object = contract
        .as_object()
        .ok_or_else(|| anyhow!("admitted structured-session contract is not an object"))?;
    let selected_routes = object
        .get("route_sets")
        .and_then(Value::as_object)
        .and_then(|sets| sets.get(route_set))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!("worker execution selects an unknown structured-session route set")
        })?;
    let routes = object
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("admitted structured-session contract has no route table"))?;
    let route_effects = routes
        .iter()
        .map(|route| {
            let route = route
                .as_object()
                .ok_or_else(|| anyhow!("admitted structured-session route is not an object"))?;
            let id = route
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("admitted structured-session route has no identity"))?;
            let effect = route
                .get("effect_class")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("admitted structured-session route has no effect class"))?;
            Ok((id, effect))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for route_id in selected_routes {
        let route_id = route_id
            .as_str()
            .ok_or_else(|| anyhow!("admitted structured-session route-set entry is invalid"))?;
        let effect = route_effects
            .get(route_id)
            .ok_or_else(|| anyhow!("admitted structured-session route set lost a route"))?;
        if !allowed_effect_classes
            .iter()
            .any(|allowed| allowed == effect)
        {
            bail!(
                "structured-session route `{route_id}` effect `{effect}` exceeds the root launch ceiling"
            );
        }
    }
    let initialization = object
        .get("initialization")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("admitted structured-session contract has no initialization"))?;
    for step in initialization {
        let effect = step
            .get("effect_class")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("admitted structured-session initialization has no effect class")
            })?;
        if !allowed_effect_classes
            .iter()
            .any(|allowed| allowed == effect)
        {
            bail!(
                "structured-session initialization effect `{effect}` exceeds the root launch ceiling"
            );
        }
    }
    if recover_upstream_session {
        let recovery = object
            .get("recovery")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                anyhow!(
                    "worker execution enables upstream recovery but the admitted protocol does not"
                )
            })?;
        let recovery_route_sets = recovery
            .get("route_sets")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("admitted structured-session recovery has no route sets"))?;
        if !recovery_route_sets
            .iter()
            .any(|candidate| candidate.as_str() == Some(route_set))
        {
            bail!(
                "worker execution enables upstream recovery for a route set not admitted by the protocol"
            );
        }
    }
    Ok(())
}

fn scratch_home_id(thread_id: &str) -> String {
    let digest = lillux::cas::sha256_hex(thread_id.as_bytes());
    format!("scratch-{}", &digest[..32])
}

fn create_dedicated_runtime_workspace(
    state: &AppState,
    controller_lifeline: &Arc<ryeos_app::temp_dir_guard::TempDirGuard>,
    workspace_id: &str,
    thread_id: &str,
    launch_owner: &str,
) -> Result<(
    WorkspaceRecord,
    Arc<ryeos_app::temp_dir_guard::TempDirGuard>,
)> {
    let base_snapshot = lillux::cas::sha256_hex(&[]);
    let (project, guard) = ryeos_app::temp_dir_guard::create_runtime_workspace(
        &state.config.runtime_root().cache(),
        workspace_id,
    )?;
    // The controller retains this ORIGINAL owner before any adapter contact.
    // Do not mount backend-private state beneath its writable scratch root or
    // replace the guard with a pathname-only owner after creation.
    controller_lifeline.retain_owned_workspace_lifeline(guard.clone())?;
    let root = project
        .parent()
        .ok_or_else(|| anyhow!("runtime workspace project has no root"))?;
    let layout =
        ryeos_executor::execution::workspace::WorkspaceLayout::from_root(root.to_path_buf());
    state.state_store.reserve_execution_workspace(
        workspace_id,
        &base_snapshot,
        root.to_str()
            .ok_or_else(|| anyhow!("runtime workspace path is not UTF-8"))?,
    )?;
    guard.preserve_for_explicit_cleanup();
    state.state_store.transition_execution_workspace(
        workspace_id,
        &[WorkspaceState::Reserved],
        WorkspaceState::Constructing,
        None,
    )?;
    state.state_store.claim_execution_workspace_construction(
        workspace_id,
        thread_id,
        launch_owner,
    )?;
    let (backend_id, backend_version) = state
        .isolation
        .workspace_backend_identity()
        .map_err(|error| anyhow!(error.to_string()))?;
    state.state_store.prepare_execution_workspace_backend(
        workspace_id,
        thread_id,
        launch_owner,
        backend_id,
        backend_version,
    )?;
    let created = state
        .isolation
        .create_workspace(
            ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                workspace_id,
                launch_owner,
                base_snapshot: &base_snapshot,
                project_path: &layout.project,
                mount_identity: None,
            },
            &|held| {
                let identity = ryeos_app::process::execution_process_identity_from_lillux(
                    held.exact_process_identity()
                        .map_err(|error| format!("capture workspace creator identity: {error}"))?,
                    None,
                )
                .map_err(|error| error.to_string())?;
                state
                    .state_store
                    .attach_workspace_creator(workspace_id, thread_id, launch_owner, &identity)
                    .map_err(|error| error.to_string())
            },
        )
        .map_err(|error| anyhow!(error.to_string()))?;
    let evidence = created.evidence;
    guard.install_workspace_view(
        &evidence,
        created
            .created_view
            .ok_or_else(|| anyhow!("dedicated workspace Create omitted its retained view"))?,
    )?;
    let pinned = lillux::canonical_json(&serde_json::to_value(&evidence.pinned_root_identities)?)?;
    if state.isolation.is_enforced() {
        state
            .state_store
            .assert_execution_workspace_creator_reaped(workspace_id, thread_id, launch_owner)?;
    }
    state
        .state_store
        .bind_execution_workspace(WorkspaceBinding {
            workspace_id,
            thread_id,
            workspace_output_partition_identity: None,
            base_output_capture_hash: None,
            launch_owner: Some(launch_owner),
            backend_id: Some(&evidence.backend_id),
            backend_version: Some(&evidence.backend_version),
            pinned_root_identities: Some(&pinned),
            mount_identity: evidence.mount_identity.as_deref(),
        })?;
    state.state_store.bind_thread_workspace(
        thread_id,
        &ryeos_app::runtime_db::RuntimeWorkspaceBinding {
            workspace_id: workspace_id.to_owned(),
            view_identity: evidence
                .mount_identity
                .ok_or_else(|| anyhow!("created view identity is absent"))?,
            borrower_launch_owner: serde_json::from_str(launch_owner)
                .context("decode dedicated root launch owner")?,
        },
    )?;
    let record = state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow!("bound dedicated runtime workspace disappeared"))?;
    Ok((record, guard))
}

pub(super) fn status(params: &Value, state: &AppState, cap: &CallbackCapability) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Params {
        thread_id: String,
    }
    let params: Params = serde_json::from_value(params.clone())?;
    if params.thread_id != cap.thread_id {
        bail!("dedicated-session status is restricted to the callback root");
    }
    let session = state
        .state_store
        .dedicated_session(&params.thread_id)?
        .ok_or_else(|| anyhow!("dedicated session is not admitted"))?;
    attach_exact_pending_approval(state, session, &params.thread_id)
}

pub(super) async fn wait(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    let request: ryeos_runtime::callback::DedicatedSessionWaitRequest =
        serde_json::from_value(params.clone())?;
    if request.thread_id != cap.thread_id {
        bail!("dedicated-session wait is restricted to the callback root");
    }
    if request.timeout_ms == 0 || request.timeout_ms > 300_000 {
        bail!("dedicated-session wait timeout is outside its bound");
    }
    let session = ryeos_app::dedicated_session_service::wait_for_projection_change(
        state,
        &request.thread_id,
        request.observed_updated_at_ms,
        lillux::time::Duration::from_millis(request.timeout_ms),
    )
    .await?;
    attach_exact_pending_approval(state, session, &request.thread_id)
}

pub(super) async fn command(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_command_authority(state, cap)?;
    let request: DedicatedSessionCommandRequest = serde_json::from_value(params.clone())?;
    if request.thread_id != cap.thread_id {
        bail!("dedicated-session command is restricted to the callback root");
    }
    let session = state
        .state_store
        .dedicated_session(&request.thread_id)?
        .ok_or_else(|| anyhow!("dedicated session is not admitted"))?;
    match request.command_kind.as_str() {
        "reattach" if session.state == "recovering" => {}
        "route" if session.state != "recovering" => {}
        _ => bail!("dedicated-session command kind contradicts its lifecycle state"),
    }
    // The absolute beat deadline governs only a new worker contact. An exact
    // idempotent coordinate must remain observable/replayable after expiry so
    // recovery can settle or classify work that was already admitted.
    if state
        .state_store
        .dedicated_session_command_by_key(&request.thread_id, &request.idempotency_key)?
        .is_none()
    {
        super::enforce_aggregate_deadline(state, Some(cap), lillux::time::timestamp_millis())?;
    }
    ryeos_app::dedicated_session_service::execute_command(
        state,
        &request.thread_id,
        &request.idempotency_key,
        &request.command_kind,
        request.payload,
    )
    .await
}

pub(super) fn command_observation(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_command_authority(state, cap)?;
    let request: DedicatedSessionCommandObservationRequest =
        serde_json::from_value(params.clone())?;
    require_callback_root("command observation", &request.thread_id, &cap.thread_id)?;
    if request.command_sequence == 0 {
        bail!("dedicated-session command observation sequence must be positive");
    }
    ryeos_app::dedicated_session_service::command_observation(
        state,
        &request.thread_id,
        request.command_sequence,
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DedicatedSessionTerminateParams {
    thread_id: String,
    reason: String,
    #[serde(default)]
    completion: Option<HostedCommandCompletionFence>,
    #[serde(default)]
    bounded_outcome: Option<DedicatedSessionBoundedOutcome>,
}

fn validate_runtime_termination(request: &DedicatedSessionTerminateParams) -> Result<()> {
    match (
        request.reason.as_str(),
        request.completion.is_some(),
        request.bounded_outcome.is_some(),
    ) {
        ("completed", true, false) | ("cancelled", false, _) => Ok(()),
        ("completed", false, _) => {
            bail!("completed dedicated-session termination requires an exact completion fence")
        }
        ("completed", true, true) => {
            bail!("completed bounded outcome is derived from its completion fence")
        }
        ("cancelled", true, _) => {
            bail!("cancelled dedicated-session termination cannot carry a completion fence")
        }
        _ => bail!("dedicated-session termination reason is not supported"),
    }
}

pub(super) async fn terminate(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_terminate_authority(state, cap)?;
    let request: DedicatedSessionTerminateParams = serde_json::from_value(params.clone())?;
    require_callback_root("termination", &request.thread_id, &cap.thread_id)?;
    validate_runtime_termination(&request)?;
    ryeos_app::dedicated_session_service::terminate_session_with_bounded_outcome(
        state,
        &request.thread_id,
        &request.reason,
        request.completion.as_ref(),
        request.bounded_outcome.as_ref(),
    )
    .await
}

pub(super) async fn start(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_start_authority(state, cap)?;
    if cap
        .provenance
        .project_authority()
        .workspace_outputs()
        .is_some()
    {
        bail!("session-bound candidate disposition does not admit workspace output partitions");
    }
    let request: DedicatedSessionStartRequest = serde_json::from_value(params.clone())?;
    if request.thread_id != cap.thread_id {
        bail!("dedicated-session start is restricted to the callback root");
    }
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &request.thread_id,
    )
    .await?;
    let _credential_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &request.credential_profile_id,
    )
    .await?;
    ryeos_engine::protocol_vocabulary::validate_env_name(&request.credential_home_env)?;
    ryeos_engine::protocol_vocabulary::validate_env_name(&request.workspace_env)?;
    if request.route_set.is_empty()
        || request.route_set.len() > 128
        || !request.route_set.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("dedicated-session route set is not canonical");
    }
    const ALLOWED_EFFECT_CLASSES: &[&str] = &[
        "credential_delete",
        "credential_read",
        "credential_write",
        "external_effect",
        "pure_read",
        "session_mutation",
    ];
    if request.allowed_effect_classes.is_empty()
        || request.allowed_effect_classes.len() > ALLOWED_EFFECT_CLASSES.len()
        || request
            .allowed_effect_classes
            .iter()
            .any(|effect| !ALLOWED_EFFECT_CLASSES.contains(&effect.as_str()))
        || request
            .allowed_effect_classes
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        bail!("dedicated-session effect classes are not a sorted admitted subset");
    }
    if request.credential_home_env == request.workspace_env {
        bail!("credential-home and workspace environment slots must be distinct");
    }
    if request.require_pinned_cow {
        if request.required_terminal_publication != "retain_result" {
            bail!("pinned CoW worker execution requires retain_result terminal publication");
        }
        use ryeos_state::objects::{
            ExecutionProjectAuthority, PinnedProjectRealization, PinnedTerminalPublication,
        };
        let ExecutionProjectAuthority::PinnedGeneration { realization, .. } =
            cap.provenance.project_authority()
        else {
            bail!("dedicated-session launch requires pinned project authority");
        };
        let PinnedProjectRealization::Cow {
            terminal_publication,
        } = realization
        else {
            bail!("dedicated-session launch requires a private CoW realization");
        };
        if !matches!(
            terminal_publication,
            PinnedTerminalPublication::RetainResult
                | PinnedTerminalPublication::RetainCurrentHead { .. }
        ) {
            bail!("worker execution requires a retain-result pinned CoW realization");
        }
    } else if request.required_terminal_publication != "any" {
        bail!("projectless worker execution requires any terminal publication");
    }
    let candidate_disposition = match request.candidate_disposition.as_str() {
        "owner_decision" => DedicatedCandidateDisposition::OwnerDecision,
        "retained_for_review" if request.require_pinned_cow => {
            DedicatedCandidateDisposition::RetainedForReview
        }
        _ => bail!("dedicated-session candidate disposition contradicts its project policy"),
    };
    let mut aggregate_budget = execution_resource_budget(state, cap)?;
    let (capsule_hash, bounded_turn) =
        admitted_session_capsule(state, &request.thread_id, &request.dependency_ref)?;
    if bounded_turn != (candidate_disposition == DedicatedCandidateDisposition::RetainedForReview) {
        bail!("dedicated-session candidate disposition contradicts its admitted worker mode");
    }
    if aggregate_budget
        .as_ref()
        .is_some_and(|budget| budget.max_provider_contacts.is_some())
        && !bounded_turn
    {
        bail!(
            "finite aggregate provider_contacts requires an admitted bounded-turn worker; \
             interactive session commands have no mechanically provable provider-contact \
             boundary"
        );
    }
    let recovering =
        if let Some(existing) = state.state_store.dedicated_session(&request.thread_id)? {
            if existing.credential_profile_id != request.credential_profile_id {
                bail!("dedicated-session retry changed credential profile identity");
            }
            if existing.candidate_disposition != candidate_disposition {
                bail!("dedicated-session retry changed candidate disposition");
            }
            if existing.bounded_outcome.is_some() || existing.state != "recovering" {
                return attach_session_start_authority(
                    state,
                    existing,
                    aggregate_budget.as_ref(),
                    &request.thread_id,
                );
            }
            true
        } else {
            false
        };

    let thread = state
        .state_store
        .get_thread(&request.thread_id)?
        .ok_or_else(|| anyhow!("dedicated-session root thread does not exist"))?;
    if thread.status != "running" {
        bail!("dedicated-session root must already be running");
    }
    let owner = thread
        .requested_by
        .as_deref()
        .ok_or_else(|| anyhow!("dedicated-session root has no owner principal"))?;
    ryeos_app::operator_authority::retained_admitted_operator_authority_digest(
        state,
        owner,
        &thread.origin_site_id,
    )
    .context("dedicated-session root owner no longer has exact admitted authority")?;
    let profile = state
        .state_store
        .credential_profile(&request.credential_profile_id)?
        .ok_or_else(|| anyhow!("credential profile does not exist"))?;
    if !matches!(request.required_credential_state.as_str(), "any" | "active") {
        bail!("dedicated-session credential-state requirement is not canonical");
    }
    if request.required_credential_state == "active" && profile.state != "active" {
        bail!("dedicated-session credential profile is not active");
    }
    if profile.owner_principal != owner {
        bail!("credential profile is not owned by the session principal");
    }
    require_structured_session_route_effect_contract(
        state,
        &capsule_hash,
        &request.route_set,
        &request.allowed_effect_classes,
        request.recover_upstream_session,
    )?;
    if !recovering {
        if let (Some(scope), Some(accounting)) =
            (cap.accounting_scope.as_ref(), state.accounting.as_ref())
        {
            let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":1,
                "kind":"dedicated_worker_execution",
                "request":&request,
                "admitted_session_capsule_hash":&capsule_hash,
                "credential_generation":profile.credential_generation,
            }))?;
            match accounting.claim_execution_resource(
                &scope.execution_budget_id,
                ExecutionResourceDimension::WorkerExecution,
                &request.thread_id,
                &request_digest,
                lillux::time::timestamp_millis(),
            )? {
                ExecutionResourceClaimOutcome::Admitted { .. } => {}
                ExecutionResourceClaimOutcome::ReleasedUncontacted { .. } => {
                    bail!("worker-execution claim cannot be in a released state")
                }
                ExecutionResourceClaimOutcome::Denied { reason, .. } => {
                    aggregate_budget = execution_resource_budget(state, cap)?;
                    let refused = json!({
                        "state":"budget_exhausted",
                        "budget_reason":reason,
                        "bounded_outcome":bounded_budget_outcome(&reason)?,
                    });
                    return attach_execution_resource_budget(refused, aggregate_budget.as_ref());
                }
            }
        }
    }
    ryeos_app::private_artifact_home::require_within_default_limit(
        &state.config.runtime_state_dir(),
        &profile.home_id,
    )?;
    let controller_lifeline = cap
        .provenance
        .workspace_lifeline()
        .ok_or_else(|| anyhow!("dedicated controller has no original workspace lifeline"))?;
    let (workspace, workspace_lifeline) = match state
        .state_store
        .execution_workspace_for_thread(&request.thread_id)?
    {
        Some(workspace) => {
            let lifeline = controller_lifeline
                .owned_workspace_lifeline()?
                .ok_or_else(|| anyhow!("dedicated workspace lost its original retained owner"))?;
            (workspace, lifeline)
        }
        None if !request.require_pinned_cow && request.required_terminal_publication == "any" => {
            let claim = state
                .state_store
                .get_launch_claim(&request.thread_id)?
                .ok_or_else(|| anyhow!("projectless dedicated root has no launch owner"))?;
            let home_id = scratch_home_id(&request.thread_id);
            let workspace_id = format!("dedicated-{home_id}");
            create_dedicated_runtime_workspace(
                state,
                &controller_lifeline,
                &workspace_id,
                &request.thread_id,
                &claim.claimed_by,
            )?
        }
        None => bail!("dedicated-session root has no owned execution workspace"),
    };
    match workspace.state {
        WorkspaceState::Ready => {}
        WorkspaceState::Active if request.require_pinned_cow => {
            let root_process_identity =
                thread.runtime.process_identity.as_ref().ok_or_else(|| {
                    anyhow!("active dedicated workspace has no attached root process")
                })?;
            let root_process_identity = serde_json::to_string(root_process_identity)
                .context("serialize attached root process identity")?;
            if workspace.process_identity.as_deref() != Some(root_process_identity.as_str()) {
                bail!("active dedicated workspace is not owned by the callback root process");
            }
        }
        _ => bail!("dedicated-session workspace is not attachable by this worker root"),
    }
    let workspace_root = PathBuf::from(&workspace.root_path);
    let workspace_path =
        ryeos_executor::execution::workspace::WorkspaceLayout::from_root(workspace_root).project;
    if !workspace_path.is_absolute() {
        bail!("dedicated-session workspace path is not absolute");
    }
    if !workspace_lifeline.owns_effective_path(&workspace_path) {
        bail!("dedicated workspace differs from its original retained owner path");
    }
    let view_identity = workspace
        .mount_identity
        .as_deref()
        .ok_or_else(|| anyhow!("dedicated workspace has no bound view incarnation"))?;
    if workspace_lifeline.workspace_view_identity()?.as_ref()
        != Some(&(workspace.workspace_id.clone(), view_identity.to_owned()))
    {
        bail!("dedicated workspace differs from its original retained view incarnation");
    }
    let worker_instance_id = ryeos_app::thread_lifecycle::new_thread_id();
    let credential_generation = profile.credential_generation;
    if recovering {
        state.state_store.acquire_credential_profile(
            &request.credential_profile_id,
            owner,
            &worker_instance_id,
        )?;
    }
    if recovering && profile.state == "enrolling" {
        let Some(login_id) = profile.active_login_id.as_deref() else {
            return Err(release_credential_after_unadmitted_start(
                state,
                &request.credential_profile_id,
                &worker_instance_id,
                anyhow!("recovering enrollment has no active login identity"),
            ));
        };
        if let Err(error) = state.state_store.cancel_credential_enrollment(
            &request.credential_profile_id,
            &worker_instance_id,
            login_id,
            profile.login_epoch,
        ) {
            return Err(release_credential_after_unadmitted_start(
                state,
                &request.credential_profile_id,
                &worker_instance_id,
                error.context("abandon enrollment bound to the dead worker epoch"),
            ));
        }
    }
    let profile_home = ryeos_app::private_artifact_home::home_path(
        &state.config.runtime_state_dir(),
        &profile.home_id,
    )?;
    // The pinned upstream currently owns authentication, refresh, rollout and
    // thread state beneath one workload home. RyeOS therefore admits the exact
    // profile-generation home as the worker state root and serializes it with
    // the profile lock. We do not invent a second per-session home that the
    // upstream process would never use.
    let state_root = profile_home.clone();
    let handoff_reservation = if recovering {
        None
    } else {
        state
            .state_store
            .credential_profile_reservation_for_successor(&request.thread_id)?
            .filter(|reservation| reservation.state == "reserved")
    };
    if let Some(reservation) = &handoff_reservation {
        let remote = state
            .state_store
            .remote_continuation_authority(&thread.chain_root_id, &request.thread_id)?
            .ok_or_else(|| anyhow!("worker adoption successor has no remote-continuation edge"))?;
        if reservation.profile_id != request.credential_profile_id
            || reservation.owner_principal != owner
            || reservation.credential_generation != profile.credential_generation
            || reservation.operation_id != remote.operation_id
            || reservation.checkpoint_manifest_hash != remote.checkpoint_manifest_hash
            || thread.admitted_launch_capsule_hash.as_deref()
                != Some(remote.target_launch_capsule_hash.as_str())
        {
            bail!(
                "worker adoption credential reservation contradicts the authoritative remote continuation"
            );
        }
    }
    let continuation_remote_thread_id = if let Some(reservation) = &handoff_reservation {
        if !request.recover_upstream_session {
            bail!("worker adoption reservation requires upstream-session recovery");
        }
        Some(reservation.upstream_session_id.clone())
    } else if !recovering && request.recover_upstream_session {
        match thread.upstream_thread_id.as_deref() {
            Some(source_thread_id) => {
                let source = state
                    .state_store
                    .dedicated_session(source_thread_id)?
                    .ok_or_else(|| {
                        anyhow!("worker continuation source has no dedicated session")
                    })?;
                if source.chain_root_id != thread.chain_root_id || source.state != "frozen" {
                    bail!("worker continuation source is not the frozen predecessor placement");
                }
                Some(source.remote_thread_id.ok_or_else(|| {
                    anyhow!("worker continuation source has no upstream session identity")
                })?)
            }
            None => None,
        }
    } else {
        None
    };
    let admitted_epoch = if recovering {
        state.state_store.prepare_dedicated_session_recovery(
            &request.thread_id,
            credential_generation,
            &worker_instance_id,
            &workspace.workspace_id,
        )
    } else if let Some(reservation) = handoff_reservation.as_ref() {
        state
            .state_store
            .admit_dedicated_session_from_reservation(
                NewDedicatedSession {
                    placement_thread_id: &request.thread_id,
                    chain_root_id: &thread.chain_root_id,
                    owner_principal: owner,
                    admitted_capsule_hash: &capsule_hash,
                    workspace_id: &workspace.workspace_id,
                    candidate_required: request.require_pinned_cow,
                    candidate_disposition: candidate_disposition.clone(),
                    credential_profile_id: &request.credential_profile_id,
                    credential_generation,
                    credential_lock_owner: &worker_instance_id,
                },
                continuation_remote_thread_id.as_deref(),
                &reservation.reservation_id,
            )
            .map(|()| 1)
    } else if let Some(remote_thread_id) = continuation_remote_thread_id.as_deref() {
        state
            .state_store
            .admit_dedicated_session_with_remote(
                NewDedicatedSession {
                    placement_thread_id: &request.thread_id,
                    chain_root_id: &thread.chain_root_id,
                    owner_principal: owner,
                    admitted_capsule_hash: &capsule_hash,
                    workspace_id: &workspace.workspace_id,
                    candidate_required: request.require_pinned_cow,
                    candidate_disposition: candidate_disposition.clone(),
                    credential_profile_id: &request.credential_profile_id,
                    credential_generation,
                    credential_lock_owner: &worker_instance_id,
                },
                remote_thread_id,
            )
            .map(|()| 1)
    } else {
        state
            .state_store
            .admit_dedicated_session(NewDedicatedSession {
                placement_thread_id: &request.thread_id,
                chain_root_id: &thread.chain_root_id,
                owner_principal: owner,
                admitted_capsule_hash: &capsule_hash,
                workspace_id: &workspace.workspace_id,
                candidate_required: request.require_pinned_cow,
                candidate_disposition,
                credential_profile_id: &request.credential_profile_id,
                credential_generation,
                credential_lock_owner: &worker_instance_id,
            })
            .map(|()| 1)
    };
    let boot_epoch = match admitted_epoch {
        Ok(epoch) => epoch,
        // Fresh admission acquires the credential lock in the same SQLite
        // transaction as the placement row, so a failed transaction leaves no
        // lock for this path to release.
        Err(error) if !recovering => return Err(error),
        Err(error) => {
            return Err(release_credential_after_unadmitted_start(
                state,
                &request.credential_profile_id,
                &worker_instance_id,
                error,
            ));
        }
    };

    let control_channel_identity = ryeos_app::thread_lifecycle::new_thread_id();
    let boot_identity_hash = lillux::cas::sha256_hex(
        lillux::canonical_json(&json!({
            "placement_thread_id": request.thread_id,
            "worker_instance_id": worker_instance_id,
            "capsule_hash": capsule_hash,
            "credential_generation": credential_generation,
            "boot_epoch":boot_epoch,
            "control_channel_identity": control_channel_identity,
        }))?
        .as_bytes(),
    );
    let identity = ExclusivePersistentSessionIdentity {
        placement_thread_id: request.thread_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        boot_identity_hash,
        boot_epoch,
        lifecycle_generation: credential_generation,
        control_channel_identity,
    };
    let workload_client_channel = match super::workload_client::prepare_for_dedicated_boot(
        state, cap, &identity,
    ) {
        Ok(channel) => channel,
        Err(error) => {
            let detail = format!("{error:#}");
            let reason = bounded_worker_failure_reason(
                "dedicated worker workload-client admission failed: ",
                &detail,
            );
            let settlement = settle_failed_dedicated_worker_start(
                state,
                &request.thread_id,
                &worker_instance_id,
                boot_epoch,
                &reason,
                true,
            );
            return match settlement {
                Ok(()) => Err(anyhow!(reason)),
                Err(settlement) => Err(anyhow!(
                    "{reason}; explicit workload-client admission settlement also failed: {settlement:#}"
                )),
            };
        }
    };
    let runtime_environment = BTreeMap::from([
        (
            request.credential_home_env.clone(),
            profile_home.to_string_lossy().into_owned(),
        ),
        (
            request.workspace_env.clone(),
            workspace_path.to_string_lossy().into_owned(),
        ),
        (
            "RYEOS_STRUCTURED_SESSION_ROUTE_SET".to_owned(),
            request.route_set.clone(),
        ),
        (
            "RYEOS_STRUCTURED_SESSION_EFFECT_CLASSES".to_owned(),
            request.allowed_effect_classes.join(","),
        ),
    ]);
    let start_state = state.clone();
    let start_capsule = capsule_hash.clone();
    let start_workspace = workspace_path.clone();
    let start_state_root = state_root.clone();
    let start_identity = identity.clone();
    let extra_target_channels = workload_client_channel.into_iter().collect();
    let observation_state = state.clone();
    let observation_thread_id = identity.placement_thread_id.clone();
    let observation_boot_epoch = identity.boot_epoch;
    let observation_sink: ryeos_app::persistent_session::PersistentSessionObservationSink =
        Arc::new(move |raw| {
            ryeos_app::dedicated_session_service::ingest_observation_batch(
                &observation_state,
                &observation_thread_id,
                observation_boot_epoch,
                raw,
            )
        });
    // A cancelled async caller must not drop admission fences while its
    // blocking task is still preparing/contacting the exact worker process.
    let (started, _root_operation, _credential_operation) =
        tokio::task::spawn_blocking(move || {
            let started = ryeos_executor::execution::persistent_session::start_exclusive_capsule(
                &start_state,
                &start_capsule,
                &start_workspace,
                workspace_lifeline,
                Some(&start_state_root),
                &runtime_environment,
                extra_target_channels,
                &start_identity,
                observation_sink,
            )
            .map_err(|error| {
                // Failure settlement is part of worker contact, not delivery
                // of this async response. Cancellation after spawn must not
                // release the operation fences before recording an unknown
                // process/credential outcome (including pre-identity failure).
                let reason = bounded_worker_failure_reason(
                    "dedicated worker start failed: ",
                    &format!("{error:#}"),
                );
                let cleanup_proved = error
                    .downcast_ref::<ryeos_executor::execution::persistent_session::ExclusiveWorkerCleanupUnproved>()
                    .is_none();
                match settle_failed_dedicated_worker_start(
                    &start_state,
                    &start_identity.placement_thread_id,
                    &start_identity.worker_instance_id,
                    start_identity.boot_epoch,
                    &reason,
                    cleanup_proved,
                ) {
                    Ok(()) => error.context(reason),
                    Err(settlement) => error.context(format!(
                        "{reason}; explicit failed-start settlement also failed: {settlement:#}"
                    )),
                }
            });
            (started, _root_operation, _credential_operation)
        })
        .await
        .context("join dedicated-session worker start")?;
    started?;
    let session = state
        .state_store
        .dedicated_session(&request.thread_id)?
        .ok_or_else(|| anyhow!("started dedicated session disappeared"))?;
    aggregate_budget = execution_resource_budget(state, cap)?;
    attach_session_start_authority(
        state,
        session,
        aggregate_budget.as_ref(),
        &request.thread_id,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DedicatedSessionTerminateParams, bounded_worker_failure_reason, require_callback_root,
        validate_runtime_termination, validate_structured_session_route_effect_contract,
    };

    fn structured_contract() -> serde_json::Value {
        serde_json::json!({
            "initialization":[{"effect_class":"pure_read"}],
            "recovery":{
                "resume_route":"record.restore",
                "inspect_route":"record.inspect",
                "route_sets":["records"]
            },
            "route_sets":{"records":["record.read", "operation.run"]},
            "routes":[
                {"id":"record.read", "effect_class":"credential_read"},
                {"id":"operation.run", "effect_class":"external_effect"}
            ]
        })
    }

    #[test]
    fn worker_failure_reason_is_canonical_and_bounded_by_bytes() {
        let detail = format!("first line\n{}\tend", "é".repeat(2_048));
        let reason = bounded_worker_failure_reason("worker failed: ", &detail);
        assert!(reason.len() <= 2_048);
        assert!(!reason.is_empty());
        assert_eq!(reason.trim(), reason);
        assert!(!reason.chars().any(char::is_control));
        assert!(reason.starts_with("worker failed: first line "));
    }

    #[test]
    fn completion_fenced_termination_wire_is_exact_and_closed() {
        let wire = serde_json::json!({
            "thread_id":"T-worker",
            "reason":"completed",
            "completion":{
                "placement_thread_id":"T-worker",
                "admitted_capsule_hash":"a".repeat(64),
                "worker_boot_epoch":3,
                "command_sequence":2,
                "request_digest":"b".repeat(64),
                "turn_id":"turn-7",
                "completion_operation_id":"c".repeat(64),
            }
        });
        let parsed: DedicatedSessionTerminateParams = serde_json::from_value(wire.clone()).unwrap();
        validate_runtime_termination(&parsed).unwrap();
        let completion = parsed.completion.expect("completion fence");
        assert_eq!(completion.placement_thread_id, "T-worker");
        assert_eq!(completion.command_sequence, 2);

        let mut unknown = wire;
        unknown["completion"]["latest_turn"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DedicatedSessionTerminateParams>(unknown).is_err());

        let cancelled: DedicatedSessionTerminateParams = serde_json::from_value(
            serde_json::json!({"thread_id":"T-worker", "reason":"cancelled"}),
        )
        .unwrap();
        validate_runtime_termination(&cancelled).unwrap();
        assert!(cancelled.completion.is_none());

        let unfenced_completed: DedicatedSessionTerminateParams = serde_json::from_value(
            serde_json::json!({"thread_id":"T-worker", "reason":"completed"}),
        )
        .unwrap();
        assert!(validate_runtime_termination(&unfenced_completed).is_err());
    }

    #[test]
    fn exact_command_observation_and_termination_reject_other_callback_roots() {
        for operation in ["command observation", "termination"] {
            require_callback_root(operation, "T-worker", "T-worker").unwrap();
            let error = require_callback_root(operation, "T-other", "T-worker").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("restricted to the callback root")
            );
        }
    }

    #[test]
    fn route_effect_contract_is_rejected_before_worker_launch() {
        let missing_credential_read = vec!["external_effect".to_owned(), "pure_read".to_owned()];
        let error = validate_structured_session_route_effect_contract(
            &structured_contract(),
            "records",
            &missing_credential_read,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("credential_read"));

        let complete = vec![
            "credential_read".to_owned(),
            "external_effect".to_owned(),
            "pure_read".to_owned(),
        ];
        validate_structured_session_route_effect_contract(
            &structured_contract(),
            "records",
            &complete,
            true,
        )
        .unwrap();

        let error = validate_structured_session_route_effect_contract(
            &structured_contract(),
            "records",
            &complete,
            false,
        );
        assert!(error.is_ok());

        let mut contract = structured_contract();
        contract["recovery"] = serde_json::Value::Null;
        let error = validate_structured_session_route_effect_contract(
            &contract, "records", &complete, true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("enables upstream recovery"));
    }
}
