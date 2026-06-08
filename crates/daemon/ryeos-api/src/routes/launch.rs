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
//! `pre_minted_thread_id = Some(thread_id)`, so the persistence-first
//! contract holds: lifecycle events for the thread are emitted by
//! dispatch and durable in the event store before any subscriber sees
//! them.

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
    /// Optional operation name for multi-op items.
    pub operation: Option<String>,
    /// Optional op-specific inputs.
    pub inputs: Option<Value>,
}

impl Default for DispatchLaunchOptions {
    fn default() -> Self {
        Self {
            launch_mode: "inline".to_string(),
            target_site_id: None,
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            operation: None,
            inputs: None,
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
pub(crate) fn spawn_dispatch_launch(
    state: &AppState,
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    project_path: crate::routes::abs_path::AbsolutePathBuf,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
    options: DispatchLaunchOptions,
) -> tokio::task::JoinHandle<Result<(), LaunchSpawnError>> {
    let state_clone = state.clone();
    let project_path_buf = project_path.into_path_buf();
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
    let operation = options.operation;
    let inputs = options.inputs;

    tokio::spawn(async move {
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};

        let site_id = current_site_id;

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
            requested_op: operation.clone(),
            requested_inputs: inputs.clone(),
        };

        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_path_buf.clone(),
            state_clone.engine.clone(),
        );

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
            operation,
            inputs,
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
            Err(e) => Err(LaunchSpawnError::Dispatch(e)),
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
    fn launch_options_default_matches_previous_hardcoded_behavior() {
        let opts = DispatchLaunchOptions::default();
        assert_eq!(opts.launch_mode, "inline");
        assert_eq!(opts.target_site_id, None);
        assert_eq!(opts.validate_only, false);
        assert_eq!(opts.operation, None);
        assert_eq!(opts.inputs, None);
    }

    #[test]
    fn launch_options_all_fields_overridable() {
        let opts = DispatchLaunchOptions {
            launch_mode: "detached".to_string(),
            target_site_id: Some("site:remote".to_string()),
            validate_only: true,
            usage_subject: None,
            usage_subject_asserted_by: None,
            operation: Some("validate".to_string()),
            inputs: Some(serde_json::json!({"key": "val"})),
        };
        assert_eq!(opts.launch_mode, "detached");
        assert_eq!(opts.target_site_id.as_deref(), Some("site:remote"));
        assert!(opts.validate_only);
        assert_eq!(opts.operation.as_deref(), Some("validate"));
        assert_eq!(opts.inputs.as_ref().unwrap()["key"], "val");
    }
}
