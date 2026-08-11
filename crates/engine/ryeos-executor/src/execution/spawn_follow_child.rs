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

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_app::launch_metadata::{
    FollowLaunchWindow, PersistedParentExecutionContext, ResumeContext, RuntimeLaunchMetadata,
};
use ryeos_app::runtime_db::{NewFollowWaiter, follow_child_spec_hash, follow_phase};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
use ryeos_app::thread_lifecycle::{
    ResolvedExecutionRequest, SealedRootExecutionRequest, new_thread_id,
};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
use ryeos_runtime::authorizer::{AuthorizationPolicy, canonical_cap};

/// Bound on A→B→C→… follow recursion, enforced ONLY here at admission by walking
/// the server-side follow-waiter lineage (never a caller-supplied depth). Distinct
/// from the autonomous-segment continuation depth (that bounds one execution
/// segment-cutting itself); this bounds how deep follow nesting may go.
const MAX_FOLLOW_NESTING_DEPTH: usize = 8;

/// Fresh child authority stays in memory until the complete effective program
/// has been captured, validated, and finalized. Only the `Persisted` arm is a
/// durable capsule, and that arm is necessarily finalized-only.
#[derive(Clone)]
enum AdmittedFollowChildRequest {
    Fresh {
        request: ResolvedExecutionRequest,
        runtime_ref: String,
    },
    Persisted(SealedRootExecutionRequest),
}

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
    completion: ryeos_runtime::TerminalCompletion,
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: SpawnFollowChildParams = serde_json::from_value(params.clone())
        .context("invalid runtime.spawn_follow_child params")?;

    let fanout = validate_follow_launch(params.children.len(), params.launch_window_width)?;
    let children = params.children;

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
    let launch_owner = cap
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("execution callback capability has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(&parent_thread_id, launch_owner)?;

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
    let parent_lifecycle_authority = parent_launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.resume_context.as_ref())
        .map(|resume| resume.lifecycle_authority)
        .ok_or_else(|| {
            anyhow::anyhow!("follow: parent {parent_thread_id} has no sealed lifecycle authority")
        })?;
    if !parent_lifecycle_authority.permits_durable_handoff() {
        bail!("follow: request-scoped execution cannot suspend or spawn a durable cohort");
    }
    // The callback provenance already carries the parent's sealed project
    // authority. Capability authorization is evaluated separately above; it
    // must not rewrite the authority's sealed capability ceiling while
    // deriving an inherited child.
    let parent_project_authority = cap.provenance.project_authority().clone();
    let parent_snapshot_hash = parent_project_authority
        .operational_snapshot_projection()
        .map(str::to_owned);
    let follow_key = format!(
        "{parent_thread_id}/{}/{}/{}",
        params.graph_run_id, params.follow_node, params.step_count
    );

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

    // ── Admission ceiling (authorize before capture or resolution) ────────
    // Keep this pass deliberately free of item/runtime resolution. A
    // pin-at-spawn cohort must select its one immutable generation before any
    // sibling observes project content.
    let mut canonical_children = Vec::with_capacity(children.len());
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
                format!("follow: invalid ref binding '{binding_name}' value '{binding_ref}'")
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
        canonical_children.push(child_ref);
    }

    // Reserve the logical cohort before an explicit capture. Once the exact
    // child authority is bound below, every retry consumes it and therefore
    // cannot recapture a newer live generation.
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
        child_project_authority: None,
    })?;
    if waiter.expected_children as usize != children.len() {
        bail!("follow: persisted child count conflicts with re-driven cohort");
    }
    let re_drive = waiter.phase != follow_phase::RESERVED;
    if !re_drive {
        enforce_follow_nesting_depth(state, &parent.chain_root_id)?;
    }

    let mut captured_live_generation = None;
    let mut sealed_cow_generation = None;
    let child_project_authority = if let Some(authority) = waiter.child_project_authority.clone() {
        authority
    } else {
        let mut selected = match parent_project_authority.child_policy() {
            ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit => {
                parent_project_authority.clone().for_child()?
            }
            ryeos_state::objects::ChildProjectAuthorityPolicy::PinAtSpawn { realization } => {
                let snapshot_hash = if let Some(snapshot_hash) = parent_snapshot_hash.as_deref() {
                    snapshot_hash.to_string()
                } else {
                    let capture_state = state.clone();
                    let capture_path = cap.provenance.original_project_path().to_path_buf();
                    let capture_origin_site_id = parent.origin_site_id.clone();
                    let generation = crate::execution::run_bounded_project_capture(move || {
                        crate::execution::capture_live_project_snapshot(
                            &capture_state,
                            &capture_path,
                            &capture_origin_site_id,
                            "follow-pin-at-spawn",
                        )
                    })
                    .await?;
                    let snapshot_hash = generation.snapshot_hash().to_string();
                    captured_live_generation = Some(generation);
                    snapshot_hash
                };
                crate::execution::derive_pinned_child_authority(
                    &parent_project_authority,
                    snapshot_hash,
                    realization,
                )?
            }
        };

        // A pinned-COW parent's operational layer is newer than its immutable
        // lower. Freeze and select it before child resolution, rather than
        // resolving against the lower and launching against a later overlay.
        if matches!(
            &parent_project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
                ..
            }
        ) {
            let base = parent_snapshot_hash
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("follow: pinned-COW parent has no base snapshot"))?;
            let capture_state = state.clone();
            let capture_parent_thread_id = parent_thread_id.clone();
            let capture_path = cap.provenance.effective_path().to_path_buf();
            let capture_base = base.to_owned();
            let generation = crate::execution::run_bounded_project_capture(move || {
                crate::execution::seal_callback_workspace_generation(
                    &capture_state,
                    &capture_parent_thread_id,
                    &capture_path,
                    &capture_base,
                )
            })
            .await?;
            crate::execution::ensure_control_tree_unchanged(
                state,
                base,
                generation.snapshot_hash(),
            )?;
            selected = selected.transition_operational_generation(
                ryeos_state::objects::OperationalProjectAuthorityTransition::SelectPinnedChildGeneration {
                    snapshot_hash: generation.snapshot_hash(),
                },
            )?;
            sealed_cow_generation = Some(generation);
        }

        state
            .state_store
            .bind_follow_project_authority(&follow_key, &selected)?;
        // The bound waiter is now the durable GC root for this generation.
        if let Some(generation) = captured_live_generation.take() {
            generation.publish()?;
        }
        if let Some(generation) = sealed_cow_generation.take() {
            generation.publish()?;
        }
        selected
    };
    let child_snapshot_hash = child_project_authority
        .operational_snapshot_projection()
        .map(str::to_owned);
    // Child pin-at-spawn selects only the child's immutable generation. The
    // parent's continuation advances solely when the parent itself is a COW
    // generation whose operational overlay was frozen above.
    let parent_successor_operational_generation = parent_successor_operational_generation(
        &parent_project_authority,
        &child_project_authority,
    );

    // Resolve every sibling through one immutable admission view. Per-child
    // workspaces are still created later; this shared view establishes the
    // exact item/runtime/signature/policy identity for the cohort.
    let pinned_admission_context = if let Some(snapshot_hash) = child_snapshot_hash.as_deref() {
        let capture_state = state.clone();
        let capture_snapshot_hash = snapshot_hash.to_owned();
        let capture_original_path = cap.provenance.original_project_path().to_path_buf();
        let capture_checkout_id = format!("follow-admission-{parent_thread_id}");
        Some(
            crate::execution::run_bounded_project_capture(move || {
                crate::execution::project_source::resolve_pinned_snapshot_context(
                    &capture_state,
                    &capture_snapshot_hash,
                    capture_original_path,
                    &capture_checkout_id,
                    crate::execution::project_source::PinnedContextRealization::ReadOnly,
                )
            })
            .await?,
        )
    } else {
        None
    };
    let resolution_engine = pinned_admission_context
        .as_ref()
        .map(|context| &context.request_engine)
        .unwrap_or_else(|| cap.provenance.request_engine());
    let resolution_path = pinned_admission_context
        .as_ref()
        .map(|context| context.effective_path.as_path())
        .unwrap_or_else(|| cap.provenance.effective_path());
    let admission_provenance = if let Some(context) = pinned_admission_context.as_ref() {
        let workspace_lifeline = context.temp_dir.clone().ok_or_else(|| {
            anyhow::anyhow!("follow: pinned admission context has no workspace lifeline")
        })?;
        cap.provenance.root_for_pinned_child_workspace(
            context.request_engine.clone(),
            context.pinned_materialization.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "follow: pinned admission context has no verified materialization authority"
                )
            })?,
            workspace_lifeline,
            child_project_authority.clone(),
        )?
    } else {
        let provenance = cap.provenance.clone_for_borrowed_child();
        if provenance.project_authority() != &child_project_authority {
            bail!("follow: borrowed child provenance differs from sealed child authority");
        }
        provenance
    };
    let child_project_context = match admission_provenance.project_authority() {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => ProjectContext::None,
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. } => {
            ProjectContext::LocalPath {
                path: resolution_path.to_path_buf(),
            }
        }
    };
    let child_plan_context = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: thread_auth.acting_principal.clone(),
                scopes: cap.effective_caps.clone(),
            },
        ),
        project_context: child_project_context.clone(),
        subject_resolution_authority: admission_provenance.subject_resolution_authority(),
        current_site_id: parent.current_site_id.clone(),
        origin_site_id: parent.current_site_id.clone(),
        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
        validate_only: false,
    };
    let child_project_binding =
        ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
            resolution_engine,
            &child_plan_context,
            &admission_provenance,
        )?;

    let (admitted_children, persisted_child_slots) = admit_follow_child_requests(
        state,
        &children,
        &canonical_children,
        &spec_hashes,
        &follow_key,
        resolution_engine,
        &child_plan_context,
        &child_project_binding,
        &parent.current_site_id,
        &parent.origin_site_id,
        &thread_auth.acting_principal,
        &child_project_context,
    )?;

    // ── Ordered spawn sequence, idempotent by follow_key ────────────────────
    // 1. The waiter and exact child authority were reserved and bound before
    //    item resolution. Validate any durable child slots now.
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
    // say which child roots committed before a crash. Reuse durable slot IDs and
    // allocate fresh IDs in memory, then classify each child from its own row.
    let window_key = params
        .launch_window_width
        .map(|_| format!("follow:{follow_key}"));
    let expected_launch_window = params.launch_window_width.map(|width| FollowLaunchWindow {
        key: format!("follow:{follow_key}"),
        width,
    });

    // Select the exact stable identities before launch authority is prepared.
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
        accounting_scope: cap.accounting_scope.clone(),
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
        let slot = persisted_child_slots.get(&item_index).ok_or_else(|| {
            anyhow::anyhow!(
                "follow: existing child {child_id} has no persisted durable slot identity"
            )
        })?;
        if child_row.kind != resume.kind
            || child_row.item_ref != resume.item_ref
            || resume.item_ref != child_spec.item_ref
            || resume.ref_bindings != child_spec.ref_bindings
            || resume.parameters != child_spec.parameters
            || resume.launch_mode != "detached"
            || serde_json::to_value(&metadata.sealed_root_request)?
                != serde_json::to_value(&slot.sealed_root_request)?
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
    let prepared_follow_children = prepare_follow_children(
        state,
        &children,
        &authority_indices,
        &existing_created_indices,
        &persisted_launch_metadata,
        &admitted_children,
        &reserved_child_ids,
        child_snapshot_hash.as_deref(),
        &child_project_authority,
        &child_project_context,
        &expected_parent_context,
        expected_launch_window.as_ref(),
        &parent_thread_id,
        &parent.current_site_id,
        &parent.origin_site_id,
        parent_lifecycle_authority,
        &thread_auth.acting_principal,
        &cap,
        resolution_engine,
    )
    .await?;

    // Publish each fresh durable slot only after its metadata contains the
    // finalized sealed request produced by the complete authority pass. A
    // crash before this point leaves no child identity to recover; a crash
    // after it re-drives from exact finalized authority.
    for item_index in fresh_indices.iter().copied() {
        let child = &children[item_index];
        let child_id = reserved_child_ids.get(&item_index).ok_or_else(|| {
            anyhow::anyhow!("follow: missing fresh child ID at index {item_index}")
        })?;
        let metadata = prepared_follow_children
            .child_metadata
            .get(&item_index)
            .ok_or_else(|| {
                anyhow::anyhow!("follow: missing finalized child metadata at index {item_index}")
            })?;
        let sealed = metadata.sealed_root_request.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "follow: fresh child metadata has no finalized sealed request at index {item_index}"
            )
        })?;
        state.state_store.set_follow_child(
            &follow_key,
            item_index as u32,
            &child.item_ref,
            &spec_hashes[item_index],
            child_id,
            child_id,
            sealed,
        )?;
    }

    // 2. Child root row (created, NOT launched) + seeded launch identity. A follow
    //    child is a FRESH ROOT: its own chain root, no upstream braid. The root
    //    snapshot and authoritative launch audit share one signed birth commit.
    let mut prepared_by_child = commit_follow_child_roots(
        state,
        &children,
        &reserved_child_ids,
        &fresh_indices,
        &authority_indices,
        &child_project_authority,
        &parent_thread_id,
        &parent.chain_root_id,
        &params.follow_node,
        params.step_count,
        prepared_follow_children,
    )?;

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
            anyhow::anyhow!("follow: {} waiter has no parent successor", waiter.phase)
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
                    let parent_source_metadata =
                        parent_launch_metadata.as_ref().ok_or_else(|| {
                            anyhow::anyhow!(
                                "follow: parent {parent_thread_id} has no persisted launch metadata"
                            )
                        })?;
                    let mut successor_launch_metadata = parent_source_metadata
                        .for_continuation_successor(
                            &parent_thread_id,
                            ryeos_app::launch_metadata::daemon_checkpoint_dir(
                                &state.config.app_root,
                                &successor_id,
                            ),
                        );
                    if let Some(frozen) = parent_successor_operational_generation.as_deref() {
                        let resume = successor_launch_metadata
                            .resume_context
                            .as_mut()
                            .ok_or_else(|| {
                                anyhow::anyhow!("follow: successor lost its durable ResumeContext")
                            })?;
                        resume.original_snapshot_hash = Some(frozen.to_string());
                        resume.original_pushed_head_ref = None;
                        if matches!(
                            resume.project_authority,
                            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. }
                        ) {
                            resume.project_authority = resume
                                .project_authority
                                .transition_operational_generation(
                                    ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                                        result_snapshot_hash: frozen,
                                    },
                                )?;
                        }
                    }
                    let successor_resume = successor_launch_metadata
                        .resume_context
                        .as_ref()
                        .ok_or_else(|| {
                            anyhow::anyhow!("follow: successor lost its durable ResumeContext")
                        })?;
                    let successor_sealed_request = parent_source_metadata
                        .sealed_root_request
                        .as_ref()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "follow: parent {parent_thread_id} has no admitted launch capsule"
                            )
                        })?
                        .for_continuation_invocation(successor_resume)?;
                    successor_launch_metadata.set_sealed_root_request(successor_sealed_request);
                    let successor_realization =
                        crate::execution::execution_realization::transition_continuation_project_authority(
                            state,
                            parent_source_metadata,
                            &successor_launch_metadata,
                        )?;
                    successor_launch_metadata = successor_launch_metadata
                        .with_execution_realization_hash(successor_realization.hash);
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
                            base_project_snapshot_hash: successor_launch_metadata
                                .resume_context
                                .as_ref()
                                .and_then(|resume| resume.durable_project_snapshot_hash())
                                .map(str::to_owned),
                            project_authority: successor_launch_metadata
                                .resume_context
                                .as_ref()
                                .map(|resume| resume.project_authority.clone())
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "follow successor lost its sealed project authority"
                                    )
                                })?,
                            usage_subject: None,
                            usage_subject_asserted_by: None,
                            captured_history_policy: None,
                        },
                        &parent_thread_id,
                        &parent.chain_root_id,
                        &params.completion,
                        &successor_launch_metadata,
                        child_snapshot_hash.as_deref(),
                    )?;
                    drop(successor_realization.publication);
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
        if response_phase == follow_phase::WAITING
            && let Some(window_key) = window_key.as_deref()
        {
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
        let prepared = prepared_by_child
            .remove(&launch_child_id)
            .ok_or_else(|| anyhow::anyhow!("follow: admitted unknown child {launch_child_id}"))?;
        let (launch_handoff, launch_ready) = crate::execution::launch::LaunchHandoff::channel();
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

    // The follow callback is the cooperative RuntimeQuiesced boundary for the
    // current launch owner. The graph runtime is blocked throughout sealing
    // and handoff; once the successor and cohort are durable, revoke both
    // runtime capabilities before replying. Any attempted post-follow event,
    // artifact, cost, child intent, or second handoff from the predecessor is
    // therefore fenced even if a faulty runtime continues after the response.
    state.callback_tokens.invalidate(&cap.token);
    state.thread_auth.invalidate(&thread_auth.token);
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

#[allow(clippy::too_many_arguments)]
fn admit_follow_child_requests(
    state: &AppState,
    children: &[ryeos_runtime::callback::FollowChildSpec],
    canonical_children: &[CanonicalRef],
    spec_hashes: &[String],
    follow_key: &str,
    resolution_engine: &std::sync::Arc<ryeos_engine::engine::Engine>,
    child_plan_context: &ryeos_engine::contracts::PlanContext,
    child_project_binding: &ryeos_app::thread_lifecycle::AdmittedProjectBinding,
    parent_current_site_id: &str,
    parent_origin_site_id: &str,
    acting_principal: &str,
    child_project_context: &ProjectContext,
) -> Result<(
    Vec<AdmittedFollowChildRequest>,
    std::collections::BTreeMap<usize, ryeos_app::runtime_db::FollowWaiterChild>,
)> {
    let mut resolved_children = Vec::with_capacity(children.len());
    let mut persisted_child_slots = std::collections::BTreeMap::new();
    for (item_index, (child, child_ref)) in
        children.iter().zip(canonical_children.iter()).enumerate()
    {
        let item_index = u32::try_from(item_index).context("follow: too many children")?;
        let persisted_slot = state.state_store.get_follow_child(follow_key, item_index)?;
        let admitted = if let Some(slot) = persisted_slot.as_ref() {
            if slot.item_ref != child.item_ref || slot.spec_hash != spec_hashes[item_index as usize]
            {
                bail!("follow: persisted child conflicts at index {item_index}");
            }
            AdmittedFollowChildRequest::Persisted(slot.sealed_root_request.clone())
        } else {
            // Fresh admission is intentionally in-memory. The durable slot is
            // written only after generic preparation captures the complete
            // hook plan and constructs a FinalizedEffectiveProgram.
            let child_runtime = resolution_engine
                .runtimes
                .resolve_for_launch(None, &child_ref.kind)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "follow: child kind '{}' has no managed runtime — a follow child must be a \
                         managed runtime execution: {error}",
                        child_ref.kind
                    )
                })?;
            let child_runtime_ref = child_runtime.canonical_ref.to_string();
            let child_preflight = ryeos_app::thread_lifecycle::preflight_root_execution(
                ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
                    engine: resolution_engine,
                    plan_context: child_plan_context.clone(),
                    project_binding: child_project_binding.clone(),
                    node_history_policy: &state.node_history_policy,
                    item_ref: &child.item_ref,
                    launch_mode: "detached",
                    parameters: child.parameters.clone(),
                    ref_bindings: child.ref_bindings.clone(),
                    usage_subject: None,
                    usage_subject_asserted_by: None,
                    creates_chain_root: true,
                },
            )
            .with_context(|| {
                format!(
                    "follow: verified history-policy preflight for child '{}'",
                    child.item_ref
                )
            })?;
            let child_execution = child_preflight.root_admission.execution_request(
                ryeos_app::thread_lifecycle::RootExecutionRoute::ManagedRuntimeForKind(
                    &child_runtime.canonical_ref,
                ),
                "detached".to_string(),
                child.parameters.clone(),
            )?;
            if child_execution.item_ref != child.item_ref
                || child_execution.ref_bindings != child.ref_bindings
                || child_execution.parameters != child.parameters
                || child_execution.launch_mode != "detached"
                || child_execution.current_site_id != parent_current_site_id
                || child_execution.origin_site_id != parent_origin_site_id
                || child_execution.requested_by.as_deref() != Some(acting_principal)
                || child_execution.plan_context.project_context != *child_project_context
            {
                bail!("follow: fresh child admission conflicts at index {item_index}");
            }
            AdmittedFollowChildRequest::Fresh {
                request: child_execution,
                runtime_ref: child_runtime_ref,
            }
        };
        if let Some(slot) = persisted_slot {
            persisted_child_slots.insert(item_index as usize, slot);
        }
        resolved_children.push(admitted);
    }
    Ok((resolved_children, persisted_child_slots))
}

struct PreparedFollowChildren {
    child_metadata: std::collections::BTreeMap<usize, RuntimeLaunchMetadata>,
    prepared_children:
        std::collections::BTreeMap<usize, crate::execution::launch::PreparedFollowChildLaunch>,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_follow_children(
    state: &AppState,
    children: &[ryeos_runtime::callback::FollowChildSpec],
    authority_indices: &std::collections::BTreeSet<usize>,
    existing_created_indices: &std::collections::BTreeSet<usize>,
    persisted_launch_metadata: &std::collections::BTreeMap<usize, RuntimeLaunchMetadata>,
    admitted_children: &[AdmittedFollowChildRequest],
    reserved_child_ids: &std::collections::BTreeMap<usize, String>,
    child_snapshot_hash: Option<&str>,
    child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    child_project_context: &ProjectContext,
    persisted_parent_context: &PersistedParentExecutionContext,
    expected_launch_window: Option<&FollowLaunchWindow>,
    parent_thread_id: &str,
    parent_current_site_id: &str,
    parent_origin_site_id: &str,
    parent_lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    acting_principal: &str,
    cap: &ryeos_app::callback_token::CallbackCapability,
    resolution_engine: &std::sync::Arc<ryeos_engine::engine::Engine>,
) -> Result<PreparedFollowChildren> {
    let requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: acting_principal.to_owned(),
        scopes: cap.effective_caps.clone(),
    });
    let launch_parent_context = crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
        accounting_scope: cap.accounting_scope.clone(),
    };
    let mut child_metadata = std::collections::BTreeMap::new();
    let mut prepared_children = std::collections::BTreeMap::new();
    for (item_index, child) in children.iter().enumerate() {
        if !authority_indices.contains(&item_index) {
            continue;
        }
        let existing_row = existing_created_indices.contains(&item_index);
        let mut fresh_request = None;
        let mut meta = if existing_row {
            persisted_launch_metadata
                .get(&item_index)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: missing persisted launch metadata at index {item_index}"
                    )
                })?
        } else {
            let admitted = admitted_children.get(item_index).ok_or_else(|| {
                anyhow::anyhow!("follow: missing child admission at index {item_index}")
            })?;
            let (child_execution, runtime_ref, finalized_sealed) = match admitted {
                AdmittedFollowChildRequest::Fresh {
                    request,
                    runtime_ref,
                } => {
                    fresh_request = Some(request.clone());
                    (request.clone(), runtime_ref.clone(), None)
                }
                AdmittedFollowChildRequest::Persisted(sealed) => {
                    let capsule_root = ryeos_app::launch_metadata::daemon_thread_state_dir(
                        &state.config.app_root,
                        parent_thread_id,
                    )
                    .join("admission-capsules")
                    .join(format!("follow-{item_index}"));
                    (
                        sealed.restore(resolution_engine, &capsule_root)?,
                        sealed.runtime_ref().to_string(),
                        Some(sealed.clone()),
                    )
                }
            };
            let (seed_project_context, project_authority) = match finalized_sealed.as_ref() {
                Some(sealed) => durable_follow_child_seed_project_identity(
                    sealed,
                    child_project_authority,
                    child_project_context,
                ),
                None => Ok((
                    child_project_context.clone(),
                    child_project_authority.clone(),
                )),
            }
            .with_context(|| {
                format!("follow: sealed child project authority conflicts at index {item_index}")
            })?;
            let stable_project_identity = match &project_authority {
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => None,
                ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
                | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. } => Some(
                    ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                        cap.provenance.original_project_path(),
                        parent_origin_site_id,
                    )?,
                ),
            };
            let local_overlay_root = matches!(
                project_authority.environment(),
                ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { .. }
            )
            .then(|| cap.provenance.original_project_path().to_path_buf());
            let mut meta = RuntimeLaunchMetadata::default()
                .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
                .with_resume_context(ResumeContext {
                    kind: child_execution.kind.clone(),
                    item_ref: child.item_ref.clone(),
                    ref_bindings: child.ref_bindings.clone(),
                    launch_mode: "detached".to_string(),
                    parameters: child.parameters.clone(),
                    project_context: seed_project_context,
                    project_authority,
                    lifecycle_authority: parent_lifecycle_authority,
                    stable_project_identity,
                    local_overlay_root,
                    original_snapshot_hash: child_snapshot_hash.map(str::to_owned),
                    original_pushed_head_ref: None,
                    state_root: cap
                        .provenance
                        .state_root_override()
                        .map(std::path::Path::to_path_buf),
                    current_site_id: parent_current_site_id.to_owned(),
                    origin_site_id: parent_origin_site_id.to_owned(),
                    requested_by: requested_by.clone(),
                    execution_hints: ExecutionHints::default(),
                    effective_caps: Vec::new(),
                    parent_delegation_caps: Some(
                        cap.effective_caps
                            .iter()
                            .cloned()
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect(),
                    ),
                    executor_ref: Some(child_execution.executor_ref.clone()),
                    runtime_ref: Some(runtime_ref),
                });
            if let Some(sealed) = finalized_sealed {
                meta.set_sealed_root_request(sealed);
            }
            meta.follow_parent_context = Some(persisted_parent_context.clone());
            meta.follow_launch_window = expected_launch_window.cloned();
            meta
        };
        let child_thread_id = reserved_child_ids.get(&item_index).ok_or_else(|| {
            anyhow::anyhow!("follow: missing reserved child ID at index {item_index}")
        })?;
        let launch_provenance = if let Some(snapshot_hash) = child_snapshot_hash {
            let realization = crate::execution::project_source::pinned_context_realization(
                child_project_authority,
            )?;
            let capture_state = state.clone();
            let capture_snapshot_hash = snapshot_hash.to_owned();
            let capture_original_path = cap.provenance.original_project_path().to_path_buf();
            let capture_child_thread_id = child_thread_id.clone();
            let child_context = crate::execution::run_bounded_project_capture(move || {
                crate::execution::project_source::resolve_pinned_snapshot_context(
                    &capture_state,
                    &capture_snapshot_hash,
                    capture_original_path,
                    &capture_child_thread_id,
                    realization,
                )
            })
            .await?;
            let child_lifeline = child_context
                .temp_dir
                .ok_or_else(|| anyhow::anyhow!("follow: child workspace has no lifecycle guard"))?;
            cap.provenance.root_for_pinned_child_workspace(
                child_context.request_engine,
                child_context.pinned_materialization.ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: child context has no verified materialization authority"
                    )
                })?,
                child_lifeline,
                child_project_authority.clone(),
            )?
        } else {
            cap.provenance.clone_for_borrowed_child()
        };
        if launch_provenance.project_authority() != child_project_authority {
            bail!("follow: child launch provenance differs from sealed child authority");
        }
        if let Some(cohort_request) = fresh_request.take() {
            let runtime_ref = meta
                .resume_context
                .as_ref()
                .and_then(|resume| resume.runtime_ref.as_deref())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "follow: fresh child resume has no runtime at index {item_index}"
                    )
                })?;
            let launch_request = readmit_fresh_follow_child_for_launch(
                state,
                child,
                &cohort_request,
                runtime_ref,
                &launch_provenance,
            )
            .with_context(|| {
                format!(
                    "follow: bind fresh child admission to launch materialization at index {item_index}"
                )
            })?;
            meta.resume_context
                .as_mut()
                .expect("fresh follow metadata was constructed with a resume context")
                .project_context = launch_request.plan_context.project_context.clone();
            fresh_request = Some(launch_request);
        }
        let prepared = if existing_row
            || matches!(
                admitted_children.get(item_index),
                Some(AdmittedFollowChildRequest::Persisted(_))
            ) {
            crate::execution::launch::prepare_existing_follow_child_launch(
                state,
                child_thread_id,
                &meta,
                launch_provenance,
                launch_parent_context.clone(),
            )
            .await?
        } else {
            crate::execution::launch::prepare_follow_child_launch(
                state,
                child_thread_id,
                &meta,
                fresh_request.take().ok_or_else(|| {
                    anyhow::anyhow!("follow: fresh child admission vanished at index {item_index}")
                })?,
                launch_provenance,
                launch_parent_context.clone(),
            )
            .await?
        };
        child_metadata.insert(item_index, prepared.launch_metadata().clone());
        prepared_children.insert(item_index, prepared);
    }
    Ok(PreparedFollowChildren {
        child_metadata,
        prepared_children,
    })
}

/// Re-admit a fresh follow root against the exact materialization it will
/// execute from. Cohort admission proves that every requested child is valid
/// against the selected immutable generation before identities are allocated;
/// this second use of the same admission implementation binds the executable
/// request snapshot to the per-root materialization that the launcher retains.
/// Materialization identity is never weakened to path or snapshot equivalence.
fn readmit_fresh_follow_child_for_launch(
    state: &AppState,
    child: &ryeos_runtime::callback::FollowChildSpec,
    cohort_request: &ResolvedExecutionRequest,
    cohort_runtime_ref: &str,
    launch_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
) -> Result<ResolvedExecutionRequest> {
    let engine = launch_provenance.request_engine();
    let child_ref = CanonicalRef::parse(&child.item_ref)
        .with_context(|| format!("follow: invalid admitted child ref '{}'", child.item_ref))?;
    let runtime = engine
        .runtimes
        .resolve_for_launch(None, &child_ref.kind)
        .map_err(|error| {
            anyhow::anyhow!(
                "follow: child kind '{}' has no managed runtime in its launch materialization: {error}",
                child_ref.kind
            )
        })?;
    if runtime.canonical_ref.to_string() != cohort_runtime_ref {
        bail!("follow: child runtime changed between cohort admission and launch materialization");
    }

    let project_context = match launch_provenance.project_authority() {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => ProjectContext::None,
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. } => {
            ProjectContext::LocalPath {
                path: launch_provenance.effective_path().to_path_buf(),
            }
        }
    };
    let mut plan_context = cohort_request.plan_context.clone();
    plan_context.project_context = project_context;
    plan_context.subject_resolution_authority = launch_provenance.subject_resolution_authority();
    let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        engine,
        &plan_context,
        launch_provenance,
    )?;
    let preflight = ryeos_app::thread_lifecycle::preflight_root_execution(
        ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
            engine,
            plan_context,
            project_binding,
            node_history_policy: &state.node_history_policy,
            item_ref: &child.item_ref,
            launch_mode: "detached",
            parameters: child.parameters.clone(),
            ref_bindings: child.ref_bindings.clone(),
            usage_subject: None,
            usage_subject_asserted_by: None,
            creates_chain_root: true,
        },
    )?;
    let launch_request = preflight.root_admission.execution_request(
        ryeos_app::thread_lifecycle::RootExecutionRoute::ManagedRuntimeForKind(
            &runtime.canonical_ref,
        ),
        "detached".to_string(),
        child.parameters.clone(),
    )?;
    ensure_follow_admission_semantics_match(cohort_request, &launch_request)?;
    Ok(launch_request)
}

/// The two admissions intentionally differ in their operational workspace
/// binding. Everything that can change executable meaning, history policy, or
/// invocation identity must remain exact across that relocation.
fn ensure_follow_admission_semantics_match(
    cohort: &ResolvedExecutionRequest,
    launch: &ResolvedExecutionRequest,
) -> Result<()> {
    if cohort.kind != launch.kind
        || cohort.item_ref != launch.item_ref
        || cohort.executor_ref != launch.executor_ref
        || cohort.launch_mode != launch.launch_mode
        || cohort.current_site_id != launch.current_site_id
        || cohort.origin_site_id != launch.origin_site_id
        || cohort.target_site_id != launch.target_site_id
        || cohort.requested_by != launch.requested_by
        || cohort.usage_subject != launch.usage_subject
        || cohort.usage_subject_asserted_by != launch.usage_subject_asserted_by
        || cohort.parameters != launch.parameters
        || cohort.ref_bindings != launch.ref_bindings
        || cohort.root_raw_content_digest != launch.root_raw_content_digest
        || cohort.resolved_item.canonical_ref != launch.resolved_item.canonical_ref
        || cohort.resolved_item.kind != launch.resolved_item.kind
        || cohort.resolved_item.content_hash != launch.resolved_item.content_hash
    {
        bail!(
            "follow: child executable identity changed between cohort admission and launch materialization"
        );
    }
    let cohort_admission = cohort
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("follow: cohort child request has no root admission"))?;
    let launch_admission = launch
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("follow: launch child request has no root admission"))?;
    let cohort_digest = cohort_admission
        .resolution_output()
        .effective_definition_digest()?;
    let launch_digest = launch_admission
        .resolution_output()
        .effective_definition_digest()?;
    if cohort_digest != launch_digest
        || cohort_admission.thread_profile() != launch_admission.thread_profile()
        || serde_json::to_value(cohort_admission.captured_history_policy())?
            != serde_json::to_value(launch_admission.captured_history_policy())?
    {
        bail!(
            "follow: child admitted semantics changed between cohort admission and launch materialization"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_follow_child_roots(
    state: &AppState,
    children: &[ryeos_runtime::callback::FollowChildSpec],
    reserved_child_ids: &std::collections::BTreeMap<usize, String>,
    fresh_indices: &std::collections::BTreeSet<usize>,
    authority_indices: &std::collections::BTreeSet<usize>,
    child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    parent_thread_id: &str,
    parent_chain_root_id: &str,
    follow_node: &str,
    step_count: i64,
    prepared: PreparedFollowChildren,
) -> Result<std::collections::BTreeMap<String, crate::execution::launch::PreparedFollowChildLaunch>>
{
    let PreparedFollowChildren {
        mut child_metadata,
        mut prepared_children,
    } = prepared;
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
            fresh_prepared.verify_fresh_launch_authority_unchanged()?;
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
                        storage_class: ryeos_runtime::events::RuntimeEventType::ThreadFacetSet
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
                    child_project_authority.clone(),
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
                || serde_json::to_value(&persisted.sealed_root_request)?
                    != serde_json::to_value(&meta.sealed_root_request)?
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
                || serde_json::to_value(&persisted.sealed_root_request)?
                    != serde_json::to_value(&expected.sealed_root_request)?
                || persisted.follow_parent_context != expected.follow_parent_context
                || persisted.follow_launch_window != expected.follow_launch_window
            {
                bail!("follow: child metadata changed during preparation at index {item_index}");
            }
        }
        let inherited_stop =
            match state
                .state_store
                .record_child_link(parent_thread_id, &child_thread_id, "follow")
            {
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
        // parent is settled `continued`.
        match state.threads.append_child_thread_spawned_once(
            parent_chain_root_id,
            parent_thread_id,
            &child_thread_id,
            json!({
                "child_thread_id": child_thread_id,
                "node": follow_node,
                "step": step_count,
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
    Ok(prepared_by_child)
}

fn validate_follow_launch(children_len: usize, launch_window_width: Option<u32>) -> Result<bool> {
    if children_len == 0 {
        bail!("follow: children must be nonempty");
    }
    if launch_window_width == Some(0) {
        bail!("follow: launch_window_width must be greater than zero");
    }
    Ok(children_len > 1 || launch_window_width.is_some())
}

#[cfg(test)]
mod launch_shape_tests {
    use super::validate_follow_launch;

    #[test]
    fn single_child_can_use_a_launch_window() {
        assert!(validate_follow_launch(1, Some(1)).expect("single-item launch window is valid"));
        assert!(validate_follow_launch(1, Some(8)).expect("window may exceed cohort size"));
    }

    #[test]
    fn invalid_follow_launch_shapes_remain_rejected() {
        assert!(validate_follow_launch(0, Some(1)).is_err());
        assert!(validate_follow_launch(1, Some(0)).is_err());
    }
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

fn parent_successor_operational_generation(
    parent: &ryeos_state::objects::ExecutionProjectAuthority,
    child: &ryeos_state::objects::ExecutionProjectAuthority,
) -> Option<String> {
    matches!(
        parent,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        }
    )
    .then(|| child.operational_snapshot_projection().map(str::to_owned))
    .flatten()
}

/// Return the immutable project pair used to seed a follow-child row.
///
/// A RESERVED repair can reconstruct a different disposable pinned checkout
/// after the slot was committed. Only the slot's sealed pair may become the
/// row's durable ResumeContext; the reconstructed checkout is rebound later
/// as transient launch provenance. The transient context is an explicit input
/// so the production call makes the potential substitution visible while this
/// helper always returns the sealed durable pair.
fn durable_follow_child_seed_project_identity(
    sealed_root_request: &SealedRootExecutionRequest,
    expected_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    _reconstructed_launch_context: &ProjectContext,
) -> Result<(
    ProjectContext,
    ryeos_state::objects::ExecutionProjectAuthority,
)> {
    let sealed_project_authority = sealed_root_request.project_authority();
    if sealed_project_authority != expected_project_authority {
        bail!("sealed child project authority differs from the admitted child authority");
    }
    Ok((
        sealed_root_request.project_context().clone(),
        sealed_project_authority.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        durable_follow_child_seed_project_identity, parent_successor_operational_generation,
    };
    use ryeos_engine::contracts::ProjectContext;
    use ryeos_state::objects::{
        EnvironmentAuthority, ExecutionProjectAuthority, LiveProjectAccess,
        PinnedProjectRealization, PinnedTerminalPublication,
    };

    fn pinned(hash_byte: char, realization: PinnedProjectRealization) -> ExecutionProjectAuthority {
        ExecutionProjectAuthority::pinned(
            "project:test".to_string(),
            Some(std::path::PathBuf::from("/project")),
            hash_byte.to_string().repeat(64),
            realization,
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn live_pin_at_spawn_does_not_advance_the_parent_successor() {
        let project = tempfile::tempdir().unwrap();
        let parent = ExecutionProjectAuthority::live(
            project.path().canonicalize().unwrap(),
            "project:test".to_string(),
            LiveProjectAccess::ReadWrite,
            ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        let child = pinned('b', PinnedProjectRealization::ReadOnly);
        assert_eq!(
            parent_successor_operational_generation(&parent, &child),
            None
        );
    }

    #[test]
    fn pinned_cow_parent_advances_to_the_frozen_operational_generation() {
        let parent = pinned(
            'a',
            PinnedProjectRealization::Cow {
                terminal_publication: PinnedTerminalPublication::Discard,
            },
        );
        let child = pinned('b', PinnedProjectRealization::ReadOnly);
        assert_eq!(
            parent_successor_operational_generation(&parent, &child),
            Some("b".repeat(64))
        );
    }

    #[test]
    fn reserved_partial_crash_requires_the_exact_slot_authority_and_keeps_its_context() {
        let authority = pinned('c', PinnedProjectRealization::ReadOnly);
        let sealed_slot_context = ProjectContext::LocalPath {
            path: std::path::PathBuf::from("/old-owned-checkout/project"),
        };
        let sealed_slot = ryeos_app::thread_lifecycle::SealedRootExecutionRequest::
            storage_test_fixture_with_project_identity(
                sealed_slot_context.clone(),
                authority.clone(),
            );
        let reconstructed_context = ProjectContext::LocalPath {
            path: std::path::PathBuf::from("/new-owned-checkout/project"),
        };

        let (resume_context, resume_authority) = durable_follow_child_seed_project_identity(
            &sealed_slot,
            &authority,
            &reconstructed_context,
        )
        .unwrap();

        assert_eq!(resume_context, sealed_slot_context);
        assert_ne!(resume_context, reconstructed_context);
        assert_eq!(resume_authority, authority);

        let drifted_authority = authority
            .clone()
            .with_capability_ceiling(vec!["ryeos.execute.directive.other".to_string()])
            .unwrap();
        let error = durable_follow_child_seed_project_identity(
            &sealed_slot,
            &drifted_authority,
            &reconstructed_context,
        )
        .expect_err(
            "reserved repair must not combine the sealed slot context with reconstructed authority",
        );
        assert!(
            error
                .to_string()
                .contains("sealed child project authority differs")
        );
    }
}
