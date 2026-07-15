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
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] DispatchError),
}

impl LaunchSpawnError {
    /// Stable machine-readable error code matching the `DispatchError`
    /// code for the `Dispatch` variant, with one launch-specific code
    /// for `InvalidRef`.
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidRef { .. } => "invalid_ref",
            Self::Dispatch(e) => e.code(),
        }
    }
}

/// Options controlling the dispatch-launch beyond the core
/// item_ref/project/parameters identity.
pub(crate) struct DispatchLaunchOptions {
    pub ref_bindings: BTreeMap<String, String>,
    /// Launch mode (e.g. "inline", "detached").
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
    /// Exact verified subject and captured policy returned by synchronous
    /// dispatch preflight. This may name a terminal target behind the
    /// caller-named wrapper; both success and failure persistence consume this
    /// same non-optional contract.
    root_admission: ryeos_app::thread_lifecycle::RootExecutionAdmission,
    /// Canonical project authority copied from the sealed admission. Background
    /// launch never reuses the caller's pre-canonical path spelling.
    project_path: std::path::PathBuf,
}

impl DispatchLaunchOptions {
    /// Default execution controls for a synchronously admitted root.
    pub(crate) fn admitted(
        root_admission: ryeos_app::thread_lifecycle::RootExecutionAdmission,
        ref_bindings: BTreeMap<String, String>,
    ) -> anyhow::Result<Self> {
        root_admission.validate()?;
        if root_admission.ref_bindings() != &ref_bindings {
            anyhow::bail!("dispatch launch secondary identities do not match sealed admission");
        }
        let project_path = root_admission
            .project_root()
            .ok_or_else(|| anyhow::anyhow!("dispatch launch admission has no local project root"))?
            .to_path_buf();
        Ok(Self {
            ref_bindings,
            launch_mode: "inline".to_string(),
            target_site_id: None,
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            call: None,
            previous_thread_id: None,
            root_admission,
            project_path,
        })
    }

    pub(crate) fn project_path(&self) -> &std::path::Path {
        &self.project_path
    }
}

/// Run the same schema-driven dispatch walk used by the background task and
/// return the exact terminal/root contract before an id is minted. This is the
/// public-route admission boundary shared by launch, stream, and thread-input
/// callers; it deliberately understands no kind or service name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn preflight_dispatch_launch(
    state: &AppState,
    item_ref: &crate::routes::parsed_ref::ParsedItemRef,
    project_path: &crate::routes::abs_path::AbsolutePathBuf,
    parameters: &Value,
    ref_bindings: &BTreeMap<String, String>,
    principal_id: &str,
    principal_scopes: &[String],
    call: Option<ryeos_engine::method_call::MethodCall>,
    launch_mode: &str,
    validate_only: bool,
    usage_subject: Option<&ryeos_state::UsageSubject>,
    usage_subject_asserted_by: Option<&str>,
) -> Result<ryeos_executor::dispatch::RootDispatchPreflight, DispatchError> {
    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

    if ryeos_engine::contracts::LaunchMode::from_wire(launch_mode).is_none() {
        return Err(DispatchError::InvalidLaunchMode {
            other: launch_mode.to_string(),
        });
    }
    let project_path = project_path.as_path().canonicalize().map_err(|error| {
        DispatchError::ProjectSource(format!(
            "canonicalize launch project {}: {error}",
            project_path.as_path().display()
        ))
    })?;
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: principal_id.to_string(),
            scopes: principal_scopes.to_vec(),
        }),
        project_context: ProjectContext::LocalPath { path: project_path },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only,
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: principal_id.to_string(),
        caller_scopes: principal_scopes.to_vec(),
        engine: state.engine.clone(),
        plan_ctx,
        requested_call: call,
    };
    ryeos_executor::dispatch::preflight_root_dispatch(
        item_ref.as_str(),
        item_ref.kind(),
        parameters,
        ref_bindings,
        usage_subject,
        usage_subject_asserted_by,
        &exec_ctx,
        state,
    )
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
/// Spawn an acknowledged managed launch. The receiver resolves only after the
/// durable launch authority has been handed to the scheduled spawn task.
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
    let root_admission = options.root_admission;
    let ref_bindings = options.ref_bindings;

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
            ref_bindings,
            acting_principal: principal_id.as_str(),
            project_path: project_path_buf.as_path(),
            provenance,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
            usage_subject,
            usage_subject_asserted_by,
            previous_thread_id,
            root_admission: Some(root_admission.clone()),
            parent_execution_context: None,
        };

        let dispatched = match launch_handoff.as_ref() {
            Some(handoff) => ryeos_executor::dispatch::dispatch_with_launch_handoff(
                item_ref.as_str(),
                &dispatch_req,
                &exec_ctx,
                &state_clone,
                handoff,
            )
            .await,
            None => ryeos_executor::dispatch::dispatch(
                item_ref.as_str(),
                &dispatch_req,
                &exec_ctx,
                &state_clone,
            )
            .await,
        };
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
                                resolved_item: admitted_subject.clone(),
                                plan_context: exec_ctx.plan_ctx.clone(),
                                root_admission: Some(root_admission.clone()),
                            };
                        match state_clone
                            .threads
                            .create_root_thread_with_id(&pre_minted_thread_id, &failure_request)
                        {
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
}
