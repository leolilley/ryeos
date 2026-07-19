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
//! server-side trust derivation, managed-runtime resolution and preparation,
//! the fresh-root child row with its authoritative launch audit, and an
//! acknowledged managed spawn-task handoff (which is waiter-agnostic —
//! `kick_follow_resume_if_ready` is a no-op without a waiter).
//!
//! **Trust.** Every trust-bearing fact is server-side: the acting principal from
//! the validated `thread_auth`, the parent chain root / launch identity from the
//! parent thread row, the caps that bound the child from the parent's validated
//! callback capability. The action only says WHICH child to run and WHAT cohort
//! facets to stamp.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ryeos_app::callback_token::{CallbackCapability, ThreadAuthState};
use ryeos_app::execution_provenance::ExecutionProvenance;
use ryeos_app::launch_metadata::{
    FollowLaunchWindow, PersistedParentExecutionContext, ResumeContext, RuntimeLaunchMetadata,
};
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
    mut child_provenance: ExecutionProvenance,
    child_item_ref: &str,
    child_ref_bindings: &BTreeMap<String, String>,
    child_parameters: &Value,
    facets: Option<&Value>,
    launch_window: Option<&ryeos_runtime::callback::LaunchWindow>,
    operation_id: &str,
) -> Result<Value> {
    let parent_thread_id = cap.thread_id.clone();

    let child_ref = CanonicalRef::parse(child_item_ref)
        .with_context(|| format!("detach: invalid child item ref '{child_item_ref}'"))?;
    let request_identity = json!({
        "item_ref": child_ref.to_string(),
        "ref_bindings": child_ref_bindings,
        "parameters": child_parameters,
        "facets": facets,
        "launch_window": launch_window,
    });
    let request_hash = lillux::sha256_hex(
        lillux::canonical_json(&request_identity)
            .context("detach: canonicalize operation request")?
            .as_bytes(),
    );
    // Parent thread row → chain root + site identity. Never trust the caller
    // for these. This launch's mode is the daemon-selected detached mode below,
    // not the mode under which the parent itself was launched.
    let parent = state
        .threads
        .get_thread(&parent_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("detach: parent thread not found: {parent_thread_id}"))?;
    let parent_lifecycle_authority = state
        .state_store
        .get_launch_metadata(&parent_thread_id)?
        .and_then(|metadata| metadata.resume_context)
        .map(|resume| resume.lifecycle_authority)
        .ok_or_else(|| {
            anyhow::anyhow!("detach: parent {parent_thread_id} has no sealed lifecycle authority")
        })?;
    if !parent_lifecycle_authority.permits_durable_handoff() {
        anyhow::bail!("detach: request-scoped execution cannot spawn a durable child");
    }

    // The callback capability carries the chain root it was minted under; confirm
    // it against authoritative state before minting a linked child.
    cap.assert_chain_root(&parent.chain_root_id)?;
    let persisted_launch_window = launch_window.map(|window| FollowLaunchWindow {
        key: format!("{parent_thread_id}:{}", window.key),
        width: window.width,
    });
    let child_thread_id = state.state_store.reserve_detached_spawn_intent(
        operation_id,
        &parent_thread_id,
        &request_hash,
        &new_thread_id(),
        None,
    )?;
    let reserved_intent = state
        .state_store
        .get_detached_spawn_intent(operation_id)?
        .ok_or_else(|| anyhow::anyhow!("detach: reserved operation disappeared: {operation_id}"))?;

    // A retry after the child birth transaction must repair lineage and launch
    // progress, never mint another child. The immutable launch metadata saved
    // with the row is sufficient for the normal recovery launcher.
    if let Some(child) = state.threads.get_thread(&child_thread_id)? {
        if reserved_intent.child_project_authority.is_none() {
            anyhow::bail!(
                "detach: child {child_thread_id} exists without sealed project authority"
            );
        }
        let inherited_stop =
            state
                .state_store
                .record_child_link(&parent_thread_id, &child_thread_id, "dispatch")?;
        if inherited_stop.is_some() {
            crate::execution::process_attachment::finalize_requested_stop_if_present(
                state,
                &child_thread_id,
            )?;
            anyhow::bail!(
                "detach: parent {parent_thread_id} was stop-requested during child admission"
            );
        }
        let queued = if child.status == "created" {
            if let Some(window) = persisted_launch_window.as_ref() {
                let admitted = state.state_store.launch_window_enqueue(
                    &child_thread_id,
                    &window.key,
                    window.width,
                    crate::execution::launch::global_live_fanout_limit(),
                    lillux::time::timestamp_millis(),
                )?;
                for admitted_child in &admitted {
                    crate::execution::launch::launch_admitted_window_member(state, admitted_child);
                }
                !admitted.iter().any(|id| id == &child_thread_id)
            } else {
                crate::execution::launch::launch_admitted_window_member(state, &child_thread_id);
                false
            }
        } else {
            false
        };
        return Ok(detached_callback_response(
            &child_thread_id,
            if queued { "created" } else { &child.status },
            queued,
        ));
    }

    let parent_project_authority = cap
        .provenance
        .execution_project_authority(&cap.effective_caps)?;
    let mut explicit_child_generation = None;
    let child_project_authority =
        if let Some(authority) = reserved_intent.child_project_authority.clone() {
            authority
        } else {
            match parent_project_authority.child_policy() {
                ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit => {
                    parent_project_authority.clone().for_child()?
                }
                ryeos_state::objects::ChildProjectAuthorityPolicy::PinAtSpawn { realization } => {
                    let snapshot_hash = if let Some(snapshot_hash) =
                        parent_project_authority.base_snapshot_projection()
                    {
                        snapshot_hash.to_string()
                    } else {
                        let generation = crate::execution::capture_live_project_snapshot(
                            state,
                            cap.provenance.original_project_path(),
                            &parent.origin_site_id,
                            "detached-pin-at-spawn",
                        )?;
                        let snapshot_hash = generation.snapshot_hash().to_string();
                        explicit_child_generation = Some(generation);
                        snapshot_hash
                    };
                    crate::execution::derive_pinned_child_authority(
                        &parent_project_authority,
                        snapshot_hash,
                        realization,
                        &cap.effective_caps,
                    )?
                }
            }
        };

    // A borrowed daemon workspace cannot be reconstructed safely from its old
    // path: manufacturing a second TempDirGuard would race the parent's real
    // guard. Inherit only the parent's immutable snapshot and reconstruct the
    // child into a fresh non-lineage checkout after a crash/queued launch.
    let launch_snapshot_hash = parent_project_authority
        .base_snapshot_projection()
        .map(str::to_owned);
    let inherited_generation = if matches!(
        &parent_project_authority,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        }
    ) {
        launch_snapshot_hash
            .as_deref()
            .map(|base| {
                let frozen = crate::execution::seal_callback_workspace_generation(
                    state,
                    &parent_thread_id,
                    cap.provenance.effective_path(),
                    base,
                )?;
                crate::execution::ensure_control_tree_unchanged(
                    state,
                    base,
                    frozen.snapshot_hash(),
                )?;
                Ok::<_, anyhow::Error>(frozen)
            })
            .transpose()?
    } else {
        None
    };
    let inherited_snapshot_hash = inherited_generation
        .as_ref()
        .map(|generation| generation.snapshot_hash().to_string())
        .or_else(|| {
            explicit_child_generation
                .as_ref()
                .map(|generation| generation.snapshot_hash().to_string())
        })
        .or_else(|| {
            child_project_authority
                .base_snapshot_projection()
                .map(str::to_owned)
        });

    // Execute authority over the child was already enforced at the callback trust
    // boundary (`enforce_callback_caps` in the dispatch handler) against this same
    // item_id; no second check is needed here — the parent's `effective_caps`
    // bound the child under `FollowChildHybrid` at launch exactly as for follow.
    if let Some(snapshot_hash) = inherited_snapshot_hash.as_deref() {
        let child_context = crate::execution::project_source::resolve_pinned_snapshot_context(
            state,
            snapshot_hash,
            cap.provenance.original_project_path().to_path_buf(),
            &child_thread_id,
        )?;
        let child_lifeline = child_context
            .temp_dir
            .ok_or_else(|| anyhow::anyhow!("detach: child workspace has no lifecycle guard"))?;
        child_provenance = if matches!(
            &parent_project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        ) {
            cap.provenance.clone_for_pinned_child_workspace(
                child_context.effective_path,
                child_lifeline,
                snapshot_hash.to_string(),
                child_project_authority.clone(),
            )?
        } else {
            cap.provenance
                .clone_for_borrowed_child_workspace(child_context.effective_path, child_lifeline)
        };
    }

    // Managed-runtime children only: a child kind served by a registered runtime
    // resolves here; a leaf tool/service kind does not. The same lookup yields the
    // child row's `native:<binary>` executor identity.
    let child_engine = child_provenance.request_engine();
    let child_runtime = child_engine
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
    let child_thread_profile = child_engine
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
        scopes: cap.effective_caps.clone(),
    });
    let child_preflight = ryeos_app::thread_lifecycle::preflight_root_execution(
        ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
            engine: child_engine,
            node_history_policy: &state.node_history_policy,
            site_id: &parent.current_site_id,
            project_path: child_provenance.effective_path(),
            item_ref: child_item_ref,
            launch_mode: "detached",
            parameters: child_parameters.clone(),
            ref_bindings: child_ref_bindings.clone(),
            requested_by: thread_auth.acting_principal.clone(),
            usage_subject: None,
            usage_subject_asserted_by: None,
            caller_scopes: cap.effective_caps.clone(),
            validate_only: false,
            creates_chain_root: true,
        },
    )
    .context("detach: verified child history-policy preflight")?;
    let child_root_admission = child_preflight.root_admission;
    let child_execution = child_root_admission.execution_request(
        child_executor_ref.clone(),
        "detached".to_string(),
        child_parameters.clone(),
    )?;
    let sealed_root_request =
        SealedRootExecutionRequest::capture(&child_execution, child_runtime_ref.clone())?;

    // Build the complete immutable launch identity and authoritative generic
    // launch authority before minting any observable row. `effective_caps`
    // carries the PARENT's caps — the bounding authority handed to
    // `CapabilityPolicy::FollowChildHybrid`.
    let project_context = ProjectContext::LocalPath {
        path: child_provenance.effective_path().to_path_buf(),
    };
    let stable_project_identity = ryeos_app::launch_metadata::StableProjectIdentity::from_path(
        cap.provenance.original_project_path(),
        &parent.origin_site_id,
    )?;
    let project_authority = child_project_authority.clone();
    let local_overlay_root = matches!(
        project_authority.environment(),
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { .. }
    )
    .then(|| cap.provenance.original_project_path().to_path_buf());
    let mut meta = RuntimeLaunchMetadata::default()
        .with_resume_context(ResumeContext {
            kind: child_thread_profile.clone(),
            item_ref: child_item_ref.to_string(),
            ref_bindings: child_ref_bindings.clone(),
            launch_mode: "detached".to_string(),
            parameters: child_parameters.clone(),
            // Resume identity derives from validated server-side provenance, never
            // the request body — same rule as follow.
            project_context,
            project_authority,
            lifecycle_authority: parent_lifecycle_authority,
            stable_project_identity: Some(stable_project_identity),
            local_overlay_root,
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
    let launch_parent_context = crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    };
    meta.follow_parent_context = Some(PersistedParentExecutionContext {
        parent_thread_id: launch_parent_context.parent_thread_id.clone(),
        hard_limits: launch_parent_context.hard_limits.clone(),
        depth: launch_parent_context.depth,
    });
    meta.follow_launch_window = persisted_launch_window.clone();
    let prepared = crate::execution::launch::prepare_follow_child_launch(
        state,
        &child_thread_id,
        &meta,
        child_provenance,
        launch_parent_context,
    )
    .await?;
    let mut initial_events = prepared.initial_audit_events()?;
    if let Some(Value::Object(map)) = facets {
        for (key, value) in map {
            if key.trim().is_empty() {
                continue;
            }
            let value = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            initial_events.push(ryeos_app::state_store::NewEventRecord {
                event_type: RuntimeEventType::ThreadFacetSet.as_str().to_string(),
                storage_class: RuntimeEventType::ThreadFacetSet
                    .storage_class()
                    .as_str()
                    .to_string(),
                payload: json!({ "key": key, "value": value }),
            });
        }
    }
    state
        .state_store
        .bind_detached_spawn_project_authority(operation_id, &child_project_authority)?;
    state
        .threads
        .create_root_thread_with_events_and_launch_metadata(
            &child_thread_id,
            prepared.resolved_request(),
            child_project_authority.clone(),
            initial_events,
            Some(prepared.launch_metadata()),
        )?;
    let prepared = prepared.with_persisted_birth_audit();
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
    if let Some(generation) = explicit_child_generation.take() {
        generation.publish()?;
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
    if let Some(w) = persisted_launch_window.as_ref() {
        let admitted = match state.state_store.launch_window_enqueue(
            &child_thread_id,
            &w.key,
            w.width,
            crate::execution::launch::global_live_fanout_limit(),
            lillux::time::timestamp_millis(),
        ) {
            Ok(admitted) => admitted,
            Err(error) => {
                settle_detached_pre_handoff_failure(
                    state,
                    &child_thread_id,
                    "launch_window_enqueue_failed",
                    &error.to_string(),
                )?;
                return Err(error).context("detach: enqueue child in launch window");
            }
        };
        queued = !admitted.iter().any(|c| c == &child_thread_id);
        co_admitted = admitted
            .into_iter()
            .filter(|c| c != &child_thread_id)
            .collect();
    }

    // ── Launch detached ─────────────────────────────────────────────────────
    // A non-queued launch is acknowledged only once the runtime spawn task owns
    // the prepared authority and secrets. The outer task remains detached for
    // the runtime's lifetime, but preparation and pre-handoff failures are
    if !queued {
        let launch_state = state.clone();
        let launch_child_id = child_thread_id.clone();
        let (launch_handoff, launch_ready) = crate::execution::launch::LaunchHandoff::channel();
        tokio::spawn(async move {
            if let Err(e) = crate::execution::launch::launch_prepared_follow_child(
                launch_state,
                &launch_child_id,
                prepared,
                &launch_handoff,
            )
            .await
            {
                tracing::error!(
                    child_thread_id = %launch_child_id,
                    error = %e,
                    "detached child launch failed",
                );
            }
        });
        let handed_off = match launch_ready.await {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(failure)) => {
                settle_detached_pre_handoff_failure(
                    state,
                    &child_thread_id,
                    &failure.code,
                    &failure.message,
                )?;
                anyhow::bail!(
                    "detach: child launch rejected before handoff ({}): {}",
                    failure.code,
                    failure.message
                );
            }
            Err(error) => {
                settle_detached_pre_handoff_failure(
                    state,
                    &child_thread_id,
                    "launch_handoff_closed",
                    &error.to_string(),
                )?;
                return Err(error).context("detach: child launch task closed before spawn handoff");
            }
        };
        if handed_off != child_thread_id {
            settle_detached_pre_handoff_failure(
                state,
                &child_thread_id,
                "launch_handoff_identity_mismatch",
                &format!("handed off {handed_off}"),
            )?;
            anyhow::bail!(
                "detach: child launch handed off unexpected thread {handed_off} (expected {child_thread_id})"
            );
        }
    }
    // Members of the same window that this enqueue admitted alongside (slots
    // opened without a kick landing) launch on the reconcile-parity path.
    for other in &co_admitted {
        crate::execution::launch::launch_admitted_window_member(state, other);
    }
    if let Some(generation) = inherited_generation {
        generation.publish()?;
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
    Ok(detached_callback_response(
        &child_thread_id,
        if queued { "created" } else { "running" },
        queued,
    ))
}

fn detached_callback_response(child_thread_id: &str, status: &str, queued: bool) -> Value {
    json!({
        "thread": {
            "thread_id": child_thread_id,
            "status": status,
            "detached": true,
        },
        "result": {
            "detached": true,
            "child_thread_id": child_thread_id,
            "queued": queued,
        },
    })
}

fn settle_detached_pre_handoff_failure(
    state: &AppState,
    child_thread_id: &str,
    code: &str,
    reason: &str,
) -> Result<()> {
    let outcome = crate::dispatch::finalize_child_link_failure_if_current(
        state,
        child_thread_id,
        json!({ "code": code, "reason": reason }),
    )?;
    if outcome.is_settled() {
        crate::execution::launch::kick_follow_resume_if_ready(state, child_thread_id);
        crate::execution::launch::kick_launch_window_for_terminal(state, child_thread_id);
    }
    Ok(())
}
