//! `detached` dispatch — launch a lineage-linked, cohort-tagged child that the
//! calling parent does NOT wait on.
//!
//! This is the native fanout primitive: a graph node with `detach: true` (a
//! `foreach → launch` body) asks the daemon to spawn a real managed child that
//! runs concurrently while the parent walks on. Unlike CLI `--async` (a fresh
//! UNLINKED root) the child is LINEAGE-LINKED to its parent — the launcher's
//! `parent_execution_context` records the `dispatch` child-link edge and emits
//! `child_thread_spawned` — so `foreach → launch` produces a queryable tagged
//! tree instead of orphaned roots. Optional `facets` stamp cohort identity
//! (`fleet=<run id>`, `game=<id>`) on the child so `threads.list --facet` can
//! query the cohort.
//!
//! It is [`spawn_follow_child`](super::spawn_follow_child) minus the suspend
//! machinery: no waiter reservation, no parent successor, no `mark_waiting`, and
//! no native-resume gate — the parent never suspends, so it needs neither a
//! durable catch nor to be checkpoint-resumable. What it KEEPS is identical: the
//! server-side trust derivation, managed-runtime resolution, the fresh-root
//! child row + seeded launch identity, and the detached `launch_follow_child`
//! spawn (which is waiter-agnostic — `kick_follow_resume_if_ready` is a no-op
//! without a waiter).
//!
//! **Trust.** Every trust-bearing fact is server-side: the acting principal from
//! the validated `thread_auth`, the parent chain root / launch identity from the
//! parent thread row, the caps that bound the child from the parent's validated
//! callback capability. The action only says WHICH child to run and WHAT cohort
//! facets to stamp.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ryeos_app::callback_token::{CallbackCapability, ThreadAuthState};
use ryeos_app::event_store_service::{EventAppendItem, EventAppendParams};
use ryeos_app::execution_provenance::ExecutionProvenance;
use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{new_thread_id, SealedRootExecutionRequest};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
use ryeos_runtime::events::RuntimeEventType;

/// Admit and launch a detached, lineage-linked child under the calling parent.
///
/// `child_provenance` is the parent's already-borrowed child provenance (the
/// same value the inline path hands to `dispatch`), moved into the detached
/// launch so the child inherits the parent's pushed-head / effective workspace /
/// request engine — not the root-live-fs fallback. Returns the minted child
/// thread id; the child runs concurrently, its terminal outcome captured by the
/// normal thread-terminal path (the parent does not consume it).
// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_detached_child(
    state: &AppState,
    thread_auth: &ThreadAuthState,
    cap: &CallbackCapability,
    child_provenance: ExecutionProvenance,
    child_item_ref: &str,
    child_parameters: &Value,
    facets: Option<&Value>,
    launch_window: Option<&ryeos_runtime::callback::LaunchWindow>,
) -> Result<Value> {
    let parent_thread_id = cap.thread_id.clone();

    // Parent thread row → chain root + launch identity (launch_mode, site ids).
    // Never trust the caller for these.
    let parent = state
        .threads
        .get_thread(&parent_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("detach: parent thread not found: {parent_thread_id}"))?;

    // The callback capability carries the chain root it was minted under; confirm
    // it against authoritative state before minting a linked child.
    cap.assert_chain_root(&parent.chain_root_id)?;

    // A borrowed daemon workspace cannot be reconstructed safely from its old
    // path: manufacturing a second TempDirGuard would race the parent's real
    // guard. Inherit only the parent's immutable snapshot and reconstruct the
    // child into a fresh non-lineage checkout after a crash/queued launch.
    let inherited_snapshot_hash = if cap.provenance.workspace_lifeline().is_some() {
        Some(
            state
                .state_store
                .get_launch_metadata(&parent_thread_id)?
                .and_then(|metadata| metadata.resume_context)
                .and_then(|resume| {
                    resume
                        .durable_project_snapshot_hash()
                        .map(str::to_owned)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "detach: parent {parent_thread_id} owns an ephemeral workspace but has no durable project snapshot"
                    )
                })?,
        )
    } else {
        None
    };

    // Execute authority over the child was already enforced at the callback trust
    // boundary (`enforce_callback_caps` in the dispatch handler) against this same
    // item_id; no second check is needed here — the parent's `effective_caps`
    // bound the child under `FollowChildHybrid` at launch exactly as for follow.
    let child_ref = CanonicalRef::parse(child_item_ref)
        .with_context(|| format!("detach: invalid child item ref '{child_item_ref}'"))?;

    // Managed-runtime children only: a child kind served by a registered runtime
    // resolves here; a leaf tool/service kind does not. The same lookup yields the
    // child row's `native:<binary>` executor identity.
    let child_runtime = child_provenance
        .request_engine()
        .runtimes
        .resolve_for_launch(None, &child_ref.kind)
        .map_err(|e| {
            anyhow::anyhow!(
                "detach: child kind '{}' has no managed runtime — a detached child must be a \
                 managed runtime execution: {e}",
                child_ref.kind
            )
        })?;
    let child_executor_ref = format!(
        "native:{}",
        crate::dispatch::strip_binary_ref_prefix(&child_runtime.yaml.binary_ref)
            .map_err(|e| anyhow::anyhow!("detach: {e}"))?
    );
    let child_runtime_ref = child_runtime.canonical_ref.to_string();

    // The thread ROW kind is the child kind's THREAD PROFILE (e.g. `graph` →
    // `graph_run`), not the item kind: profile-driven continuation / resume /
    // operator behavior keys off the profile name, so a fresh child row and its
    // captured identity must carry the profile, exactly like a normal launch.
    let child_thread_profile = child_provenance
        .request_engine()
        .kinds
        .get(&child_ref.kind)
        .and_then(|schema| schema.execution())
        .and_then(|exec| exec.thread_profile.as_ref())
        .map(|tp| tp.name.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "detach: child kind '{}' has no execution.thread_profile",
                child_ref.kind
            )
        })?;

    // ── Child root row (created, NOT launched) + seeded launch identity ─────
    // A detached child is a FRESH ROOT: its own chain root, no upstream braid.
    // Its lineage to the parent is the `dispatch` child-link edge the launcher
    // records via `parent_execution_context`, not a shared chain root.
    let requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: thread_auth.acting_principal.clone(),
        scopes: thread_auth.caller_scopes.clone(),
    });
    let child_preflight = ryeos_app::thread_lifecycle::preflight_root_execution(
        ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
            engine: child_provenance.request_engine(),
            node_history_policy: &state.node_history_policy,
            site_id: &parent.current_site_id,
            project_path: child_provenance.effective_path(),
            item_ref: child_item_ref,
            launch_mode: &parent.launch_mode,
            parameters: child_parameters.clone(),
            requested_by: thread_auth.acting_principal.clone(),
            usage_subject: None,
            usage_subject_asserted_by: None,
            caller_scopes: thread_auth.caller_scopes.clone(),
            validate_only: false,
            creates_chain_root: true,
        },
    )
    .context("detach: verified child history-policy preflight")?;
    let child_root_admission = child_preflight.root_admission;
    let child_execution = child_root_admission.execution_request(
        child_executor_ref.clone(),
        parent.launch_mode.clone(),
        child_parameters.clone(),
    )?;
    let sealed_root_request =
        SealedRootExecutionRequest::capture(&child_execution, child_runtime_ref.clone())?;
    let child_thread_id = new_thread_id();
    state
        .threads
        .create_root_thread_with_id(&child_thread_id, &child_execution)?;

    // Persist two agreeing authorities before launch: ResumeContext carries the
    // envelope/provenance identity and the sealed request carries the exact
    // verified fresh-root subject. `effective_caps` is the PARENT's bounding
    // authority for `CapabilityPolicy::FollowChildHybrid`; the child's composed
    // caps replace it after first-launch policy resolution.
    let meta = RuntimeLaunchMetadata::default()
        .with_resume_context(ResumeContext {
            kind: child_thread_profile.clone(),
            item_ref: child_item_ref.to_string(),
            launch_mode: parent.launch_mode.clone(),
            parameters: child_parameters.clone(),
            // Resume identity derives from validated server-side provenance, never
            // the request body — same rule as follow.
            project_context: ProjectContext::LocalPath {
                path: cap.provenance.effective_path().to_path_buf(),
            },
            original_snapshot_hash: inherited_snapshot_hash,
            // A detached child borrows the parent's workspace; it never owns snapshot
            // lineage, so no pushed-head identity is seeded.
            original_pushed_head_ref: None,
            // The parent's state-root override carries to the child so its
            // state/callback anchor stays isolated with the parent's.
            state_root: cap
                .provenance
                .state_root_override()
                .map(|p| p.to_path_buf()),
            current_site_id: parent.current_site_id.clone(),
            origin_site_id: parent.origin_site_id.clone(),
            requested_by: requested_by.clone(),
            execution_hints: ExecutionHints::default(),
            effective_caps: cap.effective_caps.clone(),
            executor_ref: Some(child_executor_ref.clone()),
            runtime_ref: Some(child_runtime_ref.clone()),
        })
        .with_sealed_root_request(sealed_root_request);
    state
        .state_store
        .seed_launch_metadata(&child_thread_id, &meta)?;
    let inherited_stop =
        match state
            .state_store
            .record_child_link(&parent_thread_id, &child_thread_id, "dispatch")
        {
            Ok(inherited_stop) => inherited_stop,
            Err(error) => {
                let cleanup = crate::dispatch::finalize_child_link_failure_if_current(
                    state,
                    &child_thread_id,
                    json!({
                        "code": "child_link_failed",
                        "reason": error.to_string(),
                    }),
                );
                match cleanup {
                    Ok(outcome) if outcome.is_settled() => {
                        crate::execution::launch::kick_follow_resume_if_ready(
                            state,
                            &child_thread_id,
                        );
                        crate::execution::launch::kick_launch_window_for_terminal(
                            state,
                            &child_thread_id,
                        );
                    }
                    Ok(outcome) => tracing::warn!(
                        child_thread_id,
                        ?outcome,
                        "preserved concurrently advanced detached child after lineage failure"
                    ),
                    Err(cleanup_error) => {
                        return Err(anyhow::anyhow!(
                            "detach: record child lineage under parent {parent_thread_id}: \
                             {error}; conditional child cleanup also failed: {cleanup_error}"
                        ));
                    }
                }
                return Err(error).context(format!(
                    "detach: record child lineage under parent {parent_thread_id}"
                ));
            }
        };
    if inherited_stop.is_some() {
        crate::execution::process_attachment::finalize_requested_stop_if_present(
            state,
            &child_thread_id,
        )?;
        anyhow::bail!(
            "detach: parent {parent_thread_id} was stop-requested during child admission"
        );
    }

    // ── Cohort facets ───────────────────────────────────────────────────────
    // Stamp `(key, value)` tags BEFORE launch so a `threads.list --facet` query
    // sees the cohort the instant the child appears. Event-backed (survives a
    // projection rebuild), exactly like `threads.set_facet`.
    if let Some(Value::Object(map)) = facets {
        for (key, value) in map {
            if key.trim().is_empty() {
                continue;
            }
            let value_str = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            state
                .events
                .append(&EventAppendParams {
                    thread_id: child_thread_id.clone(),
                    event: EventAppendItem {
                        event_type: RuntimeEventType::ThreadFacetSet.as_str().to_string(),
                        storage_class: RuntimeEventType::ThreadFacetSet
                            .storage_class()
                            .as_str()
                            .to_string(),
                        payload: json!({ "key": key, "value": value_str }),
                    },
                })
                .with_context(|| format!("detach: stamping facet '{key}' on {child_thread_id}"))?;
        }
    }

    // ── Launch admission (bounded fanout) ───────────────────────────────────
    // A windowed spawn minted its row / identity / facets above
    // unconditionally — the launch table and cohort are complete the moment
    // the parent's dispatch returns — but the LAUNCH itself is admitted
    // through the window: at most `width` member chains launched-and-live at
    // once (plus the daemon-global live-fanout ceiling). A member that is
    // not admitted stays `created`; a live member's hard terminal admits it
    // (`kick_launch_window_for_terminal`), with the startup sweep as the
    // crash backstop. The window key is namespaced under the parent thread
    // id so a caller can only pace its own children.
    let mut queued = false;
    let mut co_admitted: Vec<String> = Vec::new();
    if let Some(w) = launch_window {
        let window_key = format!("{parent_thread_id}:{}", w.key);
        let admitted = state.state_store.launch_window_enqueue(
            &child_thread_id,
            &window_key,
            w.width,
            crate::execution::launch::global_live_fanout_limit(),
            lillux::time::timestamp_millis(),
        )?;
        queued = !admitted.iter().any(|c| c == &child_thread_id);
        co_admitted = admitted
            .into_iter()
            .filter(|c| c != &child_thread_id)
            .collect();
    }

    // ── Launch detached ─────────────────────────────────────────────────────
    // Fire-and-forget: preparation persists the launch claim before returning,
    // so a daemon loss is safe for the reconcile sweep to re-drive. The launch uses the parent's
    // BORROWED-CHILD provenance (moved in) — pushed-head / effective workspace /
    // request engine preserved. Parent execution ceiling from the VALIDATED cap
    // (never `child_parameters`): the child launches clamped to the parent's hard
    // limits at parent depth + 1, recording the `dispatch` child-link edge.
    if !queued {
        let launch_parent_context = crate::dispatch::ParentExecutionContext {
            parent_thread_id: cap.thread_id.clone(),
            hard_limits: cap.hard_limits.clone(),
            depth: cap.depth,
        };
        if let crate::execution::launch::RecoveryLaunchOutcome::Skipped(reason) =
            crate::execution::launch::prepare_and_spawn_follow_child(
                state.clone(),
                &child_thread_id,
                Some(child_provenance),
                Some(launch_parent_context),
            )?
        {
            tracing::debug!(
                child_thread_id = %child_thread_id,
                reason,
                "detached child launch skipped"
            );
        }
    }
    // Members of the same window that this enqueue admitted alongside (slots
    // opened without a kick landing) launch on the reconcile-parity path.
    for other in &co_admitted {
        crate::execution::launch::launch_admitted_window_member(state, other);
    }

    tracing::info!(
        parent_thread_id = %parent_thread_id,
        child_thread_id = %child_thread_id,
        child_item_ref = %child_item_ref,
        server_principal = %thread_auth.acting_principal,
        queued,
        "detached child spawned; parent continues",
    );

    // Conform to the `CallbackDispatchResponse { thread, result }` envelope the
    // graph-side client deserializes (deny_unknown_fields — no bare extra keys).
    // `thread` is the running-child snapshot the walker reads `thread_id` from to
    // emit `child_thread_spawned` + record the dispatch edge; `result` is the bare
    // value a `foreach → launch` body sees (`${result.child_thread_id}`) — there
    // is no leaf terminal result, the parent does not consume the child.
    Ok(json!({
        "thread": {
            "thread_id": child_thread_id,
            "status": if queued { "created" } else { "running" },
            "detached": true,
        },
        "result": {
            "detached": true,
            "child_thread_id": child_thread_id,
            "queued": queued,
        },
    }))
}
