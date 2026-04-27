use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct DispatchActionParams {
    callback_token: String,
    thread_id: String,
    project_path: String,
    action: ActionPayload,
}

#[derive(Debug, Deserialize)]
struct ActionPayload {
    item_id: String,
    #[serde(default)]
    params: Value,
    #[serde(default = "default_thread")]
    thread: String,
}

fn default_thread() -> String {
    "inline".to_string()
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: DispatchActionParams =
        serde_json::from_value(params.clone()).context("invalid runtime.dispatch_action params")?;

    let project_path = crate::execution::project_source::normalize_project_path(&params.project_path);

    state.callback_tokens.validate(
        &params.callback_token,
        &params.thread_id,
        &project_path,
    )?;

    handle_execute(params, state).await
}

/// V5.4 P2.3 — callback dispatch unification.
///
/// Routes `runtime.dispatch_action` through `dispatch::dispatch` (the
/// same entry point `/execute` uses) instead of calling
/// `service_executor::resolve_root_execution + run_inline` directly.
/// This preserves typed `DispatchError` mapping, the V5.3 root/runtime
/// split, the schema-driven hop loop, and (post-V5.4) the SSE seam.
///
/// **NOT in this change:** cap-bearing callback tokens. The callback
/// token continues to be a thread-binding token only; the runtime
/// remains self-policing for its own tool dispatches. Once third-party
/// runtime binaries land, callback tokens MUST carry `effective_caps`
/// and the dispatch loop here MUST enforce them — that's the deferred
/// item from V5.4.
async fn handle_execute(params: DispatchActionParams, state: &AppState) -> Result<Value> {
    // V5.4 P2 — strict typed callback contract requires every leaf
    // dispatcher reachable from a callback to emit
    // `CallbackDispatchResponse { thread, result }`. The subprocess
    // detached path (`dispatch::dispatch` → `run_detached`) instead
    // returns `{ thread, detached: true }`, which the runtime's
    // `serde(deny_unknown_fields)` deserializer would reject. Rather
    // than invent a second envelope, fail closed at the boundary:
    // callbacks are unary, inline only.
    if params.action.thread != "inline" {
        anyhow::bail!(
            "callback dispatch only supports inline results; \
             got thread={:?} (detached/forked launches must go through /execute, \
             not the runtime callback)",
            params.action.thread
        );
    }

    let project_path =
        crate::execution::project_source::normalize_project_path(&params.project_path);

    let thread = state
        .threads
        .get_thread(&params.thread_id)
        .with_context(|| format!("lookup parent thread '{}'", params.thread_id))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "callback dispatch refers to unknown thread '{}'",
                params.thread_id
            )
        })?;
    let caller_principal_id = thread.requested_by.ok_or_else(|| {
        anyhow::anyhow!(
            "callback parent thread '{}' has no requested_by — refusing \
             to fall back to daemon identity",
            params.thread_id
        )
    })?;

    let caller_scopes = vec!["execute".to_string()];
    let site_id = state.threads.site_id();

    let root_canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
        .with_context(|| format!("invalid callback item_id '{}'", params.action.item_id))?;

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
            fingerprint: caller_principal_id.clone(),
            scopes: caller_scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.clone(),
        },
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        execution_hints: Default::default(),
        validate_only: false,
    };
    let exec_ctx = crate::service_executor::ExecutionContext {
        principal_fingerprint: caller_principal_id.clone(),
        caller_scopes,
        engine: state.engine.clone(),
        plan_ctx,
    };

    let dispatch_req = crate::dispatch::DispatchRequest {
        launch_mode: params.action.thread.as_str(),
        target_site_id: None,
        project_source_is_pushed_head: false,
        validate_only: false,
        params: params.action.params.clone(),
        acting_principal: caller_principal_id.as_str(),
        project_path: &project_path,
        original_project_path: project_path.clone(),
        snapshot_hash: None,
        temp_dir: None,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
    };

    // V5.4 P2.3 cleanup — async end-to-end: the UDS dispatcher is
    // already on a tokio runtime (see `uds::server::dispatch`), so
    // we await `dispatch::dispatch` directly. The previous
    // `Handle::current().block_on(...)` was a panic/deadlock risk on
    // the P3b hot path (a runtime-thread blocking on its own runtime).
    let outcome = crate::dispatch::dispatch(
        &params.action.item_id,
        &dispatch_req,
        &exec_ctx,
        state,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    match outcome {
        crate::dispatch::DispatchOutcome::Unary(v) => Ok(v),
        crate::dispatch::DispatchOutcome::Stream(_) => {
            anyhow::bail!(
                "callback dispatch received a streaming outcome; \
                 callbacks are unary"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thread_is_inline() {
        assert_eq!(default_thread(), "inline");
    }
}
