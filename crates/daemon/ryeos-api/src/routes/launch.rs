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
//! consumes a parsed ref string and lets `dispatch::dispatch`
//! reject non-root-executable kinds with a typed error.
//!
//! Consumers today:
//!   - [`crate::routes::response_modes::event_stream_mode`] — SSE
//!     subscriber tails events for the minted thread (one-call
//!     fire-and-observe pattern used by `POST /execute/stream`).
//!   - [`crate::routes::response_modes::launch_mode`] — unary 202
//!     Accepted ack used by webhook routes; the launched thread
//!     keeps running after the HTTP response is closed.
//!
//! Both consumers share the same `dispatch::dispatch` call shape with
//! `pre_minted_thread_id = Some(thread_id)`. Dispatch normally creates
//! the pre-minted row itself; if dispatch fails before it reaches row
//! creation, this helper creates and finalizes a failed placeholder row
//! so the id already returned to a caller cannot remain phantom.

use serde_json::Value;

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
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] DispatchError),
}

impl LaunchSpawnError {
    /// Stable machine-readable error code matching the `DispatchError`
    /// code for the `Dispatch` variant, with one launch-specific code
    /// for `InvalidRef`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRef { .. } => "invalid_ref",
            Self::Dispatch(e) => e.code(),
        }
    }
}

/// Options controlling the dispatch-launch beyond the core
/// item_ref/project/parameters identity.
pub(crate) struct DispatchLaunchOptions {
    /// Launch mode (e.g. "inline", "detached"). Defaults to "inline".
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
}

impl Default for DispatchLaunchOptions {
    fn default() -> Self {
        Self {
            launch_mode: "inline".to_string(),
            target_site_id: None,
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            call: None,
            previous_thread_id: None,
        }
    }
}

/// Spawn the kind-agnostic dispatch-launch task on the global tokio
/// runtime. Returns the join handle so callers that need to observe
/// task completion (the SSE source) can await it; callers that don't
/// (the unary ack mode) drop the handle and let the task run to
/// completion in the background.
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
/// `options` carries launch-mode, target-site, validate-only, and
/// op/inputs overrides. When `Default::default()` is used, the
/// behavior is identical to the previous hard-coded defaults.
// Execution plumbing: each argument is a distinct leg of the launch's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_dispatch_launch(
    state: &AppState,
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    project_path: crate::routes::abs_path::AbsolutePathBuf,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
    provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    options: DispatchLaunchOptions,
) -> tokio::task::JoinHandle<Result<(), LaunchSpawnError>> {
    let state_clone = state.clone();
    let project_path_buf = project_path.into_path_buf();
    assert_eq!(
        provenance.effective_path(),
        project_path_buf,
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

    tokio::spawn(async move {
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

        let site_id = current_site_id;
        let current_site_id_for_failure_row = site_id.clone();
        let origin_site_id_for_failure_row = site_id.clone();

        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: principal_id.clone(),
                scopes: principal_scopes.clone(),
            }),
            project_context: ProjectContext::LocalPath {
                path: project_path_buf.clone(),
            },
            current_site_id: site_id.clone(),
            origin_site_id: site_id,
            execution_hints: Default::default(),
            validate_only,
        };

        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: state_clone.engine.clone(),
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
            acting_principal: principal_id.as_str(),
            project_path: project_path_buf.as_path(),
            provenance,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
            usage_subject,
            usage_subject_asserted_by,
            previous_thread_id,
            parent_execution_context: None,
        };

        match ryeos_executor::dispatch::dispatch(
            item_ref.as_str(),
            &dispatch_req,
            &exec_ctx,
            &state_clone,
        )
        .await
        {
            Ok(_value) => Ok(()),
            Err(e) => {
                // Persistence-first safety net: if dispatch created the
                // pre-minted thread row but failed before finalizing it
                // (e.g. a managed `build_and_launch` policy/trust/grant
                // failure that returns before spawn), finalize it `failed`.
                // If dispatch failed before creating the row at all (TOCTOU
                // after accepted preflight, invalidated bundle item, etc.),
                // create a failed placeholder row first so the id returned by
                // accepted launch never remains phantom. No-ops when the
                // runtime already drove the row terminal.
                let error_payload = serde_json::json!({
                    "code": e.code(),
                    "reason": e.to_string(),
                });
                let should_finalize = match state_clone.threads.get_thread(&pre_minted_thread_id) {
                    Ok(Some(detail)) => {
                        !ryeos_state::objects::ThreadStatus::from_str_lossy(&detail.status)
                            .is_some_and(|s| s.is_terminal())
                    }
                    Ok(None) => {
                        let failure_thread_kind = state_clone
                            .engine
                            .kinds
                            .get(item_ref.kind())
                            .and_then(|schema| schema.execution())
                            .and_then(|exec| exec.thread_profile.as_ref())
                            .map(|profile| profile.name.clone())
                            .unwrap_or_else(|| "system_task".to_string());
                        let _ = state_clone.threads.create_thread(
                            &ryeos_app::thread_lifecycle::ThreadCreateParams {
                                thread_id: pre_minted_thread_id.clone(),
                                chain_root_id: pre_minted_thread_id.clone(),
                                kind: failure_thread_kind,
                                item_ref: item_ref.as_str().to_string(),
                                executor_ref: item_ref.as_str().to_string(),
                                launch_mode: launch_mode.clone(),
                                current_site_id: current_site_id_for_failure_row.clone(),
                                origin_site_id: origin_site_id_for_failure_row.clone(),
                                upstream_thread_id: None,
                                requested_by: Some(principal_id.clone()),
                                usage_subject: usage_subject_for_failure_row.clone(),
                                usage_subject_asserted_by:
                                    usage_subject_asserted_by_for_failure_row.clone(),
                            },
                        );
                        state_clone
                            .threads
                            .get_thread(&pre_minted_thread_id)
                            .ok()
                            .flatten()
                            .is_some_and(|detail| {
                                !ryeos_state::objects::ThreadStatus::from_str_lossy(&detail.status)
                                    .is_some_and(|s| s.is_terminal())
                            })
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_spawn_error_code_invalid_ref() {
        let e = LaunchSpawnError::InvalidRef {
            ref_str: "x".into(),
            reason: "bad".into(),
        };
        assert_eq!(e.code(), "invalid_ref");
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

    #[test]
    fn launch_options_default_is_inline_local() {
        let opts = DispatchLaunchOptions::default();
        assert_eq!(opts.launch_mode, "inline");
        assert_eq!(opts.target_site_id, None);
        assert!(!opts.validate_only);
        assert!(opts.call.is_none());
    }

    #[test]
    fn launch_options_all_fields_overridable() {
        let opts = DispatchLaunchOptions {
            launch_mode: "detached".to_string(),
            target_site_id: Some("site:remote".to_string()),
            validate_only: true,
            usage_subject: None,
            usage_subject_asserted_by: None,
            call: Some(ryeos_engine::method_call::MethodCall {
                method: Some("validate".to_string()),
                args: Some(serde_json::json!({"key": "val"})),
            }),
            previous_thread_id: None,
        };
        assert_eq!(opts.launch_mode, "detached");
        assert_eq!(opts.target_site_id.as_deref(), Some("site:remote"));
        assert!(opts.validate_only);
        assert_eq!(opts.call.as_ref().unwrap().method(), Some("validate"));
        assert_eq!(opts.call.as_ref().unwrap().args().unwrap()["key"], "val");
    }
}
