use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_app::callback_token::CallbackCapability;
use ryeos_app::runtime_db::{
    NewDedicatedSession, WorkspaceBinding, WorkspaceRecord, WorkspaceState,
};
use ryeos_app::state::AppState;
use ryeos_executor::execution::persistent_session::ExclusivePersistentSessionIdentity;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_runtime::callback::{DedicatedSessionCommandRequest, DedicatedSessionStartRequest};

const START_CAPABILITY: &str = "ryeos.runtime.dedicated_session.start";
const COMMAND_CAPABILITY: &str = "ryeos.runtime.dedicated_session.command";
const TERMINATE_CAPABILITY: &str = "ryeos.runtime.dedicated_session.terminate";

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

fn admitted_session_capsule(
    state: &AppState,
    thread_id: &str,
    dependency_ref: &str,
) -> Result<String> {
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
    prepared
        .admitted_sessions
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("admitted dependency has no retained session capsule"))
}

fn scratch_home_id(thread_id: &str) -> String {
    let digest = lillux::cas::sha256_hex(thread_id.as_bytes());
    format!("scratch-{}", &digest[..32])
}

fn create_dedicated_runtime_workspace(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
    launch_owner: &str,
) -> Result<WorkspaceRecord> {
    let lower_snapshot = lillux::cas::sha256_hex(&[]);
    let (project, guard) = ryeos_app::temp_dir_guard::create_runtime_workspace(
        &state.config.runtime_root().cache(),
        workspace_id,
    )?;
    let root = project
        .parent()
        .ok_or_else(|| anyhow!("runtime workspace project has no root"))?;
    let layout =
        ryeos_executor::execution::workspace::WorkspaceLayout::from_root(root.to_path_buf());
    state.state_store.reserve_execution_workspace(
        workspace_id,
        &lower_snapshot,
        root.to_str()
            .ok_or_else(|| anyhow!("runtime workspace path is not UTF-8"))?,
    )?;
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
        .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
            workspace_id,
            launch_owner,
            lower_snapshot: &lower_snapshot,
            lower_path: &layout.lower,
            upper_path: &layout.upper,
            work_path: &layout.work,
        })
        .map_err(|error| anyhow!(error.to_string()))?;
    let pinned = lillux::canonical_json(&serde_json::to_value(&created.pinned_root_identities)?)?;
    state
        .state_store
        .bind_execution_workspace(WorkspaceBinding {
            workspace_id,
            thread_id,
            launch_owner: Some(launch_owner),
            backend_id: Some(&created.backend_id),
            backend_version: Some(&created.backend_version),
            pinned_root_identities: Some(&pinned),
            mount_identity: Some(&created.mount_identity),
        })?;
    guard.disarm();
    state
        .state_store
        .execution_workspace(workspace_id)?
        .ok_or_else(|| anyhow!("bound dedicated runtime workspace disappeared"))
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
    Ok(serde_json::to_value(session)?)
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
        std::time::Duration::from_millis(request.timeout_ms),
    )
    .await?;
    Ok(serde_json::to_value(session)?)
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
    ryeos_app::dedicated_session_service::execute_command(
        state,
        &request.thread_id,
        &request.idempotency_key,
        &request.command_kind,
        request.payload,
    )
    .await
}

pub(super) async fn terminate(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_terminate_authority(state, cap)?;
    let request: ryeos_runtime::callback::DedicatedSessionTerminateRequest =
        serde_json::from_value(params.clone())?;
    if request.thread_id != cap.thread_id {
        bail!("dedicated-session termination is restricted to the callback root");
    }
    ryeos_app::dedicated_session_service::terminate_session(
        state,
        &request.thread_id,
        &request.reason,
    )
    .await
}

pub(super) async fn start(
    params: &Value,
    state: &AppState,
    cap: &CallbackCapability,
) -> Result<Value> {
    require_start_authority(state, cap)?;
    let request: DedicatedSessionStartRequest = serde_json::from_value(params.clone())?;
    if request.thread_id != cap.thread_id {
        bail!("dedicated-session start is restricted to the callback root");
    }
    let _root_operation =
        ryeos_app::hosted_operation::begin_hosted_root_operation(&request.thread_id)?;
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
        if *terminal_publication != PinnedTerminalPublication::RetainResult {
            bail!("worker execution requires a retain-result pinned CoW realization");
        }
    } else if request.required_terminal_publication != "any" {
        bail!("projectless worker execution requires any terminal publication");
    }
    let recovering =
        if let Some(existing) = state.state_store.dedicated_session(&request.thread_id)? {
            if existing.credential_profile_id != request.credential_profile_id {
                bail!("dedicated-session retry changed credential profile identity");
            }
            if existing.state != "recovering" {
                return Ok(serde_json::to_value(existing)?);
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
    let operator = ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
        .context("load configured local operator identity")?;
    if owner != operator.principal_id() {
        bail!("dedicated-session root is not owned by the configured local operator");
    }
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
    ryeos_app::private_artifact_home::require_within_default_limit(
        &state.config.runtime_state_dir(),
        &profile.home_id,
    )?;
    let workspace = match state
        .state_store
        .execution_workspace_for_thread(&request.thread_id)?
    {
        Some(workspace) => workspace,
        None if !request.require_pinned_cow && request.required_terminal_publication == "any" => {
            let claim = state
                .state_store
                .get_launch_claim(&request.thread_id)?
                .ok_or_else(|| anyhow!("projectless dedicated root has no launch owner"))?;
            let home_id = scratch_home_id(&request.thread_id);
            let workspace_id = format!("dedicated-{home_id}");
            create_dedicated_runtime_workspace(
                state,
                &workspace_id,
                &request.thread_id,
                &claim.claimed_by,
            )?
        }
        None => bail!("dedicated-session root has no owned execution workspace"),
    };
    if workspace.state != WorkspaceState::Ready {
        bail!("dedicated-session workspace is not ready for worker attachment");
    }
    let workspace_root = PathBuf::from(&workspace.root_path);
    let workspace_path =
        ryeos_executor::execution::workspace::WorkspaceLayout::from_root(workspace_root).lower;
    if !workspace_path.is_absolute() {
        bail!("dedicated-session workspace path is not absolute");
    }
    let capsule_hash =
        admitted_session_capsule(state, &request.thread_id, &request.dependency_ref)?;
    let worker_instance_id = ryeos_app::thread_lifecycle::new_thread_id();
    let credential_generation = state.state_store.acquire_credential_profile(
        &request.credential_profile_id,
        owner,
        &worker_instance_id,
    )?;
    if recovering && profile.state == "enrolling" {
        let Some(login_id) = profile.active_login_id.as_deref() else {
            let _ = state
                .state_store
                .release_credential_profile(&request.credential_profile_id, &worker_instance_id);
            bail!("recovering enrollment has no active login identity");
        };
        if let Err(error) = state.state_store.cancel_credential_enrollment(
            &request.credential_profile_id,
            &worker_instance_id,
            login_id,
            profile.login_epoch,
        ) {
            let _ = state
                .state_store
                .release_credential_profile(&request.credential_profile_id, &worker_instance_id);
            return Err(error.context("abandon enrollment bound to the dead worker epoch"));
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
    let admitted_epoch = if recovering {
        state.state_store.prepare_dedicated_session_recovery(
            &request.thread_id,
            credential_generation,
            &worker_instance_id,
        )
    } else {
        state
            .state_store
            .admit_dedicated_session(NewDedicatedSession {
                session_id: &request.thread_id,
                root_thread_id: &request.thread_id,
                owner_principal: owner,
                admitted_capsule_hash: &capsule_hash,
                workspace_id: &workspace.workspace_id,
                candidate_required: request.require_pinned_cow,
                credential_profile_id: &request.credential_profile_id,
                credential_generation,
                credential_lock_owner: &worker_instance_id,
            })
            .map(|()| 1)
    };
    let boot_epoch = match admitted_epoch {
        Ok(epoch) => epoch,
        Err(error) => {
            let _ = state
                .state_store
                .release_credential_profile(&request.credential_profile_id, &worker_instance_id);
            return Err(error);
        }
    };

    let control_channel_identity = ryeos_app::thread_lifecycle::new_thread_id();
    let boot_identity_hash = lillux::cas::sha256_hex(
        lillux::canonical_json(&json!({
            "session_id": request.thread_id,
            "worker_instance_id": worker_instance_id,
            "capsule_hash": capsule_hash,
            "credential_generation": credential_generation,
            "boot_epoch":boot_epoch,
            "control_channel_identity": control_channel_identity,
        }))?
        .as_bytes(),
    );
    let identity = ExclusivePersistentSessionIdentity {
        session_id: request.thread_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        boot_identity_hash,
        boot_epoch,
        lifecycle_generation: credential_generation,
        control_channel_identity,
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
    let started = tokio::task::spawn_blocking(move || {
        ryeos_executor::execution::persistent_session::start_exclusive_capsule(
            &start_state,
            &start_capsule,
            &start_workspace,
            Some(&start_state_root),
            &runtime_environment,
            &start_identity,
        )
    })
    .await
    .context("join dedicated-session worker start")?;
    if let Err(error) = started {
        let reason = format!("dedicated worker start failed: {error:#}");
        let mut cleanup_proved = error
            .downcast_ref::<
                ryeos_executor::execution::persistent_session::ExclusiveWorkerCleanupUnproved,
            >()
            .is_none();
        if let Some(worker) = state.state_store.worker_process(&worker_instance_id)? {
            let cleanup_state = ryeos_app::dedicated_session_service::retire_worker_process(
                state,
                &request.thread_id,
                &worker,
            )?;
            cleanup_proved = cleanup_state == "reaped";
            state.state_store.settle_worker_process(
                &worker_instance_id,
                &request.thread_id,
                boot_epoch,
                cleanup_state,
                &reason,
            )?;
        }
        state.state_store.fail_dedicated_session_start(
            &request.thread_id,
            &worker_instance_id,
            &reason,
            cleanup_proved,
        )?;
        return Err(anyhow!(reason));
    }
    let observation_state = state.clone();
    let observation_session_id = request.thread_id.clone();
    if let Err(error) = state
        .persistent_sessions
        .install_exclusive_observation_sink(&request.thread_id, move |body| {
            ryeos_app::dedicated_session_service::ingest_observation_batch(
                &observation_state,
                &observation_session_id,
                boot_epoch,
                body,
            )
        })
    {
        let worker = state
            .state_store
            .worker_process(&worker_instance_id)?
            .ok_or_else(|| anyhow!("observation-sink failure lost its worker identity"))?;
        let cleanup_state = ryeos_app::dedicated_session_service::retire_worker_process(
            state,
            &request.thread_id,
            &worker,
        )?;
        let reason = format!("install worker observation sink: {error:#}");
        match cleanup_state {
            "reaped" => {
                state.state_store.settle_worker_process(
                    &worker_instance_id,
                    &request.thread_id,
                    boot_epoch,
                    "reaped",
                    &reason,
                )?;
                state.state_store.terminalize_dedicated_session(
                    &request.thread_id,
                    &worker_instance_id,
                    boot_epoch,
                    "cancelled",
                )?;
            }
            _ => {
                state.state_store.fence_abandoned_worker_process(
                    &worker_instance_id,
                    &request.thread_id,
                    boot_epoch,
                    "unproved",
                )?;
                return Err(anyhow!(
                    "{reason}; worker cleanup could not be proved and the credential profile remains fenced"
                ));
            }
        }
        return Err(anyhow!(reason));
    }
    let session = state
        .state_store
        .dedicated_session(&request.thread_id)?
        .ok_or_else(|| anyhow!("started dedicated session disappeared"))?;
    Ok(serde_json::to_value(session)?)
}
