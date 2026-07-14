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

use anyhow::{bail, Result};
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

/// Resolve and verify any item ref (kind-agnostic).
///
/// Steps:
/// 1. Parse the ref string into a `CanonicalRef`.
/// 2. Resolve through the engine.
/// 3. Verify trust chain (signature + content hash).
///
/// Error wording is keyed off `ref_kind_label`: `None` produces neutral
/// "ref '<...>' ..." messages; `Some("service")` produces the original
/// service-flavored "service '<...>' ..." wording so existing pin tests
/// and callers see no diff.
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
) -> Result<ServiceExecutionResult> {
    let verified = resolve_and_verify(&ctx.engine, &ctx.plan_ctx, service_ref, Some("service"))?;
    execute_service_verified(verified, service_ref, params, mode, ctx, state, None).await
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
pub async fn execute_service_verified(
    verified: ryeos_engine::contracts::VerifiedItem,
    service_ref: &str,
    params: Value,
    mode: ExecutionMode,
    ctx: &ExecutionContext,
    state: &AppState,
    pre_minted_thread_id: Option<&str>,
) -> Result<ServiceExecutionResult> {
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
                bail!(
                    "insufficient capabilities: required {:?}, caller has {:?}",
                    required_caps,
                    ctx.caller_scopes
                );
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
        None => format!(
            "svc-{}-{:08x}",
            lillux::time::timestamp_millis(),
            rand::random::<u32>()
        ),
    };

    let create_params = ryeos_app::thread_lifecycle::ThreadCreateParams {
        thread_id: invocation_id.clone(),
        chain_root_id: invocation_id.clone(),
        kind: "service_run".to_string(),
        item_ref: service_ref.to_string(),
        executor_ref: endpoint.clone(),
        launch_mode: "inline".to_string(),
        current_site_id: ctx.plan_ctx.current_site_id.clone(),
        origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
        upstream_thread_id: None,
        requested_by: Some(ctx.principal_fingerprint.clone()),
        project_root: match &ctx.plan_ctx.project_context {
            ryeos_engine::contracts::ProjectContext::LocalPath { path } => {
                Some(path.canonicalize().unwrap_or_else(|_| path.clone()))
            }
            _ => None,
        },
        usage_subject: None,
        usage_subject_asserted_by: None,
    };

    let audit_ok = record_thread && state.threads.create_thread(&create_params).is_ok();
    if audit_ok {
        let _ = state.threads.mark_running(&invocation_id);
    }

    // 6. Inject typed handler context for service handlers.
    //    Handlers opt in via `_ctx: HandlerContext` with `#[serde(default)]`.
    //    For executor-sourced requests (CLI/UDS), we inject here;
    //    for route-sourced requests, service_invocation.rs does it.
    //    Both live and standalone modes inject verified=true:
    //    - live: cap enforcement already passed (step 5)
    //    - standalone: operator authority from filesystem

    let hctx = ryeos_app::handler_context::HandlerContext::new(
        ctx.principal_fingerprint.clone(),
        ctx.caller_scopes.clone(),
        true,
    );

    // 7. Dispatch to handler
    let handler = state
        .services
        .get(&endpoint)
        .ok_or_else(|| anyhow::anyhow!("service handler '{}' not registered", endpoint))?
        .clone();

    let state_arc = Arc::new(state.clone());
    let dispatch_result = handler(params.clone(), hctx, state_arc).await;

    // 7b. Finalize audit with success or failure
    if audit_ok {
        match &dispatch_result {
            Ok(value) => {
                let _ = state.threads.finalize_thread(
                    &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                        thread_id: invocation_id.clone(),
                        status: "completed".to_string(),
                        outcome_code: Some("success".to_string()),
                        result: Some(value.clone()),
                        error: None,
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    },
                );
            }
            Err(e) => {
                let _ = state.threads.finalize_thread(
                    &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                        thread_id: invocation_id.clone(),
                        status: "failed".to_string(),
                        outcome_code: Some("handler_error".to_string()),
                        result: None,
                        error: Some(serde_json::json!({ "error": e.to_string() })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    },
                );
            }
        }
    }

    // 7c. Standalone NDJSON audit (only in Standalone mode, not projection-backed)
    if mode == ExecutionMode::Standalone && record_thread {
        let audit_path = standalone_audit::default_audit_path(&state.config.app_root);
        let record = standalone_audit::StandaloneAuditRecord {
            ts: lillux::time::iso8601_now(),
            mode: "standalone",
            service_ref: service_ref.to_string(),
            endpoint: endpoint.clone(),
            status: match &dispatch_result {
                Ok(_) => "success",
                Err(_) => "failure",
            },
            error_message: match &dispatch_result {
                Err(e) => Some(e.to_string()),
                Ok(_) => None,
            },
            uid: standalone_audit::current_uid(),
            pid: std::process::id(),
            requested_by: "local-operator",
            params_hash: standalone_audit::params_hash(&params),
        };
        if let Err(e) = standalone_audit::write_audit_record(&audit_path, &record) {
            tracing::warn!(error = %e, path = %audit_path.display(), "failed to write standalone audit record");
        }
    }

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
            _ => crate::dispatch_error::DispatchError::Internal(e),
        }
    })?;

    Ok(ServiceExecutionResult {
        value,
        endpoint,
        trust_class,
        effective_caps,
        invocation_id,
        recorded: audit_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_unknown_is_error() {
        // Empty descriptor table — any endpoint is "unknown".
        assert!(availability_for_endpoint(&[], "future.service").is_err());
        assert!(availability_for_endpoint(&[], "nonexistent").is_err());
    }
}
