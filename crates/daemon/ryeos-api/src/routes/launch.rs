//! Background dispatch-launch helper shared by route components
//! that mint a thread id, fire `dispatch::dispatch` with that id
//! pre-minted, and observe (or acknowledge) the launch outcome
//! out-of-band.
//!
//! Kind-agnostic: this helper does not pattern-match on the
//! canonical ref's kind name. Whether the launched item is a
//! directive, tool, service, graph, or any other root-executable
//! kind is decided by the engine's kind-schema registry inside
//! `dispatch::dispatch`. Callers parse the ref via
//! `CanonicalRef::parse` for syntactic validation; the helper
//! consumes a parsed ref only after the caller's synchronous admission
//! verified the kind schema declares a root-executable thread profile.
//!
//! Consumers today:
//!   - [`crate::routes::response_modes::event_stream_mode`] — SSE
//!     subscriber tails events for the minted thread (one-call
//!     fire-and-observe pattern used by `POST /execute/stream`).
//!   - [`crate::routes::response_modes::launch_mode`] — unary 202
//!     Accepted ack used by webhook routes; the launched thread
//!     keeps running after the HTTP response is closed.
//!
//! Acknowledged consumers use the launch-handoff variant and expose the ID only
//! after durable row/audit creation and spawn-task authority transfer.

use anyhow::Context;
use serde_json::Value;
use std::collections::BTreeMap;

use ryeos_app::state::AppState;
use ryeos_executor::dispatch_error::DispatchError;

/// Typed error returned by the background dispatch-launch task.
///
/// Replaces the previous `anyhow::Result<()>` which downgraded
/// `DispatchError` to a plain string. Every variant carries a
/// stable `code()` for structured SSE error events and tracing.
#[derive(thiserror::Error, Debug)]
pub enum LaunchSpawnError {
    #[error("invalid item_ref '{ref_str}': {reason}")]
    InvalidRef { ref_str: String, reason: String },
    #[error("launch planning admission is no longer active: {0}")]
    PlanningCancelled(String),
    #[error("failed to read launch planning state: {0}")]
    PlanningStateCheckFailed(String),
    #[error("active launch task signal registry reached its bounded capacity")]
    AbortRegistryCapacityExceeded,
    #[error("failed to register launch task cancellation signal: {0}")]
    AbortRegistrationFailed(String),
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] DispatchError),
}

impl LaunchSpawnError {
    /// Stable machine-readable error code matching the `DispatchError`
    /// code for the `Dispatch` variant and using launch-owned codes for
    /// failures before dispatch authority transfers.
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidRef { .. } => "invalid_ref",
            Self::PlanningCancelled(_) => "launch_cancelled",
            Self::PlanningStateCheckFailed(_) => "launch_planning_state_check_failed",
            Self::AbortRegistryCapacityExceeded => "launch_abort_registry_capacity_exceeded",
            Self::AbortRegistrationFailed(_) => "launch_abort_registration_failed",
            Self::Dispatch(e) => e.code(),
        }
    }

    pub fn http_status(&self) -> axum::http::StatusCode {
        match self {
            Self::InvalidRef { .. } => axum::http::StatusCode::BAD_REQUEST,
            Self::PlanningCancelled(_) => axum::http::StatusCode::CONFLICT,
            Self::PlanningStateCheckFailed(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Self::AbortRegistryCapacityExceeded => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Self::AbortRegistrationFailed(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Self::Dispatch(error) => error.http_status(),
        }
    }
}

fn map_launch_planning_check_error(error: anyhow::Error) -> LaunchSpawnError {
    if error
        .chain()
        .any(|cause| cause.is::<ryeos_app::state_store::LaunchPlanningInactive>())
    {
        LaunchSpawnError::PlanningCancelled(error.to_string())
    } else {
        LaunchSpawnError::PlanningStateCheckFailed(error.to_string())
    }
}

struct LaunchPlanningTaskGuard {
    state: AppState,
    reserved_thread_id: String,
}

impl Drop for LaunchPlanningTaskGuard {
    fn drop(&mut self) {
        if let Err(error) = self
            .state
            .state_store
            .unregister_launch_task_abort(&self.reserved_thread_id)
        {
            tracing::error!(
                thread_id = %self.reserved_thread_id,
                error = %error,
                "failed to unregister durable launch task cancellation signal"
            );
        }
        if let Err(error) = self
            .state
            .state_store
            .settle_launch_planning_task_exit(&self.reserved_thread_id)
        {
            tracing::error!(
                thread_id = %self.reserved_thread_id,
                error = %error,
                "failed to settle durable launch planning task exit"
            );
        }
    }
}

fn abort_launch_task_with_typed_error(
    task: tokio::task::JoinHandle<Result<(), LaunchSpawnError>>,
    error: LaunchSpawnError,
) -> tokio::task::JoinHandle<Result<(), LaunchSpawnError>> {
    task.abort();
    tokio::spawn(async move {
        // Awaiting the aborted task ensures its captured planning guard has
        // settled the durable admission before callers observe the typed
        // launch error. The Tokio cancellation itself is never exposed.
        let _ = task.await;
        Err(error)
    })
}

/// Options controlling the dispatch-launch beyond the core
/// item_ref/project/parameters identity.
pub(crate) struct DispatchLaunchOptions {
    pub ref_bindings: BTreeMap<String, String>,
    /// Caller response mode (`"wait"` or `"detached"`).
    pub launch_mode: String,
    /// Target site id for remote forwarding. `None` means local execution.
    pub target_site_id: Option<String>,
    /// Whether to validate composition only, without execution.
    pub validate_only: bool,
    pub usage_subject: Option<ryeos_state::UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    /// Optional method call (`call.method`/`call.args`) for multi-method items.
    pub call: Option<ryeos_engine::method_call::MethodCall>,
    /// Chained-resume turn: daemon-internal callers only (the
    /// thread-input service); never populated from raw HTTP bodies.
    pub previous_thread_id: Option<String>,
    /// Optional request-local observability, never execution authority.
    pub launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    /// Exact verified subject and captured policy returned by synchronous
    /// dispatch preflight. This may name a terminal target behind the
    /// caller-named wrapper; both success and failure persistence consume this
    /// same non-optional contract.
    root_admission: ryeos_app::thread_lifecycle::RootExecutionAdmission,
    /// Canonical project authority copied from the sealed admission. Background
    /// launch never reuses the caller's pre-canonical path spelling.
    project_path: std::path::PathBuf,
    /// Move-only live-capture lease retained by the background task. It is
    /// released only after dispatch has either committed the captured snapshot
    /// into authoritative history or failed without exposing a root.
    captured_generation: Option<ryeos_executor::execution::CapturedProjectGeneration>,
}

impl DispatchLaunchOptions {
    /// Default execution controls for a synchronously admitted root.
    pub(crate) fn admitted(
        root_admission: ryeos_app::thread_lifecycle::RootExecutionAdmission,
        execution_workspace: &std::path::Path,
        ref_bindings: BTreeMap<String, String>,
        lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    ) -> anyhow::Result<Self> {
        root_admission.validate()?;
        lifecycle_authority.validate()?;
        if root_admission.ref_bindings() != &ref_bindings {
            anyhow::bail!("dispatch launch secondary identities do not match sealed admission");
        }
        let project_path = execution_workspace.canonicalize().with_context(|| {
            format!(
                "canonicalize dispatch launch workspace {}",
                execution_workspace.display()
            )
        })?;
        if let Some(admitted_workspace) = root_admission.execution_workspace() {
            if admitted_workspace != project_path {
                anyhow::bail!(
                    "dispatch launch workspace {} differs from sealed execution materialization {}",
                    project_path.display(),
                    admitted_workspace.display()
                );
            }
        }
        Ok(Self {
            ref_bindings,
            launch_mode: "wait".to_string(),
            target_site_id: None,
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            call: None,
            previous_thread_id: None,
            launch_timings: None,
            lifecycle_authority,
            root_admission,
            project_path,
            captured_generation: None,
        })
    }

    pub(crate) fn retain_captured_generation(
        mut self,
        captured: Option<ryeos_executor::execution::CapturedProjectGeneration>,
    ) -> Self {
        self.captured_generation = captured;
        self
    }
}

/// Run the same schema-driven dispatch walk used by the background task and
/// return the exact terminal/root contract before an internally reserved id is
/// exposed or authoritatively published. This is the public-route admission
/// boundary shared by launch, stream, and thread-input callers; it deliberately
/// understands no kind or service name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn preflight_dispatch_launch(
    state: &AppState,
    item_ref: &crate::routes::parsed_ref::ParsedItemRef,
    project: &ryeos_executor::execution::project_source::ResolvedProjectContext,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    parameters: &Value,
    ref_bindings: &BTreeMap<String, String>,
    principal_id: &str,
    principal_scopes: &[String],
    origin_site_id: &str,
    call: Option<ryeos_engine::method_call::MethodCall>,
    launch_mode: &str,
    validate_only: bool,
    usage_subject: Option<&ryeos_state::UsageSubject>,
    usage_subject_asserted_by: Option<&str>,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<ryeos_executor::dispatch::RootDispatchPreflight, DispatchError> {
    preflight_dispatch_launch_core(BorrowedDispatchPreflight {
        state,
        item_ref,
        project_path: &project.effective_path,
        request_engine: &project.request_engine,
        provenance,
        parameters,
        ref_bindings,
        principal_id,
        principal_scopes,
        origin_site_id,
        call: call.as_ref(),
        launch_mode,
        validate_only,
        usage_subject,
        usage_subject_asserted_by,
        launch_timings,
    })
}

struct BorrowedDispatchPreflight<'a> {
    state: &'a AppState,
    item_ref: &'a crate::routes::parsed_ref::ParsedItemRef,
    project_path: &'a std::path::Path,
    request_engine: &'a std::sync::Arc<ryeos_engine::engine::Engine>,
    provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
    parameters: &'a Value,
    ref_bindings: &'a BTreeMap<String, String>,
    principal_id: &'a str,
    principal_scopes: &'a [String],
    origin_site_id: &'a str,
    call: Option<&'a ryeos_engine::method_call::MethodCall>,
    launch_mode: &'a str,
    validate_only: bool,
    usage_subject: Option<&'a ryeos_state::UsageSubject>,
    usage_subject_asserted_by: Option<&'a str>,
    launch_timings: Option<&'a ryeos_app::launch_stage_timings::LaunchStageTimings>,
}

fn preflight_dispatch_launch_core(
    request: BorrowedDispatchPreflight<'_>,
) -> Result<ryeos_executor::dispatch::RootDispatchPreflight, DispatchError> {
    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

    if ryeos_engine::contracts::LaunchMode::from_wire(request.launch_mode).is_none() {
        return Err(DispatchError::InvalidLaunchMode {
            other: request.launch_mode.to_string(),
        });
    }
    let project_path = request.project_path.canonicalize().map_err(|error| {
        DispatchError::ProjectSource(format!(
            "canonicalize launch project {}: {error}",
            request.project_path.display()
        ))
    })?;
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: request.principal_id.to_string(),
            scopes: request.principal_scopes.to_vec(),
        }),
        project_context: ProjectContext::LocalPath { path: project_path },
        current_site_id: request.state.threads.site_id().to_string(),
        origin_site_id: request.origin_site_id.to_string(),
        execution_hints: Default::default(),
        validate_only: request.validate_only,
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: request.principal_id.to_string(),
        caller_scopes: request.principal_scopes.to_vec(),
        engine: std::sync::Arc::clone(request.request_engine),
        plan_ctx,
        requested_call: request.call.cloned(),
    };
    let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        request.request_engine,
        &exec_ctx.plan_ctx,
        request.provenance,
    )
    .map_err(DispatchError::Internal)?;
    ryeos_executor::dispatch::preflight_root_dispatch(
        request.item_ref.as_str(),
        request.item_ref.kind(),
        request.parameters,
        request.ref_bindings,
        request.usage_subject,
        request.usage_subject_asserted_by,
        &project_binding,
        &exec_ctx,
        request.state,
        request.launch_timings,
    )
}

pub(crate) struct OwnedDispatchPreflight {
    pub state: AppState,
    pub item_ref: crate::routes::parsed_ref::ParsedItemRef,
    pub project_path: std::path::PathBuf,
    pub request_engine: std::sync::Arc<ryeos_engine::engine::Engine>,
    pub provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    pub parameters: Value,
    pub ref_bindings: BTreeMap<String, String>,
    pub principal_id: String,
    pub principal_scopes: Vec<String>,
    pub origin_site_id: String,
    pub call: Option<ryeos_engine::method_call::MethodCall>,
    pub launch_mode: String,
    pub validate_only: bool,
    pub usage_subject: Option<ryeos_state::UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    pub launch_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
}

/// Keep verified resolution/composition off the async HTTP executor. Every
/// input is owned so the blocking worker never borrows request state.
pub(crate) async fn preflight_dispatch_launch_off_thread(
    request: OwnedDispatchPreflight,
) -> Result<ryeos_executor::dispatch::RootDispatchPreflight, DispatchError> {
    let queue_timer = request
        .launch_timings
        .as_ref()
        .map(|timings| timings.nested("preflight_admission", "preflight_blocking_queue_wait"));
    tokio::task::spawn_blocking(move || {
        drop(queue_timer);
        let _work_timer = request
            .launch_timings
            .as_ref()
            .map(|timings| timings.nested("preflight_admission", "preflight_blocking_work"));
        preflight_dispatch_launch_core(BorrowedDispatchPreflight {
            state: &request.state,
            item_ref: &request.item_ref,
            project_path: &request.project_path,
            request_engine: &request.request_engine,
            provenance: &request.provenance,
            parameters: &request.parameters,
            ref_bindings: &request.ref_bindings,
            principal_id: &request.principal_id,
            principal_scopes: &request.principal_scopes,
            origin_site_id: &request.origin_site_id,
            call: request.call.as_ref(),
            launch_mode: &request.launch_mode,
            validate_only: request.validate_only,
            usage_subject: request.usage_subject.as_ref(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.as_deref(),
            launch_timings: request.launch_timings.as_ref(),
        })
    })
    .await
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "preflight admission blocking worker failed: {error}"
        ))
    })?
}
/// Spawn the kind-agnostic dispatch-launch task on the global tokio runtime.
///
/// This helper does not pattern-match on the canonical ref's kind
/// name. Whether the launched item is root-executable is the engine's
/// call, made inside `dispatch::dispatch` against the kind-schema
/// registry.
///
/// `principal_id` is fed verbatim into both `PlanContext.requested_by`
/// (as the local fingerprint) and `DispatchRequest.acting_principal`.
/// For human callers it is `fp:<sha256>`; for webhook callers it is a
/// stable verifier-derived id like `webhook:hmac:<route_id>`.
///
/// `options` carries the complete secondary identity, launch controls, and the
/// mandatory captured root policy produced by synchronous admission.
// Execution plumbing: each argument is a distinct leg of the launch's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
/// Spawn an acknowledged subprocess launch. The receiver resolves only after
/// durable execution authority has been handed to the scheduled task.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_dispatch_launch_with_handoff(
    state: &AppState,
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    options: DispatchLaunchOptions,
) -> (
    tokio::task::JoinHandle<Result<(), LaunchSpawnError>>,
    tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) {
    let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
    let task = spawn_dispatch_launch_inner(
        state,
        item_ref,
        parameters,
        principal_id,
        principal_scopes,
        pre_minted_thread_id,
        provenance,
        options,
        Some(handoff),
    );
    (task, ready)
}

#[allow(clippy::too_many_arguments)]
fn spawn_dispatch_launch_inner(
    state: &AppState,
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    options: DispatchLaunchOptions,
    launch_handoff: Option<ryeos_executor::execution::launch::LaunchHandoff>,
) -> tokio::task::JoinHandle<Result<(), LaunchSpawnError>> {
    let state_clone = state.clone();
    let project_path_buf = options.project_path.clone();
    assert_eq!(
        provenance.effective_path(),
        project_path_buf.as_path(),
        "spawn_dispatch_launch provenance/project path mismatch"
    );
    // Resolve the effective target_site_id for the dispatch request.
    // Self-target (target == current) is normalized to None so local
    // protocol capability checks don't reject it.
    let current_site_id = state_clone.threads.site_id().to_string();
    let dispatch_target_site_id: Option<String> =
        options.target_site_id.filter(|t| t != &current_site_id);

    // Pre-extract values that would borrow `options` across the
    // async move boundary.
    let launch_mode = options.launch_mode;
    let validate_only = options.validate_only;
    let usage_subject = options.usage_subject;
    let usage_subject_asserted_by = options.usage_subject_asserted_by;
    let call = options.call;
    let previous_thread_id = options.previous_thread_id;
    let launch_timings = options.launch_timings;
    let lifecycle_authority = options.lifecycle_authority;
    let root_admission = options.root_admission;
    let ref_bindings = options.ref_bindings;
    let captured_generation = options.captured_generation;
    let first_poll_timer = launch_timings.as_ref().map(|timings| {
        timings.mark("background_task_scheduled");
        timings.nested("background_dispatch", "background_task_spawn_to_first_poll")
    });

    let registration_state = state_clone.clone();
    let registration_thread_id = pre_minted_thread_id.clone();
    // Construct the settlement guard before spawning and move it into the
    // future. It is therefore captured state even if Tokio aborts the task
    // before its first poll (including abort-registry capacity refusal), so a
    // durable planning row cannot be stranded by an unpolled task.
    let planning_task_guard = LaunchPlanningTaskGuard {
        state: state_clone.clone(),
        reserved_thread_id: pre_minted_thread_id.clone(),
    };
    let task = tokio::spawn(async move {
        let _planning_task_guard = planning_task_guard;
        drop(first_poll_timer);
        if let Some(timings) = launch_timings.as_ref() {
            timings.mark("background_dispatch_entered");
        }
        state_clone
            .state_store
            .ensure_launch_planning_active(&pre_minted_thread_id)
            .map_err(map_launch_planning_check_error)?;
        // Keep the live capture's durable recovery roots pinned across request
        // cancellation and the complete background dispatch. The authoritative
        // birth/result rows become the long-lived roots before this drops.
        let _captured_generation = captured_generation;
        root_admission
            .ensure_matches_provenance(&provenance)
            .map_err(DispatchError::Internal)?;
        let plan_ctx = root_admission.plan_context().clone();
        let request_engine = root_admission.request_engine().clone();
        let current_site_id_for_failure_row = plan_ctx.current_site_id.clone();
        let origin_site_id_for_failure_row = plan_ctx.origin_site_id.clone();

        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: request_engine,
            plan_ctx,
            requested_call: call,
        };

        let usage_subject_for_failure_row = usage_subject.clone();
        let usage_subject_asserted_by_for_failure_row = usage_subject_asserted_by.clone();

        let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: &launch_mode,
            target_site_id: dispatch_target_site_id.as_deref(),
            validate_only,
            params: parameters,
            ref_bindings,
            acting_principal: principal_id.as_str(),
            project_path: project_path_buf.as_path(),
            provenance,
            lifecycle_authority,
            launch_timings: launch_timings.clone(),
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
            usage_subject,
            usage_subject_asserted_by,
            previous_thread_id,
            root_admission: Some(root_admission.clone()),
            parent_execution_context: None,
        };

        let dispatched = match launch_handoff.as_ref() {
            Some(handoff) => {
                ryeos_executor::dispatch::dispatch_with_launch_handoff(
                    item_ref.as_str(),
                    &dispatch_req,
                    &exec_ctx,
                    &state_clone,
                    handoff,
                )
                .await
            }
            None => {
                ryeos_executor::dispatch::dispatch(
                    item_ref.as_str(),
                    &dispatch_req,
                    &exec_ctx,
                    &state_clone,
                )
                .await
            }
        };
        if dispatched.is_err() {
            if let Some(timings) = launch_timings.as_ref() {
                timings.record_top_level_from_milestone(
                    "background_dispatch",
                    "background_dispatch_entered",
                );
                timings.emit("background_dispatch_failed");
            }
        }
        match dispatched {
            Ok(_value) => Ok(()),
            Err(e) => {
                // Persistence-first safety net: if dispatch created the
                // pre-minted thread row but failed before finalizing it
                // (e.g. a managed `build_and_launch` policy/trust/grant
                // failure that returns before spawn), finalize it `failed`.
                // If verified preflight authority exists but dispatch failed
                // before creating the row, persist a terminal diagnostic under
                // that same captured authority. No-op when the runtime already
                // drove the row terminal.
                let error_payload = serde_json::json!({
                    "code": e.code(),
                    "reason": e.to_string(),
                });
                let should_finalize = match state_clone.threads.get_thread(&pre_minted_thread_id) {
                    Ok(Some(detail)) => {
                        let admitted_ref = root_admission
                            .verified_subject()
                            .resolved
                            .canonical_ref
                            .to_string();
                        let captured_policy_matches = state_clone
                            .state_store
                            .with_projection(|projection| {
                                projection.chain_retention_projection(&pre_minted_thread_id)
                            })
                            .ok()
                            .flatten()
                            .is_some_and(|projection| {
                                &projection.captured_policy
                                    == root_admission.captured_history_policy()
                            });
                        if detail.chain_root_id != pre_minted_thread_id
                            || detail.item_ref != admitted_ref
                            || detail.kind != root_admission.thread_profile()
                            || !captured_policy_matches
                        {
                            tracing::error!(
                                thread_id = %pre_minted_thread_id,
                                persisted_item_ref = %detail.item_ref,
                                admitted_item_ref = %admitted_ref,
                                "refusing to finalize a pre-existing row that does not match launch admission"
                            );
                            false
                        } else {
                            !ryeos_state::objects::ThreadStatus::from_str_lossy(&detail.status)
                                .is_some_and(|s| s.is_terminal())
                        }
                    }
                    Ok(None) => {
                        let admitted_subject = &root_admission.verified_subject().resolved;
                        let failure_request =
                            ryeos_app::thread_lifecycle::ResolvedExecutionRequest {
                                kind: root_admission.thread_profile().to_string(),
                                item_ref: admitted_subject.canonical_ref.to_string(),
                                executor_ref: admitted_subject.canonical_ref.to_string(),
                                launch_mode: launch_mode.clone(),
                                current_site_id: current_site_id_for_failure_row.clone(),
                                origin_site_id: origin_site_id_for_failure_row.clone(),
                                target_site_id: None,
                                requested_by: Some(principal_id.clone()),
                                usage_subject: usage_subject_for_failure_row.clone(),
                                usage_subject_asserted_by:
                                    usage_subject_asserted_by_for_failure_row.clone(),
                                parameters: dispatch_req.params.clone(),
                                ref_bindings: root_admission.ref_bindings().clone(),
                                root_raw_content_digest: admitted_subject
                                    .raw_content_digest
                                    .clone(),
                                resolved_item: admitted_subject.clone(),
                                plan_context: exec_ctx.plan_ctx.clone(),
                                root_admission: Some(root_admission.clone()),
                            };
                        match state_clone.threads.create_root_thread_with_id(
                            &pre_minted_thread_id,
                            &failure_request,
                            dispatch_req.provenance.project_authority().clone(),
                        ) {
                            Ok(_) => state_clone
                                .threads
                                .get_thread(&pre_minted_thread_id)
                                .ok()
                                .flatten()
                                .is_some_and(|detail| {
                                    !ryeos_state::objects::ThreadStatus::from_str_lossy(
                                        &detail.status,
                                    )
                                    .is_some_and(|s| s.is_terminal())
                                }),
                            Err(error) => {
                                // A collision must never authorize this
                                // request to finalize somebody else's row.
                                tracing::error!(
                                    thread_id = %pre_minted_thread_id,
                                    error = %error,
                                    "failed to persist admitted launch failure row"
                                );
                                false
                            }
                        }
                    }
                    Err(_) => false,
                };
                if should_finalize {
                    let _ = state_clone.threads.finalize_thread(
                        &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                            thread_id: pre_minted_thread_id.clone(),
                            status: "failed".to_string(),
                            outcome_code: Some("failed".to_string()),
                            result: None,
                            error: Some(error_payload),
                            metadata: None,
                            artifacts: Vec::new(),
                            final_cost: None,
                            summary_json: None,
                        },
                    );
                }
                Err(LaunchSpawnError::Dispatch(e))
            }
        }
    });
    match registration_state
        .state_store
        .register_launch_task_abort(&registration_thread_id, task.abort_handle())
    {
        Ok(()) => task,
        Err(ryeos_app::state_store::LaunchTaskAbortRegistrationError::CapacityExceeded) => {
            tracing::warn!(
                thread_id = %registration_thread_id,
                "active launch task signal registry reached its bounded capacity"
            );
            abort_launch_task_with_typed_error(
                task,
                LaunchSpawnError::AbortRegistryCapacityExceeded,
            )
        }
        Err(ryeos_app::state_store::LaunchTaskAbortRegistrationError::Internal(error)) => {
            tracing::error!(
                thread_id = %registration_thread_id,
                error = %error,
                "failed to register durable launch task cancellation signal"
            );
            abort_launch_task_with_typed_error(
                task,
                LaunchSpawnError::AbortRegistrationFailed(error.to_string()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn launch_spawn_error_code_invalid_ref() {
        let e = LaunchSpawnError::InvalidRef {
            ref_str: "x".into(),
            reason: "bad".into(),
        };
        assert_eq!(e.code(), "invalid_ref");
    }

    #[test]
    fn launch_spawn_error_code_for_cancelled_planning_is_stable() {
        let error = LaunchSpawnError::PlanningCancelled("cancelled".to_string());
        assert_eq!(error.code(), "launch_cancelled");
        assert_eq!(error.http_status(), axum::http::StatusCode::CONFLICT);
    }

    #[test]
    fn planning_check_only_maps_the_typed_inactive_marker_to_cancellation() {
        let cancelled =
            map_launch_planning_check_error(ryeos_app::state_store::LaunchPlanningInactive.into());
        assert!(matches!(cancelled, LaunchSpawnError::PlanningCancelled(_)));

        let internal =
            map_launch_planning_check_error(anyhow::anyhow!("runtime database unavailable"));
        assert!(matches!(
            internal,
            LaunchSpawnError::PlanningStateCheckFailed(_)
        ));
        assert_eq!(
            internal.http_status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn launch_spawn_error_for_abort_registry_capacity_is_stable() {
        let error = LaunchSpawnError::AbortRegistryCapacityExceeded;
        assert_eq!(error.code(), "launch_abort_registry_capacity_exceeded");
        assert_eq!(
            error.http_status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            error.to_string(),
            "active launch task signal registry reached its bounded capacity"
        );
    }

    #[test]
    fn launch_spawn_error_code_dispatch_delegates() {
        let de = DispatchError::NotRootExecutable {
            kind: "k".into(),
            detail: "d".into(),
        };
        let e = LaunchSpawnError::Dispatch(de);
        assert_eq!(e.code(), "not_root_executable");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guard_captured_before_spawn_is_dropped_on_pre_poll_abort() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropProbe(dropped.clone());
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });

        // A current-thread runtime cannot poll the spawned task until this
        // test yields, so this exercises the same unpolled-abort boundary as
        // abort-registry refusal in `spawn_dispatch_launch_inner`.
        task.abort();
        assert!(task
            .await
            .expect_err("task must be aborted before first poll")
            .is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capacity_error_waits_for_aborted_guard_settlement() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropProbe(dropped.clone());
        let original = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<Result<(), LaunchSpawnError>>().await
        });
        let replacement = abort_launch_task_with_typed_error(
            original,
            LaunchSpawnError::AbortRegistryCapacityExceeded,
        );
        let error = replacement
            .await
            .expect("typed replacement task must not panic")
            .expect_err("capacity refusal must remain a launch error");

        assert!(dropped.load(Ordering::SeqCst));
        assert!(matches!(
            error,
            LaunchSpawnError::AbortRegistryCapacityExceeded
        ));
        assert_eq!(error.code(), "launch_abort_registry_capacity_exceeded");
    }
}
