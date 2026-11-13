//! The launch-time host-access gate.
//!
//! Runs right after launch trust, before finalization, any secret read or any
//! process start. A root whose admitted composed subject projects the
//! host-direct posture is admitted only through
//! [`ryeos_app::host_access::admit`], now with its resolved runtime; every
//! confined root passes with `None`. The execute route ran the same check at
//! admission; this one also covers graph children and schedule fires, which
//! never pass through that route.

use ryeos_app::host_access::{
    AdmittedHostAccess, HostAccessCaller, HostAccessRefusal, HostAccessSubject,
};

use crate::dispatch_error::DispatchError;

pub(crate) fn refusal(refusal: HostAccessRefusal) -> DispatchError {
    DispatchError::LaunchPolicyForbidden {
        code: refusal.code.to_owned(),
        message: refusal.message,
        binding: None,
    }
}

/// The posture the admitted composed subject projects; `Confined` for a
/// request with no admitted subject only when its kind has no host-authority
/// projection.
pub(crate) fn admitted_posture(
    engine: &ryeos_engine::engine::Engine,
    resolved: &ryeos_app::thread_lifecycle::ResolvedExecutionRequest,
) -> Result<ryeos_engine::contracts::ExecutionPosture, DispatchError> {
    ryeos_app::thread_lifecycle::project_execution_posture(engine, resolved).map_err(|error| {
        DispatchError::SchemaMisconfigured {
            kind: resolved.resolved_item.kind.clone(),
            detail: format!("{error:#}"),
        }
    })
}

/// Admit a host-direct root at launch, or `None` for a confined root.
pub(crate) fn enforce_host_access(
    state: &ryeos_app::state::AppState,
    engine: &ryeos_engine::engine::Engine,
    resolved: &ryeos_app::thread_lifecycle::ResolvedExecutionRequest,
    terminal: &ryeos_engine::plan_builder::ResolvedTerminalExecutor,
    request: &crate::dispatch::DispatchRequest<'_>,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
) -> Result<Option<AdmittedHostAccess>, DispatchError> {
    if !admitted_posture(engine, resolved)?.is_host_direct() {
        return Ok(None);
    }
    let admission = resolved.root_admission.as_ref().ok_or_else(|| {
        DispatchError::Internal(anyhow::anyhow!(
            "host-direct launch of `{}` has no admitted composed subject",
            resolved.item_ref
        ))
    })?;
    let output = admission.resolution_output();
    let source_closure = ryeos_app::host_access::source_closure_digest(
        output
            .composed
            .derived
            .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY),
    )
    .map_err(DispatchError::Internal)?;
    let runtime = terminal.chain_authorities.first().ok_or_else(|| {
        refusal(HostAccessRefusal::new(
            HostAccessRefusal::NOT_GRANTED,
            format!(
                "`{}` resolved no runtime executor; a host-access grant pins one",
                output.root.resolved_ref
            ),
        ))
    })?;
    let subject = HostAccessSubject {
        item: &output.root.resolved_ref,
        signer: output.root.signer_fingerprint.as_deref(),
        source_closure: source_closure.as_deref(),
        runtime: Some((
            runtime.canonical_ref.as_str(),
            runtime.content_hash.as_str(),
        )),
    };
    let current_site_id = state.threads.site_id();
    ryeos_app::host_access::admit(
        state,
        engine,
        &subject,
        &HostAccessCaller {
            acting_principal: request.acting_principal,
            handler_context,
            target_site_id: request
                .target_site_id
                .filter(|target| *target != current_site_id),
            current_site_id,
            origin_site_id: &resolved.origin_site_id,
            parent_thread_id: request
                .parent_execution_context
                .as_ref()
                .map(|parent| parent.parent_thread_id.as_str()),
            scheduled: resolved.plan_context.scheduled_fire.is_some(),
        },
    )
    .map(Some)
    .map_err(refusal)
}

/// For a root that was not admitted by the execute route (a graph child or a
/// schedule fire), judge host access before its root admission is sealed and,
/// when it is granted, return its provenance with the project authority
/// converted to `unconfined_host`. The root admission is then sealed from that
/// converted provenance, so the child's record says what runs while its parent
/// keeps its own confinement. Returns `None` for every confined root, and for a
/// root that does not resolve here: dispatch reports that itself, and the
/// launch gate still refuses a host-direct launch sealed confined.
pub(crate) fn host_direct_provenance_before_admission(
    state: &ryeos_app::state::AppState,
    engine: &ryeos_engine::engine::Engine,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    item: &ryeos_engine::canonical_ref::CanonicalRef,
    request: &crate::dispatch::DispatchRequest<'_>,
    handler_context: Option<&ryeos_app::handler_context::HandlerContext>,
) -> Result<Option<ryeos_app::execution_provenance::ExecutionProvenance>, DispatchError> {
    let Some(execution) = engine
        .kinds
        .get(&item.kind)
        .and_then(|schema| schema.execution.as_ref())
        .filter(|execution| execution.host_authority.is_some())
    else {
        return Ok(None);
    };
    let project_root = match &plan_ctx.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };
    let Ok((_, output)) =
        engine.effective_item_with_resolution(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: item.clone(),
            expected_kind: Some(item.kind.clone()),
            project_root,
            subject_resolution_authority: plan_ctx.subject_resolution_authority.clone(),
        })
    else {
        return Ok(None);
    };
    let posture = execution
        .project_execution_posture(&output.composed.composed)
        .map_err(|error| DispatchError::SchemaMisconfigured {
            kind: item.kind.clone(),
            detail: error.to_string(),
        })?;
    if !posture.is_host_direct() {
        return Ok(None);
    }
    let source_closure = ryeos_app::host_access::source_closure_digest(
        output
            .composed
            .derived
            .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY),
    )
    .map_err(DispatchError::Internal)?;
    let subject = HostAccessSubject {
        item: &output.root.resolved_ref,
        signer: output.root.signer_fingerprint.as_deref(),
        source_closure: source_closure.as_deref(),
        runtime: None,
    };
    let current_site_id = state.threads.site_id();
    ryeos_app::host_access::admit(
        state,
        engine,
        &subject,
        &HostAccessCaller {
            acting_principal: request.acting_principal,
            handler_context,
            target_site_id: request
                .target_site_id
                .filter(|target| *target != current_site_id),
            current_site_id,
            origin_site_id: &plan_ctx.origin_site_id,
            parent_thread_id: request
                .parent_execution_context
                .as_ref()
                .map(|parent| parent.parent_thread_id.as_str()),
            scheduled: plan_ctx.scheduled_fire.is_some(),
        },
    )
    .map_err(refusal)?;
    request
        .provenance
        .clone()
        .into_host_direct()
        .map(Some)
        .map_err(|error| {
            refusal(HostAccessRefusal::new(
                HostAccessRefusal::PROJECT_REFUSED,
                format!("{error:#}"),
            ))
        })
}

/// Require the sealed project authority to be true for a host process: a
/// projectless execution, or a read-write live project sealed as
/// `unconfined_host`. The execute route seals a host-direct root that way, and
/// [`host_direct_provenance_before_admission`] seals a graph child or schedule
/// fire that way before its root admission. Anything else is refused rather
/// than run under a record that says it was confined.
pub(crate) fn require_host_direct_project(
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
) -> Result<(), DispatchError> {
    use ryeos_state::objects::{
        ExecutionProjectAuthority, LiveFilesystemConfinement, LiveProjectAccess,
    };
    let honest = match provenance.project_authority() {
        ExecutionProjectAuthority::Projectless { .. } => true,
        ExecutionProjectAuthority::LiveProject { live_access, .. } => {
            live_access.access == LiveProjectAccess::ReadWrite
                && live_access.confinement == LiveFilesystemConfinement::UnconfinedHost
        }
        ExecutionProjectAuthority::PinnedGeneration { .. } => false,
    };
    if honest {
        return Ok(());
    }
    Err(refusal(HostAccessRefusal::new(
        HostAccessRefusal::PROJECT_REFUSED,
        "a host-direct launch runs unconfined on the host, but its sealed project authority is pinned, read-only or confined; run it with a read-write live or projectless policy",
    )))
}

/// Require a built plan to agree with the launch's admission: a host-direct
/// plan only with its grant, pinned to the runtime the plan selected, and a
/// confined plan never with one.
pub(crate) fn require_plan_matches_admission(
    plan: &ryeos_engine::contracts::ExecutionPlan,
    host_access: Option<&AdmittedHostAccess>,
) -> anyhow::Result<()> {
    match (plan.execution_posture.is_host_direct(), host_access) {
        (false, None) => Ok(()),
        (false, Some(_)) => {
            anyhow::bail!(
                "a confined plan for `{}` carries a host-access grant",
                plan.root_ref
            )
        }
        (true, None) => anyhow::bail!(
            "{}: host-direct plan for `{}` has no launch-time host-access admission; a host-direct run is never reconstructed or recovered",
            HostAccessRefusal::NOT_GRANTED,
            plan.root_ref
        ),
        (true, Some(admitted)) => {
            let grant = admitted.grant();
            let runtime = plan.runtime_identity.as_ref().ok_or_else(|| {
                anyhow::anyhow!("host-direct plan for `{}` has no runtime", plan.root_ref)
            })?;
            if plan.root_ref != grant.item
                || runtime.runtime_ref != grant.runtime_ref
                || runtime.runtime_content_hash != grant.runtime_content_hash
            {
                anyhow::bail!(
                    "{}: host-direct plan for `{}` selected runtime {}@{}, but grant `{}` pins {}@{}",
                    HostAccessRefusal::GRANT_STALE,
                    plan.root_ref,
                    runtime.runtime_ref,
                    runtime.runtime_content_hash,
                    grant.grant_id,
                    grant.runtime_ref,
                    grant.runtime_content_hash
                );
            }
            Ok(())
        }
    }
}

/// The isolation runtime one launch compiles against: the node's shared
/// runtime for a confined launch, or the per-launch host-direct view.
pub(crate) fn launch_isolation(
    state: &ryeos_app::state::AppState,
    host_access: Option<&AdmittedHostAccess>,
) -> anyhow::Result<std::sync::Arc<ryeos_engine::isolation::IsolationRuntime>> {
    match host_access {
        None => Ok(state.isolation.clone()),
        Some(admitted) => admitted
            .launch_isolation(&state.isolation)
            .map(std::sync::Arc::new)
            .map_err(|refusal| anyhow::anyhow!("{refusal}")),
    }
}
