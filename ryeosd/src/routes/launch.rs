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
//!   - [`crate::routes::streaming_sources::dispatch_launch`] — SSE
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

use crate::dispatch_error::DispatchError;
use crate::state::AppState;

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
pub(crate) fn spawn_dispatch_launch(
    state: &AppState,
    item_ref: crate::routes::parsed_ref::ParsedItemRef,
    project_path: crate::routes::abs_path::AbsolutePathBuf,
    parameters: Value,
    principal_id: String,
    principal_scopes: Vec<String>,
    pre_minted_thread_id: String,
) -> tokio::task::JoinHandle<Result<(), LaunchSpawnError>> {
    let state_clone = state.clone();
    let project_path_buf = project_path.into_path_buf();

    tokio::spawn(async move {
        use ryeos_engine::contracts::{
            EffectivePrincipal, PlanContext, Principal, ProjectContext,
        };

        let site_id = state_clone.threads.site_id().to_string();

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
            validate_only: false,
        };

        let exec_ctx = crate::service_executor::ExecutionContext {
            principal_fingerprint: principal_id.clone(),
            caller_scopes: principal_scopes,
            engine: state_clone.engine.clone(),
            plan_ctx,
        };

        let dispatch_req = crate::dispatch::DispatchRequest {
            launch_mode: "inline",
            target_site_id: None,
            project_source_is_pushed_head: false,
            validate_only: false,
            params: parameters,
            acting_principal: principal_id.as_str(),
            project_path: project_path_buf.as_path(),
            original_project_path: project_path_buf.clone(),
            snapshot_hash: None,
            temp_dir: None,
            original_root_kind: item_ref.kind(),
            pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
        };

        match crate::dispatch::dispatch(item_ref.as_str(), &dispatch_req, &exec_ctx, &state_clone).await {
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
}

