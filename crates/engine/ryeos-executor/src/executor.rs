//! Shared service executor used by both live (`/execute`) and standalone
//! (`run-service`) dispatch paths.
//!
//! Steps (same in both modes):
//! 1. Resolve service ref through engine.
//! 2. Verify trust chain (signature + content hash).
//! 3. Extract endpoint + required_caps from verified metadata.
//! 4. Check availability for this mode (DaemonOnly + Standalone → error,
//!    OfflineOnly + Live → error).
//! 5. **Live mode only:** enforce caps (AND semantics — all required caps
//!    must be in caller scopes).
//! 6. Dispatch to handler in the registry.
//! 7. Emit audit record. Create record BEFORE dispatch, finalize on success
//!    or failure so failures are captured.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ryeos_runtime::authorizer::AuthorizationPolicy;
use serde_json::Value;

pub use ryeos_app::service_registry::ServiceAvailability;
use ryeos_app::service_registry::{extract_endpoint, extract_required_caps, ServiceDescriptor};
use ryeos_app::standalone_audit;
use ryeos_app::state::AppState;

/// Execution mode — determines which checks and audit path to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Live mode: daemon is up, caller may be remote, cap enforcement active.
    Live,
    /// Standalone mode: daemon is down, operator has shell access, no cap check.
    Standalone,
}

/// Closed source of project authority for a potentially recorded service.
///
/// Recording is determined by the verified service contract (or a pre-minted
/// thread ID), never by this enum. Each caller must instead state which
/// authority it can supply if recording is required.
pub enum ServiceRecordingAuthoritySource<'a> {
    Execution {
        provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
    },
    /// An explicitly projectless executor surface. The execution mode remains
    /// an independent sealed input; projectless authority does not need
    /// separate live/standalone aliases.
    ExplicitProjectless,
    UnrecordedOnly,
}

/// Complete authority and usage-attribution input at the service execution
/// boundary. There is deliberately no implicit projectless or unattributed
/// default.
pub struct ServiceRecordingContext<'a> {
    pub authority_source: ServiceRecordingAuthoritySource<'a>,
    pub usage_subject: Option<&'a ryeos_state::UsageSubject>,
    pub usage_subject_asserted_by: Option<&'a str>,
}

/// Per-endpoint availability lookup.
///
/// Derives from the supplied descriptor slice — no separate match arm.
/// Unknown endpoint → error (fail-closed). The daemon's
/// `services::handlers::ALL` table is passed in via `AppState`'s
/// `service_descriptors` field so the executor crate stays unaware of
/// daemon-side handler bodies.
pub fn availability_for_endpoint(
    descriptors: &[ServiceDescriptor],
    endpoint: &str,
) -> Result<ServiceAvailability> {
    descriptors
        .iter()
        .find(|d| d.endpoint == endpoint)
        .map(|d| d.availability)
        .ok_or_else(|| {
            anyhow::anyhow!("unknown service endpoint '{endpoint}'; not in the operational catalog")
        })
}

/// Execution context passed to `execute_service`.
pub struct ExecutionContext {
    /// Who's making this request (for audit).
    pub principal_fingerprint: String,
    /// In live mode: the caller's capability scopes.
    /// In standalone mode: empty (operator authority from filesystem).
    pub caller_scopes: Vec<String>,
    /// Engine instance for resolve + verify.
    pub engine: Arc<ryeos_engine::engine::Engine>,
    /// Plan context for engine operations.
    pub plan_ctx: ryeos_engine::contracts::PlanContext,
    /// **Method dispatch**: the caller's `{ method, args }` intent, from the
    /// `/execute` request's `call` block, the graph callback action's `call`,
    /// or accepted-launch options. This is the SINGLE source of truth for
    /// method dispatch: `resolve_dispatch_hop` reads the method here and arg
    /// validation reads the args here. `None`/empty → the kind's default
    /// method. Ignored for terminator/delegate paths.
    pub requested_call: Option<ryeos_engine::method_call::MethodCall>,
}

impl ExecutionContext {
    /// The requested method name, if a `call.method` was provided.
    pub fn requested_method(&self) -> Option<&str> {
        self.requested_call.as_ref().and_then(|c| c.method())
    }

    /// The requested method args, if `call.args` were provided.
    pub fn requested_args(&self) -> Option<&serde_json::Value> {
        self.requested_call.as_ref().and_then(|c| c.args())
    }

    /// True when the caller expressed a method call (a method and/or args).
    /// Used to reject a method call aimed at a kind that declares no methods.
    pub fn has_requested_call(&self) -> bool {
        self.requested_call.as_ref().is_some_and(|c| !c.is_empty())
    }
}

/// Result of a service execution, including metadata for audit.
pub struct ServiceExecutionResult {
    /// The service's return value.
    pub value: Value,
    /// The endpoint that was dispatched to.
    pub endpoint: String,
    /// The trust class of the verified service YAML.
    pub trust_class: ryeos_engine::contracts::TrustClass,
    /// Effective caps after enforcement (live mode only; empty in standalone).
    pub effective_caps: Vec<String>,
    /// Correlation ID for this invocation. It is retrievable as a durable
    /// thread only when `recorded` is true.
    pub invocation_id: String,
    pub recorded: bool,
}

pub fn mint_service_invocation_id() -> String {
    format!(
        "svc-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    )
}

struct RecordedServiceTerminalGuard {
    state: AppState,
    thread_id: String,
    owner: ryeos_app::state_store::InProcessHandlerControl,
    terminal_armed: bool,
    /// Once the handler returns, guard cleanup must retry that exact authored
    /// outcome. Replacing it with `service_interrupted` would falsify the
    /// handler audit merely because the first terminal acknowledgement failed.
    completed_handler_terminal: Option<ryeos_app::thread_lifecycle::ThreadFinalizeParams>,
    ownership_registered: bool,
}

impl RecordedServiceTerminalGuard {
    fn registered(state: &AppState, thread_id: &str) -> Result<Self> {
        let control = state.state_store.register_in_process_handler(thread_id)?;
        Ok(Self {
            state: state.clone(),
            thread_id: thread_id.to_string(),
            owner: control,
            terminal_armed: false,
            completed_handler_terminal: None,
            ownership_registered: true,
        })
    }

    fn arm_terminal(&mut self) {
        self.terminal_armed = true;
    }

    fn disarm_terminal(&mut self) {
        self.terminal_armed = false;
    }

    fn record_completed_handler_terminal(
        &mut self,
        terminal: &ryeos_app::thread_lifecycle::ThreadFinalizeParams,
    ) {
        self.completed_handler_terminal = Some(terminal.clone());
    }

    fn owner(&self) -> &ryeos_app::state_store::InProcessHandlerControl {
        &self.owner
    }
}

impl Drop for RecordedServiceTerminalGuard {
    fn drop(&mut self) {
        if self.terminal_armed && self.owner.has_committed_birth() {
            let params = self.completed_handler_terminal.clone().unwrap_or_else(|| {
                ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id: self.thread_id.clone(),
                    status: "failed".to_string(),
                    outcome_code: Some("service_interrupted".to_string()),
                    result: None,
                    error: Some(serde_json::json!({
                        "error": "recorded service execution ended before the handler returned"
                    })),
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                }
            });
            if let Err(error) = finalize_recorded_service_exact(&self.state, &self.owner, &params) {
                tracing::warn!(
                    thread_id = %self.thread_id,
                    error = %error,
                    "failed to confirm recorded service terminal outcome during owner cleanup"
                );
            }
        }
        if self.ownership_registered {
            // Retire a confirmed reservation while the volatile registry still
            // proves this owner is active. Shutdown audit is gated on an empty
            // registry, so it cannot race this final owner cleanup.
            if self.owner.has_terminal_confirmed() {
                match self
                    .state
                    .state_store
                    .delete_terminal_in_process_handler_reservation_owned(
                        &self.thread_id,
                        &self.owner,
                    ) {
                    Ok(true) | Ok(false) => {}
                    Err(error) => tracing::warn!(
                        thread_id = %self.thread_id,
                        error = %error,
                        "left terminal in-process reservation residue for reconciliation"
                    ),
                }
            }
            match self
                .state
                .state_store
                .unregister_in_process_handler(&self.thread_id, &self.owner)
            {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    thread_id = %self.thread_id,
                    "active in-process service handler registration disappeared before guard drop"
                ),
                Err(error) => tracing::warn!(
                    thread_id = %self.thread_id,
                    error = %error,
                    "failed to unregister active in-process service handler"
                ),
            }
        }
    }
}

fn recording_integrity(detail: impl Into<String>) -> anyhow::Error {
    crate::dispatch_error::DispatchError::RecordingIntegrity {
        detail: detail.into(),
    }
    .into()
}

fn select_service_handler_context(
    metadata: &std::collections::HashMap<String, Value>,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    principal: &str,
    scopes: &[String],
    current_site_id: &str,
    origin_site_id: &str,
) -> Result<ryeos_app::handler_context::HandlerContext> {
    match ryeos_app::service_registry::extract_ui_dispatch(metadata)? {
        ryeos_app::service_registry::UiDispatchMode::SessionLocal => local_handler_context
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session-local service dispatch requires a trusted local handler context"
                )
            }),
        ryeos_app::service_registry::UiDispatchMode::Verified => {
            if let Some(context) = local_handler_context {
                let mut expected_scopes = scopes.to_vec();
                expected_scopes.sort();
                expected_scopes.dedup();
                let mut supplied_scopes = context.scopes.clone();
                supplied_scopes.sort();
                supplied_scopes.dedup();
                if context.fingerprint != principal
                    || supplied_scopes != expected_scopes
                    || context.execution_origin(current_site_id) != origin_site_id
                {
                    bail!(
                        "trusted service handler context differs from the sealed execution principal/scopes/origin"
                    );
                }
                Ok(context)
            } else {
                Ok(ryeos_app::handler_context::HandlerContext::new_with_origin(
                    principal.to_string(),
                    scopes.to_vec(),
                    true,
                    (origin_site_id != current_site_id).then(|| origin_site_id.to_string()),
                ))
            }
        }
    }
}

/// Confirm every durable representation of one recorded service outcome.
fn confirm_recorded_service_terminal(
    state: &AppState,
    params: &ryeos_app::thread_lifecycle::ThreadFinalizeParams,
) -> Result<()> {
    let snapshot = state
        .state_store
        .get_authoritative_root_thread_snapshot(&params.thread_id)?
        .ok_or_else(|| anyhow::anyhow!("authoritative terminal snapshot is missing"))?;
    let expected_status = ryeos_state::objects::ThreadStatus::from_str_lossy(&params.status)
        .ok_or_else(|| {
            anyhow::anyhow!("requested terminal status `{}` is invalid", params.status)
        })?;
    if snapshot.status != expected_status {
        bail!("authoritative terminal status differs");
    }
    if snapshot.outcome_code != params.outcome_code {
        bail!("authoritative terminal outcome_code differs");
    }
    if snapshot.result != params.result {
        bail!("authoritative terminal result differs");
    }
    if snapshot.error != params.error {
        bail!("authoritative terminal error differs");
    }
    if snapshot.result_project_snapshot_hash.is_some() {
        bail!("authoritative terminal result project snapshot is unexpectedly present");
    }
    if snapshot.budget.is_some() {
        bail!("authoritative terminal final_cost is unexpectedly present");
    }
    if snapshot
        .facets
        .contains_key("runtime.terminal_envelope_json")
    {
        bail!("authoritative managed terminal envelope is unexpectedly present");
    }
    if !snapshot.artifacts.is_empty() {
        bail!("terminal artifacts are unexpectedly present");
    }
    Ok(())
}

/// Commit and confirm a recorded service outcome before allowing the handler
/// result to escape. A failed write acknowledgement is not decisive because
/// the write may have committed; exact readback after each bounded attempt
/// makes retry safe without replaying the handler.
fn finalize_recorded_service_exact(
    state: &AppState,
    owner: &ryeos_app::state_store::InProcessHandlerControl,
    params: &ryeos_app::thread_lifecycle::ThreadFinalizeParams,
) -> Result<()> {
    if params.metadata.is_some()
        || !params.artifacts.is_empty()
        || params.final_cost.is_some()
        || params.summary_json.is_some()
    {
        return Err(recording_integrity(
            "recorded service terminal shape contains unsupported auxiliary fields",
        ));
    }

    let mut attempt_diagnostics = Vec::new();
    for attempt in 1..=2 {
        let (finalize_diagnostic, postcommit_complete) = match state
            .threads
            .finalize_recorded_service_owned(params, owner)
        {
            Ok(ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::Finalized(_)) => {
                ("terminal write committed".to_string(), true)
            }
            Ok(ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status,
            }) => (
                format!("row was already terminal with status `{status}`"),
                false,
            ),
            Ok(ryeos_app::thread_lifecycle::FinalizeIfNonterminalOutcome::PreservedForShutdown) => {
                (
                    "terminal write was preserved for shutdown".to_string(),
                    false,
                )
            }
            Err(error) => (format!("terminal write failed: {error}"), false),
        };
        match confirm_recorded_service_terminal(state, params) {
            Ok(()) => {
                if !postcommit_complete {
                    if let Err(postcommit_error) = state
                        .threads
                        .repair_recorded_service_terminal_postcommit(params)
                        .context("repair exact recorded-service terminal postcommit")
                    {
                        attempt_diagnostics.push(format!(
                            "attempt {attempt}: {finalize_diagnostic}; terminal confirmed but postcommit repair failed: {postcommit_error}"
                        ));
                        continue;
                    }
                }
                match state
                    .state_store
                    .settle_in_process_handler_reservation_owned(
                        &params.thread_id,
                        owner,
                    )
                {
                    Ok(true) => {}
                    Ok(false) => tracing::debug!(
                        thread_id = %params.thread_id,
                        "confirmed recorded-service terminal reservation was already retired"
                    ),
                    Err(settlement_error) => tracing::warn!(
                        thread_id = %params.thread_id,
                        error = %settlement_error,
                        "confirmed recorded-service terminal left reservation settlement for reconciliation"
                    ),
                }
                owner.mark_terminal_confirmed();
                return Ok(());
            }
            Err(confirmation_error) => attempt_diagnostics.push(format!(
                "attempt {attempt}: {finalize_diagnostic}; confirmation failed: {confirmation_error}"
            )),
        }
    }
    Err(recording_integrity(format!(
        "thread {} terminal outcome could not be confirmed after bounded retries: {}",
        params.thread_id,
        attempt_diagnostics.join("; ")
    )))
}

/// Resolve and verify any item ref (kind-agnostic).
///
/// Steps:
/// 1. Parse the ref string into a `CanonicalRef`.
/// 2. Resolve through the engine.
/// 3. Verify trust chain (signature + content hash).
///
/// `ref_kind_label` affects diagnostic wording only; resolution and routing
/// always come from the parsed canonical ref and verified registries.
pub fn resolve_and_verify(
    engine: &Arc<ryeos_engine::engine::Engine>,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    item_ref: &str,
    ref_kind_label: Option<&str>,
) -> Result<ryeos_engine::contracts::VerifiedItem> {
    use ryeos_engine::canonical_ref::CanonicalRef;

    let label = ref_kind_label.unwrap_or("ref");

    let canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| anyhow::anyhow!("invalid {label} ref '{item_ref}': {e}"))?;

    // Keep the typed `EngineError` as the anyhow source so callers can
    // downcast (dispatch maps `ItemNotFound` for `service:` refs to a
    // structured `service_not_installed` error). The context string is
    // the Display surface — wording unchanged.
    let resolved = engine.resolve(plan_ctx, &canonical).map_err(|e| {
        let msg = format!("{label} '{item_ref}' failed to resolve: {e}");
        anyhow::Error::new(e).context(msg)
    })?;

    let verified = engine
        .verify(plan_ctx, resolved)
        .map_err(|e| anyhow::anyhow!("{label} '{item_ref}' failed verification: {e}"))?;

    Ok(verified)
}

/// Execute a service with failure-capturing audit.
///
/// Steps (same in both live and standalone modes):
/// 1. Resolve service ref through engine.
/// 2. Verify trust chain (signature + content hash).
/// 3. Extract endpoint + required_caps from verified metadata.
/// 4. Check availability for this mode.
/// 5. **Live mode only:** enforce caps (AND semantics).
/// 6. Create audit record BEFORE dispatch.
/// 7. Dispatch to handler.
/// 8. Finalize audit with success or failure.
pub async fn execute_service(
    service_ref: &str,
    params: Value,
    mode: ExecutionMode,
    ctx: &ExecutionContext,
    state: &AppState,
    recording: ServiceRecordingContext<'_>,
) -> Result<ServiceExecutionResult> {
    let verified = resolve_and_verify(&ctx.engine, &ctx.plan_ctx, service_ref, Some("service"))?;
    execute_service_verified(
        verified,
        service_ref,
        params,
        mode,
        ctx,
        state,
        recording,
        None,
        None,
    )
    .await
}

/// Execute a service given an already-verified item.
///
/// This is the post-resolve/verify portion of `execute_service`: availability
/// check, cap enforcement, audit record creation, handler dispatch, audit
/// finalization. Split out so future kind-agnostic dispatch can reuse the
/// resolve+verify step independently.
///
/// `pre_minted_thread_id`: when `Some(id)`, the audit row uses that id
/// verbatim. External subscribers registered against `id` (e.g. an SSE
/// source that minted the id before launch) receive every persisted event
/// from the very first lifecycle event onward. When `None`, a fresh
/// `svc-<ts>-<rand>` id is minted as before.
///
/// `local_handler_context` is trusted out-of-band context supplied by a local
/// transport. Session-local services require it. Ordinary verified services
/// may receive it only when its fingerprint/scopes exactly match `ctx`; this
/// preserves transport-authenticated properties such as `verified: false`
/// without permitting an identity or capability override.
// Verified subject, execution mode/context, pre-minted identity, and trusted
// local handler context remain explicit at the service execution boundary.
#[allow(clippy::too_many_arguments)]
pub async fn execute_service_verified(
    verified: ryeos_engine::contracts::VerifiedItem,
    service_ref: &str,
    params: Value,
    mode: ExecutionMode,
    ctx: &ExecutionContext,
    state: &AppState,
    recording: ServiceRecordingContext<'_>,
    pre_minted_thread_id: Option<&str>,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
) -> Result<ServiceExecutionResult> {
    let (planning_fingerprint, planning_scopes) = match &ctx.plan_ctx.requested_by {
        ryeos_engine::contracts::EffectivePrincipal::Local(principal) => {
            (principal.fingerprint.as_str(), principal.scopes.as_slice())
        }
        ryeos_engine::contracts::EffectivePrincipal::Delegated(delegated) => (
            delegated.caller_fingerprint.as_str(),
            delegated.delegated_scopes.as_slice(),
        ),
    };
    let mut planning_scopes = planning_scopes.to_vec();
    planning_scopes.sort();
    planning_scopes.dedup();
    let mut execution_scopes = ctx.caller_scopes.clone();
    execution_scopes.sort();
    execution_scopes.dedup();
    if planning_fingerprint != ctx.principal_fingerprint || planning_scopes != execution_scopes {
        bail!("service execution identity differs from its sealed planning principal/scopes");
    }
    let trust_class = verified.trust_class;

    // 3. Extract endpoint + required_caps
    let endpoint = extract_endpoint(&verified.resolved.metadata.extra)?;
    let required_caps = extract_required_caps(&verified.resolved.metadata.extra);
    let authored_record_thread =
        ryeos_app::service_registry::extract_record_thread(&verified.resolved.metadata.extra)?;
    // A caller that pre-minted an externally subscribed ID has promised a
    // durable stream. Preserve that promise regardless of the service's normal
    // high-frequency read policy.
    let record_thread = authored_record_thread || pre_minted_thread_id.is_some();

    // 4. Availability check
    let avail = availability_for_endpoint(state.service_descriptors, &endpoint)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match (mode, avail) {
        (ExecutionMode::Standalone, ServiceAvailability::DaemonOnly) => {
            bail!("{service_ref} is DaemonOnly; start the daemon and call /execute");
        }
        (ExecutionMode::Live, ServiceAvailability::OfflineOnly) => {
            bail!(
                "{service_ref} is OfflineOnly; engine reload not implemented; \
                 run `ryeosd run-service {service_ref}` while daemon is stopped"
            );
        }
        _ => {}
    }

    // 5. Cap enforcement (live mode only)
    let effective_caps = if mode == ExecutionMode::Live {
        let policy = AuthorizationPolicy::require_all(
            &required_caps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        );
        match state.authorizer.authorize(&ctx.caller_scopes, &policy) {
            Ok(()) => required_caps.clone(),
            Err(_) => {
                return Err(crate::dispatch_error::DispatchError::ServiceCapDenied {
                    service_ref: service_ref.to_string(),
                    required: required_caps.join(", "),
                    caller_scopes: ctx.caller_scopes.clone(),
                }
                .into());
            }
        }
    } else {
        Vec::new()
    };

    // 7a. Create audit record BEFORE dispatch.
    // Honor a caller-supplied thread id when provided so external
    // subscribers (route SSE sources) registered against the id see
    // every persisted lifecycle event from the very first one.
    let invocation_id = match pre_minted_thread_id {
        Some(id) => id.to_string(),
        None => mint_service_invocation_id(),
    };

    let thread_profile = ctx
        .engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "verified executable kind '{}' has no execution.thread_profile",
                verified.resolved.kind
            )
        })?;

    // Decide recording before deriving persistence-only project authority.
    // `UnrecordedOnly` is an assertion by the caller, not a way to override a
    // verified service's recording contract.
    let project_binding = match (&recording.authority_source, record_thread) {
        (ServiceRecordingAuthoritySource::Execution { provenance }, true) => Some(
            ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                &ctx.engine,
                &ctx.plan_ctx,
                provenance,
            )?,
        ),
        (ServiceRecordingAuthoritySource::Execution { .. }, false) => None,
        (ServiceRecordingAuthoritySource::ExplicitProjectless, should_record) => {
            if !matches!(
                &ctx.plan_ctx.project_context,
                ryeos_engine::contracts::ProjectContext::None
            ) {
                bail!("explicit projectless service authority requires no project context");
            }
            if should_record {
                Some(
                    ryeos_app::thread_lifecycle::AdmittedProjectBinding::explicit_projectless(
                        &ctx.engine,
                        &ctx.plan_ctx,
                    )?,
                )
            } else {
                None
            }
        }
        (ServiceRecordingAuthoritySource::UnrecordedOnly, true) => {
            return Err(recording_integrity(format!(
                "caller asserted unrecorded-only execution for recorded service `{service_ref}`"
            )));
        }
        (ServiceRecordingAuthoritySource::UnrecordedOnly, false) => None,
    };

    let dispatch_result = if let Some(project_binding) = project_binding {
        let root_admission = ryeos_app::thread_lifecycle::admit_verified_root_execution(
            &ctx.engine,
            &ctx.plan_ctx,
            &ctx.plan_ctx,
            project_binding,
            verified.clone(),
            &state.node_history_policy,
            thread_profile,
            std::collections::BTreeMap::new(),
            recording.usage_subject.cloned(),
            recording.usage_subject_asserted_by.map(str::to_owned),
        )?;
        let recorded_admission = ryeos_app::thread_lifecycle::RecordedServiceAdmission::new(
            root_admission,
            endpoint.clone(),
        )?;
        // Registration precedes publication, while launch metadata and the
        // created→running transition commit atomically with root birth.
        let mut lifecycle_guard = RecordedServiceTerminalGuard::registered(state, &invocation_id)?;
        // Arm the owner-qualified fallback before durable publication. If any
        // post-commit step unwinds or returns an error, the exact registered
        // owner remains responsible for terminal settlement.
        lifecycle_guard.arm_terminal();
        let launch_metadata = ryeos_app::launch_metadata::RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            );
        state.threads.create_recorded_service_root(
            &invocation_id,
            &recorded_admission,
            &launch_metadata,
            lifecycle_guard.owner(),
        )?;

        // The handler and terminal commit are daemon-owned once the running
        // root exists. Dropping a request future detaches this task instead of
        // cancelling it and abandoning a running audit row.
        let task_state = state.clone();
        let task_endpoint = endpoint.clone();
        let task_service_ref = service_ref.to_string();
        let task_invocation_id = invocation_id.clone();
        let task_principal = ctx.principal_fingerprint.clone();
        let task_scopes = ctx.caller_scopes.clone();
        let task_current_site_id = ctx.plan_ctx.current_site_id.clone();
        let task_origin_site_id = ctx.plan_ctx.origin_site_id.clone();
        let task_metadata = verified.resolved.metadata.extra.clone();
        let task_params = params.clone();
        let task = tokio::spawn(async move {
            let dispatch_result: Result<Value> = async {
                let hctx = select_service_handler_context(
                    &task_metadata,
                    local_handler_context,
                    &task_principal,
                    &task_scopes,
                    &task_current_site_id,
                    &task_origin_site_id,
                )?;
                let handler = task_state
                    .services
                    .get(&task_endpoint)
                    .ok_or_else(|| {
                        anyhow::anyhow!("service handler '{}' not registered", task_endpoint)
                    })?
                    .clone();
                handler(task_params.clone(), hctx, Arc::new(task_state.clone())).await
            }
            .await;

            let terminal = match &dispatch_result {
                Ok(value) => ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id: task_invocation_id.clone(),
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result: Some(value.clone()),
                    error: None,
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                },
                Err(error) => ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id: task_invocation_id.clone(),
                    status: "failed".to_string(),
                    outcome_code: Some("handler_error".to_string()),
                    result: None,
                    error: Some(
                        match ryeos_app::handler_error::extract_handler_error(error) {
                            Some(ryeos_app::handler_error::HandlerError::Structured {
                                body,
                                ..
                            }) => body,
                            _ => serde_json::json!({ "error": error.to_string() }),
                        },
                    ),
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                },
            };
            lifecycle_guard.record_completed_handler_terminal(&terminal);
            let terminal_confirmation =
                finalize_recorded_service_exact(&task_state, lifecycle_guard.owner(), &terminal);
            if terminal_confirmation.is_ok() {
                lifecycle_guard.disarm_terminal();
            }

            // This audit describes the handler execution, not the later
            // persistence acknowledgement. Always attempt it once the handler
            // has returned, including when terminal confirmation fails.
            if mode == ExecutionMode::Standalone {
                let audit_path = standalone_audit::default_audit_path(&task_state.config.app_root);
                let record = standalone_audit::StandaloneAuditRecord {
                    ts: lillux::time::iso8601_now(),
                    mode: "standalone",
                    service_ref: task_service_ref,
                    endpoint: task_endpoint,
                    status: match &dispatch_result {
                        Ok(_) => "success",
                        Err(_) => "failure",
                    },
                    error_message: match &dispatch_result {
                        Err(error) => Some(error.to_string()),
                        Ok(_) => None,
                    },
                    uid: standalone_audit::current_uid(),
                    pid: std::process::id(),
                    requested_by: "local-operator",
                    params_hash: standalone_audit::params_hash(&task_params),
                };
                if let Err(error) = standalone_audit::write_audit_record(&audit_path, &record) {
                    tracing::warn!(
                        error = %error,
                        path = %audit_path.display(),
                        "failed to write standalone audit record"
                    );
                }
            }

            terminal_confirmation?;
            dispatch_result
        });

        match task.await {
            Ok(result) => result,
            Err(join_error) => {
                return Err(recording_integrity(format!(
                    "recorded service task failed before returning its handler outcome; its owner guard applied the no-replay interruption policy: {join_error}"
                )));
            }
        }
    } else {
        // Unrecorded services remain hot reads: no admission, persistence-only
        // project projection, ownership registration, or detached task.
        let hctx = select_service_handler_context(
            &verified.resolved.metadata.extra,
            local_handler_context,
            &ctx.principal_fingerprint,
            &ctx.caller_scopes,
            &ctx.plan_ctx.current_site_id,
            &ctx.plan_ctx.origin_site_id,
        )?;
        let handler = state
            .services
            .get(&endpoint)
            .ok_or_else(|| anyhow::anyhow!("service handler '{}' not registered", endpoint))?
            .clone();
        handler(params, hctx, Arc::new(state.clone())).await
    };

    let value = dispatch_result.map_err(|e| {
        // Extract typed HandlerError to preserve HTTP semantics.
        // Without this, HandlerError::NotFound surfaces as 500 via
        // the generic Internal(#[from] anyhow::Error) path.
        //
        // Walk the whole error chain (not just the root) so a HandlerError
        // wrapped in `.context(...)` still maps to the right status — this
        // matches the route path's `extract_handler_error`. A root-only
        // `downcast_ref` here silently degraded wrapped NotFound/Conflict to
        // 500, diverging from the route (which returned 404/409 for the same
        // handler error).
        use ryeos_app::handler_error::HandlerError;
        let e = match e.downcast::<crate::dispatch_error::DispatchError>() {
            Ok(dispatch_error) => return dispatch_error,
            Err(error) => error,
        };
        match ryeos_app::handler_error::extract_handler_error(&e) {
            Some(HandlerError::NotFound) => crate::dispatch_error::DispatchError::NotFound,
            Some(HandlerError::Conflict(msg)) => {
                crate::dispatch_error::DispatchError::Conflict(msg)
            }
            Some(HandlerError::Forbidden(msg)) => {
                crate::dispatch_error::DispatchError::ServiceCapDenied {
                    service_ref: service_ref.to_string(),
                    required: msg,
                    caller_scopes: ctx.caller_scopes.clone(),
                }
            }
            Some(HandlerError::BadRequest(msg)) => {
                crate::dispatch_error::DispatchError::MethodInvalidArg {
                    method: endpoint.clone(),
                    reason: msg,
                }
            }
            Some(HandlerError::Structured { code, status, body }) => {
                crate::dispatch_error::DispatchError::StructuredService { code, status, body }
            }
            _ => crate::dispatch_error::DispatchError::Internal(e),
        }
    })?;

    Ok(ServiceExecutionResult {
        value,
        endpoint,
        trust_class,
        effective_caps,
        invocation_id,
        recorded: record_thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn availability_unknown_is_error() {
        // Empty descriptor table — any endpoint is "unknown".
        assert!(availability_for_endpoint(&[], "future.service").is_err());
        assert!(availability_for_endpoint(&[], "nonexistent").is_err());
    }

    #[test]
    fn session_local_dispatch_requires_explicit_trusted_context() {
        let metadata = HashMap::from([(
            "ui_dispatch".to_string(),
            serde_json::json!("session_local"),
        )]);

        let error = select_service_handler_context(
            &metadata,
            None,
            "fp:verified-request",
            &["cap:verified".to_string()],
            "site:local",
            "site:local",
        )
        .expect_err("session-local dispatch must not fall back to request identity");
        assert!(error
            .to_string()
            .contains("requires a trusted local handler context"));
    }

    #[test]
    fn session_local_dispatch_preserves_the_exact_supplied_context() {
        let metadata = HashMap::from([(
            "ui_dispatch".to_string(),
            serde_json::json!("session_local"),
        )]);
        let supplied = ryeos_app::handler_context::HandlerContext::new_with_origin(
            "session:one".to_string(),
            vec!["ui.read".to_string()],
            false,
            Some("site:browser".to_string()),
        );

        let selected = select_service_handler_context(
            &metadata,
            Some(supplied),
            "fp:verified-request",
            &["cap:verified".to_string()],
            "site:local",
            "site:local",
        )
        .expect("select session-local context");
        assert_eq!(selected.fingerprint, "session:one");
        assert_eq!(selected.scopes, vec!["ui.read"]);
        assert!(!selected.verified);
        assert_eq!(
            selected.authenticated_origin_site_id.as_deref(),
            Some("site:browser")
        );
    }

    #[test]
    fn verified_dispatch_rejects_a_handler_context_with_different_identity() {
        let metadata = HashMap::new();
        let supplied = ryeos_app::handler_context::HandlerContext::new(
            "session:one".to_string(),
            vec!["ui.read".to_string()],
            false,
        );

        let error = select_service_handler_context(
            &metadata,
            Some(supplied),
            "fp:verified-request",
            &["cap:b".to_string(), "cap:a".to_string()],
            "site:local",
            "site:local",
        )
        .expect_err("mismatched handler identity must fail closed");
        assert!(error
            .to_string()
            .contains("differs from the sealed execution principal/scopes"));
    }

    #[test]
    fn verified_dispatch_preserves_matching_unverified_route_context() {
        let selected = select_service_handler_context(
            &HashMap::new(),
            Some(ryeos_app::handler_context::HandlerContext::anonymous()),
            "",
            &[],
            "site:local",
            "site:local",
        )
        .expect("matching anonymous route context");
        assert_eq!(selected.fingerprint, "");
        assert!(selected.scopes.is_empty());
        assert!(!selected.verified);
    }
}
