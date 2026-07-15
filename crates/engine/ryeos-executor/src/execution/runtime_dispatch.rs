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
    #[serde(default)]
    hook_dispatch: Option<ryeos_runtime::callback::HookDispatchIdentity>,
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
    crate::execution::launch_preparation::validate_ref_bindings(&params.action.ref_bindings)?;

    // V5.5 P2 — daemon-enforced callback caps. The token carries the
    // composed `effective_caps` minted at launch time; the runtime is
    // no longer trusted to self-police what it dispatches. An empty
    // cap-set is deny-all; a wildcard `*` short-circuits to allow.
    enforce_callback_caps(
        &params.action.item_id,
        &cap.effective_caps,
        &state.authorizer,
    )?;
    for binding_ref in params.action.ref_bindings.values() {
        enforce_callback_caps(binding_ref, &cap.effective_caps, &state.authorizer)?;
    }

    let child_provenance = cap.provenance.clone_for_borrowed_child();

    let thread_auth = state
        .thread_auth
        .validate(&params.thread_auth_token, &params.thread_id)?;

    // The chain root is authority, not callback input. Bind hook replay to the
    // durable caller row and prove the callback capability was minted for that
    // same chain before consulting the ledger.
    let caller_thread = state
        .threads
        .get_thread(&params.thread_id)?
        .ok_or_else(|| anyhow::anyhow!("callback caller thread not found: {}", params.thread_id))?;
    cap.assert_chain_root(&caller_thread.chain_root_id)?;

    if let Some(hook) = params.hook_dispatch.as_ref() {
        let callback_root_item_ref = cap.item_ref.as_deref().ok_or_else(|| {
            hook_integrity("hook callback capability is missing its root item ref")
        })?;
        validate_hook_dispatch_preflight(
            hook,
            &params.action,
            callback_root_item_ref,
            &cap.root_content_digest,
            state,
        )?;
    }

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

    handle_execute(
        params,
        state,
        &thread_auth,
        &cap,
        &caller_thread.chain_root_id,
        child_provenance,
    )
    .await
}

fn hook_integrity(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(crate::dispatch_error::DispatchError::HookDispatchIntegrity {
        detail: detail.into(),
    })
}

fn validate_hook_dispatch_preflight(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
    callback_root_item_ref: &str,
    callback_root_content_digest: &str,
    state: &AppState,
) -> Result<()> {
    let canonical = validate_hook_identity_authority(
        hook,
        action,
        callback_root_item_ref,
        callback_root_content_digest,
    )?;
    let managed_thread_target = state
        .engine
        .kinds
        .get(&canonical.kind)
        .and_then(|schema| schema.execution())
        .is_some_and(|execution| execution.delegate.is_some());
    if managed_thread_target {
        return Err(hook_integrity(format!(
            "hook `{}` targets managed-thread kind `{}`; hooks must settle inline",
            hook.hook_id, canonical.kind
        )));
    }
    Ok(())
}

fn validate_hook_identity_authority(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
    callback_root_item_ref: &str,
    callback_root_content_digest: &str,
) -> Result<ryeos_engine::canonical_ref::CanonicalRef> {
    let canonical_sha256 = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if !canonical_sha256(&hook.context_hash) {
        return Err(hook_integrity(
            "hook context_hash is not a canonical lowercase SHA-256 digest",
        ));
    }
    if hook.hook_id.is_empty() || hook.hook_id.len() > 4 * 1024 {
        return Err(hook_integrity(
            "hook_id must contain between 1 and 4096 UTF-8 bytes",
        ));
    }
    let (coordinates, expected_definition_kind, definition_ref) = match &hook.occurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStarted {
            graph_run_id,
            definition_ref,
            definition_hash,
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::GraphCompleted {
            graph_run_id,
            definition_ref,
            definition_hash,
            ..
        } => (
            vec![
                ("graph_run_id", graph_run_id.as_str()),
                ("definition_ref", definition_ref.as_str()),
                ("definition_hash", definition_hash.as_str()),
            ],
            "graph",
            definition_ref.as_str(),
        ),
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
            graph_run_id,
            definition_ref,
            definition_hash,
            node,
            ..
        } => (
            vec![
                ("graph_run_id", graph_run_id.as_str()),
                ("definition_ref", definition_ref.as_str()),
                ("definition_hash", definition_hash.as_str()),
                ("node", node.as_str()),
            ],
            "graph",
            definition_ref.as_str(),
        ),
        ryeos_runtime::callback::HookDispatchOccurrence::DirectiveAfterStep {
            definition_ref,
            definition_hash,
            turn,
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::DirectiveContinuation {
            definition_ref,
            definition_hash,
            turn,
        } => {
            if *turn == 0 {
                return Err(hook_integrity("directive hook turn must be greater than zero"));
            }
            (
                vec![
                    ("definition_ref", definition_ref.as_str()),
                    ("definition_hash", definition_hash.as_str()),
                ],
                "directive",
                definition_ref.as_str(),
            )
        }
    };
    for (field, value) in coordinates {
        if value.is_empty() || value.len() > 4 * 1024 {
            return Err(hook_integrity(format!(
                "hook occurrence field `{field}` must contain between 1 and 4096 UTF-8 bytes"
            )));
        }
        if field == "definition_hash" && !canonical_sha256(value) {
            return Err(hook_integrity(
                "hook definition_hash is not a canonical lowercase SHA-256 digest",
            ));
        }
    }
    let canonical_definition = ryeos_engine::canonical_ref::CanonicalRef::parse(definition_ref)
        .map_err(|error| hook_integrity(format!("invalid hook definition_ref: {error}")))?;
    if canonical_definition.kind != expected_definition_kind {
        return Err(hook_integrity(format!(
            "hook definition_ref kind must be `{expected_definition_kind}`, got `{}`",
            canonical_definition.kind
        )));
    }
    let canonical_callback_root =
        ryeos_engine::canonical_ref::CanonicalRef::parse(callback_root_item_ref).map_err(
            |error| hook_integrity(format!("invalid callback root item ref: {error}")),
        )?;
    if canonical_callback_root != canonical_definition {
        return Err(hook_integrity(format!(
            "hook definition_ref `{definition_ref}` does not match callback root `{callback_root_item_ref}`"
        )));
    }
    if !canonical_sha256(callback_root_content_digest) {
        return Err(hook_integrity(
            "callback capability root content digest is not a canonical lowercase SHA-256 digest",
        ));
    }
    let occurrence_definition_hash = match &hook.occurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStarted {
            definition_hash,
            ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
            definition_hash,
            ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::GraphCompleted {
            definition_hash,
            ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::DirectiveAfterStep {
            definition_hash,
            ..
        }
        | ryeos_runtime::callback::HookDispatchOccurrence::DirectiveContinuation {
            definition_hash,
            ..
        } => definition_hash,
    };
    if occurrence_definition_hash != callback_root_content_digest {
        return Err(hook_integrity(
            "hook definition_hash does not match launch-captured root content digest",
        ));
    }
    if action.thread != "inline" {
        return Err(hook_integrity(format!(
            "hook `{}` requested non-inline thread mode {:?}",
            hook.hook_id, action.thread
        )));
    }
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&action.item_id)
        .map_err(|error| hook_integrity(format!("invalid hook item ref: {error}")))?;
    Ok(canonical)
}

/// V5.5 P2: enforce the callback's composed `effective_caps` against
/// the requested item ref. Uses the unified `Authorizer` for wildcard
/// and implication expansion. An empty cap-set is deny-all — the
/// trust-boundary default for tokens minted without a composition step.
fn enforce_callback_caps(
    item_id: &str,
    effective_caps: &[String],
    authorizer: &ryeos_runtime::authorizer::Authorizer,
) -> std::result::Result<(), crate::dispatch_error::DispatchError> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_id).map_err(|error| {
        crate::dispatch_error::DispatchError::InvalidRef(item_id.to_string(), error.to_string())
    })?;
    let required = format!("ryeos.execute.{}.{}", canonical.kind, canonical.bare_id);

    if effective_caps.is_empty() {
        return Err(crate::dispatch_error::DispatchError::MissingCap { required });
    }

    let policy = AuthorizationPolicy::require_all(&[&required]);
    if authorizer.authorize(effective_caps, &policy).is_err() {
        return Err(crate::dispatch_error::DispatchError::MissingCap { required });
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
    authoritative_chain_root_id: &str,
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
            &params.action.ref_bindings,
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
    if let Ok(child_ref) = ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
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

    let hook_ledger = if let Some(identity) = params.hook_dispatch.as_ref() {
        let callback_root_item_ref = cap.item_ref.as_deref().ok_or_else(|| {
            hook_integrity("hook callback capability is missing its root item ref")
        })?;
        let (seed, request_hash) = hook_dispatch_ledger_seed(
            identity,
            &params.action,
            authoritative_chain_root_id,
            &params.thread_id,
            &cap.project_path,
            &thread_auth.acting_principal,
            &cap.effective_caps,
            &cap.hard_limits,
            cap.depth,
            callback_root_item_ref,
        )?;
        match state
            .state_store
            .reserve_hook_dispatch(&seed)
            .map_err(|error| {
                hook_integrity(format!("could not reserve hook dispatch: {error:#}"))
            })? {
            ryeos_app::state_store::HookDispatchReservation::Execute => {
                Some((seed.dispatch_key, request_hash))
            }
            ryeos_app::state_store::HookDispatchReservation::Replay(response) => {
                return Ok(response);
            }
            ryeos_app::state_store::HookDispatchReservation::PendingUnknown => {
                return Err(hook_integrity(format!(
                    "hook dispatch `{}` has an unknown outcome and cannot be issued again",
                    seed.dispatch_key
                )));
            }
        }
    } else {
        None
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
        ref_bindings: params.action.ref_bindings.clone(),
        acting_principal: caller_principal_id.as_str(),
        project_path: project_path.as_path(),
        provenance: child_provenance,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
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
    match hook_ledger {
        None => result.map_err(anyhow::Error::new),
        Some((dispatch_key, request_hash)) => {
            let response = result.map_err(|error| {
                hook_integrity(format!(
                    "reserved hook dispatch `{dispatch_key}` failed after reservation: {error:#}"
                ))
            })?;
            serde_json::from_value::<
                ryeos_runtime::callback_contract::CallbackDispatchResponse,
            >(response.clone())
            .map_err(|error| {
                hook_integrity(format!(
                    "reserved hook dispatch `{dispatch_key}` returned an invalid callback response: {error}"
                ))
            })?;
            state
                .state_store
                .complete_hook_dispatch(&dispatch_key, &request_hash, &response)
                .map_err(|error| {
                    hook_integrity(format!(
                        "could not complete reserved hook dispatch `{dispatch_key}`: {error:#}"
                    ))
                })?;
            Ok(response)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn hook_dispatch_ledger_seed(
    identity: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
    chain_root_id: &str,
    caller_thread_id: &str,
    validated_project_path: &std::path::Path,
    acting_principal: &str,
    effective_caps: &[String],
    hard_limits: &Value,
    depth: u32,
    callback_root_item_ref: &str,
) -> Result<(ryeos_app::state_store::NewHookDispatch, String)> {
    let mut effective_caps = effective_caps.to_vec();
    effective_caps.sort();
    let dispatch_identity = serde_json::json!({
        "chain_root_id": chain_root_id,
        "occurrence": &identity.occurrence,
        "hook_id": &identity.hook_id,
    });
    let canonical_dispatch_identity = lillux::canonical_json(&dispatch_identity).map_err(|error| {
        hook_integrity(format!(
            "hook dispatch identity cannot be represented as canonical JSON: {error}"
        ))
    })?;
    let dispatch_key = lillux::sha256_hex(canonical_dispatch_identity.as_bytes());
    let request_identity = serde_json::json!({
        "hook_dispatch": identity,
        "action": action,
        "chain_root_id": chain_root_id,
        "validated_project_path": validated_project_path.to_string_lossy(),
        "acting_principal": acting_principal,
        "effective_caps": effective_caps,
        "hard_limits": hard_limits,
        "depth": depth,
        "callback_root_item_ref": callback_root_item_ref,
    });
    let canonical_request_identity = lillux::canonical_json(&request_identity).map_err(|error| {
        hook_integrity(format!(
            "hook dispatch request cannot be represented as canonical JSON: {error}"
        ))
    })?;
    let request_hash = lillux::sha256_hex(canonical_request_identity.as_bytes());
    let seed = ryeos_app::state_store::NewHookDispatch {
        dispatch_key,
        chain_root_id: chain_root_id.to_string(),
        caller_thread_id: caller_thread_id.to_string(),
        event: identity.occurrence.event().to_string(),
        hook_id: identity.hook_id.clone(),
        request_hash: request_hash.clone(),
    };
    Ok((seed, request_hash))
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
            root_content_digest: "0".repeat(64),
            hard_limits: serde_json::json!({"turns": 6, "tokens": 1000}),
            depth: 4,
        };

        let ctx = parent_execution_context_from_capability(&cap);
        assert_eq!(ctx.parent_thread_id, "T-parent");
        assert_eq!(
            ctx.hard_limits,
            serde_json::json!({"turns": 6, "tokens": 1000})
        );
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
            root_content_digest: "0".repeat(64),
            hard_limits: serde_json::json!({}),
            depth: 0,
        };

        assert_eq!(
            callback_dispatch_scopes(&cap),
            vec!["ryeos.execute.knowledge.arc/*".to_string()]
        );
    }

    #[test]
    fn hook_ledger_key_is_chain_occurrence_scoped_and_request_hash_binds_action() {
        let identity = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence:
                ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
                    graph_run_id: "run-1".to_string(),
                    definition_ref: "graph:test/fixture".to_string(),
                    definition_hash: "d".repeat(64),
                    step: 3,
                    node: "audit".to_string(),
                },
            hook_id: "audit-hook".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            context_hash: "c".repeat(64),
        };
        let action = ryeos_runtime::callback::ActionPayload {
            item_id: "tool:test/audit".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: serde_json::json!({"value": 1}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        let (seed_a, request_a) = hook_dispatch_ledger_seed(
            &identity,
            &action,
            "T-root",
            "T-segment-a",
            std::path::Path::new("/project"),
            "principal",
            &["cap:b".to_string(), "cap:a".to_string()],
            &serde_json::json!({"turns": 4}),
            2,
            "graph:test/fixture",
        )
        .unwrap();
        let (seed_b, request_b) = hook_dispatch_ledger_seed(
            &identity,
            &action,
            "T-root",
            "T-segment-b",
            std::path::Path::new("/project"),
            "principal",
            &["cap:a".to_string(), "cap:b".to_string()],
            &serde_json::json!({"turns": 4}),
            2,
            "graph:test/fixture",
        )
        .unwrap();
        assert_eq!(seed_a.dispatch_key, seed_b.dispatch_key);
        assert_eq!(request_a, request_b);

        let mut changed_action = action;
        changed_action.params = serde_json::json!({"value": 2});
        let (changed_seed, changed_request) = hook_dispatch_ledger_seed(
            &identity,
            &changed_action,
            "T-root",
            "T-segment-a",
            std::path::Path::new("/project"),
            "principal",
            &["cap:a".to_string(), "cap:b".to_string()],
            &serde_json::json!({"turns": 4}),
            2,
            "graph:test/fixture",
        )
        .unwrap();
        assert_eq!(seed_a.dispatch_key, changed_seed.dispatch_key);
        assert_ne!(request_a, changed_request);
    }

    #[test]
    fn hook_identity_must_match_launch_captured_root_authority() {
        let hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: ryeos_runtime::callback::HookDispatchOccurrence::DirectiveAfterStep {
                definition_ref: "directive:test/fixture".to_string(),
                definition_hash: "a".repeat(64),
                turn: 1,
            },
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            context_hash: "b".repeat(64),
        };
        let action = ryeos_runtime::callback::ActionPayload {
            item_id: "tool:test/audit".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: serde_json::json!({}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        assert!(validate_hook_identity_authority(
            &hook,
            &action,
            "directive:test/fixture",
            &"a".repeat(64),
        )
        .is_ok());
        assert!(validate_hook_identity_authority(
            &hook,
            &action,
            "directive:test/other",
            &"a".repeat(64),
        )
        .is_err());
        assert!(validate_hook_identity_authority(
            &hook,
            &action,
            "directive:test/fixture",
            &"c".repeat(64),
        )
        .is_err());
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
        assert_eq!(err.code(), "missing_cap");
        assert!(err.to_string().contains("ryeos.execute.tool.foo/bar"));
    }

    #[test]
    fn caps_kind_wildcard_matches_any_id_in_kind() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.*".to_string()];
        assert!(enforce_callback_caps("tool:any/echo", &caps, &auth).is_ok());
        assert!(enforce_callback_caps("tool:other/foo", &caps, &auth).is_ok());
        // Different kind — denied.
        let err = enforce_callback_caps("directive:foo/bar", &caps, &auth).unwrap_err();
        assert_eq!(err.code(), "missing_cap");
    }

    #[test]
    fn caps_exact_match_with_slash_subject() {
        let auth = test_auth();
        // `tool:foo/bar` → required cap `ryeos.execute.tool.foo/bar`.
        // Slash is preserved in subject, matching the canonical format.
        let caps = vec!["ryeos.execute.tool.foo/bar".to_string()];
        assert!(enforce_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        let err = enforce_callback_caps("tool:foo/baz", &caps, &auth).unwrap_err();
        assert_eq!(err.code(), "missing_cap");
    }

    #[test]
    fn caps_invalid_item_id_rejected() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.foo".to_string()];
        let err = enforce_callback_caps("not-a-canonical-ref", &caps, &auth).unwrap_err();
        assert!(
            err.code() == "invalid_ref",
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
        assert!(matches!(
            err,
            crate::dispatch_error::DispatchError::MissingCap { required }
                if required == "ryeos.execute.tool.foobar"
        ));
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
