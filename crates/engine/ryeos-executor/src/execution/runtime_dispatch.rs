use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use ryeos_runtime::authorizer::AuthorizationPolicy;

use ryeos_app::callback_token::ThreadAuthState;
use ryeos_app::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchActionParams {
    callback_token: String,
    thread_id: String,
    project_path: String,
    thread_auth_token: String,
    // Use the shared callback wire type directly — no local duplicate — so the
    // action payload (incl. its `call` block) can't drift from the runtime
    // side of the wire.
    action: ryeos_runtime::callback::ActionPayload,
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: DispatchActionParams =
        serde_json::from_value(params.clone()).context("invalid runtime.dispatch_action params")?;

    // Use the raw project_path as-is. The token was minted with the raw
    // PathBuf at runner.rs's launch site (no normalization); we must
    // compare against the same form here or PathBuf equality will fail.
    let project_path = std::path::PathBuf::from(&params.project_path);

    let cap =
        state
            .callback_tokens
            .validate(&params.callback_token, &params.thread_id, &project_path)?;

    // V5.5 P2 — daemon-enforced callback caps. The token carries the
    // composed `effective_caps` minted at launch time; the runtime is
    // no longer trusted to self-police what it dispatches. An empty
    // cap-set is deny-all; a wildcard `*` short-circuits to allow.
    enforce_callback_caps(
        &params.action.item_id,
        &cap.effective_caps,
        &state.authorizer,
    )?;

    let child_provenance = cap.provenance.clone_for_borrowed_child();

    let thread_auth = state
        .thread_auth
        .validate(&params.thread_auth_token, &params.thread_id)?;

    // Note: DispatchActionParams has `deny_unknown_fields` and no
    // `principal` field — the request body cannot supply (and so
    // cannot spoof) a principal. The principal logged here is read
    // strictly from the validated server-side ThreadAuthState.
    tracing::info!(
        thread_id = %params.thread_id,
        server_principal = %thread_auth.acting_principal,
        project_path = %params.project_path,
        borrowed_dir = %child_provenance.effective_path().display(),
        project_source = ?child_provenance.project_source(),
        "thread auth token validated: using server-side principal",
    );

    handle_execute(params, state, &thread_auth, &cap, child_provenance).await
}

/// V5.5 P2: enforce the callback's composed `effective_caps` against
/// the requested item ref. Uses the unified `Authorizer` for wildcard
/// + implication expansion. An empty cap-set is deny-all — the
/// trust-boundary default for tokens minted without a composition step.
fn enforce_callback_caps(
    item_id: &str,
    effective_caps: &[String],
    authorizer: &ryeos_runtime::authorizer::Authorizer,
) -> Result<()> {
    if effective_caps.is_empty() {
        anyhow::bail!(
            "callback denied: no effective_caps on token (deny-all); \
             requested item '{item_id}' cannot be dispatched"
        );
    }

    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_id)
        .with_context(|| format!("invalid callback item_id '{item_id}'"))?;
    let required = format!("ryeos.execute.{}.{}", canonical.kind, canonical.bare_id);

    let policy = AuthorizationPolicy::require_all(&[&required]);
    if authorizer.authorize(effective_caps, &policy).is_err() {
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
async fn handle_execute(
    params: DispatchActionParams,
    state: &AppState,
    thread_auth: &ThreadAuthState,
    cap: &ryeos_app::callback_token::CallbackCapability,
    child_provenance: ryeos_app::execution_provenance::ExecutionProvenance,
) -> Result<Value> {
    // V5.4 P2 — strict typed callback contract requires every leaf
    // dispatcher reachable from a callback to emit
    // `CallbackDispatchResponse { thread, result }`. The subprocess
    // detached path (`dispatch::dispatch` → `run_detached`) instead
    // returns `{ thread, detached: true }`, which the runtime's
    // `serde(deny_unknown_fields)` deserializer would reject.
    //
    // `detached` is the ONE non-inline mode a callback may request: the
    // native fanout primitive. It does not return a leaf result — it mints
    // a lineage-linked, cohort-tagged child that runs concurrently while the
    // calling parent walks on — so it routes to `spawn_detached_child` (which
    // returns `{ thread: "detached", detached: true, child_thread_id }`), not
    // the inline leaf dispatch below. Any other non-inline mode fails closed:
    // callback leaf results are unary and inline only.
    if params.action.thread == "detached" {
        return crate::execution::spawn_detached_child::spawn_detached_child(
            state,
            thread_auth,
            cap,
            child_provenance,
            &params.action.item_id,
            &params.action.params,
            params.action.facets.as_ref(),
            params.action.launch_window.as_ref(),
        )
        .await;
    }
    if params.action.thread != "inline" {
        anyhow::bail!(
            "callback dispatch only supports inline results or a `detached` \
             fanout launch; got thread={:?}",
            params.action.thread
        );
    }

    // Inline is the LEAF contract: terminator kinds (tool/service) and
    // method-dispatch kinds (e.g. a knowledge query) return a value and
    // settle. A delegate-via-runtime-registry kind (directive/graph) is a
    // native THREAD RUN — awaiting it inline holds the callback wire for the
    // child's entire lifetime, leaves the run invisible in the parent's braid
    // until commit, and cannot checkpoint across the wait. Those semantics
    // belong to the daemon's suspend/fanout machinery, so fail closed with
    // the fix: `follow: true` (await via durable suspend) or `detach: true`
    // (lineage-linked fire-and-forget).
    if let Ok(child_ref) =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
    {
        let is_thread_run_kind = state
            .engine
            .kinds
            .get(&child_ref.kind)
            .and_then(|schema| schema.execution())
            .is_some_and(|exec| exec.delegate.is_some());
        if is_thread_run_kind {
            anyhow::bail!(
                "inline callback dispatch of `{}` is not supported: kind `{}` executes as \
                 a native thread run. Mark the node `follow: true` to await its result via \
                 durable suspend, or `detach: true` for a lineage-linked fire-and-forget \
                 child",
                params.action.item_id,
                child_ref.kind
            );
        }
    }

    let caller_principal_id = thread_auth.acting_principal.clone();
    // Authority for callback-dispatched work comes from the callback
    // capability minted at the parent launch boundary. Thread-auth proves the
    // runtime process identity/liveness, but its scopes are intentionally
    // narrow transport scopes (currently `execute`) and are not the parent's
    // composed execution grants. Recursive dispatches (for example
    // `tool:ryeos/knowledge/compose` → `knowledge:<ref>`) must see the same
    // effective caps that `enforce_callback_caps` checked at this boundary.
    let caller_scopes = callback_dispatch_scopes(cap);
    let site_id = state.threads.site_id();

    let root_canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
            .with_context(|| format!("invalid callback item_id '{}'", params.action.item_id))?;

    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
            fingerprint: caller_principal_id.clone(),
            scopes: caller_scopes.clone(),
        }),
        project_context: ProjectContext::LocalPath {
            path: child_provenance.effective_path().to_path_buf(),
        },
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        execution_hints: Default::default(),
        validate_only: false,
    };
    let exec_ctx = crate::executor::ExecutionContext {
        principal_fingerprint: caller_principal_id.clone(),
        caller_scopes,
        // Use the parent's per-request engine — never the daemon engine.
        engine: child_provenance.request_engine().clone(),
        plan_ctx,
        // Method selector from the graph node's action `call` block — the
        // single source of truth for method dispatch (resolver + arg
        // validation both read it). `None` → the kind's default method.
        requested_call: params.action.call.clone(),
    };

    let project_path = child_provenance.effective_path().to_path_buf();
    // C0 diagnostic: snapshot the run's resolution source before `provenance` is
    // moved into the dispatch request, so a content-hash mismatch can be pinned
    // to its origin below.
    let diag_source = child_provenance.project_source();
    let diag_effective_path = child_provenance.effective_path().to_path_buf();
    let dispatch_req = crate::dispatch::DispatchRequest {
        launch_mode: params.action.thread.as_str(),
        target_site_id: None,
        validate_only: false,
        params: params.action.params.clone(),
        acting_principal: caller_principal_id.as_str(),
        project_path: project_path.as_path(),
        provenance: child_provenance,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        parent_execution_context: Some(parent_execution_context_from_capability(cap)),
    };

    // V5.4 P2.3 cleanup — async end-to-end: the UDS dispatcher is
    // already on a tokio runtime (see `uds::server::dispatch`), so
    // we await `dispatch::dispatch` directly. The previous
    // `Handle::current().block_on(...)` was a panic/deadlock risk on
    // the P3b hot path (a runtime-thread blocking on its own runtime).
    let result =
        crate::dispatch::dispatch(&params.action.item_id, &dispatch_req, &exec_ctx, state).await;
    if let Err(err) = &result {
        // C0: attribute a content-hash mismatch to its resolution source. A
        // `LiveFs` run means the dispatched item's bytes were re-signed on disk
        // mid-run; a `PushedHead` run means dispatch read a stale materialized
        // checkout (`effective_path`). This is the signal the re-sign/pin
        // investigation needs before any pin policy is designed.
        if err.to_string().contains("content hash mismatch") {
            tracing::warn!(
                item_id = %params.action.item_id,
                project_source = ?diag_source,
                effective_path = %diag_effective_path.display(),
                error = %err,
                "C0: content-hash mismatch during callback dispatch",
            );
        }
    }
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

fn parent_execution_context_from_capability(
    cap: &ryeos_app::callback_token::CallbackCapability,
) -> crate::dispatch::ParentExecutionContext {
    crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    }
}

fn callback_dispatch_scopes(cap: &ryeos_app::callback_token::CallbackCapability) -> Vec<String> {
    cap.effective_caps.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // ── V5.5 P2: enforce_callback_caps ──────────────────────────────

    fn test_auth() -> ryeos_runtime::authorizer::Authorizer {
        ryeos_runtime::authorizer::Authorizer::new()
    }

    fn minimal_engine() -> Arc<ryeos_engine::engine::Engine> {
        Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            vec![],
        ))
    }

    #[test]
    fn callback_capability_maps_to_parent_execution_context_without_kind_checks() {
        let cap = ryeos_app::callback_token::CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-parent".to_string(),
            chain_root_id: "T-parent".to_string(),
            project_path: PathBuf::from("/project"),
            expires_at: Instant::now() + Duration::from_secs(300),
            effective_caps: vec!["ryeos.*".to_string()],
            provenance: ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                PathBuf::from("/project"),
                minimal_engine(),
            ),
            effective_bundle_id: None,
            item_ref: Some("graph:team/parent".to_string()),
            hard_limits: serde_json::json!({"turns": 6, "tokens": 1000}),
            depth: 4,
        };

        let ctx = parent_execution_context_from_capability(&cap);
        assert_eq!(ctx.parent_thread_id, "T-parent");
        assert_eq!(ctx.hard_limits, serde_json::json!({"turns": 6, "tokens": 1000}));
        assert_eq!(ctx.depth, 4);
    }

    #[test]
    fn callback_dispatch_scopes_use_effective_caps_not_transport_scopes() {
        let cap = ryeos_app::callback_token::CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-parent".to_string(),
            chain_root_id: "T-parent".to_string(),
            project_path: PathBuf::from("/project"),
            expires_at: Instant::now() + Duration::from_secs(300),
            effective_caps: vec!["ryeos.execute.knowledge.arc/*".to_string()],
            provenance: ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                PathBuf::from("/project"),
                minimal_engine(),
            ),
            effective_bundle_id: None,
            item_ref: Some("graph:arc/solve".to_string()),
            hard_limits: serde_json::json!({}),
            depth: 0,
        };

        assert_eq!(
            callback_dispatch_scopes(&cap),
            vec!["ryeos.execute.knowledge.arc/*".to_string()]
        );
    }

    #[test]
    fn caps_full_wildcard_allows_everything() {
        let auth = test_auth();
        // The `ryeos.*` cap (or expansion) covers all kinds.
        let caps = vec!["ryeos.*".to_string()];
        assert!(enforce_callback_caps("tool:any/thing", &caps, &auth).is_ok());
        assert!(enforce_callback_caps("directive:any/thing", &caps, &auth).is_ok());
    }

    #[test]
    fn caps_empty_denies_everything() {
        let auth = test_auth();
        let caps: Vec<String> = vec![];
        let err = enforce_callback_caps("tool:foo/bar", &caps, &auth).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deny-all") && msg.contains("tool:foo/bar"),
            "deny-all error must mention the requested item; got: {msg}"
        );
    }

    #[test]
    fn caps_kind_wildcard_matches_any_id_in_kind() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.*".to_string()];
        assert!(enforce_callback_caps("tool:any/echo", &caps, &auth).is_ok());
        assert!(enforce_callback_caps("tool:other/foo", &caps, &auth).is_ok());
        // Different kind — denied.
        let err = enforce_callback_caps("directive:foo/bar", &caps, &auth).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn caps_exact_match_with_slash_subject() {
        let auth = test_auth();
        // `tool:foo/bar` → required cap `ryeos.execute.tool.foo/bar`.
        // Slash is preserved in subject, matching the canonical format.
        let caps = vec!["ryeos.execute.tool.foo/bar".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        let err = enforce_callback_caps("tool:foo/baz", &caps, &auth).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn caps_invalid_item_id_rejected() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.foo".to_string()];
        let err = enforce_callback_caps("not-a-canonical-ref", &caps, &auth).unwrap_err();
        assert!(
            err.to_string().contains("invalid callback item_id"),
            "must point at canonical-ref parse failure; got: {}",
            err
        );
    }

    #[test]
    fn caps_path_prefix_wildcard_matches_slash_subject() {
        let auth = test_auth();
        // `ryeos.execute.tool.foo/*` matches `tool:foo/bar` because
        // `/*` is the path-prefix wildcard convention.
        let caps = vec!["ryeos.execute.tool.foo/*".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        // A sibling `tool:foobar` requires `ryeos.execute.tool.foobar`,
        // which does NOT match `ryeos.execute.tool.foo/*` — the `/`
        // separator is required.
        let err = enforce_callback_caps("tool:foobar", &caps, &auth).unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn caps_full_kind_wildcard_matches_any_subject() {
        let auth = test_auth();
        // `ryeos.execute.tool.*` matches any tool subject, including
        // those with `/` separators.
        let caps = vec!["ryeos.execute.tool.*".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        assert!(enforce_callback_caps("tool:baz/qux/deep", &caps, &auth).is_ok());
    }
}
