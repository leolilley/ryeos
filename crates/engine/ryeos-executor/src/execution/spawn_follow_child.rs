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
//! **Ordering.** Admit the complete cohort, reserve stable child identities,
//! authoritatively prepare each exact identity, commit each child root together
//! with its launch audit, create
//! the parent successor (which settles the parent `continued`), and mark the
//! waiter `waiting` before launching admitted children. The call acknowledges
//! only after each immediate launch crosses the managed spawn-task handoff.
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
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
use ryeos_app::thread_lifecycle::new_thread_id;
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
    children: Vec<ryeos_runtime::callback::FollowChildSpec>,
    #[serde(default)]
    launch_window_width: Option<u32>,
    #[serde(default)]
    frontier_id: Option<String>,
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: SpawnFollowChildParams = serde_json::from_value(params.clone())
        .context("invalid runtime.spawn_follow_child params")?;

    if params.children.is_empty() {
        bail!("follow: children must be nonempty");
    }
    let fanout = params.children.len() > 1;
    let children = params.children;
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
    let mut child_refs = Vec::with_capacity(children.len());
    for child in &children {
        crate::execution::launch_preparation::validate_ref_bindings(&child.ref_bindings)?;
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

        for (binding_name, binding_ref) in &child.ref_bindings {
            let canonical = CanonicalRef::parse(binding_ref).with_context(|| {
                format!(
                    "follow: invalid ref binding '{binding_name}' value '{binding_ref}'"
                )
            })?;
            let required = canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
            let policy = AuthorizationPolicy::require_all(&[&required]);
            if state
                .authorizer
                .authorize(&cap.effective_caps, &policy)
                .is_err()
            {
                bail!(
                    "follow admission denied: parent lacks execute authority '{required}' over \
                     ref binding '{binding_name}'"
                );
            }
        }
        child_refs.push(child_ref);
    }

    // ── Ordered spawn sequence, idempotent by follow_key ────────────────────
    let follow_key = format!(
        "{parent_thread_id}/{}/{}/{}",
        params.graph_run_id, params.follow_node, params.step_count
    );

    // Normalize and validate the complete immutable cohort before persistence or
    // any idempotent return. A re-drive may not silently adopt a changed count,
    // ref, or spec, and non-JCS values must not leave a reserved waiter behind.
    let spec_hashes: Vec<String> = children
        .iter()
        .map(|child| {
            follow_child_spec_hash(
                &child.item_ref,
                &child.ref_bindings,
                &child.parameters,
                child.facets.as_ref(),
            )
        })
        .collect::<Result<Vec<_>>>()?;

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

    // The waiter phase says whether the parent suspension committed; it does not
    // say which child roots committed before a crash. Reserve every stable slot
    // first, then classify each slot from its own durable row.
    let re_drive = waiter.phase != follow_phase::RESERVED;
    let window_key = params
        .launch_window_width
        .map(|_| format!("follow:{follow_key}"));
    let expected_launch_window = params.launch_window_width.map(|width| FollowLaunchWindow {
        key: format!("follow:{follow_key}"),
        width,
    });

    if !re_drive {
        // Follow-nesting depth is an admission limit, not a condition that can
        // retroactively invalidate an already-admitted duplicate drive.
        enforce_follow_nesting_depth(state, &parent.chain_root_id)?;
    }

    // Reserve the exact stable identities before launch authority is prepared.
    // Augmentations, checkpoint paths, audit, metadata, and the eventual root
    // commit must all name the same child ID.
    let mut reserved_child_ids = std::collections::BTreeMap::new();
    for (item_index, child) in children.iter().enumerate() {
        let spec_hash = &spec_hashes[item_index];
        let child_thread_id = match state
            .state_store
            .get_follow_child(&follow_key, item_index as u32)?
        {
            Some(existing) => {
                if existing.item_ref != child.item_ref || existing.spec_hash != *spec_hash {
                    bail!("follow: child spec conflict at index {item_index}");
                }
                if existing.child_chain_root_id != existing.child_thread_id {
                    bail!("follow: child slot at index {item_index} is not a root identity");
                }
                existing.child_thread_id
            }
            None if !re_drive => {
                let child_id = new_thread_id();
                state.state_store.set_follow_child(
                    &follow_key,
                    item_index as u32,
                    &child.item_ref,
                    spec_hash,
                    &child_id,
                    &child_id,
                )?;
                child_id
            }
            None => {
                bail!("follow: persisted cohort is missing child index {item_index}");
            }
        };
        reserved_child_ids.insert(item_index, child_thread_id);
    }

    let expected_parent_context = PersistedParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    };
    let mut child_thread_ids = Vec::with_capacity(children.len());
    let mut queued_child_thread_ids = Vec::new();
    let mut fresh_indices = std::collections::BTreeSet::new();
    let mut existing_created_indices = std::collections::BTreeSet::new();
    let mut existing_launchable_indices = std::collections::BTreeSet::new();
    let mut persisted_launch_metadata = std::collections::BTreeMap::new();
    for (item_index, child_spec) in children.iter().enumerate() {
        let child_id = reserved_child_ids
            .get(&item_index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("follow: missing child ID at index {item_index}"))?;
        child_thread_ids.push(child_id.clone());
        let Some(child_row) = state.threads.get_thread(&child_id)? else {
            if re_drive {
                bail!("follow: persisted child row is missing: {child_id}");
            }
            fresh_indices.insert(item_index);
            continue;
        };
        let metadata = state
            .state_store
            .get_launch_metadata(&child_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("follow: child {child_id} has no authoritative launch metadata")
            })?;
        let resume = metadata.resume_context.as_ref().ok_or_else(|| {
            anyhow::anyhow!("follow: child {child_id} has no persisted ResumeContext")
        })?;
        if child_row.kind != resume.kind
            || child_row.item_ref != resume.item_ref
            || resume.item_ref != child_spec.item_ref
            || resume.ref_bindings != child_spec.ref_bindings
            || resume.parameters != child_spec.parameters
            || resume.launch_mode != "detached"
            || metadata.follow_parent_context.as_ref() != Some(&expected_parent_context)
            || metadata.follow_launch_window != expected_launch_window
        {
            bail!("follow: child metadata conflicts at index {item_index}");
        }
        persisted_launch_metadata.insert(item_index, metadata);
        if child_row.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
            continue;
        }
        existing_created_indices.insert(item_index);
        if let Some(window) = expected_launch_window.as_ref() {
            if !state.state_store.launch_window_is_member(&child_id)? {
                state.state_store.launch_window_insert_only(
                    &child_id,
                    &window.key,
                    window.width,
                    lillux::time::timestamp_millis(),
                )?;
            }
            if state.state_store.launch_window_is_queued(&child_id)? {
                queued_child_thread_ids.push(child_id);
                continue;
            }
        }
        existing_launchable_indices.insert(item_index);
    }

    // A reserved partial crash may contain any mix of committed and missing
    // roots. Every missing root needs fresh authority; every existing Created
    // root uses its persisted birth identity. A later-phase duplicate prepares
    // only rows that are already admitted and need a handoff now.
    let authority_indices: std::collections::BTreeSet<usize> = if re_drive {
        existing_launchable_indices.clone()
    } else {
        fresh_indices
            .union(&existing_created_indices)
            .copied()
            .collect()
    };

    // Complete the generic authority pass before any missing child row becomes
    // observable. Fresh rows use current generic authority; existing rows use
    // their exact stored birth identity and never recapture a snapshot. The
    // in-memory values own secret material and are consumed exactly once.
    let requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: thread_auth.acting_principal.clone(),
        scopes: thread_auth.caller_scopes.clone(),
    });
    let persisted_parent_context = expected_parent_context.clone();
    let launch_provenance = cap.provenance.clone_for_borrowed_child();
    let launch_parent_context = crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
    };
    let child_engine = cap.provenance.request_engine();
    let mut resolved_children = std::collections::BTreeMap::new();
    for (item_index, child_ref) in child_refs.iter().enumerate() {
        if !fresh_indices.contains(&item_index) {
            // Existing rows carry their immutable runtime/executor identity in
            // persisted launch metadata. Do not replace it from today's kind
            // registry merely because the waiter still says `reserved`.
            continue;
        }
        // Managed-runtime children only: a child kind served by a registered
        // runtime resolves here; a leaf tool/service kind does not. Duplicate
        // rows that are already running, terminal, or queued never consult the
        // current registry because they need no new launch authority.
        let child_runtime = child_engine
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

        // The thread row carries the child kind's thread profile, not the item
        // kind, matching ordinary launches and continuation policy lookup.
        let child_thread_profile = child_engine
            .kinds
            .get(&child_ref.kind)
            .and_then(|schema| schema.execution())
            .and_then(|exec| exec.thread_profile.as_ref())
            .map(|profile| profile.name.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "follow: child kind '{}' has no execution.thread_profile",
                    child_ref.kind
                )
            })?;
        resolved_children.insert(
            item_index,
            (child_thread_profile, child_executor_ref, child_runtime_ref),
        );
    }
    let mut child_metadata = std::collections::BTreeMap::new();
    let mut prepared_children = std::collections::BTreeMap::new();
    for (item_index, child) in children.iter().enumerate() {
        if !authority_indices.contains(&item_index) {
            continue;
        }
        let existing_row = existing_created_indices.contains(&item_index);
        let meta = if existing_row {
            persisted_launch_metadata
                .get(&item_index)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: missing persisted launch metadata at index {item_index}"
                    )
                })?
        } else {
            let (child_thread_profile, child_executor_ref, child_runtime_ref) = resolved_children
                .get(&item_index)
                .ok_or_else(|| {
                    anyhow::anyhow!("follow: missing child resolution at index {item_index}")
                })?;
            let mut meta = RuntimeLaunchMetadata::default().with_resume_context(ResumeContext {
                kind: child_thread_profile.clone(),
                item_ref: child.item_ref.clone(),
                ref_bindings: child.ref_bindings.clone(),
                launch_mode: "detached".to_string(),
                parameters: child.parameters.clone(),
                project_context: ProjectContext::LocalPath {
                    path: cap.provenance.effective_path().to_path_buf(),
                },
                original_snapshot_hash: inherited_snapshot_hash.clone(),
                original_pushed_head_ref: None,
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
            meta.follow_parent_context = Some(persisted_parent_context.clone());
            meta.follow_launch_window = expected_launch_window.clone();
            meta
        };
        let child_thread_id = reserved_child_ids.get(&item_index).ok_or_else(|| {
            anyhow::anyhow!("follow: missing reserved child ID at index {item_index}")
        })?;
        let prepared = if existing_row {
            crate::execution::launch::prepare_existing_follow_child_launch(
                state,
                child_thread_id,
                &meta,
                launch_provenance.clone(),
                launch_parent_context.clone(),
            )
            .await?
        } else {
            crate::execution::launch::prepare_follow_child_launch(
                state,
                child_thread_id,
                &meta,
                launch_provenance.clone(),
                launch_parent_context.clone(),
            )
            .await?
        };
        child_metadata.insert(item_index, prepared.launch_metadata().clone());
        prepared_children.insert(item_index, prepared);
    }

    // 2. Child root row (created, NOT launched) + seeded launch identity. A follow
    //    child is a FRESH ROOT: its own chain root, no upstream braid. The root
    //    snapshot and authoritative launch audit share one signed birth commit.
    let mut prepared_by_child = std::collections::BTreeMap::new();
    for (item_index, child) in children.iter().enumerate() {
        let child_thread_id = reserved_child_ids
            .get(&item_index)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("follow: missing reserved child ID at index {item_index}")
            })?;
        let mut prepared = prepared_children.remove(&item_index);
        if fresh_indices.contains(&item_index) {
            let meta = child_metadata.remove(&item_index).ok_or_else(|| {
                anyhow::anyhow!("follow: missing prepared metadata for child index {item_index}")
            })?;
            let fresh_prepared = prepared.take().ok_or_else(|| {
                anyhow::anyhow!("follow: missing prepared authority for child index {item_index}")
            })?;
            let mut initial_events = fresh_prepared.initial_audit_events()?;
            if let Some(Value::Object(facets)) = child.facets.as_ref() {
                for (key, value) in facets {
                    if key.trim().is_empty() {
                        continue;
                    }
                    let value = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    initial_events.push(NewEventRecord {
                        event_type: ryeos_runtime::events::RuntimeEventType::ThreadFacetSet
                            .as_str()
                            .to_string(),
                        storage_class:
                            ryeos_runtime::events::RuntimeEventType::ThreadFacetSet
                                .storage_class()
                                .as_str()
                                .to_string(),
                        payload: json!({"key": key, "value": value}),
                    });
                }
            }
            state
                .threads
                .create_root_thread_with_events_and_launch_metadata(
                    &child_thread_id,
                    fresh_prepared.resolved_request(),
                    initial_events,
                    Some(fresh_prepared.launch_metadata()),
                )?;
            prepared = Some(fresh_prepared.with_persisted_birth_audit());
            let persisted = state
                .state_store
                .get_launch_metadata(&child_thread_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: child {child_thread_id} has no authoritative launch metadata"
                    )
                })?;
            if persisted.resume_context != meta.resume_context
                || persisted.follow_parent_context != meta.follow_parent_context
                || persisted.follow_launch_window != meta.follow_launch_window
            {
                bail!("follow: child metadata conflicts at index {item_index}");
            }
        } else if authority_indices.contains(&item_index) {
            let expected = child_metadata.remove(&item_index).ok_or_else(|| {
                anyhow::anyhow!("follow: missing persisted metadata at child index {item_index}")
            })?;
            let persisted = state
                .state_store
                .get_launch_metadata(&child_thread_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: child {child_thread_id} has no authoritative launch metadata"
                    )
                })?;
            if persisted.resume_context != expected.resume_context
                || persisted.follow_parent_context != expected.follow_parent_context
                || persisted.follow_launch_window != expected.follow_launch_window
            {
                bail!("follow: child metadata changed during preparation at index {item_index}");
            }
        }
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
        // Portable cross-chain lineage: unlike an ordinary graph dispatch, a
        // follow child is spawned inside this daemon callback, so the graph
        // walker never receives a dispatch result from which it could emit
        // `child_thread_spawned`. Record the durable edge here before the
        // parent is settled `continued`. The store serializes the edge absence
        // check and signed append, making concurrent RESERVED-phase re-drives
        // exactly-once while the event stays rebuild-safe (runtime_db's child
        // link remains the separate operational cascade copy).
        match state.threads.append_child_thread_spawned_once(
            &parent.chain_root_id,
            &parent_thread_id,
            &child_thread_id,
            json!({
                "child_thread_id": child_thread_id,
                "node": params.follow_node,
                "step": params.step_count,
                "item_id": child.item_ref,
                "cohort_index": item_index,
                "spawn_reason": "follow",
            }),
        )? {
            ryeos_app::state_store::ChildLineageAppendOutcome::Appended
            | ryeos_app::state_store::ChildLineageAppendOutcome::AlreadyPresent => {}
            ryeos_app::state_store::ChildLineageAppendOutcome::ParentSettled => {
                bail!(
                    "follow: parent {parent_thread_id} settled before child lineage was recorded"
                );
            }
        }
        if let Some(prepared) = prepared {
            prepared_by_child.insert(child_thread_id, prepared);
        }
    }

    // 3. Establish launch-window membership before the irreversible parent
    //    continuation commit. A membership failure now leaves the parent running
    //    and the reserved waiter safely re-drivable.
    let mut admitted = if re_drive {
        existing_launchable_indices
            .iter()
            .map(|item_index| child_thread_ids[*item_index].clone())
            .collect()
    } else if let (Some(width), Some(window_key)) =
        (params.launch_window_width, window_key.as_deref())
    {
        for item_index in fresh_indices.iter().copied() {
            let child_id = &child_thread_ids[item_index];
            state.state_store.launch_window_insert_only(
                child_id,
                window_key,
                width,
                lillux::time::timestamp_millis(),
            )?;
        }
        Vec::new()
    } else {
        authority_indices
            .iter()
            .map(|item_index| child_thread_ids[*item_index].clone())
            .collect()
    };

    // 4. Parent successor row (created, NOT launched). This atomically settles the
    //    parent `continued` and copies the parent's captured launch identity to the
    //    successor (requires the parent running + the single-successor invariant).
    //    The successor is launched later, on child-terminal, by the reconcile /
    //    follow-resume path — never here.
    let parent_successor_thread_id = if re_drive {
        waiter.parent_successor_thread_id.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "follow: {} waiter has no parent successor",
                waiter.phase
            )
        })?
    } else {
        match waiter.parent_successor_thread_id.clone() {
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
                    if let Err(error) = state
                        .state_store
                        .set_follow_parent_successor(&follow_key, &existing)
                    {
                        tracing::error!(
                            follow_key,
                            successor_id = %existing,
                            error = %error,
                            "follow successor adoption was not recorded; reserved reconciliation will repair it"
                        );
                    }
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
                            project_root: parent
                                .project_root
                                .as_ref()
                                .map(std::path::PathBuf::from),
                            usage_subject: None,
                            usage_subject_asserted_by: None,
                        },
                        &parent_thread_id,
                        &parent.chain_root_id,
                    )?;
                    if let Err(error) = state
                        .state_store
                        .set_follow_parent_successor(&follow_key, &successor_id)
                    {
                        tracing::error!(
                            follow_key,
                            successor_id = %successor_id,
                            error = %error,
                            "follow successor committed but waiter update failed; reserved reconciliation will repair it"
                        );
                    }
                    successor_id
                }
            }
        }
    };

    // 5. Commit the fresh waiter's truthful post-suspension phase: all IDs and
    // window membership are recorded, and the parent is suspended. A cohort
    // that settled concurrently advances directly to `ready` and must resume;
    // its terminal children must never be launched again.
    let mut response_phase = waiter.phase.clone();
    if !re_drive {
        match state.state_store.mark_follow_waiting(&follow_key) {
            Ok(phase) => response_phase = phase,
            Err(error) => {
                // The parent continuation is already authoritative. Returning
                // an error would invite a caller retry that cannot undo it;
                // retain the reserved waiter and let its reconciler adopt the
                // successor/complete the waiting transition.
                tracing::error!(
                    follow_key,
                    error = %error,
                    "follow suspension committed but waiter transition failed; accepted for reserved reconciliation"
                );
                admitted.clear();
                queued_child_thread_ids.extend(
                    authority_indices
                        .iter()
                        .map(|item_index| child_thread_ids[*item_index].clone()),
                );
            }
        }
        if response_phase == follow_phase::WAITING {
            if let Some(window_key) = window_key.as_deref() {
                match state.state_store.launch_window_admit(
                    window_key,
                    crate::execution::launch::global_live_fanout_limit(),
                    lillux::time::timestamp_millis(),
                ) {
                    Ok(newly_admitted) => admitted = newly_admitted,
                    Err(error) => {
                        // Membership and `waiting` are durable. Report a truthful
                        // queued acceptance and let the periodic/startup window
                        // sweep retry admission; never turn an already-continued
                        // parent into an error response.
                        tracing::error!(
                            follow_key,
                            error = %error,
                            "follow launch-window admission failed after suspension; queued for sweep"
                        );
                    }
                }
                queued_child_thread_ids.extend(
                    authority_indices
                        .iter()
                        .map(|item_index| child_thread_ids[*item_index].clone())
                        .filter(|child_id| !admitted.contains(child_id)),
                );
            }
        }
    }
    queued_child_thread_ids.retain(|child_id| !admitted.contains(child_id));
    if response_phase == follow_phase::READY {
        admitted.clear();
        queued_child_thread_ids.clear();
        for child_thread_id in &child_thread_ids {
            crate::execution::launch::kick_launch_window_for_terminal(state, child_thread_id);
        }
        if let Some(child_thread_id) = child_thread_ids.first() {
            crate::execution::launch::kick_follow_resume_if_ready(state, child_thread_id);
        }
    }
    queued_child_thread_ids.sort();
    queued_child_thread_ids.dedup();

    // 6. ONLY NOW launch admitted children. Each task consumes the exact
    // pre-birth authority and must cross the managed spawn handoff before this
    // callback acknowledges the cohort.
    let mut launch_receivers = Vec::new();
    for launch_child_id in admitted {
        let launch_state = state.clone();
        let prepared = prepared_by_child.remove(&launch_child_id).ok_or_else(|| {
            anyhow::anyhow!("follow: admitted unknown child {launch_child_id}")
        })?;
        let (launch_handoff, launch_ready) =
            crate::execution::launch::LaunchHandoff::channel();
        launch_receivers.push((launch_child_id.clone(), launch_ready));
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
                    "follow child detached launch failed",
                );
            }
        });
    }
    for (expected_child_id, receiver) in launch_receivers {
        let handed_off = receiver
            .await
            .context("follow: child launch task closed before spawn handoff")?
            .map_err(|failure| {
                anyhow::anyhow!(
                    "follow: child launch rejected before handoff ({}): {}",
                    failure.code,
                    failure.message
                )
            })?;
        if handed_off != expected_child_id {
            bail!(
                "follow: child launch handed off unexpected thread {handed_off} \
                 (expected {expected_child_id})"
            );
        }
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
        "phase": response_phase,
        "child_thread_id": child_thread_id,
        "child_thread_ids": child_thread_ids,
        "queued_child_thread_ids": queued_child_thread_ids,
        "parent_successor_thread_id": parent_successor_thread_id,
        "idempotent": re_drive,
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
