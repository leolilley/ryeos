//! Shared service executor used by both live (`/execute`) and standalone
//! (`run-service`) dispatch paths.
//!
//! Steps (same in both modes):
//! 1. Resolve service ref through engine.
//! 2. Verify trust chain (signature + content hash).
//! 3. Extract endpoint + required_caps from verified metadata.
//! 4. Check availability for this mode (DaemonOnly + Standalone → error,
//!    StoppedNodeOnly + Live → error).
//! 5. **Live mode only:** enforce caps (AND semantics — all required caps
//!    must be in caller scopes).
//! 6. Dispatch to handler in the registry.
//! 7. Emit audit record. Create record BEFORE dispatch, finalize on success
//!    or failure so failures are captured.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use serde_json::Value;

pub use ryeos_app::service_registry::ServiceAvailability;
use ryeos_app::service_registry::ServiceDescriptor;
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
    /// An inline callback action issued by a running parent thread for a
    /// pre-minted child. A recorded service thread born here is an inline
    /// in-process child: a thread in its parent's chain with the parent as
    /// upstream, exactly like an inline tool leaf.
    InlineCallbackChild {
        provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
        parent_thread_id: &'a str,
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
#[derive(Clone)]
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
    /// How much of a service's answer this caller may see. Classified by the
    /// key that acts (the configured operator key, another admitted key, or no
    /// key), never by what kind of client holds it. Every construction names
    /// it; there is no default.
    pub disclosure_class: ryeos_node_runtime::disclosure::DisclosureClass,
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

/// Validate the correlation coordinate minted for an in-process service
/// execution. Service invocations are audit threads, but they intentionally do
/// not claim the runtime `T-...` identity class.
pub fn validate_service_invocation_id(value: &str) -> anyhow::Result<()> {
    let body = value
        .strip_prefix("svc-")
        .ok_or_else(|| anyhow::anyhow!("service invocation id must start with `svc-`"))?;
    let (millis, nonce) = body
        .rsplit_once('-')
        .ok_or_else(|| anyhow::anyhow!("service invocation id is missing its nonce"))?;
    if !(10..=20).contains(&millis.len()) || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("service invocation id has an invalid timestamp");
    }
    if nonce.len() != 8
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("service invocation id has an invalid nonce");
    }
    Ok(())
}

struct RecordedServiceTerminalGuard {
    state: AppState,
    thread_id: String,
    /// Chain the recorded thread is born into: its own id for a direct
    /// invocation root, the parent's chain for an inline in-process child.
    chain_root_id: String,
    owner: ryeos_app::state_store::InProcessHandlerControl,
    terminal_armed: bool,
    /// Once the handler returns, guard cleanup must retry that exact authored
    /// outcome. Replacing it with `service_interrupted` would falsify the
    /// handler audit merely because the first terminal acknowledgement failed.
    completed_handler_terminal: Option<ryeos_app::thread_lifecycle::ThreadFinalizeParams>,
    ownership_registered: bool,
}

impl RecordedServiceTerminalGuard {
    fn registered(state: &AppState, thread_id: &str, chain_root_id: &str) -> Result<Self> {
        let control = state.state_store.register_in_process_handler(thread_id)?;
        Ok(Self {
            state: state.clone(),
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
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

    fn chain_root_id(&self) -> &str {
        &self.chain_root_id
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
            if let Err(error) = finalize_recorded_service_exact(
                &self.state,
                &self.owner,
                &self.chain_root_id,
                &params,
            ) {
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
                context
                    .validate_execution_authority(
                        principal,
                        scopes,
                        current_site_id,
                        origin_site_id,
                    )
                    .context(
                        "trusted service handler context differs from sealed execution authority",
                    )?;
                Ok(context)
            } else {
                // Node-internal launches have an execution principal but no
                // cryptographically authenticated handler boundary. Preserve
                // that distinction; never reconstruct `verified: true` from a
                // sealed principal string.
                Ok(ryeos_app::handler_context::HandlerContext::new(
                    principal.to_string(),
                    scopes.to_vec(),
                    false,
                ))
            }
        }
    }
}

/// The terminal a durable stop intent commits for any later finalizer. A
/// newborn inline child whose parent already carries a stop is settled to
/// exactly this shape so the owner's confirmation readback matches.
fn inherited_stop_terminal(
    thread_id: &str,
    intent: ryeos_app::state_store::StopIntent,
) -> ryeos_app::thread_lifecycle::ThreadFinalizeParams {
    let status = match intent {
        ryeos_app::state_store::StopIntent::Cancel => ryeos_state::objects::ThreadStatus::Cancelled,
        ryeos_app::state_store::StopIntent::Kill => ryeos_state::objects::ThreadStatus::Killed,
    };
    ryeos_app::thread_lifecycle::ThreadFinalizeParams {
        thread_id: thread_id.to_owned(),
        status: status.as_str().to_owned(),
        outcome_code: Some(status.as_str().to_owned()),
        result: None,
        error: Some(serde_json::json!({
            "reason": "durable_stop_intent",
            "intent": intent.as_str(),
        })),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }
}

/// Confirm every durable representation of one recorded service outcome. The
/// thread is read at its exact chain position: a direct invocation is its own
/// chain root, an inline in-process child lives in its parent's chain.
fn confirm_recorded_service_terminal(
    state: &AppState,
    chain_root_id: &str,
    params: &ryeos_app::thread_lifecycle::ThreadFinalizeParams,
) -> Result<()> {
    let (snapshot, _last_event, _chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(chain_root_id, &params.thread_id)?
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
    if !ryeos_app::thread_lifecycle::recorded_service_terminal_json_equal(
        snapshot.result.as_ref(),
        params.result.as_ref(),
    )
    .context("compare authoritative terminal result canonical JSON")?
    {
        bail!("authoritative terminal result differs");
    }
    if !ryeos_app::thread_lifecycle::recorded_service_terminal_json_equal(
        snapshot.error.as_ref(),
        params.error.as_ref(),
    )
    .context("compare authoritative terminal error canonical JSON")?
    {
        bail!("authoritative terminal error differs");
    }
    if snapshot.result_project_snapshot_hash.is_some() {
        bail!("authoritative terminal result project snapshot is unexpectedly present");
    }
    if snapshot.budget.is_some() {
        bail!("authoritative terminal final_cost is unexpectedly present");
    }
    if snapshot.managed_runtime_terminal.is_some() {
        bail!("authoritative managed terminal envelope is unexpectedly present");
    }
    if !snapshot.artifacts.is_empty() {
        bail!("terminal artifacts are unexpectedly present");
    }
    Ok(())
}

/// [`disclose_service_result`] with the floor of the node's own secrets. The
/// floor is read only for a caller other than the operator. When it cannot be
/// built the answer is refused, never served unchecked.
fn disclose_node_service_result(
    state: &AppState,
    class: ryeos_node_runtime::disclosure::DisclosureClass,
    disclosure: Option<&ryeos_node_runtime::disclosure::ServiceDisclosure>,
    result: Result<Value>,
) -> Result<Value> {
    if class.is_operator() {
        return disclose_service_result(
            class,
            disclosure,
            result,
            &ryeos_app::secret_floor::SecretFloor::from_values(Vec::new()),
        );
    }
    match ryeos_app::secret_floor::SecretFloor::for_node(state) {
        Ok(floor) => disclose_service_result(class, disclosure, result, &floor),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "secret floor unavailable; service outcome withheld from a non-operator caller"
            );
            Err(anyhow::Error::new(ryeos_app::handler_error::typed_refusal(
                ryeos_app::secret_floor::SECRET_FLOOR_WITHHELD,
                403,
                "the answer was withheld from this caller",
            )))
        }
    }
}

/// What the caller of a service receives of its outcome. The operator gets
/// the outcome whole. Any other caller gets the item's declared tier of an
/// answer, or a refusal when the item declares none, and a failure shows only
/// a stable code and a fixed sentence: the failure's own text can carry a
/// path, probe output or a key.
fn disclose_service_result(
    class: ryeos_node_runtime::disclosure::DisclosureClass,
    disclosure: Option<&ryeos_node_runtime::disclosure::ServiceDisclosure>,
    result: Result<Value>,
    floor: &ryeos_app::secret_floor::SecretFloor,
) -> Result<Value> {
    use ryeos_app::handler_error::HandlerError;
    if class.is_operator() {
        return result;
    }
    // Whatever a non-operator caller is about to receive passes the floor
    // last: an answer is checked after its tier is projected, and a failure
    // is checked as the text it will show.
    let withheld = |error: HandlerError| anyhow::Error::new(error);
    match result {
        Ok(value) => ryeos_node_runtime::disclosure::shape_result(
            class,
            disclosure,
            value,
            disclosure.map_or_else(Default::default, |declared| declared.bounds()),
        )
        .map_err(|refusal| {
            tracing::warn!(
                code = refusal.code(),
                "service answer withheld from a non-operator caller"
            );
            anyhow::Error::new(HandlerError::Structured {
                code: refusal.code().to_owned(),
                status: 403,
                body: serde_json::json!({
                    "code": refusal.code(),
                    "error": refusal.to_string(),
                    "retryable": false,
                }),
            })
        })
        .and_then(|shaped| floor.withhold(shaped).map_err(withheld)),
        Err(error) => {
            if error
                .downcast_ref::<crate::dispatch_error::DispatchError>()
                .is_some()
            {
                // A dispatch failure keeps its own variant and text, so the
                // text passes the floor before it is returned.
                floor
                    .withhold(Value::String(format!("{error:#}")))
                    .map_err(withheld)?;
                return Err(error);
            }
            tracing::warn!(
                error = %format!("{error:#}"),
                "service failure withheld from a non-operator caller"
            );
            let shown = match ryeos_app::handler_error::extract_handler_error(&error) {
                Some(HandlerError::NotFound) => anyhow::Error::new(HandlerError::NotFound),
                // A message that begins with a stable `code:` keeps that
                // code and its status; the rest of the sentence is
                // withheld. A message with no code is replaced by a fixed
                // sentence of the same status.
                Some(HandlerError::Forbidden(message)) => coded_or_fixed(
                    &message,
                    403,
                    HandlerError::Forbidden("the service refused the request".to_owned()),
                ),
                Some(HandlerError::BadRequest(message)) => coded_or_fixed(
                    &message,
                    400,
                    HandlerError::BadRequest("the request was not valid".to_owned()),
                ),
                Some(HandlerError::Conflict(message)) => coded_or_fixed(
                    &message,
                    409,
                    HandlerError::Conflict("the request conflicts with current state".to_owned()),
                ),
                // A typed refusal keeps its code and status, which are
                // part of the contract a client branches on. Its body
                // can carry a path or a key, so only the stable code, the
                // retry flag and a fixed sentence reach a non-operator.
                Some(HandlerError::Structured { code, status, body }) => {
                    let body = redact_refusal_body(&body);
                    // The code is handler-authored text: it passes too.
                    floor
                        .withhold(serde_json::json!([&code, &body]))
                        .map_err(withheld)?;
                    anyhow::Error::new(HandlerError::Structured { body, code, status })
                }
                _ => {
                    let (code, sentence) =
                        ryeos_node_runtime::disclosure::redacted_failure(&format!("{error:#}"));
                    // The code is cut from the failure's own text.
                    floor
                        .withhold(Value::String(code.clone()))
                        .map_err(withheld)?;
                    anyhow::Error::new(HandlerError::Structured {
                        status: 500,
                        body: serde_json::json!({"error": {"code": code, "message": sentence}}),
                        code,
                    })
                }
            };
            // A code can be cut from the failure's own text, so what the
            // caller will see passes the floor in the end.
            floor
                .withhold(Value::String(format!("{shown:#}")))
                .map_err(withheld)?;
            Err(shown)
        }
    }
}

/// A refusal whose message begins with a stable `code:` keeps the code and its
/// status, with a fixed sentence; any other message becomes the fixed form.
fn coded_or_fixed(
    message: &str,
    status: u16,
    fixed: ryeos_app::handler_error::HandlerError,
) -> anyhow::Error {
    let code = message
        .split_once(':')
        .map(|(prefix, _)| prefix.trim())
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
    match code {
        Some(code) => anyhow::Error::new(ryeos_app::handler_error::HandlerError::Structured {
            code: code.to_owned(),
            status,
            body: serde_json::json!({
                "code": code,
                "error": "the service refused the request",
                "retryable": false,
            }),
        }),
        None => anyhow::Error::new(fixed),
    }
}

/// What a non-operator caller is told of a typed refusal's body: the same
/// shape the client already reads (`code`, `error_code`, a nested `error`
/// object, `retryable`, `outcome`), with every value reduced to a stable code,
/// a flag, an enum, or the fixed sentence. Free text, paths and detail fields
/// are dropped, so nothing the refusal carried can leak.
fn redact_refusal_body(body: &Value) -> Value {
    const FIXED: &str = "the service refused the request";
    let safe_code = |text: &str| {
        !text.is_empty()
            && text.len() <= 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    let Value::Object(map) = body else {
        return serde_json::json!({"error": FIXED});
    };
    let mut shown = serde_json::Map::new();
    for (key, value) in map {
        match (key.as_str(), value) {
            ("code" | "error_code", Value::String(text)) if safe_code(text) => {
                shown.insert(key.clone(), value.clone());
            }
            // A capability name (`ryeos.…`) is policy vocabulary the caller
            // needs to be told it lacks; only a plain one passes.
            ("capability", Value::String(text))
                if text.starts_with("ryeos.")
                    && text.len() <= 128
                    && text.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._:/-*".contains(&b)
                    }) =>
            {
                shown.insert(key.clone(), value.clone());
            }
            ("retryable", Value::Bool(_)) => {
                shown.insert(key.clone(), value.clone());
            }
            // `outcome` is a two-valued enum a client branches on ("unknown"
            // means do not resubmit a mutation that may have happened). It
            // passes only when it is exactly one of the two.
            ("outcome", Value::String(text)) if matches!(text.as_str(), "refused" | "unknown") => {
                shown.insert(key.clone(), value.clone());
            }
            ("error" | "message", Value::String(_)) => {
                shown.insert(key.clone(), Value::from(FIXED));
            }
            ("error", Value::Object(_)) => {
                shown.insert(key.clone(), redact_refusal_body(value));
            }
            _ => {}
        }
    }
    Value::Object(shown)
}

/// Build the exact daemon-owned terminal that the recorded service path will
/// commit. The handler's live result remains untouched for its caller; only
/// this durable terminal projection applies the frozen, kind-declared policy.
fn recorded_service_terminal_projection(
    thread_id: &str,
    dispatch_result: &Result<Value>,
    result_policy: &ryeos_engine::history_policy::ResolvedThreadResultPolicy,
) -> (
    ryeos_app::thread_lifecycle::ThreadFinalizeParams,
    Option<anyhow::Error>,
) {
    let (durable_result, durable_error, result_retention_error) = match dispatch_result {
        // An answer that is `null` (an acknowledgement a non-operator tier
        // leaves empty) is stored as no result, the way the thread store
        // reads it back.
        Ok(Value::Null) => (None, None, None),
        Ok(value) => {
            match ryeos_app::thread_lifecycle::retained_thread_result(value, result_policy) {
                Ok(value) => (Some(value), None, None),
                Err(error) => (
                    None,
                    None,
                    Some(recording_integrity(format!(
                        "apply admitted durable-result policy: {error:#}"
                    ))),
                ),
            }
        }
        Err(error) => {
            let live_error = match ryeos_app::handler_error::extract_handler_error(error) {
                Some(ryeos_app::handler_error::HandlerError::Structured { body, .. }) => body,
                _ => serde_json::json!({ "error": format!("{error:#}") }),
            };
            match ryeos_app::thread_lifecycle::retained_thread_error(&live_error, result_policy) {
                Ok(value) => (None, Some(value), None),
                Err(error) => (
                    None,
                    None,
                    Some(recording_integrity(format!(
                        "apply admitted durable-error policy: {error:#}"
                    ))),
                ),
            }
        }
    };

    let terminal = match (dispatch_result, &result_retention_error) {
        (Ok(_), None) => ryeos_app::thread_lifecycle::ThreadFinalizeParams {
            thread_id: thread_id.to_owned(),
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result: durable_result,
            error: None,
            metadata: None,
            artifacts: Vec::new(),
            final_cost: None,
            summary_json: None,
        },
        (Ok(_), Some(error)) => ryeos_app::thread_lifecycle::ThreadFinalizeParams {
            thread_id: thread_id.to_owned(),
            status: "failed".to_string(),
            outcome_code: Some("result_retention_failed".to_string()),
            result: None,
            error: Some(serde_json::json!({ "error": error.to_string() })),
            metadata: None,
            artifacts: Vec::new(),
            final_cost: None,
            summary_json: None,
        },
        (Err(_), _) => ryeos_app::thread_lifecycle::ThreadFinalizeParams {
            thread_id: thread_id.to_owned(),
            status: "failed".to_string(),
            outcome_code: Some("handler_error".to_string()),
            result: None,
            error: durable_error.or_else(|| {
                Some(serde_json::json!({
                    "error": "durable error retention failed"
                }))
            }),
            metadata: None,
            artifacts: Vec::new(),
            final_cost: None,
            summary_json: None,
        },
    };
    (terminal, result_retention_error)
}

/// Commit and confirm a recorded service outcome before allowing the handler
/// result to escape. A failed write acknowledgement is not decisive because
/// the write may have committed; exact readback after each bounded attempt
/// makes retry safe without replaying the handler.
fn finalize_recorded_service_exact(
    state: &AppState,
    owner: &ryeos_app::state_store::InProcessHandlerControl,
    chain_root_id: &str,
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
            Err(error) => (format!("terminal write failed: {error:#}"), false),
        };
        match confirm_recorded_service_terminal(state, chain_root_id, params) {
            Ok(()) => {
                if !postcommit_complete {
                    let repair_result = state
                        .threads
                        .repair_recorded_service_terminal_postcommit(params)
                        .context("repair exact recorded-service terminal postcommit");
                    if let Err(postcommit_error) = repair_result {
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
    let canonical = CanonicalRef::parse(service_ref).map_err(|error| {
        anyhow::anyhow!("invalid service-registry ref `{service_ref}`: {error}")
    })?;
    let schema = ctx.engine.kinds.get(&canonical.kind).ok_or_else(|| {
        anyhow::anyhow!("service-registry ref `{service_ref}` has no admitted kind schema")
    })?;
    let admitted = schema.execution().is_some_and(|execution| {
        matches!(
            execution.terminator.as_ref(),
            Some(ryeos_engine::kind_registry::TerminatorDecl::InProcess {
                registry: ryeos_engine::kind_registry::InProcessRegistryKind::Services,
                ..
            })
        )
    });
    anyhow::ensure!(
        admitted,
        "kind `{}` is not admitted to the Services in-process registry",
        canonical.kind
    );
    let verified = resolve_and_verify(
        &ctx.engine,
        &ctx.plan_ctx,
        service_ref,
        Some(&canonical.kind),
    )?;
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
/// `admitted_root`: the exact synchronous root admission produced by generic
/// dispatch preflight. When the service is recorded, the in-process terminator
/// verifies and persists that admission; it never recompiles authority from
/// mutable state. Authored hot-read services do not create a thread and
/// therefore do not consume a root admission.
///
/// `local_handler_context` is trusted out-of-band context supplied by a local
/// transport. Session-local services require it. Ordinary verified services
/// may receive it only when its fingerprint/scopes exactly match `ctx`; this
/// preserves transport-authenticated properties such as `verified: false`
/// without permitting an identity or capability override.
// Verified subject, execution mode/context, pre-minted identity, and trusted
// local handler context remain explicit at the service execution boundary.
pub async fn execute_service_verified(
    verified: ryeos_engine::contracts::VerifiedItem,
    service_ref: &str,
    params: Value,
    mode: ExecutionMode,
    ctx: &ExecutionContext,
    state: &AppState,
    recording: ServiceRecordingContext<'_>,
    admitted_root: Option<ryeos_app::thread_lifecycle::RootExecutionAdmission>,
    pre_minted_thread_id: Option<&str>,
    local_handler_context: Option<ryeos_app::handler_context::HandlerContext>,
) -> Result<ServiceExecutionResult> {
    // Every in-process entry point (generic dispatch, service routes, UI
    // invocations, the local socket) runs through here, so the item is
    // refused here before anything is recorded or the handler is reached.
    crate::dispatch::refuse_in_process_node_requirements(&verified.resolved, service_ref)?;
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
    // An inline callback child is recorded in its parent's chain. The parent
    // is named only by the callback authority source and only together with
    // the pre-minted child identity that the callback reserved.
    let inline_parent_thread_id = match (&recording.authority_source, pre_minted_thread_id) {
        (
            ServiceRecordingAuthoritySource::InlineCallbackChild {
                parent_thread_id, ..
            },
            Some(_),
        ) => Some(*parent_thread_id),
        (ServiceRecordingAuthoritySource::InlineCallbackChild { .. }, None) => {
            bail!("inline callback child service execution requires its pre-minted child identity");
        }
        (
            ServiceRecordingAuthoritySource::Execution { .. }
            | ServiceRecordingAuthoritySource::ExplicitProjectless
            | ServiceRecordingAuthoritySource::UnrecordedOnly,
            _,
        ) => None,
    };

    // 3. Admit the item: its space and trust class must be admitted for its
    // kind, and it must name exactly its compiled handler's endpoint and caps.
    // The endpoint run and the caps checked are the compiled descriptor's.
    let crate::execution::launch_trust_gate::AdmittedService {
        endpoint,
        required_caps,
    } = crate::execution::launch_trust_gate::admit_verified_service(
        &verified,
        &ctx.engine.kinds,
        &state.node_policy,
        state.service_descriptors,
    )?;
    let authored_record_thread =
        ryeos_app::service_registry::extract_record_thread(&verified.resolved.metadata.extra)?;
    // A caller that pre-minted an externally subscribed ID has promised a
    // durable stream. Preserve that promise regardless of the service's normal
    // high-frequency read policy.
    let record_thread = authored_record_thread || pre_minted_thread_id.is_some();

    // 4. Availability check
    let avail = availability_for_endpoint(state.service_descriptors, &endpoint)
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    match (mode, avail) {
        (ExecutionMode::Standalone, ServiceAvailability::DaemonOnly) => {
            bail!("{service_ref} is DaemonOnly; start the daemon and call /execute");
        }
        (ExecutionMode::Live, ServiceAvailability::StoppedNodeOnly) => {
            bail!(
                "{service_ref} requires stopped-node authority; engine reload is unavailable; \
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

    // 5b. Disclosure: a caller other than the operator receives only the
    // tiers the service item declares. Parsed here so a malformed block
    // refuses the call before anything is recorded.
    let disclosure_class = ctx.disclosure_class;
    let disclosure = if disclosure_class.is_operator() {
        None
    } else {
        verified
            .resolved
            .metadata
            .extra
            .get("disclosure")
            .map(ryeos_node_runtime::disclosure::ServiceDisclosure::parse)
            .transpose()?
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

    // A preflighted runtime action carries the exact root admission that
    // minted its child identity. Consume that authority directly instead of
    // resolving mutable project/bundle state again at the in-process
    // terminator. Other service surfaces still compile their admission here.
    let recorded_root_admission = if !record_thread {
        None
    } else if let Some(root_admission) = admitted_root {
        root_admission.ensure_matches_plan_context(&ctx.engine, &ctx.plan_ctx)?;
        root_admission.ensure_matches_subject(&ctx.engine, &verified, &thread_profile)?;
        root_admission.ensure_matches_usage_attribution(
            recording.usage_subject,
            recording.usage_subject_asserted_by,
        )?;
        match &recording.authority_source {
            ServiceRecordingAuthoritySource::Execution { provenance }
            | ServiceRecordingAuthoritySource::InlineCallbackChild { provenance, .. } => {
                root_admission.ensure_matches_provenance(provenance)?;
            }
            ServiceRecordingAuthoritySource::ExplicitProjectless => {
                if !matches!(
                    &ctx.plan_ctx.project_context,
                    ryeos_engine::contracts::ProjectContext::None
                ) {
                    bail!("explicit projectless service authority requires no project context");
                }
            }
            ServiceRecordingAuthoritySource::UnrecordedOnly => {
                return Err(recording_integrity(format!(
                    "caller asserted unrecorded-only execution for admitted service root `{service_ref}`"
                )));
            }
        }
        Some(root_admission)
    } else {
        // Decide recording before deriving persistence-only project authority.
        // `UnrecordedOnly` is an assertion by the caller, not a way to
        // override a verified service's recording contract.
        let project_binding = match (&recording.authority_source, record_thread) {
            (
                ServiceRecordingAuthoritySource::Execution { provenance }
                | ServiceRecordingAuthoritySource::InlineCallbackChild { provenance, .. },
                true,
            ) => Some(
                ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                    &ctx.engine,
                    &ctx.plan_ctx,
                    provenance,
                )?,
            ),
            (
                ServiceRecordingAuthoritySource::Execution { .. }
                | ServiceRecordingAuthoritySource::InlineCallbackChild { .. },
                false,
            ) => None,
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
        match project_binding {
            Some(project_binding) => {
                let admitted_request_snapshot =
                    project_binding.admit_request_authority_snapshot(&ctx.engine, &ctx.plan_ctx)?;
                let verified_attestation = match admitted_request_snapshot.as_ref() {
                    Some(authority) => ctx.engine.resolve_attested_under_admitted_authority(
                        &ctx.plan_ctx,
                        &verified.resolved.canonical_ref,
                        project_binding.execution_workspace().ok_or_else(|| {
                            anyhow::anyhow!(
                                "content-addressed recorded service has no execution workspace"
                            )
                        })?,
                        authority,
                    ),
                    None => ctx
                        .engine
                        .verify_attested(&ctx.plan_ctx, verified.resolved.clone()),
                }
                .map_err(|error| {
                    anyhow::anyhow!("attest recorded service subject before persistence: {error}")
                })?;
                Some(ryeos_app::thread_lifecycle::admit_verified_root_execution(
                    &ctx.engine,
                    &ctx.plan_ctx,
                    &ctx.plan_ctx,
                    project_binding,
                    verified_attestation,
                    state.node_history_policy()?,
                    thread_profile,
                    std::collections::BTreeMap::new(),
                    Vec::new(),
                    recording.usage_subject.cloned(),
                    recording.usage_subject_asserted_by.map(str::to_owned),
                )?)
            }
            None => None,
        }
    };

    let dispatch_result = if let Some(root_admission) = recorded_root_admission {
        let result_policy = root_admission.resolved_result_policy().clone();
        let recorded_admission = ryeos_app::thread_lifecycle::RecordedServiceAdmission::new(
            root_admission,
            endpoint.clone(),
            &params,
        )?;
        // An inline in-process child is born in its parent's chain; a direct
        // invocation is its own chain root. The parent row is read before
        // owner registration so the guard knows the chain it must settle in.
        // A parent already terminal is refused here, before any registration
        // or write; the store's birth repeats the check under its locks.
        let inline_parent = inline_parent_thread_id
            .map(|parent_thread_id| {
                let parent = state.threads.get_thread(parent_thread_id)?.ok_or_else(|| {
                    anyhow::anyhow!("inline callback parent thread not found: {parent_thread_id}")
                })?;
                if ryeos_app::state_store::is_terminal_status(&parent.status) {
                    bail!(
                        "refusing inline service child {invocation_id}: parent {parent_thread_id} is already {}",
                        parent.status
                    );
                }
                Ok(parent)
            })
            .transpose()?;
        let chain_root_id = inline_parent.as_ref().map_or_else(
            || invocation_id.clone(),
            |parent| parent.chain_root_id.clone(),
        );
        // A service called from inside an execution records, on every launch
        // its handler makes, the chain that execution originated from: the
        // callback parent's own sealed origin when it has one, else the
        // parent's chain. A direct invocation was made by no execution and
        // records none.
        let service_launch_origin = inline_parent
            .as_ref()
            .map(|parent| -> Result<String> {
                let sealed_origin = state
                    .state_store
                    .get_launch_metadata(&parent.thread_id)?
                    .and_then(|metadata| metadata.resume_context)
                    .and_then(|resume| resume.evaluator_origin_chain);
                Ok(ryeos_app::launch_metadata::child_launch_origin_chain(
                    sealed_origin.as_deref(),
                    &parent.chain_root_id,
                ))
            })
            .transpose()?;
        // Registration precedes publication, while launch metadata and the
        // created→running transition commit atomically with the birth.
        let mut lifecycle_guard =
            RecordedServiceTerminalGuard::registered(state, &invocation_id, &chain_root_id)?;
        // Arm the owner-qualified fallback before durable publication. If any
        // post-commit step unwinds or returns an error, the exact registered
        // owner remains responsible for terminal settlement.
        lifecycle_guard.arm_terminal();
        let launch_metadata = ryeos_app::launch_metadata::RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            );
        match inline_parent.as_ref() {
            Some(parent) => {
                let inherited_stop = state.threads.create_recorded_service_child(
                    parent,
                    &invocation_id,
                    &recorded_admission,
                    &launch_metadata,
                    lifecycle_guard.owner(),
                )?;
                if let Some(intent) = inherited_stop {
                    // The parent's durable stop reached the newborn child
                    // before its handler ran. Settle the child to the exact
                    // outcome that durable stop dominance commits, then refuse
                    // the launch; the handler never starts.
                    let terminal = inherited_stop_terminal(&invocation_id, intent);
                    lifecycle_guard.record_completed_handler_terminal(&terminal);
                    finalize_recorded_service_exact(
                        state,
                        lifecycle_guard.owner(),
                        lifecycle_guard.chain_root_id(),
                        &terminal,
                    )?;
                    lifecycle_guard.disarm_terminal();
                    bail!(
                        "parent {} was stop-requested before inline service child {invocation_id} launched",
                        parent.thread_id
                    );
                }
            }
            None => state.threads.create_recorded_service_root(
                &invocation_id,
                &recorded_admission,
                &launch_metadata,
                lifecycle_guard.owner(),
            )?,
        }

        // The handler and terminal commit are daemon-owned once the running
        // root exists. Dropping a request future detaches this task instead of
        // cancelling it and abandoning a running audit row.
        let task_state = state.clone();
        let task_endpoint = endpoint.clone();
        let task_service_ref = service_ref.to_string();
        let task_invocation_id = invocation_id.clone();
        let task_launch_origin = service_launch_origin;
        let task_principal = ctx.principal_fingerprint.clone();
        let task_scopes = ctx.caller_scopes.clone();
        let task_current_site_id = ctx.plan_ctx.current_site_id.clone();
        let task_origin_site_id = ctx.plan_ctx.origin_site_id.clone();
        let task_metadata = verified.resolved.metadata.extra.clone();
        let task_disclosure = disclosure.clone();
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
                )?
                .with_recorded_service_root_id(task_invocation_id.clone())?
                .with_launch_origin_chain(task_launch_origin);
                let handler = task_state
                    .services
                    .get(&task_endpoint)
                    .ok_or_else(|| {
                        anyhow::anyhow!("service handler '{}' not registered", task_endpoint)
                    })?
                    .clone();
                let scope = hctx.task_scope().cloned();
                let call = handler(task_params.clone(), hctx, Arc::new(task_state.clone()));
                let result = match scope {
                    Some(scope) => scope.enter(call).await,
                    None => call.await,
                };
                drop(handler);
                result
            }
            .await;
            // Shaped before it is recorded, so the durable result and the
            // caller's answer are the same disclosed value.
            let dispatch_result = disclose_node_service_result(
                &task_state,
                disclosure_class,
                task_disclosure.as_ref(),
                dispatch_result,
            );

            let (terminal, result_retention_error) = recorded_service_terminal_projection(
                &task_invocation_id,
                &dispatch_result,
                &result_policy,
            );
            lifecycle_guard.record_completed_handler_terminal(&terminal);
            let terminal_confirmation = finalize_recorded_service_exact(
                &task_state,
                lifecycle_guard.owner(),
                lifecycle_guard.chain_root_id(),
                &terminal,
            );
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
                        Err(error) => Some(format!("{error:#}")),
                        Ok(_) => None,
                    },
                    uid: standalone_audit::current_uid(),
                    pid: std::process::id(),
                    requested_by: "local-operator",
                    params_hash: standalone_audit::params_hash(&task_params),
                };
                if let Err(error) = standalone_audit::write_audit_record(&audit_path, &record) {
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        path = %audit_path.display(),
                        "failed to write standalone audit record"
                    );
                }
            }

            terminal_confirmation?;
            if let Some(error) = result_retention_error {
                return Err(error);
            }
            dispatch_result
        });

        let joined = task.await;
        match joined {
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
        let result = handler(params, hctx, Arc::new(state.clone())).await;
        drop(handler);
        disclose_node_service_result(&state, disclosure_class, disclosure.as_ref(), result)
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
            // The handler's own refusal, in its own words. The capability
            // check above already passed, so this is never reported as a
            // missing capability.
            Some(HandlerError::Forbidden(reason)) => {
                crate::dispatch_error::DispatchError::ServiceRefused {
                    service_ref: service_ref.to_string(),
                    reason,
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

    fn no_floor() -> ryeos_app::secret_floor::SecretFloor {
        ryeos_app::secret_floor::SecretFloor::from_values(vec![])
    }

    fn principal_tier(pointers: &[&str]) -> ryeos_node_runtime::disclosure::ServiceDisclosure {
        ryeos_node_runtime::disclosure::ServiceDisclosure::parse(&serde_json::json!({
            "principal": pointers,
        }))
        .unwrap()
    }

    #[test]
    fn a_principal_receives_the_tier_and_never_an_unlisted_field() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let tier = principal_tier(&["/id", "/items/*/name"]);
        let answer = serde_json::json!({
            "id": "x",
            "path": "/home/someone/secret",
            "items": [{"name": "a", "path": "/etc/passwd"}],
            "added_later": 1,
        });
        let shaped = disclose_service_result(
            DisclosureClass::Principal,
            Some(&tier),
            Ok(answer.clone()),
            &no_floor(),
        )
        .unwrap();
        assert_eq!(
            shaped,
            serde_json::json!({"id": "x", "items": [{"name": "a"}]})
        );
        let operator = disclose_service_result(
            DisclosureClass::Operator,
            Some(&tier),
            Ok(answer.clone()),
            &no_floor(),
        )
        .unwrap();
        assert_eq!(operator, answer);
    }

    #[test]
    fn a_service_without_a_block_is_refused_to_a_principal_but_not_the_operator() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let answer = serde_json::json!({"id": "x"});
        let refused = disclose_service_result(
            DisclosureClass::Principal,
            None,
            Ok(answer.clone()),
            &no_floor(),
        )
        .unwrap_err();
        // The refusal is typed: a 403 that keeps its stable `read_*` code.
        assert!(matches!(
            ryeos_app::handler_error::extract_handler_error(&refused),
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status: 403, .. })
                if code == "read_not_disclosable"
        ));
        assert_eq!(
            disclose_service_result(
                DisclosureClass::Operator,
                None,
                Ok(answer.clone()),
                &no_floor()
            )
            .unwrap(),
            answer
        );
    }

    #[test]
    fn a_typed_refusal_keeps_its_code_and_status_for_a_principal_without_its_body() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let refused = disclose_service_result(
            DisclosureClass::Principal,
            None,
            Err(anyhow::Error::new(
                ryeos_app::handler_error::HandlerError::Structured {
                    code: "ui_binding_stale".to_owned(),
                    status: 409,
                    body: serde_json::json!({
                        "code": "ui_binding_stale",
                        "error": "item /home/someone/x changed",
                        "retryable": false,
                    }),
                },
            )),
            &no_floor(),
        )
        .unwrap_err();
        match ryeos_app::handler_error::extract_handler_error(&refused) {
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) => {
                assert_eq!((code.as_str(), status), ("ui_binding_stale", 409));
                assert!(!body.to_string().contains("/home/someone"), "{body}");
                assert_eq!(body["retryable"], false);
            }
            other => panic!("typed refusal lost: {other:?}"),
        }
    }

    #[test]
    fn a_message_with_a_stable_code_keeps_the_code_and_a_bare_one_the_status() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let refused = |error: ryeos_app::handler_error::HandlerError| {
            disclose_service_result(
                DisclosureClass::Principal,
                None,
                Err(anyhow::Error::new(error)),
                &no_floor(),
            )
            .unwrap_err()
        };
        let coded = refused(ryeos_app::handler_error::HandlerError::Forbidden(
            "records_unverified_requires_capability: ask with the capability /srv/x".to_owned(),
        ));
        match ryeos_app::handler_error::extract_handler_error(&coded) {
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) => {
                assert_eq!(
                    (code.as_str(), status),
                    ("records_unverified_requires_capability", 403)
                );
                assert!(!body.to_string().contains("/srv/x"), "{body}");
            }
            other => panic!("code lost: {other:?}"),
        }
        let bare = refused(ryeos_app::handler_error::HandlerError::Conflict(
            "head moved on /srv/x".to_owned(),
        ));
        assert!(matches!(
            ryeos_app::handler_error::extract_handler_error(&bare),
            Some(ryeos_app::handler_error::HandlerError::Conflict(text)) if !text.contains("/srv/x")
        ));
    }

    #[test]
    fn a_redacted_refusal_keeps_the_wire_shape_the_client_reads() {
        let shown = redact_refusal_body(&serde_json::json!({
            "error_code": "grant_renewal_not_permitted",
            "error": {"code": "grant_renewal_not_permitted", "message": "granter fp:abc at /home/x"},
            "retryable": false,
            "hint": "/home/x",
            "details": {"path": "/home/x"},
        }));
        assert_eq!(
            shown,
            serde_json::json!({
                "error_code": "grant_renewal_not_permitted",
                "error": {
                    "code": "grant_renewal_not_permitted",
                    "message": "the service refused the request"
                },
                "retryable": false,
            })
        );
        // A code that is not a plain code never reaches the caller.
        let odd = redact_refusal_body(&serde_json::json!({"code": "has /path in it"}));
        assert_eq!(odd, serde_json::json!({}));
    }

    #[test]
    fn a_refusal_outcome_passes_only_as_its_two_enum_values() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let shown = |outcome: serde_json::Value| {
            let refused = disclose_service_result(
                DisclosureClass::Principal,
                None,
                Err(anyhow::Error::new(
                    ryeos_app::handler_error::HandlerError::Structured {
                        code: "write_unconfirmed".to_owned(),
                        status: 502,
                        body: serde_json::json!({"outcome": outcome, "detail": "/srv/private"}),
                    },
                )),
                &no_floor(),
            )
            .unwrap_err();
            match ryeos_app::handler_error::extract_handler_error(&refused) {
                Some(ryeos_app::handler_error::HandlerError::Structured { body, .. }) => body,
                other => panic!("typed refusal lost: {other:?}"),
            }
        };
        assert_eq!(shown(serde_json::json!("unknown"))["outcome"], "unknown");
        assert_eq!(shown(serde_json::json!("refused"))["outcome"], "refused");
        for other in [
            serde_json::json!("failed"),
            serde_json::json!("/srv/private/path"),
            serde_json::json!(7),
            serde_json::json!({"a": "unknown"}),
        ] {
            let body = shown(other);
            assert!(body.get("outcome").is_none(), "{body}");
            assert!(!body.to_string().contains("/srv/private"), "{body}");
        }
    }

    #[test]
    fn a_failure_reaches_a_principal_as_a_fixed_sentence_without_its_text() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        let tier = principal_tier(&["/id"]);
        let failed = disclose_service_result(
            DisclosureClass::Principal,
            Some(&tier),
            Err(anyhow::anyhow!(
                "read_failed: could not read /home/someone/secret"
            )),
            &no_floor(),
        )
        .unwrap_err();
        let shown = format!("{failed:#}");
        assert!(!shown.contains("/home/someone"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn a_principal_answer_holding_a_node_secret_is_refused_without_it() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        const SECRET: &str = "sk-live-0123456789";
        let floor = ryeos_app::secret_floor::SecretFloor::from_values(vec![SECRET.to_owned()]);
        let tier = principal_tier(&["/id", "/note"]);
        let answer = serde_json::json!({"id": "x", "note": format!("token {SECRET}")});
        let refused = disclose_service_result(
            DisclosureClass::Principal,
            Some(&tier),
            Ok(answer.clone()),
            &floor,
        )
        .unwrap_err();
        assert!(matches!(
            ryeos_app::handler_error::extract_handler_error(&refused),
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status: 403, .. })
                if code == ryeos_app::secret_floor::SECRET_FLOOR_WITHHELD
        ));
        assert!(!format!("{refused:#}").contains(SECRET));
        assert!(!format!("{refused:?}").contains(SECRET));
        // The operator receives the same answer unchanged.
        assert_eq!(
            disclose_service_result(
                DisclosureClass::Operator,
                Some(&tier),
                Ok(answer.clone()),
                &floor
            )
            .unwrap(),
            answer
        );
        // A principal answer without the secret is unchanged.
        let clean = serde_json::json!({"id": "x", "note": "fine"});
        assert_eq!(
            disclose_service_result(
                DisclosureClass::Principal,
                Some(&tier),
                Ok(clean.clone()),
                &floor
            )
            .unwrap(),
            clean
        );
    }

    #[test]
    fn a_secret_only_in_a_projected_away_field_does_not_refuse() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        const SECRET: &str = "sk-live-0123456789";
        let floor = ryeos_app::secret_floor::SecretFloor::from_values(vec![SECRET.to_owned()]);
        let tier = principal_tier(&["/id"]);
        let answer = serde_json::json!({"id": "x", "hidden": SECRET});
        assert_eq!(
            disclose_service_result(DisclosureClass::Principal, Some(&tier), Ok(answer), &floor)
                .unwrap(),
            serde_json::json!({"id": "x"})
        );
    }

    #[test]
    fn a_dispatch_failure_text_holding_a_node_secret_is_refused_to_a_principal() {
        use ryeos_node_runtime::disclosure::DisclosureClass;
        const SECRET: &str = "sk-live-0123456789";
        let floor = ryeos_app::secret_floor::SecretFloor::from_values(vec![SECRET.to_owned()]);
        let failed = disclose_service_result(
            DisclosureClass::Principal,
            None,
            Err(anyhow::Error::new(
                crate::dispatch_error::DispatchError::MethodInvalidArg {
                    method: "m".to_owned(),
                    reason: format!("bad {SECRET}"),
                },
            )),
            &floor,
        )
        .unwrap_err();
        assert!(!format!("{failed:#}").contains(SECRET));
        assert!(matches!(
            ryeos_app::handler_error::extract_handler_error(&failed),
            Some(ryeos_app::handler_error::HandlerError::Structured { code, .. })
                if code == ryeos_app::secret_floor::SECRET_FLOOR_WITHHELD
        ));
    }

    #[test]
    fn service_invocation_identity_is_a_distinct_strict_coordinate() {
        let minted = mint_service_invocation_id();
        validate_service_invocation_id(&minted).unwrap();
        for invalid in [
            "T-runtime-thread",
            "svc-short-deadbeef",
            "svc-1787790000000-DEADBEEF",
            "svc-1787790000000-deadbee",
            "svc-1787790000000-deadbeef-extra",
        ] {
            assert!(
                validate_service_invocation_id(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn inherited_stop_terminal_is_the_durable_stop_outcome() {
        for (intent, status) in [
            (ryeos_app::state_store::StopIntent::Cancel, "cancelled"),
            (ryeos_app::state_store::StopIntent::Kill, "killed"),
        ] {
            let terminal = inherited_stop_terminal("T-inline-child", intent);
            assert_eq!(terminal.thread_id, "T-inline-child");
            assert_eq!(terminal.status, status);
            assert_eq!(terminal.outcome_code.as_deref(), Some(status));
            assert!(terminal.result.is_none());
            assert_eq!(
                terminal.error,
                Some(serde_json::json!({
                    "reason": "durable_stop_intent",
                    "intent": intent.as_str(),
                }))
            );
            assert!(terminal.metadata.is_none());
            assert!(terminal.artifacts.is_empty());
            assert!(terminal.final_cost.is_none());
            assert!(terminal.summary_json.is_none());
        }
    }

    fn digest_only_result_policy() -> ryeos_engine::history_policy::ResolvedThreadResultPolicy {
        ryeos_engine::history_policy::ResolvedThreadResultPolicy {
            retention: ryeos_engine::history_policy::ThreadResultRetention::DigestOnly,
            canonical_item_ref: "service:test/sensitive".to_string(),
            item_content_hash: "1".repeat(64),
            item_signer_fingerprint: Some("2".repeat(64)),
            item_trust_class: ryeos_engine::contracts::TrustClass::Trusted,
            kind_schema_content_hash: "3".repeat(64),
            source: ryeos_engine::history_policy::ResultPolicyProvenance::ItemAuthored {
                composed_path: "result_retention".to_string(),
                effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            },
        }
    }

    #[test]
    fn recorded_service_terminal_applies_digest_only_to_success_and_failure() {
        let policy = digest_only_result_policy();
        let live_success = serde_json::json!({
            "userCode":"SERVICE-SUCCESS-SECRET",
            "verificationUri":"https://example.invalid"
        });
        let success: Result<Value> = Ok(live_success.clone());
        let (terminal, retention_error) =
            recorded_service_terminal_projection("thread:test", &success, &policy);
        assert!(retention_error.is_none());
        assert_eq!(terminal.status, "completed");
        assert_eq!(
            terminal.result.as_ref().unwrap()["kind"],
            "ryeos.digest_only_result"
        );
        assert!(
            !serde_json::to_string(&terminal.result)
                .unwrap()
                .contains("SERVICE-SUCCESS-SECRET")
        );
        assert!(
            matches!(&success, Ok(value) if *value == live_success),
            "live result is unchanged"
        );

        let failure: Result<Value> = Err(anyhow::anyhow!("SERVICE-FAILURE-SECRET"));
        let (terminal, retention_error) =
            recorded_service_terminal_projection("thread:test", &failure, &policy);
        assert!(retention_error.is_none());
        assert_eq!(terminal.status, "failed");
        assert_eq!(
            terminal.error.as_ref().unwrap()["kind"],
            "ryeos.digest_only_error"
        );
        assert!(
            !serde_json::to_string(&terminal.error)
                .unwrap()
                .contains("SERVICE-FAILURE-SECRET")
        );
        assert!(
            matches!(&failure, Err(error) if error.to_string().contains("SERVICE-FAILURE-SECRET"))
        );
    }

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
        assert!(
            error
                .to_string()
                .contains("requires a trusted local handler context")
        );
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
    fn verified_dispatch_preserves_the_supplied_authorized_key_class() {
        let metadata = HashMap::from([("ui_dispatch".to_string(), serde_json::json!("verified"))]);
        let supplied = ryeos_app::handler_context::HandlerContext::new_with_authority(
            "fp:operator".to_string(),
            vec!["cap:verified".to_string()],
            true,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemotePrincipal),
            Some("site:source".to_string()),
        );
        let selected = select_service_handler_context(
            &metadata,
            Some(supplied),
            "fp:operator",
            &["cap:verified".to_string()],
            "site:target",
            "site:source",
        )
        .unwrap();
        assert_eq!(
            selected.authorized_key_class,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemotePrincipal)
        );
        assert_eq!(
            selected.authenticated_origin_site_id.as_deref(),
            Some("site:source")
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
        assert!(
            error
                .to_string()
                .contains("differs from sealed execution authority")
        );
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

    #[test]
    fn verified_service_without_ingress_context_remains_unverified() {
        let selected = select_service_handler_context(
            &HashMap::new(),
            None,
            "machine:scheduled",
            &["service.read".to_string()],
            "site:local",
            "site:local",
        )
        .expect("node-internal service context");
        assert_eq!(selected.fingerprint, "machine:scheduled");
        assert_eq!(selected.scopes, vec!["service.read"]);
        assert!(!selected.verified);
        assert!(selected.authorized_key_class.is_none());
        assert!(selected.authenticated_origin_site_id.is_none());
    }
}
