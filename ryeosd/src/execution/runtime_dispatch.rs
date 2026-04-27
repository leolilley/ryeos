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

    let cap = state.callback_tokens.validate(
        &params.callback_token,
        &params.thread_id,
        &project_path,
    )?;

    // V5.5 P2 — daemon-enforced callback caps. The token carries the
    // composed `effective_caps` minted at launch time; the runtime is
    // no longer trusted to self-police what it dispatches. An empty
    // cap-set is deny-all; a wildcard `*` short-circuits to allow.
    enforce_callback_caps(&params.action.item_id, &cap.effective_caps)?;

    handle_execute(params, state).await
}

/// V5.5 P2: enforce the callback's composed `effective_caps` against
/// the requested item ref. Mirrors `dispatch::enforce_runtime_caps`
/// but for the callback boundary — the runtime is the untrusted
/// principal here, the daemon is the gate.
///
/// Cap matching delegates to `ryeos_runtime::check_capability`, which
/// is the same primitive runtime emitters use to self-check today.
/// The required cap is built `rye.execute.<kind>.<bare_id>` with `/`
/// → `.` substitution in the bare id (matching the convention in
/// graph/directive runtime permission checks). An empty cap-set is
/// deny-all — the trust-boundary default for tokens minted without
/// a composition step.
fn enforce_callback_caps(item_id: &str, effective_caps: &[String]) -> Result<()> {
    if effective_caps.is_empty() {
        anyhow::bail!(
            "callback denied: no effective_caps on token (deny-all); \
             requested item '{item_id}' cannot be dispatched"
        );
    }

    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_id)
        .with_context(|| format!("invalid callback item_id '{item_id}'"))?;
    let bare_dotted = canonical.bare_id.replace('/', ".");
    let required = format!("rye.execute.{}.{}", canonical.kind, bare_dotted);

    if !ryeos_runtime::check_capability(effective_caps, &required) {
        anyhow::bail!(
            "callback denied: required cap '{required}' not present in \
             effective_caps {effective_caps:?}"
        );
    }
    Ok(())
}

/// V5.4 P2.3 — callback dispatch unification.
///
/// Routes `runtime.dispatch_action` through `dispatch::dispatch` (the
/// same entry point `/execute` uses) instead of calling
/// `service_executor::resolve_root_execution + run_inline` directly.
/// This preserves typed `DispatchError` mapping, the V5.3 root/runtime
/// split, the schema-driven hop loop, and the V5.5 route-system seam.
///
/// **V5.5 P2:** callback tokens carry composed `effective_caps`; the
/// daemon enforces them at the trust boundary in `handle()` via
/// `enforce_callback_caps` BEFORE dispatch reaches this function.
/// The runtime is no longer self-policing.
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
    crate::dispatch::dispatch(
        &params.action.item_id,
        &dispatch_req,
        &exec_ctx,
        state,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thread_is_inline() {
        assert_eq!(default_thread(), "inline");
    }

    // ── V5.5 P2: enforce_callback_caps ──────────────────────────────

    #[test]
    fn caps_full_wildcard_allows_everything() {
        // The `rye.*` cap (or expansion) covers all kinds.
        let caps = vec!["rye.*".to_string()];
        assert!(enforce_callback_caps("tool:any/thing", &caps).is_ok());
        assert!(enforce_callback_caps("directive:any/thing", &caps).is_ok());
    }

    #[test]
    fn caps_empty_denies_everything() {
        let caps: Vec<String> = vec![];
        let err = enforce_callback_caps("tool:foo/bar", &caps).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deny-all") && msg.contains("tool:foo/bar"),
            "deny-all error must mention the requested item; got: {msg}"
        );
    }

    #[test]
    fn caps_kind_wildcard_matches_any_id_in_kind() {
        let caps = vec!["rye.execute.tool.*".to_string()];
        assert!(enforce_callback_caps("tool:any/echo", &caps).is_ok());
        assert!(enforce_callback_caps("tool:other/foo", &caps).is_ok());
        // Different kind — denied.
        let err = enforce_callback_caps("directive:foo/bar", &caps).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn caps_exact_match_after_slash_to_dot_substitution() {
        // `tool:foo/bar` → required cap `rye.execute.tool.foo.bar`.
        // The slash-to-dot substitution mirrors the convention in
        // graph/directive permission checks.
        let caps = vec!["rye.execute.tool.foo.bar".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps).is_ok());
        let err = enforce_callback_caps("tool:foo/baz", &caps).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn caps_invalid_item_id_rejected() {
        // A bogus item_id MUST surface a parse error, not silently
        // proceed past the gate.
        let caps = vec!["rye.execute.tool.foo".to_string()];
        let err = enforce_callback_caps("not-a-canonical-ref", &caps).unwrap_err();
        assert!(
            err.to_string().contains("invalid callback item_id"),
            "must point at canonical-ref parse failure; got: {}",
            err
        );
    }

    #[test]
    fn caps_subtree_prefix_matches_with_slash_to_dot() {
        // `rye.execute.tool.foo.*` matches `tool:foo/bar` because the
        // bare id is dot-substituted to `foo.bar` before matching.
        let caps = vec!["rye.execute.tool.foo.*".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps).is_ok());
        // A sibling `tool:foobar` substitutes to `rye.execute.tool.foobar`,
        // which does NOT match `rye.execute.tool.foo.*` — the cap_matches
        // regex anchors `.*` at a `.` boundary, not arbitrary substring.
        let err = enforce_callback_caps("tool:foobar", &caps).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }
}
