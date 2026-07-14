//! `runtime.spawn_follow_child` — the daemon-managed follow admission + spawn.
//!
//! A graph node with `follow: true` asks the daemon to launch a detached CHILD
//! execution and suspend the calling parent until the child's whole continuation
//! chain reaches terminal. This handler is the trust boundary and the ordered,
//! idempotent spawn that sets that up. Capturing the child's terminal outcome and
//! resuming the suspended parent are separate concerns handled elsewhere (the
//! child-terminal hook + the reconcile/wakeup sweep); this handler only admits and
//! spawns.
//!
//! **Trust.** Every trust-bearing fact is derived from validated server-side
//! state, never from the request body: the acting principal from the validated
//! `thread_auth_token`, the parent chain root / site identity from the parent
//! thread row, the caps that bound the child from the parent's validated
//! callback token (source-aware follow bounding). The request only says WHICH
//! follow this is and WHAT child to run.
//!
//! **Ordering.** Reserve the waiter, mint the child root row (created, not
//! launched), create the parent successor row (which settles the parent
//! `continued`), mark the waiter `waiting` — and ONLY THEN launch the child
//! detached. The child can never reach terminal before a durable waiter exists to
//! catch it.
//!
//! **Idempotency.** Get-or-create by `follow_key`; each step is guarded by the
//! waiter's recorded IDs so a same-call re-drive converges rather than
//! duplicating. Recovery from a crash BETWEEN steps is the reconcile sweep's job,
//! not this handler's — it owns the happy-path ordering plus same-call
//! idempotency, and provisions the launch entry point the sweep re-drives through.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_app::launch_metadata::{
    FollowLaunchWindow, PersistedParentExecutionContext, ResumeContext, RuntimeLaunchMetadata,
};
use ryeos_app::runtime_db::{follow_child_spec_hash, follow_phase, NewFollowWaiter};
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_app::thread_lifecycle::{new_thread_id, ThreadCreateParams};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
use ryeos_runtime::authorizer::{canonical_cap, AuthorizationPolicy};

/// Bound on A→B→C→… follow recursion, enforced ONLY here at admission by walking
/// the server-side follow-waiter lineage (never a caller-supplied depth). Distinct
/// from the autonomous-segment continuation depth (that bounds one execution
/// segment-cutting itself); this bounds how deep follow nesting may go.
const MAX_FOLLOW_NESTING_DEPTH: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnFollowChildParams {
    callback_token: String,
    thread_auth_token: String,
    /// The caller's own thread — the graph (parent) issuing the follow.
    thread_id: String,
    project_path: String,
    graph_run_id: String,
    follow_node: String,
    step_count: i64,
    #[serde(default)]
    child_item_ref: Option<String>,
    #[serde(default)]
    child_parameters: Value,
    #[serde(default)]
    children: Option<Vec<ryeos_runtime::callback::FollowChildSpec>>,
    #[serde(default)]
    launch_window_width: Option<u32>,
    #[serde(default)]
    frontier_id: Option<String>,
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: SpawnFollowChildParams = serde_json::from_value(params.clone())
        .context("invalid runtime.spawn_follow_child params")?;

    let fanout = params.children.is_some();
    let children = match (params.child_item_ref.as_ref(), params.children.as_ref()) {
        (Some(item_ref), None) => vec![ryeos_runtime::callback::FollowChildSpec {
            item_ref: item_ref.clone(),
            parameters: params.child_parameters.clone(),
            facets: None,
        }],
        (None, Some(children)) if !children.is_empty() => children.clone(),
        _ => {
            bail!("follow: exactly one of child_item_ref or a nonempty children cohort is required")
        }
    };
    if params.launch_window_width == Some(0) {
        bail!("follow: launch_window_width must be greater than zero");
    }
    if !fanout && params.launch_window_width.is_some() {
        bail!("follow: launch_window_width is only valid for a cohort");
    }

    let parent_thread_id = params.thread_id.clone();
    let project_path = std::path::PathBuf::from(&params.project_path);

    // ── Trust derivation (all server-side) ──────────────────────────────────
    // Parent callback token → the PARENT's effective caps (bound the child under
    // `FollowChildHybrid`) + provenance. Validated against the parent thread +
    // project path exactly like `runtime.dispatch_action`.
    let cap =
        state
            .callback_tokens
            .validate(&params.callback_token, &parent_thread_id, &project_path)?;

    // Per-request identity proof → the server-side acting principal. The request
    // body carries no principal field (`deny_unknown_fields`) so it cannot spoof
    // one; the principal is read strictly from validated state.
    let thread_auth = state
        .thread_auth
        .validate(&params.thread_auth_token, &parent_thread_id)?;

    // Parent thread row → chain root, site identity, launch identity. Never trust
    // the caller for these.
    let parent = state
        .threads
        .get_thread(&parent_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("follow: parent thread not found: {parent_thread_id}"))?;

    // The callback token carries the chain root it was minted under; confirm it
    // against authoritative state before wiring a cross-chain follow edge.
    cap.assert_chain_root(&parent.chain_root_id)?;

    // Follow suspends the parent into a follow-resume successor that is later
    // resumed from its checkpoint with the child's result injected — only a
    // native-resume parent can host that. Gate on that DECLARED capability (never
    // a kind identity): a parent that cannot be checkpoint-resumed could never be
    // woken to consume the child, so it must not be allowed to suspend for follow.
    let parent_launch_metadata = state.state_store.get_launch_metadata(&parent_thread_id)?;
    let parent_is_native_resume = parent_launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.native_resume.as_ref())
        .is_some();
    if !parent_is_native_resume {
        bail!(
            "follow: parent {parent_thread_id} is not a native-resume execution; \
             runtime.spawn_follow_child requires a checkpoint-resumable parent"
        );
    }
    let inherited_snapshot_hash = if cap.provenance.workspace_lifeline().is_some() {
        Some(
            parent_launch_metadata
                .as_ref()
                .and_then(|metadata| metadata.resume_context.as_ref())
                .and_then(ResumeContext::durable_project_snapshot_hash)
                .map(str::to_owned)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: parent {parent_thread_id} owns an ephemeral workspace but has no durable project snapshot"
                    )
                })?,
        )
    } else {
        None
    };

    // ── Admission (authorize before resource resolution; before any mutation) ─
    let mut resolved_children = Vec::with_capacity(children.len());
    for child in &children {
        let child_ref = CanonicalRef::parse(&child.item_ref)
            .with_context(|| format!("follow: invalid child item_ref '{}'", child.item_ref))?;

        // Parent execute authority over the child (wildcard-aware), checked FIRST so
        // an unauthorized follow is refused before any runtime resolution. The
        // follow-child launch policy re-checks this too, but fail fast here so a
        // parent that could never dispatch the child never suspends behind it.
        let child_execute_cap = canonical_cap(&child_ref.kind, &child_ref.bare_id, "execute");
        let policy = AuthorizationPolicy::require_all(&[&child_execute_cap]);
        if state
            .authorizer
            .authorize(&cap.effective_caps, &policy)
            .is_err()
        {
            bail!(
            "follow admission denied: parent lacks execute authority '{child_execute_cap}' over \
             child '{}'",
            child.item_ref
        );
        }

        // Managed-runtime children only: a child kind served by a registered runtime
        // resolves here; a leaf tool/service kind does not. The same lookup yields the
        // child row's `native:<binary>` executor identity.
        let child_runtime = state
            .engine
            .runtimes
            .resolve_for_launch(None, &child_ref.kind)
            .map_err(|e| {
                anyhow::anyhow!(
                    "follow: child kind '{}' has no managed runtime — a follow child must be a \
                 managed runtime execution: {e}",
                    child_ref.kind
                )
            })?;
        let child_executor_ref = format!(
            "native:{}",
            crate::dispatch::strip_binary_ref_prefix(&child_runtime.yaml.binary_ref)
                .map_err(|e| anyhow::anyhow!("follow: {e}"))?
        );
        let child_runtime_ref = child_runtime.canonical_ref.to_string();

        // The thread ROW kind is the child kind's THREAD PROFILE (e.g. `graph` →
        // `graph_run`), not the item kind: profile-driven continuation / resume /
        // operator behavior keys off the profile name, so a fresh child row and its
        // captured identity must carry the profile, exactly like a normal launch.
        let child_thread_profile = state
            .engine
            .kinds
            .get(&child_ref.kind)
            .and_then(|schema| schema.execution())
            .and_then(|exec| exec.thread_profile.as_ref())
            .map(|tp| tp.name.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "follow: child kind '{}' has no execution.thread_profile",
                    child_ref.kind
                )
            })?;
        resolved_children.push((child_thread_profile, child_executor_ref, child_runtime_ref));
    }

    // Follow-nesting depth: walk the follow-waiter lineage server-side. If the
    // parent's chain is itself some waiter's child chain, the parent is a follow
    // child; hop up via that waiter's parent chain and count. Enforced ONLY at
    // admission — never re-enforced when reconcile re-drives an admitted waiter.
    enforce_follow_nesting_depth(state, &parent.chain_root_id)?;

    // ── Ordered spawn sequence, idempotent by follow_key ────────────────────
    let follow_key = format!(
        "{parent_thread_id}/{}/{}/{}",
        params.graph_run_id, params.follow_node, params.step_count
    );

    // 1. Reserve the waiter. ALWAYS go through `reserve_follow` (never a pre-read):
    //    it is the get-or-create primitive AND validates that an existing row's
    //    seed matches this request, so a duplicate follow_key with conflicting seed
    //    fields (e.g. a different frontier_id) is rejected, not silently adopted.
    let waiter = state.state_store.reserve_follow(&NewFollowWaiter {
        follow_key: follow_key.clone(),
        parent_thread_id: parent_thread_id.clone(),
        parent_chain_root_id: parent.chain_root_id.clone(),
        follow_node: params.follow_node.clone(),
        graph_run_id: params.graph_run_id.clone(),
        step_count: params.step_count,
        frontier_id: params.frontier_id.clone(),
        fanout,
        expected_children: u32::try_from(children.len()).context("follow: too many children")?,
    })?;

    // Normalize and validate the complete immutable cohort before any idempotent
    // return. A re-drive may not silently adopt a changed count, ref, or spec.
    let spec_hashes: Vec<String> = children
        .iter()
        .map(|child| {
            follow_child_spec_hash(&child.item_ref, &child.parameters, child.facets.as_ref())
        })
        .collect();
    if waiter.expected_children as usize != children.len() {
        bail!("follow: persisted child count conflicts with re-driven cohort");
    }
    for (index, child) in children.iter().enumerate() {
        if let Some(slot) = state
            .state_store
            .get_follow_child(&follow_key, index as u32)?
        {
            if slot.item_ref != child.item_ref || slot.spec_hash != spec_hashes[index] {
                bail!("follow: persisted child conflicts at index {index}");
            }
        } else if waiter.phase != follow_phase::RESERVED {
            bail!("follow: persisted cohort is missing child index {index}");
        }
    }

    // Already past reservation (waiting/ready/resuming): a duplicate call for an
    // in-flight or completed follow. Idempotent no-op — return the recorded IDs.
    if waiter.phase != follow_phase::RESERVED {
        return Ok(json!({
            "follow_key": follow_key,
            "phase": waiter.phase,
            "child_thread_id": state.state_store.get_follow_child(&follow_key, 0)?.map(|c| c.child_thread_id),
            "child_thread_ids": (0..waiter.expected_children).map(|i| state.state_store.get_follow_child(&follow_key, i).map(|c| c.map(|c| c.child_thread_id))).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect::<Vec<_>>(),
            "parent_successor_thread_id": waiter.parent_successor_thread_id,
            "idempotent": true,
        }));
    }

    // 2. Child root row (created, NOT launched) + seeded launch identity. A follow
    //    child is a FRESH ROOT: its own chain root, no upstream braid.
    let requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: thread_auth.acting_principal.clone(),
        scopes: thread_auth.caller_scopes.clone(),
    });
    let persisted_parent_context = PersistedParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    };
    let mut child_thread_ids = Vec::with_capacity(children.len());
    for (item_index, (child, (child_thread_profile, child_executor_ref, child_runtime_ref))) in
        children.iter().zip(resolved_children.iter()).enumerate()
    {
        let spec_hash = spec_hashes[item_index].clone();
        let child_thread_id = match state
            .state_store
            .get_follow_child(&follow_key, item_index as u32)?
        {
            Some(existing) => {
                if existing.spec_hash != spec_hash {
                    bail!("follow: child spec conflict at index {item_index}");
                }
                existing.child_thread_id
            }
            None => {
                // Persist the stable slot identity first. If creation crashes, a
                // re-drive recreates this exact root rather than minting an orphan.
                let child_id = new_thread_id();
                state.state_store.set_follow_child(
                    &follow_key,
                    item_index as u32,
                    &child.item_ref,
                    &spec_hash,
                    &child_id,
                    &child_id,
                )?;
                child_id
            }
        };

        // The slot is the stable identity authority. Every reserved re-drive repairs
        // the row and all pre-launch materialization before proceeding.
        if state.threads.get_thread(&child_thread_id)?.is_none() {
            state.threads.create_thread(&ThreadCreateParams {
                thread_id: child_thread_id.clone(),
                chain_root_id: child_thread_id.clone(),
                kind: child_thread_profile.clone(),
                item_ref: child.item_ref.clone(),
                executor_ref: child_executor_ref.clone(),
                launch_mode: "detached".to_string(),
                current_site_id: parent.current_site_id.clone(),
                origin_site_id: parent.origin_site_id.clone(),
                upstream_thread_id: None,
                requested_by: Some(thread_auth.acting_principal.clone()),
                project_root: parent.project_root.as_ref().map(std::path::PathBuf::from),
                usage_subject: None,
                usage_subject_asserted_by: None,
            })?;
        }

        // Build the expected MINIMAL launch identity on every drive. The
        // detached launcher re-resolves the
        // item + envelope off the callback hot path. `effective_caps` carries
        // the PARENT's caps — the bounding authority the launcher hands to
        // `CapabilityPolicy::FollowChildHybrid`, overwritten with the child's
        // own composed caps once policy resolution succeeds.
        let meta = RuntimeLaunchMetadata::default().with_resume_context(ResumeContext {
            kind: child_thread_profile.clone(),
            item_ref: child.item_ref.clone(),
            launch_mode: "detached".to_string(),
            parameters: child.parameters.clone(),
            // Resume identity derives from validated server-side state
            // (the token's provenance), never the request body: the wire
            // `project_path` is the token-equality proof and, under a
            // state-root override, points at the STATE root — persisting
            // it here would make a reconcile relaunch resolve items
            // against runtime state instead of the source.
            project_context: ProjectContext::LocalPath {
                path: cap.provenance.effective_path().to_path_buf(),
            },
            original_snapshot_hash: inherited_snapshot_hash.clone(),
            // A follow child borrows the parent's workspace; it never
            // owns snapshot lineage, so no pushed-head identity is
            // seeded (rebuilding one would take over pin/foldback the
            // parent owns).
            original_pushed_head_ref: None,
            // The parent's state-root override carries to the child so
            // its state/callback anchor stays isolated with the parent's.
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
        });
        let mut meta = meta;
        meta.follow_parent_context = Some(persisted_parent_context.clone());
        meta.follow_launch_window = params.launch_window_width.map(|width| FollowLaunchWindow {
            key: format!("follow:{follow_key}"),
            width,
        });
        let persisted_meta = state.state_store.get_launch_metadata(&child_thread_id)?;
        let inherited_stop = match state.state_store.record_child_link(
            &parent_thread_id,
            &child_thread_id,
            "dispatch",
        ) {
            Ok(inherited_stop) => inherited_stop,
            Err(error) => {
                // The conditional transition proves Created + unattached +
                // unclaimed under the same store lock as finalization. A
                // same-slot re-drive can therefore never finalize a child that
                // advanced after the row read above.
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
                        "preserved concurrently advanced follow child after lineage failure"
                    ),
                    Err(cleanup_error) => {
                        return Err(anyhow::anyhow!(
                            "follow: record child lineage under parent {parent_thread_id}: \
                             {error}; conditional child cleanup also failed: {cleanup_error}"
                        ));
                    }
                }
                return Err(error).context(format!(
                    "follow: record child lineage under parent {parent_thread_id}"
                ));
            }
        };
        if inherited_stop.is_some() {
            crate::execution::process_attachment::finalize_requested_stop_if_present(
                state,
                &child_thread_id,
            )?;
            bail!("follow: parent {parent_thread_id} was stop-requested during child admission");
        }
        if let Some(persisted) = persisted_meta
            .as_ref()
            .filter(|m| m.resume_context.is_some())
        {
            if persisted.resume_context != meta.resume_context
                || persisted.follow_parent_context != meta.follow_parent_context
                || persisted.follow_launch_window != meta.follow_launch_window
            {
                bail!("follow: launched child metadata conflicts at index {item_index}");
            }
        } else {
            if let Some(Value::Object(facets)) = child.facets.as_ref() {
                let current: std::collections::HashMap<_, _> = state
                    .state_store
                    .get_facets(&child_thread_id)?
                    .into_iter()
                    .collect();
                for (key, value) in facets {
                    if key.trim().is_empty() {
                        continue;
                    }
                    let value = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    if current.get(key) == Some(&value) {
                        continue;
                    }
                    state
                        .events
                        .append(&ryeos_app::event_store_service::EventAppendParams {
                            thread_id: child_thread_id.clone(),
                            event: ryeos_app::event_store_service::EventAppendItem {
                                event_type: ryeos_runtime::events::RuntimeEventType::ThreadFacetSet
                                    .as_str()
                                    .to_string(),
                                storage_class:
                                    ryeos_runtime::events::RuntimeEventType::ThreadFacetSet
                                        .storage_class()
                                        .as_str()
                                        .to_string(),
                                payload: json!({"key": key, "value": value}),
                            },
                        })?;
                }
            }
            // resume_context is the commit marker and is written last.
            state
                .state_store
                .seed_launch_metadata(&child_thread_id, &meta)?;
        }
        child_thread_ids.push(child_thread_id);
    }

    // 3. Parent successor row (created, NOT launched). This atomically settles the
    //    parent `continued` and copies the parent's captured launch identity to the
    //    successor (requires the parent running + the single-successor invariant).
    //    The successor is launched later, on child-terminal, by the reconcile /
    //    follow-resume path — never here.
    let parent_successor_thread_id = match waiter.parent_successor_thread_id.clone() {
        Some(id) => id,
        None => {
            // Creating the successor atomically settles the parent `continued`, so
            // a prior attempt that crashed AFTER creating it but BEFORE recording it
            // on the waiter leaves the parent already continued. Re-creating would
            // fail (parent no longer running) and strand the follow — so first
            // recover: if the parent already carries its follow-resume successor,
            // adopt it onto the waiter and continue.
            if let Some(existing) = parent.successor_thread_id.clone() {
                if !state
                    .state_store
                    .is_follow_resume_successor(&parent_thread_id, &existing)?
                {
                    bail!(
                        "follow: parent {parent_thread_id} already continued into a non-follow \
                         successor {existing}; cannot suspend it for follow"
                    );
                }
                state
                    .state_store
                    .set_follow_parent_successor(&follow_key, &existing)?;
                existing
            } else {
                let successor_id = new_thread_id();
                // Via the lifecycle service so the parent-`continued` + successor-
                // `created` events reach live subscribers, not just the event store.
                state.threads.create_follow_resume_successor(
                    &NewThreadRecord {
                        thread_id: successor_id.clone(),
                        chain_root_id: parent.chain_root_id.clone(),
                        kind: parent.kind.clone(),
                        item_ref: parent.item_ref.clone(),
                        executor_ref: parent.executor_ref.clone(),
                        launch_mode: parent.launch_mode.clone(),
                        current_site_id: parent.current_site_id.clone(),
                        origin_site_id: parent.origin_site_id.clone(),
                        upstream_thread_id: Some(parent_thread_id.clone()),
                        requested_by: parent.requested_by.clone(),
                        project_root: parent.project_root.as_ref().map(std::path::PathBuf::from),
                        usage_subject: None,
                        usage_subject_asserted_by: None,
                    },
                    &parent_thread_id,
                    &parent.chain_root_id,
                )?;
                state
                    .state_store
                    .set_follow_parent_successor(&follow_key, &successor_id)?;
                successor_id
            }
        }
    };

    // 4. Enqueue the complete ordered cohort before exposing `waiting`. Calling
    // enqueue once per member would admit early members before later FIFO rows
    // existed, but none are launched until the complete cohort is durable.
    let admitted = if let Some(width) = params.launch_window_width {
        let window_key = format!("follow:{follow_key}");
        let mut admitted = Vec::new();
        for child_id in &child_thread_ids {
            admitted.extend(state.state_store.launch_window_enqueue(
                child_id,
                &window_key,
                width,
                crate::execution::launch::global_live_fanout_limit(),
                lillux::time::timestamp_millis(),
            )?);
        }
        admitted
    } else {
        child_thread_ids.clone()
    };

    // 5. Mark the waiter durably `waiting`: all IDs and window membership are
    // recorded, and the parent is suspended.
    state.state_store.mark_follow_waiting(&follow_key)?;

    // 6. ONLY NOW launch the child, detached. The durable `waiting` waiter above
    //    means a terminal child can be matched to its suspended parent even if this
    //    daemon dies right after the spawn. Fire-and-forget: the launcher is claim-
    //    guarded, so a lost spawn is safe for the reconcile sweep to re-drive.
    // Hot-path launch uses the parent's BORROWED-CHILD provenance (derived from
    // the validated callback token), preserving pushed-head / effective workspace
    // / request engine — not the root-live-fs fallback the resume-context path
    // would reconstruct. Reconcile re-drives with `None` (documented limit).
    let launch_provenance = cap.provenance.clone_for_borrowed_child();
    // Parent execution ceiling from the VALIDATED cap (never `child_parameters`),
    // so the child is clamped to the parent's hard limits and launches at parent
    // depth + 1 — the same context a normal callback-dispatched child receives.
    let launch_parent_context = crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    };
    for launch_child_id in admitted {
        let launch_state = state.clone();
        let launch_provenance = launch_provenance.clone();
        let launch_parent_context = launch_parent_context.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::execution::launch::launch_follow_child(
                launch_state,
                &launch_child_id,
                Some(launch_provenance),
                Some(launch_parent_context),
            )
            .await
            {
                tracing::error!(
                    child_thread_id = %launch_child_id,
                    error = %e,
                    "follow child detached launch failed",
                );
            }
        });
    }

    let child_thread_id = child_thread_ids[0].clone();

    tracing::info!(
        follow_key = %follow_key,
        parent_thread_id = %parent_thread_id,
        child_thread_id = %child_thread_id,
        parent_successor_thread_id = %parent_successor_thread_id,
        server_principal = %thread_auth.acting_principal,
        "follow child spawned; parent suspended, child launching detached",
    );

    Ok(json!({
        "follow_key": follow_key,
        "phase": follow_phase::WAITING,
        "child_thread_id": child_thread_id,
        "child_thread_ids": child_thread_ids,
        "parent_successor_thread_id": parent_successor_thread_id,
    }))
}

/// Walk the follow-waiter lineage from `chain_root_id` upward and refuse a new
/// follow that would exceed [`MAX_FOLLOW_NESTING_DEPTH`]. Never trusts a
/// caller-supplied depth: each level is a server-side waiter whose child chain is
/// the level below it.
fn enforce_follow_nesting_depth(state: &AppState, chain_root_id: &str) -> Result<()> {
    let mut depth = 0usize;
    let mut chain = chain_root_id.to_string();
    // Guard against a malformed cyclic lineage as well as depth.
    while let Some(waiter) = state.state_store.get_follow_waiter_by_child_chain(&chain)? {
        depth += 1;
        if depth >= MAX_FOLLOW_NESTING_DEPTH {
            bail!(
                "follow nesting depth limit reached ({depth}/{MAX_FOLLOW_NESTING_DEPTH}); \
                 refusing to nest another follow"
            );
        }
        chain = waiter.parent_chain_root_id;
    }
    Ok(())
}
