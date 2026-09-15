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
    thread_auth_token: String,
    // Use the shared callback wire type directly — no local duplicate — so the
    // action payload (incl. its `call` block) can't drift from the runtime
    // side of the wire.
    action: ryeos_runtime::callback::ActionPayload,
    #[serde(default)]
    hook_dispatch: Option<ryeos_runtime::callback::HookDispatchIdentity>,
    #[serde(default)]
    effect_dispatch: Option<ryeos_runtime::callback::EffectDispatchRequest>,
    #[serde(default)]
    workload_invocation: Option<ryeos_runtime::workload_client::WorkloadInvocationSource>,
}

/// Exact, threadless callee admission captured before a durable-effect lookup.
///
/// The preflight owns the verified/composed subject and selected route.  A
/// cache miss consumes this same value through dispatch; it is never reduced
/// to a public digest and resolved a second time.
struct PreparedCallbackDispatch {
    context: crate::executor::ExecutionContext,
    handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    preflight: crate::dispatch::RootDispatchPreflight,
    effect_authority: Option<ryeos_effect_contract::PreparedEffectDispatchAuthority>,
    /// Exact preflight proved a managed producer with an admitted retained
    /// workspace-output recipe. Only this class may cross the otherwise-closed
    /// managed-inline boundary as an independently owned awaited root.
    awaited_product_root: bool,
    prepared_product_launch: Option<super::launch_preparation::PreparedRuntimeLaunch>,
}

/// Exact durable workspace coordinate derived from the boot-bound admitted
/// grant and the root's already-sealed project provenance.
///
/// This is only a projection into `RuntimeActionIntent`; it is not a second
/// lease or child ledger. The workspace journal remains owned by
/// `execution_workspace`, and worker/session liveness is rechecked by
/// `StateStore` in the reservation transaction.
#[derive(Debug, Clone)]
struct PreparedWorkloadWorkspaceOperation {
    workspace_id: String,
    access: ryeos_engine::kind_registry::WorkspaceAccess,
    worker_instance_id: String,
    worker_boot_epoch: u64,
    worker_boot_identity_hash: String,
    project_authority_digest: String,
    grant_digest: String,
    input_base_snapshot_hash: String,
    invocation: ryeos_runtime::workload_client::WorkloadInvocationSource,
}

impl PreparedWorkloadWorkspaceOperation {
    fn seed(&self) -> ryeos_app::runtime_db::NewRuntimeWorkspaceOperation<'_> {
        ryeos_app::runtime_db::NewRuntimeWorkspaceOperation {
            workspace_id: &self.workspace_id,
            access: self.access,
            worker_instance_id: &self.worker_instance_id,
            worker_boot_epoch: self.worker_boot_epoch,
            worker_boot_identity_hash: &self.worker_boot_identity_hash,
            project_authority_digest: &self.project_authority_digest,
            workload_client_grant_digest: &self.grant_digest,
            invocation: self.invocation.clone(),
        }
    }
}

fn canonical_authority_digest(value: &impl serde::Serialize) -> Result<String> {
    let value = serde_json::to_value(value)?;
    let canonical = lillux::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn verify_workload_owner_project_authority(
    owner_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    admitted_digest: &str,
) -> Result<String> {
    let digest = canonical_authority_digest(owner_authority)?;
    if digest != admitted_digest {
        anyhow::bail!("workload-client grant project authority changed after boot");
    }
    Ok(digest)
}

fn prepare_workload_workspace_operation(
    state: &AppState,
    cap: &ryeos_app::callback_token::CallbackCapability,
    thread_auth: &ThreadAuthState,
    authoritative_chain_root_id: &str,
    authoritative_current_site_id: &str,
    authoritative_origin_site_id: &str,
    child_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    access: ryeos_engine::kind_registry::WorkspaceAccess,
    invocation: &ryeos_runtime::workload_client::WorkloadInvocationSource,
) -> Result<PreparedWorkloadWorkspaceOperation> {
    let grant = cap
        .workload_client_grant
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workspace access has no admitted workload-client grant"))?;
    grant.validate()?;
    if state.node_policy.generation_digest() != grant.node_policy_generation_digest
        || grant.chain_root_id != authoritative_chain_root_id
        || grant.placement_thread_id != cap.thread_id
        || grant.owner_principal != thread_auth.acting_principal
        || grant.effective_caps != thread_auth.caller_scopes
        || grant.origin_site_id != authoritative_origin_site_id
        || authoritative_current_site_id != state.threads.site_id()
        || grant.effective_caps != cap.effective_caps
    {
        anyhow::bail!(
            "workload-client grant contradicts the current callback/thread placement authority"
        );
    }
    let placement = state
        .threads
        .get_thread(&cap.thread_id)?
        .ok_or_else(|| anyhow::anyhow!("workload-client placement thread disappeared"))?;
    if placement.status != "running"
        || placement.chain_root_id != grant.chain_root_id
        || placement.requested_by.as_deref() != Some(grant.owner_principal.as_str())
        || placement.origin_site_id != grant.origin_site_id
        || placement.admitted_launch_capsule_hash.as_deref()
            != Some(grant.root_launch_capsule_hash.as_str())
    {
        anyhow::bail!("workload-client root launch authority changed after boot");
    }
    let session = state
        .state_store
        .dedicated_session(&cap.thread_id)?
        .ok_or_else(|| anyhow::anyhow!("workload-client hosted session disappeared"))?;
    let worker = state
        .state_store
        .worker_process(&grant.worker_instance_id)?
        .ok_or_else(|| anyhow::anyhow!("workload-client worker boot disappeared"))?;
    if session.chain_root_id != grant.chain_root_id
        || session.admitted_capsule_hash != grant.session_capsule_hash
        || session.worker_instance_id.as_deref() != Some(grant.worker_instance_id.as_str())
        || session.worker_boot_epoch != Some(grant.worker_boot_epoch)
        || !matches!(
            session.state.as_str(),
            "idle" | "turn_running" | "awaiting_approval"
        )
        || worker.daemon_generation_id != ryeos_app::runtime_db::daemon_generation_id()
        || worker.placement_thread_id != grant.placement_thread_id
        || worker.boot_epoch != grant.worker_boot_epoch
        || worker.boot_identity_hash != grant.worker_boot_identity_hash
        || worker.session_capsule_hash != grant.session_capsule_hash
        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Live
        || worker.cleanup_state != "owned"
    {
        anyhow::bail!("workload-client grant no longer names the exact live worker boot");
    }
    if state
        .state_store
        .current_chain_placement_thread_id(authoritative_chain_root_id)?
        .as_deref()
        != Some(cap.thread_id.as_str())
    {
        anyhow::bail!("workload-client caller is not the authoritative chain placement");
    }
    // The boot grant binds the owning placement's sealed project authority.
    // Borrowed-child derivation intentionally replaces COW publication with
    // Discard, so its digest is NOT that grant identity. Keep this owner check
    // exact; do not undo for_child() or omit publication from the hash to make
    // them match. Child authority remains separately bound by the action
    // request hash and is used below for workspace/child admission.
    let project_authority_digest = verify_workload_owner_project_authority(
        cap.provenance.project_authority(),
        &grant.project_authority_digest,
    )?;
    let input_base_snapshot_hash = child_provenance
        .pinned_snapshot_hash()
        .ok_or_else(|| anyhow::anyhow!("shared-workspace execution requires pinned provenance"))?
        .to_owned();
    if !matches!(
        child_provenance.project_authority(),
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        }
    ) {
        anyhow::bail!("shared-workspace execution requires a private pinned-CoW root");
    }
    let workspace = crate::execution::workspace::WorkspaceLayout::from_project(
        child_provenance.effective_path(),
    )?;
    let workspace_id = workspace
        .root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("execution workspace id is not canonical UTF-8"))?
        .to_owned();
    Ok(PreparedWorkloadWorkspaceOperation {
        workspace_id,
        access,
        worker_instance_id: grant.worker_instance_id.clone(),
        worker_boot_epoch: grant.worker_boot_epoch,
        worker_boot_identity_hash: grant.worker_boot_identity_hash.clone(),
        project_authority_digest,
        grant_digest: grant.digest()?,
        input_base_snapshot_hash,
        invocation: invocation.clone(),
    })
}

fn enforce_inline_result_retention(
    item_ref: &str,
    retention: ryeos_engine::history_policy::ThreadResultRetention,
) -> Result<()> {
    if retention == ryeos_engine::history_policy::ThreadResultRetention::DigestOnly {
        return Err(anyhow::Error::new(
            crate::dispatch_error::DispatchError::LaunchPolicyForbidden {
                code: "inline_result_not_replayable".to_owned(),
                message: format!(
                    "inline callback dispatch of `{item_ref}` requires a replayable full result; \
                     the exact signed result policy is digest_only"
                ),
                binding: None,
            },
        ));
    }
    Ok(())
}

fn enforce_inline_dispatch_class(
    item_ref: &str,
    class: crate::dispatch::RootDispatchClass,
    awaited_product_root: bool,
) -> Result<()> {
    if matches!(class, crate::dispatch::RootDispatchClass::ManagedSubprocess)
        && !awaited_product_root
    {
        anyhow::bail!(
            "inline callback dispatch of `{item_ref}` is not supported: the exact admitted route \
             executes as a managed thread run. Mark the node `follow: true` to await its result \
             via durable suspend, or `detach: true` for a lineage-linked fire-and-forget child"
        );
    }
    Ok(())
}

fn selected_effect_authorization(
    params: &DispatchActionParams,
    cap: &ryeos_app::callback_token::CallbackCapability,
) -> Result<Option<ryeos_effect_contract::AdmittedEffectAuthorization>> {
    let Some(request) = params.effect_dispatch.as_ref() else {
        return Ok(None);
    };
    let index = cap
        .effect_dispatch_authorizations
        .binary_search_by(|authorization| {
            authorization
                .authorization_id
                .as_str()
                .cmp(request.authorization_id.as_str())
        })
        .map_err(|_| {
            anyhow::anyhow!(
                "effect dispatch authorization `{}` was not admitted for this launch",
                request.authorization_id
            )
        })?;
    let authorization = cap.effect_dispatch_authorizations[index].clone();
    authorization.validate()?;
    if cap.item_ref.as_deref() != Some(&authorization.source_definition_ref)
        || cap.effective_definition_digest.as_deref()
            != Some(&authorization.source_effective_definition_digest)
    {
        anyhow::bail!(
            "effect dispatch authorization source identity contradicts its callback capability"
        );
    }
    Ok(Some(authorization))
}

fn callback_execution_context(
    params: &DispatchActionParams,
    state: &AppState,
    thread_auth: &ThreadAuthState,
    dispatch_caps: &[String],
    current_site_id: &str,
    origin_site_id: &str,
    child_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
) -> Result<(
    crate::executor::ExecutionContext,
    Option<ryeos_app::handler_context::HandlerContext>,
)> {
    use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, ProjectContext};

    let callback_subject_authority = child_provenance.subject_resolution_authority();
    let callback_project_context = if matches!(
        callback_subject_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ) {
        ProjectContext::None
    } else {
        ProjectContext::LocalPath {
            // Admission remains bound to the initiating immutable subject
            // generation. A workload-delegated child may later receive a
            // separately captured execution input, but that input is not a
            // project definition and must never become item-resolution
            // authority.
            path: child_provenance.subject_effective_path().to_path_buf(),
        }
    };
    if current_site_id != state.threads.site_id() {
        anyhow::bail!("callback caller current site differs from the serving node");
    }
    let parent_resume = state
        .state_store
        .get_launch_metadata(&params.thread_id)?
        .and_then(|metadata| metadata.resume_context)
        .ok_or_else(|| anyhow::anyhow!("callback parent has no sealed resume context"))?;
    // Child launch inherits normalized realizations from the admitted parent
    // capsule. Parent selection controls are not forwarded as child inputs.
    let scheduled_fire = parent_resume.scheduled_fire;
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
            fingerprint: thread_auth.acting_principal.clone(),
            scopes: dispatch_caps.to_vec(),
        }),
        project_context: callback_project_context,
        subject_resolution_authority: callback_subject_authority,
        current_site_id: current_site_id.to_string(),
        origin_site_id: origin_site_id.to_string(),
        execution_hints: Default::default(),
        scheduled_fire,
        validate_only: false,
    };
    let handler_context = thread_auth.narrowed_handler_context(
        dispatch_caps.to_vec(),
        current_site_id,
        origin_site_id,
    )?;
    Ok((
        crate::executor::ExecutionContext {
            principal_fingerprint: thread_auth.acting_principal.clone(),
            caller_scopes: dispatch_caps.to_vec(),
            // Use the parent's per-request engine — never the daemon engine.
            engine: child_provenance.request_engine().clone(),
            plan_ctx,
            requested_call: params.action.call.clone(),
        },
        handler_context,
    ))
}

async fn prepare_callback_dispatch(
    params: &DispatchActionParams,
    state: &AppState,
    thread_auth: &ThreadAuthState,
    dispatch_caps: &[String],
    current_site_id: &str,
    origin_site_id: &str,
    child_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    authorization: Option<ryeos_effect_contract::AdmittedEffectAuthorization>,
) -> Result<PreparedCallbackDispatch> {
    if params.action.thread != "inline" {
        anyhow::bail!("callback preflight is only valid for inline actions");
    }
    let root = ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
        .with_context(|| format!("invalid callback item_id '{}'", params.action.item_id))?;
    let (context, handler_context) = callback_execution_context(
        params,
        state,
        thread_auth,
        dispatch_caps,
        current_site_id,
        origin_site_id,
        child_provenance,
    )?;
    let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &context.engine,
        &context.plan_ctx,
        child_provenance,
    )?;
    let preflight = crate::dispatch::preflight_root_dispatch_for_provenance(
        &params.action.item_id,
        root.kind.as_str(),
        &params.action.params,
        &params.action.ref_bindings,
        &params.action.product_selections,
        None,
        None,
        &project_binding,
        &context,
        child_provenance,
        state,
        None,
    )
    .map_err(anyhow::Error::new)?;
    let admitted_result_policy = preflight
        .root_admission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("inline callback preflight has no root admission"))?
        .resolved_result_policy();
    // An inline runtime action must be exactly replayable after any crash
    // following child contact. Digest-only retention deliberately discards
    // that response body, so refuse the combination at preflight—before an
    // operation intent, launch owner, service birth, or worker contact exists.
    enforce_inline_result_retention(&params.action.item_id, admitted_result_policy.retention)?;
    let prepared_product_launch = if matches!(
        preflight.class,
        crate::dispatch::RootDispatchClass::ManagedSubprocess
    ) && authorization.is_some()
    {
        let admission = preflight
            .root_admission
            .as_ref()
            .context("managed callback preflight has no root admission")?;
        let runtime = context
            .engine
            .runtimes
            .resolve_for_launch(None, &preflight.requested_subject.resolved.kind)
            .map_err(anyhow::Error::msg)?
            .clone();
        crate::dispatch::prepare_admitted_launch_contract(
            &crate::dispatch::LaunchContractApplicability::ManagedEnvelope {
                runtime: Box::new(runtime),
            },
            admission,
            &params.action.ref_bindings,
            &lifecycle_authority,
            child_provenance,
            &context,
            state,
        )
        .await?
    } else {
        None
    };
    let awaited_product_root = prepared_product_launch
        .as_ref()
        .map(super::workspace_outputs::admission::requires_output_partition)
        .transpose()?
        .unwrap_or(false);
    let effect_authority = authorization
        .map(|authorization| {
            if !preflight
                .requested_subject
                .resolved
                .metadata
                .required_secrets
                .is_empty()
            {
                return Err(anyhow::Error::new(
                    crate::dispatch_error::DispatchError::LaunchPolicyForbidden {
                        code: "durable_effect_opaque_inputs_unversioned".to_owned(),
                        message: format!(
                            "durable effect dispatch of `{}` is ineligible because its required secret values have no sealed generation authority",
                            params.action.item_id
                        ),
                        binding: None,
                    },
                ));
            }
            crate::dispatch::enforce_preflight_effect_class(&preflight, Some(authorization.class))
                .map_err(anyhow::Error::new)?;
            let authority = ryeos_effect_contract::PreparedEffectDispatchAuthority {
                authorization,
                action_digest: ryeos_runtime::callback::dispatch_action_digest(&params.action)?,
                subject_effect_class_ceiling: preflight.effect_class_ceiling.ok_or_else(|| {
                    anyhow::anyhow!("durable effect preflight lost its admitted subject ceiling")
                })?,
            };
            authority.validate()?;
            Ok::<_, anyhow::Error>(authority)
        })
        .transpose()?;
    Ok(PreparedCallbackDispatch {
        context,
        handler_context,
        preflight,
        effect_authority,
        awaited_product_root,
        prepared_product_launch,
    })
}

fn validate_action_occurrence_contract(params: &DispatchActionParams) -> Result<()> {
    match (
        params.hook_dispatch.is_some(),
        params.action.operation_id.as_deref(),
    ) {
        (true, None) => Ok(()),
        (true, Some(_)) => {
            anyhow::bail!("hook callback action cannot also claim an ordinary runtime operation_id")
        }
        (false, Some(operation_id))
            if ryeos_runtime::callback::valid_action_operation_id(operation_id) =>
        {
            Ok(())
        }
        (false, Some(_)) => anyhow::bail!(
            "ordinary callback action operation_id is not a canonical lowercase SHA-256 digest"
        ),
        (false, None) => anyhow::bail!("ordinary callback action has no operation_id"),
    }
}

pub async fn handle(params: &Value, state: &AppState) -> Result<Value> {
    // This is the single generic callback-to-child dispatch boundary. A
    // hosted workload-client surface must arrive here through a narrowly
    // admitted method/action projection on the existing callback capability.
    // A private bridge-local per-invocation broker may adapt transport for a
    // sandboxed workload, but it must not add a second daemon endpoint,
    // authentication store, dispatcher, operation ledger, child launcher, or
    // provenance path.
    let params: DispatchActionParams =
        serde_json::from_value(params.clone()).context("invalid runtime.dispatch_action params")?;
    validate_action_occurrence_contract(&params)?;

    let cap = state
        .callback_tokens
        .validate_token_and_thread(&params.callback_token, &params.thread_id)?;
    cap.runtime_method_surface
        .authorize(ryeos_runtime::RUNTIME_DISPATCH_ACTION_METHOD)?;
    if let Some(grant) = cap.workload_client_grant.as_ref() {
        if params.hook_dispatch.is_some() || params.effect_dispatch.is_some() {
            anyhow::bail!(
                "workload-client callback authority cannot select hook or effect dispatch"
            );
        }
        grant.authorize_action(&params.action)?;
        let source = params.workload_invocation.as_ref().ok_or_else(|| {
            anyhow::anyhow!("workload dispatch has no trusted ingress provenance")
        })?;
        if !grant.ingresses.contains(&source.ingress())
            || params.action.operation_id.as_deref()
                != Some(source.runtime_operation_id(&grant.digest()?)?.as_str())
        {
            anyhow::bail!("workload dispatch occurrence contradicts admitted ingress authority");
        }
        if matches!(
            source,
            ryeos_runtime::workload_client::WorkloadInvocationSource::StructuredSession { .. }
        ) && let Some(retained) = state
            .state_store
            .runtime_action_for_protocol_invocation(&params.thread_id, source)?
            && params.action.operation_id.as_deref() != Some(retained.operation_id.as_str())
        {
            // Reattachment rotates the grant, never the logical callback.
            // Cross-boot outcome recovery must use the original retained
            // intent, not authorize a fresh child from the successor grant.
            return Err(runtime_action_outcome_unknown(
                &retained.operation_id,
                "protocol call is fenced to its original boot's runtime action; no new child was authorized",
            ));
        }
    } else if params.workload_invocation.is_some() {
        anyhow::bail!("ordinary callback cannot assert workload ingress provenance");
    }
    // Join the existing root gate before input preflight or reservation.
    // Queued bytes are not accepted execution authority; a terminalizer may
    // refuse them, but must drain every invocation admitted through this gate.
    // Retain this one lease through child settlement, not a second broker gate.
    let _hosted_root_operation = if cap.workload_client_grant.is_some() {
        Some(
            ryeos_app::hosted_operation::try_begin_hosted_child_operation_async(
                &state.state_store,
                &params.thread_id,
            )
            .await?,
        )
    } else {
        None
    };
    let launch_owner = cap
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("execution callback capability has no launch owner"))?;
    state
        .state_store
        .assert_launch_owner(&params.thread_id, launch_owner)?;
    crate::execution::launch_preparation::validate_ref_bindings(&params.action.ref_bindings)?;
    ryeos_state::external_content::products::composition::validate_product_selection_inputs(
        &params.action.product_selections,
    )?;
    if params.action.thread != "inline" && !params.action.product_selections.is_empty() {
        anyhow::bail!("product-selected callbacks require an inline action");
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

    let dispatch_caps = if let Some(hook) = params.hook_dispatch.as_ref() {
        let callback_root_item_ref = cap.item_ref.as_deref().ok_or_else(|| {
            hook_integrity("hook callback capability is missing its root item ref")
        })?;
        validate_hook_dispatch_preflight(
            hook,
            &params.action,
            callback_root_item_ref,
            &cap.root_raw_content_digest,
            cap.effective_definition_digest.as_deref(),
            &cap.hook_dispatch_authorizations,
        )?
        .dispatch_caps
        .clone()
    } else {
        cap.effective_caps.clone()
    };

    // Authority is selected before capability evaluation. Ordinary callbacks
    // use the root program's grants; hook callbacks use only the exact hook
    // source's captured grants and can never borrow root authority.
    enforce_callback_caps(
        &params.action.item_id,
        &dispatch_caps,
        &state.authorizer,
        &child_provenance.request_engine().kinds,
    )?;
    for binding_ref in params.action.ref_bindings.values() {
        enforce_callback_caps(
            binding_ref,
            &dispatch_caps,
            &state.authorizer,
            &child_provenance.request_engine().kinds,
        )?;
    }

    // Note: DispatchActionParams has `deny_unknown_fields` and no
    // `principal` field — the request body cannot supply (and so
    // cannot spoof) a principal. The principal logged here is read
    // strictly from the validated server-side ThreadAuthState.
    tracing::info!(
        thread_id = %params.thread_id,
        server_principal = %thread_auth.acting_principal,
        project_path = %cap.project_path.display(),
        borrowed_dir = %child_provenance.effective_path().display(),
        project_source = ?child_provenance.project_source(),
        "thread auth token validated: using server-side principal",
    );

    // Resolve and admit the exact callee before a durable-effect lookup. A
    // miss carries this same prepared subject through dispatch; terminal
    // preparation completes the identity with the exact admitted capsule.
    let selected_effect_authorization = selected_effect_authorization(&params, &cap)?;
    let lifecycle_authority = state
        .state_store
        .get_launch_metadata(&cap.thread_id)?
        .and_then(|metadata| metadata.resume_context)
        .map(|resume| resume.lifecycle_authority)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "callback parent {} has no sealed lifecycle authority",
                cap.thread_id
            )
        })?;
    let prepared_callback_dispatch = if params.action.thread == "inline" {
        Some(
            prepare_callback_dispatch(
                &params,
                state,
                &thread_auth,
                &dispatch_caps,
                &caller_thread.current_site_id,
                &caller_thread.origin_site_id,
                &child_provenance,
                lifecycle_authority,
                selected_effect_authorization,
            )
            .await?,
        )
    } else {
        if selected_effect_authorization.is_some() {
            anyhow::bail!("durable effect replay is only valid for inline callback actions");
        }
        None
    };
    let workload_workspace_access = if let Some(grant) = cap.workload_client_grant.as_ref() {
        let prepared = prepared_callback_dispatch.as_ref().ok_or_else(|| {
            anyhow::anyhow!("workload-client dispatch lost its exact child preflight")
        })?;
        grant.authorize_effect_class(
            &params.action.item_id,
            prepared.preflight.effect_class_ceiling,
        )?;
        Some(grant.authorize_workspace_access(
            &params.action.item_id,
            prepared.preflight.workspace_access,
        )?)
    } else {
        None
    };

    let result = handle_execute(
        params,
        state,
        &thread_auth,
        &cap,
        dispatch_caps,
        &caller_thread.chain_root_id,
        &caller_thread.current_site_id,
        &caller_thread.origin_site_id,
        child_provenance,
        prepared_callback_dispatch,
        workload_workspace_access,
        lifecycle_authority,
    )
    .await;
    drop(caller_thread);
    drop(thread_auth);
    result
}

fn hook_integrity(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(
        crate::dispatch_error::DispatchError::HookDispatchIntegrity {
            detail: detail.into(),
        },
    )
}

fn runtime_action_outcome_unknown(operation_id: &str, detail: impl Into<String>) -> anyhow::Error {
    let detail = detail.into();
    // The runtime's typed recovery control does not preserve its captured
    // stderr. Keep the daemon-owned refusal at its original boundary without
    // logging the action parameters or response body.
    tracing::warn!(
        operation_id,
        detail = %ryeos_runtime::workload_client::bounded_error_message(&detail),
        "runtime action requires retained-outcome recovery"
    );
    anyhow::Error::new(
        crate::dispatch_error::DispatchError::RuntimeActionOutcomeUnknown {
            operation_id: operation_id.to_owned(),
            detail,
        },
    )
}

fn runtime_action_recovery_error(
    operation_id: &str,
    boundary: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    tracing::warn!(
        operation_id,
        boundary,
        error = %ryeos_runtime::workload_client::bounded_error_message(&format!("{error:#}")),
        "runtime action terminal reconstruction refused"
    );
    if matches!(
        error.downcast_ref::<crate::dispatch_error::DispatchError>(),
        Some(
            crate::dispatch_error::DispatchError::RuntimeActionOutcomeUnknown { .. }
                | crate::dispatch_error::DispatchError::RuntimeActionResultUnavailable { .. }
        )
    ) {
        return error;
    }
    runtime_action_outcome_unknown(
        operation_id,
        format!("{boundary} could not produce a replay-safe terminal response: {error:#}"),
    )
}

fn awaited_chain_recovery_error(
    operation_id: &str,
    boundary: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    if matches!(
        error.downcast_ref::<crate::dispatch_error::DispatchError>(),
        Some(crate::dispatch_error::DispatchError::SubprocessRunFailed { .. })
    ) {
        return error;
    }
    runtime_action_recovery_error(operation_id, boundary, error)
}

fn into_dispatch_error(error: anyhow::Error) -> crate::dispatch_error::DispatchError {
    match error.downcast::<crate::dispatch_error::DispatchError>() {
        Ok(error) => error,
        Err(error) => crate::dispatch_error::DispatchError::Internal(error),
    }
}

fn retained_detached_child_error(
    operation_id: &str,
    child: Option<(&str, &str)>,
    error: anyhow::Error,
) -> anyhow::Error {
    if matches!(
        error.downcast_ref::<crate::dispatch_error::DispatchError>(),
        Some(
            crate::dispatch_error::DispatchError::RuntimeActionOutcomeUnknown { .. }
                | crate::dispatch_error::DispatchError::RuntimeActionResultUnavailable { .. }
        )
    ) {
        return error;
    }
    let Some((child_thread_id, status)) = child else {
        return error;
    };
    runtime_action_recovery_error(
        operation_id,
        "detached child post-birth recovery",
        anyhow::anyhow!(
            "retained child {child_thread_id} exists with status {status}; original failure: {error:#}"
        ),
    )
}

/// Classify a detached-launch error from durable state, not from which async
/// step happened to return it. A retained child proves the runtime action has
/// crossed birth and the caller may only replay the same operation identity;
/// an ordinary failure here could cause a behaviorally equivalent second
/// child. Failure to inspect the intent/child is itself an unknown outcome.
fn classify_detached_runtime_action_error(
    state: &AppState,
    operation_id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let intent = match state.state_store.get_runtime_action_intent(operation_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return error,
        Err(inspect_error) => {
            return runtime_action_recovery_error(
                operation_id,
                "detached runtime-action intent inspection",
                anyhow::anyhow!(
                    "original failure: {error:#}; intent inspection failed: {inspect_error:#}"
                ),
            );
        }
    };
    match state.threads.get_thread(&intent.child_thread_id) {
        Ok(Some(child)) => retained_detached_child_error(
            operation_id,
            Some((&intent.child_thread_id, &child.status)),
            error,
        ),
        Ok(None) => error,
        Err(inspect_error) => runtime_action_recovery_error(
            operation_id,
            "detached child progress inspection",
            anyhow::anyhow!(
                "original failure: {error:#}; retained child {} could not be inspected: {inspect_error:#}",
                intent.child_thread_id
            ),
        ),
    }
}

/// Reconstruct an admitted managed producer into its own private pinned COW
/// root. The action intent owns the selected project authority before checkout
/// materialization, so a crash/re-drive cannot silently select another source
/// generation or publication policy.
async fn prepare_awaited_product_root(
    state: &AppState,
    parent_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    operation_id: &str,
    child_thread_id: &str,
    partition: &ryeos_state::objects::WorkspaceOutputPartition,
) -> Result<ryeos_app::execution_provenance::ExecutionProvenance> {
    if parent_provenance.candidate_evaluation_scope().is_some()
        || parent_provenance
            .project_authority()
            .workspace_outputs()
            .is_some()
    {
        anyhow::bail!(
            "awaited product producer requires an output-free, non-candidate caller root"
        );
    }
    let source_snapshot = parent_provenance
        .project_authority()
        .operational_snapshot_projection()
        .ok_or_else(|| {
            anyhow::anyhow!("awaited product producer requires an already-pinned caller generation")
        })?;
    let retained = state
        .state_store
        .get_runtime_action_intent(operation_id)?
        .ok_or_else(|| anyhow::anyhow!("awaited root action intent disappeared"))?;
    if retained.mode != ryeos_app::runtime_db::RuntimeActionMode::AwaitedRoot
        || retained.child_thread_id != child_thread_id
    {
        anyhow::bail!("awaited root action intent changed mode or child identity");
    }
    let caller = state
        .threads
        .get_thread(&retained.first_caller_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("awaited producer caller disappeared"))?;
    if caller.chain_root_id != retained.chain_root_id
        || caller.status != "running"
        || caller.runtime.stop_intent.is_some()
        || state
            .state_store
            .current_chain_placement_thread_id(&retained.chain_root_id)?
            .as_deref()
            != Some(retained.first_caller_thread_id.as_str())
    {
        anyhow::bail!("awaited producer caller is no longer the active chain placement");
    }
    let child_authority = match retained.child_project_authority {
        Some(authority) => {
            if authority.operational_snapshot_projection() != Some(source_snapshot) {
                anyhow::bail!(
                    "awaited producer retained another source generation than its caller admission"
                );
            }
            if authority
                .workspace_outputs()
                .map(|outputs| &outputs.partition)
                != Some(partition)
            {
                anyhow::bail!(
                    "awaited producer retained another output partition than its admitted recipe"
                );
            }
            authority
        }
        None => {
            let authority = crate::execution::derive_pinned_child_authority(
                parent_provenance.project_authority(),
                source_snapshot.to_owned(),
                ryeos_state::objects::PinnedChildProjectRealization::CowRetainResult,
            )?
            .condition_initial_workspace_outputs(partition.clone())?;
            // The action intent receives the complete source generation,
            // result-publication policy, and output partition before checkout
            // creation or any child launch. Sealing later must match this
            // authority byte-for-byte; there is no post-bind refinement.
            state
                .state_store
                .bind_root_action_project_authority(operation_id, &authority)?;
            authority
        }
    };
    let capture_state = state.clone();
    let snapshot_hash = source_snapshot.to_owned();
    let original_path = parent_provenance.original_project_path().to_path_buf();
    let checkout_id = child_thread_id.to_owned();
    let context = crate::execution::run_bounded_project_capture(move || {
        crate::execution::project_source::resolve_pinned_snapshot_context(
            &capture_state,
            &snapshot_hash,
            original_path,
            &checkout_id,
            crate::execution::project_source::PinnedContextRealization::Cow,
        )
    })
    .await?;
    let lifeline = context
        .temp_dir
        .ok_or_else(|| anyhow::anyhow!("awaited producer workspace has no lifecycle guard"))?;
    let provenance = parent_provenance.root_for_pinned_child_workspace(
        context.request_engine,
        context.pinned_materialization.ok_or_else(|| {
            anyhow::anyhow!("awaited producer workspace has no verified materialization")
        })?,
        lifeline,
        child_authority,
    )?;
    let caller = state
        .threads
        .get_thread(&retained.first_caller_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("awaited producer caller disappeared during checkout"))?;
    if caller.status != "running"
        || caller.runtime.stop_intent.is_some()
        || state
            .state_store
            .current_chain_placement_thread_id(&retained.chain_root_id)?
            .as_deref()
            != Some(retained.first_caller_thread_id.as_str())
    {
        anyhow::bail!("awaited producer caller lost active authority during checkout");
    }
    Ok(provenance)
}

fn validate_hook_dispatch_preflight<'a>(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
    callback_root_item_ref: &str,
    callback_root_raw_content_digest: &str,
    callback_effective_definition_digest: Option<&str>,
    admitted_hooks: &'a [ryeos_app::callback_token::HookDispatchAuthorization],
) -> Result<&'a ryeos_app::callback_token::HookDispatchAuthorization> {
    let authorization = select_hook_admission(hook, admitted_hooks)?;
    validate_hook_result_policy(hook, action)?;
    validate_hook_identity_authority(
        hook,
        action,
        callback_root_item_ref,
        callback_root_raw_content_digest,
        callback_effective_definition_digest,
        authorization,
    )?;
    Ok(authorization)
}

fn select_hook_admission<'a>(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    admitted_hooks: &'a [ryeos_app::callback_token::HookDispatchAuthorization],
) -> Result<&'a ryeos_app::callback_token::HookDispatchAuthorization> {
    let event = hook_occurrence_event(&hook.occurrence);
    if let Some(candidate) = admitted_hooks.iter().find(|candidate| {
        candidate.hook_id == hook.hook_id
            && candidate.event == event
            && candidate.layer == hook.layer
            && candidate.result_mode == hook.result_mode
    }) {
        return Ok(candidate);
    }
    Err(hook_integrity(format!(
        "hook `{}` ({}/{}/{}) was not admitted by the launch-captured hook set",
        hook.hook_id,
        hook.layer.as_str(),
        event,
        hook.result_mode.as_str(),
    )))
}

fn hook_occurrence_event(occurrence: &ryeos_runtime::callback::HookDispatchOccurrence) -> &str {
    occurrence.event()
}

fn validate_hook_result_policy(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
) -> Result<()> {
    if hook.layer.is_observer_only()
        && hook.result_mode == ryeos_runtime::hooks_loader::HookResultMode::Control
    {
        return Err(hook_integrity(
            "infrastructure hooks cannot declare result `control`",
        ));
    }
    if hook.result_mode == ryeos_runtime::hooks_loader::HookResultMode::Observation {
        ryeos_runtime::envelope::validate_hook_observation_action(&serde_json::json!({
            "params": &action.params,
        }))
        .map_err(|error| hook_integrity(format!("observation action rejected: {error}")))?;
    }
    Ok(())
}

fn validate_hook_identity_authority(
    hook: &ryeos_runtime::callback::HookDispatchIdentity,
    action: &ryeos_runtime::callback::ActionPayload,
    callback_root_item_ref: &str,
    callback_root_raw_content_digest: &str,
    callback_effective_definition_digest: Option<&str>,
    authorization: &ryeos_app::callback_token::HookDispatchAuthorization,
) -> Result<()> {
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
    let occurrence = &hook.occurrence;
    let coordinates = [
        ("owner_kind", occurrence.owner_kind.as_str()),
        ("event", occurrence.event.as_str()),
        ("definition_ref", occurrence.definition_ref.as_str()),
        (
            "root_raw_content_digest",
            occurrence.root_raw_content_digest.as_str(),
        ),
        (
            "effective_definition_digest",
            occurrence.effective_definition_digest.as_str(),
        ),
    ];
    for (field, value) in coordinates {
        if value.is_empty() || value.len() > 4 * 1024 {
            return Err(hook_integrity(format!(
                "hook occurrence field `{field}` must contain between 1 and 4096 UTF-8 bytes"
            )));
        }
        if matches!(
            field,
            "root_raw_content_digest" | "effective_definition_digest"
        ) && !canonical_sha256(value)
        {
            return Err(hook_integrity(format!(
                "hook {field} is not a canonical lowercase SHA-256 digest"
            )));
        }
    }
    // Coordinates are bounded and shape-checked only; per-event presence and
    // values (e.g. a positive `turn`) are runtime-asserted by the admitted
    // runtime. Occurrence identity is trusted to that runtime, and projection
    // degrades malformed coordinates rather than erroring history — see the
    // standing disciplines in effective-programs.md.
    if occurrence.coordinates.len() > 64 {
        return Err(hook_integrity(
            "hook occurrence has more than 64 scalar coordinates",
        ));
    }
    for (key, value) in &occurrence.coordinates {
        if key.is_empty() || key.len() > 256 {
            return Err(hook_integrity(
                "hook occurrence coordinate names must contain between 1 and 256 UTF-8 bytes",
            ));
        }
        if let ryeos_runtime::callback::HookDispatchCoordinate::Text(value) = value
            && (value.is_empty() || value.len() > 4 * 1024)
        {
            return Err(hook_integrity(format!(
                "hook occurrence coordinate `{key}` must contain between 1 and 4096 UTF-8 bytes"
            )));
        }
    }
    let canonical_definition =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&occurrence.definition_ref)
            .map_err(|error| hook_integrity(format!("invalid hook definition_ref: {error}")))?;
    if canonical_definition.kind != occurrence.owner_kind {
        return Err(hook_integrity(format!(
            "hook definition_ref kind must be `{}`, got `{}`",
            occurrence.owner_kind, canonical_definition.kind
        )));
    }
    let canonical_callback_root =
        ryeos_engine::canonical_ref::CanonicalRef::parse(callback_root_item_ref)
            .map_err(|error| hook_integrity(format!("invalid callback root item ref: {error}")))?;
    if canonical_callback_root != canonical_definition {
        return Err(hook_integrity(format!(
            "hook definition_ref `{}` does not match callback root `{callback_root_item_ref}`",
            occurrence.definition_ref,
        )));
    }
    if authorization.owner_kind != occurrence.owner_kind {
        return Err(hook_integrity(format!(
            "hook authorization owner kind `{}` does not match occurrence kind `{}`",
            authorization.owner_kind, occurrence.owner_kind,
        )));
    }
    if authorization.event != occurrence.event {
        return Err(hook_integrity(format!(
            "hook authorization event `{}` does not match occurrence event `{}`",
            authorization.event, occurrence.event,
        )));
    }
    if hook.context_contract != authorization.context_contract {
        return Err(hook_integrity(format!(
            "hook `{}` context contract differs from its launch-captured authorization",
            hook.hook_id
        )));
    }
    if !canonical_sha256(callback_root_raw_content_digest) {
        return Err(hook_integrity(
            "callback capability root raw-content digest is not a canonical lowercase SHA-256 digest",
        ));
    }
    let callback_effective_definition_digest =
        callback_effective_definition_digest.ok_or_else(|| {
            hook_integrity("callback capability has no admitted effective-definition identity")
        })?;
    if !canonical_sha256(callback_effective_definition_digest) {
        return Err(hook_integrity(
            "callback capability effective definition digest is not a canonical lowercase SHA-256 digest",
        ));
    }
    if occurrence.root_raw_content_digest != callback_root_raw_content_digest {
        return Err(hook_integrity(
            "hook root_raw_content_digest does not match launch-captured root raw-content digest",
        ));
    }
    if occurrence.effective_definition_digest != callback_effective_definition_digest {
        return Err(hook_integrity(
            "hook effective_definition_digest does not match launch-captured effective definition digest",
        ));
    }
    if action.thread != "inline" {
        return Err(hook_integrity(format!(
            "hook `{}` requested non-inline thread mode {:?}",
            hook.hook_id, action.thread
        )));
    }
    ryeos_engine::canonical_ref::CanonicalRef::parse(&action.item_id)
        .map_err(|error| hook_integrity(format!("invalid hook item ref: {error}")))?;
    Ok(())
}

/// V5.5 P2: enforce the callback's composed `effective_caps` against
/// the requested item ref. Uses the unified `Authorizer` for wildcard
/// and implication expansion. An empty cap-set is deny-all — the
/// trust-boundary default for tokens minted without a composition step.
fn enforce_callback_caps(
    item_id: &str,
    effective_caps: &[String],
    authorizer: &ryeos_runtime::authorizer::Authorizer,
    kinds: &ryeos_engine::kind_registry::KindRegistry,
) -> std::result::Result<(), crate::dispatch_error::DispatchError> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_id).map_err(|error| {
        crate::dispatch_error::DispatchError::InvalidRef(item_id.to_string(), error.to_string())
    })?;
    let required = kinds
        .get(&canonical.kind)
        .and_then(|schema| schema.inventory_policy.admission.as_ref())
        .map(|admission| admission.required_capability(&canonical))
        .ok_or_else(
            || crate::dispatch_error::DispatchError::SchemaMisconfigured {
                kind: canonical.kind.clone(),
                detail: "kind has no signed execution-capability projection".to_owned(),
            },
        )?;

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
/// `service_executor::resolve_root_execution + run_and_wait` directly.
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
    dispatch_caps: Vec<String>,
    authoritative_chain_root_id: &str,
    authoritative_current_site_id: &str,
    authoritative_origin_site_id: &str,
    mut child_provenance: ryeos_app::execution_provenance::ExecutionProvenance,
    prepared_callback_dispatch: Option<PreparedCallbackDispatch>,
    workload_workspace_access: Option<ryeos_engine::kind_registry::WorkspaceAccess>,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
) -> Result<Value> {
    let action_digest = ryeos_runtime::callback::dispatch_action_digest(&params.action)?;
    let prepared_workspace_operation = workload_workspace_access
        .map(|access| {
            prepare_workload_workspace_operation(
                state,
                cap,
                thread_auth,
                authoritative_chain_root_id,
                authoritative_current_site_id,
                authoritative_origin_site_id,
                &child_provenance,
                access,
                params.workload_invocation.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("workspace operation lost its workload ingress provenance")
                })?,
            )
        })
        .transpose()?;
    let runtime_action_request_hash = if params.hook_dispatch.is_none() {
        Some(runtime_action_request_hash(
            &action_digest,
            &params.action,
            authoritative_chain_root_id,
            authoritative_current_site_id,
            authoritative_origin_site_id,
            thread_auth,
            cap,
            &dispatch_caps,
            &child_provenance,
            lifecycle_authority,
            prepared_callback_dispatch.as_ref(),
            prepared_workspace_operation.as_ref(),
        )?)
    } else {
        None
    };
    // `detached` is the ONE non-inline mode a callback may request: the
    // native fanout primitive. It does not return a leaf result — it mints
    // a lineage-linked, cohort-tagged child that runs concurrently while the
    // calling parent walks on — so it routes to `spawn_detached_child`, not the
    // inline leaf dispatch below. The raw detached response is completed with
    // the same daemon-owned live dispatch evidence as every other callback
    // response before it crosses the UDS boundary. Any other non-inline mode
    // fails closed: callback leaf results are unary and inline only.
    if params.action.thread == "detached" {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("detached callback action has no operation_id"))?;
        let response = crate::execution::spawn_detached_child::spawn_detached_child(
            state,
            thread_auth,
            cap,
            child_provenance,
            &params.action.item_id,
            &params.action.ref_bindings,
            &params.action.params,
            params.action.facets.as_ref(),
            params.action.launch_window.as_ref(),
            operation_id,
            runtime_action_request_hash.as_deref().ok_or_else(|| {
                anyhow::anyhow!("detached callback action has no reserved request authority")
            })?,
        )
        .await
        .map_err(|error| classify_detached_runtime_action_error(state, operation_id, error))?;
        return attach_runtime_dispatch_evidence(response, &action_digest, false).map_err(
            |error| {
                runtime_action_recovery_error(
                    operation_id,
                    "detached runtime-action evidence validation",
                    error,
                )
            },
        );
    }
    if params.action.thread != "inline" {
        anyhow::bail!(
            "callback dispatch only supports inline results or a `detached` \
             fanout launch; got thread={:?}",
            params.action.thread
        );
    }

    let mut prepared = prepared_callback_dispatch.ok_or_else(|| {
        anyhow::anyhow!("inline callback action lost its exact preflight authority")
    })?;
    if prepared.awaited_product_root && prepared_workspace_operation.is_some() {
        anyhow::bail!("awaited product producer cannot borrow or mutate its caller's workspace");
    }
    if prepared.awaited_product_root && params.hook_dispatch.is_some() {
        anyhow::bail!("hook dispatch cannot create an independently awaited producer root");
    }
    let awaited_partition = if prepared.awaited_product_root {
        let launch = prepared.prepared_product_launch.as_ref().ok_or_else(|| {
            anyhow::anyhow!("awaited product preflight lost its prepared launch contract")
        })?;
        let admission = prepared
            .preflight
            .root_admission
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("awaited product preflight lost root admission"))?;
        Some(
            super::workspace_outputs::admission::derive_initial_partition(
                state,
                child_provenance.request_engine(),
                admission.resolution_output(),
                launch,
                child_provenance
                    .project_authority()
                    .operational_snapshot_projection(),
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("awaited product preflight did not derive an output partition")
            })?,
        )
    } else {
        None
    };

    // Inline is the LEAF contract: terminal and method routes return a value
    // and settle. Managed routes are native thread runs; awaiting one inline
    // would hold the callback wire for the child's lifetime and cannot
    // checkpoint across the wait. Consume the exact class selected by the
    // caller's per-request preflight. Re-resolving the kind against the
    // daemon-global engine would be a second, potentially contradictory route
    // authority for project/pinned engines and aliases.
    enforce_inline_dispatch_class(
        &params.action.item_id,
        prepared.preflight.class,
        prepared.awaited_product_root,
    )?;

    let caller_principal_id = thread_auth.acting_principal.clone();
    let root_canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
            .with_context(|| format!("invalid callback item_id '{}'", params.action.item_id))?;

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
            &dispatch_caps,
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
                Some((seed.dispatch_key, request_hash, identity.clone()))
            }
            ryeos_app::state_store::HookDispatchReservation::Replay(completed) => {
                if identity.result_mode == ryeos_runtime::hooks_loader::HookResultMode::Observation
                {
                    state
                        .state_store
                        .append_completed_hook_outcome(
                            &params.thread_id,
                            identity,
                            &completed.dispatch_key,
                            &completed.request_hash,
                        )
                        .map_err(|error| {
                            hook_integrity(format!(
                                "could not append replayed hook observation `{}`: {error:#}",
                                completed.dispatch_key
                            ))
                        })?;
                }
                return Ok(completed.response);
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

    let runtime_action_child_id = if params.hook_dispatch.is_none() {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("ordinary callback action lost its operation_id"))?;
        let request_hash = runtime_action_request_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("ordinary callback action lost its request authority")
        })?;
        let action_mode = if prepared.awaited_product_root {
            ryeos_app::runtime_db::RuntimeActionMode::AwaitedRoot
        } else {
            ryeos_app::runtime_db::RuntimeActionMode::Inline
        };
        let proposed_child_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        let child_thread_id = match prepared_workspace_operation.as_ref() {
            Some(workspace_operation) => state
                .state_store
                .reserve_runtime_action_intent_with_workspace(
                    operation_id,
                    &params.thread_id,
                    action_mode,
                    request_hash,
                    &proposed_child_thread_id,
                    None,
                    &workspace_operation.seed(),
                )?,
            None => state.state_store.reserve_runtime_action_intent(
                operation_id,
                &params.thread_id,
                action_mode,
                request_hash,
                &proposed_child_thread_id,
                None,
            )?,
        };
        if state.threads.get_thread(&child_thread_id)?.is_some() {
            if prepared_workspace_operation.is_some() {
                let retained = state
                    .state_store
                    .get_runtime_action_intent(operation_id)?
                    .and_then(|intent| intent.workspace_operation)
                    .ok_or_else(|| {
                        runtime_action_outcome_unknown(
                            operation_id,
                            "retained workload action lost its workspace-operation authority",
                        )
                    })?;
                if retained.phase != ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released
                {
                    return Err(runtime_action_outcome_unknown(
                        operation_id,
                        "retained workspace child is still owned by its original quiescence/settlement path",
                    ));
                }
            }
            let recovered = if prepared.awaited_product_root {
                recover_awaited_root_chain_response(
                    state,
                    operation_id,
                    &child_thread_id,
                    &params.action.item_id,
                    prepared.effect_authority.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("awaited producer recovery lost effect authority")
                    })?,
                )
                .await
                .map_err(|error| {
                    awaited_chain_recovery_error(
                        operation_id,
                        "retained awaited-root child recovery",
                        error,
                    )
                })?
            } else {
                recover_runtime_action_child_response(
                    state,
                    operation_id,
                    &child_thread_id,
                    &params.action.item_id,
                    prepared.effect_authority.as_ref(),
                )
                .await
                .map_err(|error| {
                    runtime_action_recovery_error(
                        operation_id,
                        "retained runtime-action child recovery",
                        error,
                    )
                })?
            };
            return attach_runtime_dispatch_evidence(
                recovered,
                &action_digest,
                prepared.effect_authority.is_some(),
            )
            .map_err(|error| {
                runtime_action_recovery_error(
                    operation_id,
                    "retained runtime-action evidence validation",
                    error,
                )
            });
        }
        if state
            .state_store
            .get_launch_claim(&child_thread_id)?
            .is_some()
        {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "a launch owner was retained before child publication; only the same operation may be replayed",
            ));
        }
        if let Some(reservation) = state
            .state_store
            .in_process_handler_reservation(&child_thread_id)?
        {
            let safely_uncommitted = reservation.phase
                == ryeos_app::runtime_db::InProcessHandlerReservationPhase::Pending
                && !state
                    .state_store
                    .is_in_process_handler_active(&child_thread_id)?;
            if safely_uncommitted {
                state
                    .state_store
                    .discard_uncommitted_in_process_handler_birth(&child_thread_id)
                    .context("discard proven-uncommitted runtime-action service birth")?;
            } else {
                return Err(runtime_action_outcome_unknown(
                    operation_id,
                    format!(
                        "an in-process handler reservation in phase {:?} remains before child publication",
                        reservation.phase
                    ),
                ));
            }
        }
        if let Some(partition) = awaited_partition.as_ref() {
            let initial_subject = &prepared.preflight.requested_subject;
            let expected_subject_identity = (
                initial_subject.resolved.canonical_ref.to_string(),
                initial_subject.resolved.raw_content_digest.clone(),
                initial_subject.resolved.content_hash.clone(),
                initial_subject.signer.clone(),
                initial_subject.trust_class,
            );
            let expected_effect_authority = prepared
                .effect_authority
                .clone()
                .ok_or_else(|| anyhow::anyhow!("awaited product root has no effect authority"))?;
            child_provenance = prepare_awaited_product_root(
                state,
                &child_provenance,
                operation_id,
                &child_thread_id,
                partition,
            )
            .await?;
            let reparsed = prepare_callback_dispatch(
                &params,
                state,
                thread_auth,
                &dispatch_caps,
                authoritative_current_site_id,
                authoritative_origin_site_id,
                &child_provenance,
                lifecycle_authority,
                Some(expected_effect_authority.authorization.clone()),
            )
            .await?;
            let reparsed_subject = &reparsed.preflight.requested_subject;
            let reparsed_subject_identity = (
                reparsed_subject.resolved.canonical_ref.to_string(),
                reparsed_subject.resolved.raw_content_digest.clone(),
                reparsed_subject.resolved.content_hash.clone(),
                reparsed_subject.signer.clone(),
                reparsed_subject.trust_class,
            );
            if !reparsed.awaited_product_root
                || reparsed_subject_identity != expected_subject_identity
                || reparsed.effect_authority.as_ref() != Some(&expected_effect_authority)
            {
                anyhow::bail!(
                    "awaited producer changed subject or effect authority after independent root binding"
                );
            }
            let reparsed_launch = reparsed.prepared_product_launch.as_ref().ok_or_else(|| {
                anyhow::anyhow!("awaited producer re-preflight lost its prepared launch")
            })?;
            super::workspace_outputs::admission::verify_prepared_partition(
                reparsed_launch,
                partition,
                true,
            )?;
            prepared = reparsed;
        }
        Some(child_thread_id)
    } else {
        None
    };

    let mut exclusive_workspace_quiescence = None;
    if let Some(workspace_operation) = prepared_workspace_operation.as_ref() {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("workspace action lost its operation id"))?;
        let retained = state
            .state_store
            .get_runtime_action_intent(operation_id)?
            .and_then(|intent| intent.workspace_operation)
            .ok_or_else(|| anyhow::anyhow!("workspace action lost its durable intent"))?;
        if retained.phase != ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Reserved {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                format!(
                    "workspace action is retained in phase {:?}; its original process authority must settle it",
                    retained.phase
                ),
            ));
        }
        match workspace_operation.access {
            ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration => {
                let capture_state = state.clone();
                let capture_operation_id = operation_id.to_owned();
                let capture_thread_id = params.thread_id.clone();
                let capture_project_path = child_provenance.effective_path().to_path_buf();
                let capture_base_snapshot = workspace_operation.input_base_snapshot_hash.clone();
                let pending = crate::execution::run_bounded_project_capture(move || {
                    crate::execution::capture_runtime_workspace_input_generation(
                        &capture_state,
                        &capture_operation_id,
                        &capture_thread_id,
                        &capture_project_path,
                        &capture_base_snapshot,
                    )
                })
                .await?;
                let (input_generation, publication, quiesced) =
                    pending.into_unpublished_generation_and_quiesced()?;
                if let Some(publication) = publication
                    && let Err(error) = publication.publish()
                {
                    return match quiesced.resume_or_terminate() {
                        Ok(()) => {
                            state.state_store.transition_runtime_workspace_operation(
                                operation_id,
                                &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
                                ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                            )?;
                            Err(error.context("publish captured workspace-input generation"))
                        }
                        Err(settle_error) => Err(error.context(format!(
                            "workspace-input publication failed and exact root resume/termination was not proved: {settle_error:#}"
                        ))),
                    };
                }
                let materialize_state = state.clone();
                let materialize_snapshot_hash = input_generation.snapshot_hash.clone();
                let materialize_original_path =
                    child_provenance.original_project_path().to_path_buf();
                let materialize_checkout_id = format!("workspace-input-{operation_id}");
                let materialized = crate::execution::run_bounded_project_capture(move || {
                    crate::execution::project_source::resolve_pinned_snapshot_context(
                        &materialize_state,
                        &materialize_snapshot_hash,
                        materialize_original_path,
                        &materialize_checkout_id,
                        crate::execution::project_source::PinnedContextRealization::ReadOnly,
                    )
                })
                .await;
                let materialized = match materialized {
                    Ok(materialized) => materialized,
                    Err(error) => {
                        return match quiesced.resume_or_terminate() {
                            Ok(()) => {
                                state.state_store.transition_runtime_workspace_operation(
                                    operation_id,
                                    &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
                                    ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                                )?;
                                Err(anyhow::Error::new(error))
                            }
                            Err(settle_error) => Err(anyhow::anyhow!(
                                "workspace-input materialization failed ({error}) and exact root resume/termination was not proved: {settle_error:#}"
                            )),
                        };
                    }
                };
                let materialized_authority = (|| -> Result<_> {
                    let input_lifeline = materialized.temp_dir.ok_or_else(|| {
                        anyhow::anyhow!("workspace-input materialization has no lifecycle guard")
                    })?;
                    let input_materialization =
                        materialized.pinned_materialization.ok_or_else(|| {
                            anyhow::anyhow!(
                                "workspace-input materialization has no verified authority"
                            )
                        })?;
                    Ok((input_materialization, input_lifeline))
                })();
                let (input_materialization, input_lifeline) = match materialized_authority {
                    Ok(authority) => authority,
                    Err(error) => {
                        return match quiesced.resume_or_terminate() {
                            Ok(()) => {
                                state.state_store.transition_runtime_workspace_operation(
                                    operation_id,
                                    &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
                                    ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                                )?;
                                Err(error)
                            }
                            Err(settle_error) => Err(error.context(format!(
                                "workspace-input authority was incomplete and exact root resume/termination was not proved: {settle_error:#}"
                            ))),
                        };
                    }
                };
                child_provenance = match child_provenance.with_immutable_workspace_input(
                    input_generation,
                    input_materialization,
                    input_lifeline,
                ) {
                    Ok(provenance) => provenance,
                    Err(error) => {
                        return match quiesced.resume_or_terminate() {
                            Ok(()) => {
                                state.state_store.transition_runtime_workspace_operation(
                                    operation_id,
                                    &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
                                    ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
                                )?;
                                Err(error)
                            }
                            Err(settle_error) => Err(error.context(format!(
                                "immutable workspace provenance failed and exact root resume/termination was not proved: {settle_error:#}"
                            ))),
                        };
                    }
                };
                quiesced.resume_or_terminate().with_context(|| {
                    format!(
                        "resume or terminate exact root after capturing workspace input for `{operation_id}`"
                    )
                })?;
            }
            ryeos_engine::kind_registry::WorkspaceAccess::SharedExclusive => {
                let quiesce_state = state.clone();
                let quiesce_operation_id = operation_id.to_owned();
                let quiesce_thread_id = params.thread_id.clone();
                let quiesce_project_path = child_provenance.effective_path().to_path_buf();
                let quiesce_base_snapshot = workspace_operation.input_base_snapshot_hash.clone();
                exclusive_workspace_quiescence = Some(
                    crate::execution::run_bounded_project_capture(move || {
                        crate::execution::quiesce_runtime_workspace_exclusive(
                            &quiesce_state,
                            &quiesce_operation_id,
                            &quiesce_thread_id,
                            &quiesce_project_path,
                            &quiesce_base_snapshot,
                        )
                    })
                    .await?,
                );
            }
        }
        if let Err(error) = state.state_store.transition_runtime_workspace_operation(
            operation_id,
            &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
            ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::ChildRunning,
        ) {
            if let Some(quiesced) = exclusive_workspace_quiescence.take() {
                quiesced.resume_or_terminate()?;
            }
            state.state_store.transition_runtime_workspace_operation(
                operation_id,
                &[ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Quiesced],
                ryeos_app::runtime_db::RuntimeWorkspaceOperationPhase::Released,
            )?;
            return Err(error.context("advance workspace action to child-running"));
        }
    }

    let project_path = child_provenance.effective_path().to_path_buf();
    // C0 diagnostic: snapshot the run's resolution source before `provenance` is
    // moved into the dispatch request, so a content-hash mismatch can be pinned
    // to its origin below.
    let diag_source = child_provenance.project_source();
    let diag_effective_path = child_provenance.effective_path().to_path_buf();
    let PreparedCallbackDispatch {
        context: exec_ctx,
        handler_context,
        preflight,
        effect_authority,
        awaited_product_root,
        prepared_product_launch: _,
    } = prepared;
    let prepared_verified = preflight.requested_subject;
    let prepared_admission = preflight.root_admission;
    let prepared_dispatch_evidence = Some(preflight.root_dispatch_evidence);
    let durable_effect_requested = effect_authority.is_some();
    let retained_effect_authority = effect_authority.clone();
    let retained_runtime_action_child_id = runtime_action_child_id.clone();
    let dispatch_req = crate::dispatch::DispatchRequest {
        // Callback `thread=inline` is the unary leaf protocol. Persist the
        // execution dispatch vocabulary independently: the caller waits for
        // this leaf, so its thread launch mode is `wait`.
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params: params.action.params.clone(),
        ref_bindings: params.action.ref_bindings.clone(),
        product_selections: params.action.product_selections.clone(),
        acting_principal: caller_principal_id.as_str(),
        project_path: project_path.as_path(),
        provenance: child_provenance,
        lifecycle_authority,
        launch_timings: None,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: runtime_action_child_id,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: prepared_admission,
        root_dispatch_evidence: prepared_dispatch_evidence,
        parent_execution_context: Some(parent_execution_context_from_capability(cap)),
        effect_authority,
    };

    // V5.4 P2.3 cleanup — async end-to-end: the UDS dispatcher is
    // already on a tokio runtime (see `uds::server::dispatch`), so
    // we await `dispatch::dispatch` directly. The previous
    // `Handle::current().block_on(...)` was a panic/deadlock risk on
    // the P3b hot path (a runtime-thread blocking on its own runtime).
    let mut result = match handler_context {
        Some(context) => {
            crate::dispatch::dispatch_verified_with_handler_context(
                &params.action.item_id,
                prepared_verified,
                context,
                &dispatch_req,
                &exec_ctx,
                state,
            )
            .await
        }
        None => {
            crate::dispatch::dispatch_verified(
                &params.action.item_id,
                prepared_verified,
                &dispatch_req,
                &exec_ctx,
                state,
            )
            .await
        }
    };
    let completed_response = if awaited_product_root {
        match (
            result.as_ref().ok(),
            retained_runtime_action_child_id.as_deref(),
        ) {
            (Some(response), Some(child_thread_id)) => {
                completed_awaited_root_response(response, &action_digest, child_thread_id)?
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some(response) = completed_response {
        result = Ok(response);
    } else if awaited_product_root
        && let Some(child_thread_id) = retained_runtime_action_child_id.as_deref()
        && state.threads.get_thread(child_thread_id)?.is_some()
    {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .expect("awaited runtime action was validated before dispatch");
        result = recover_awaited_root_chain_response(
            state,
            operation_id,
            child_thread_id,
            &params.action.item_id,
            retained_effect_authority
                .as_ref()
                .expect("awaited product root was admitted with effect authority before dispatch"),
        )
        .await
        .map_err(|error| {
            into_dispatch_error(awaited_chain_recovery_error(
                operation_id,
                "awaited producer chain recovery",
                error,
            ))
        });
    }
    result = result.and_then(|response| {
        attach_runtime_dispatch_evidence(response, &action_digest, durable_effect_requested)
            .map_err(crate::dispatch_error::DispatchError::Internal)
    });
    let retained_terminal_failure = result.as_ref().err().is_some_and(|error| {
        matches!(
            error,
            crate::dispatch_error::DispatchError::SubprocessRunFailed { .. }
        )
    });
    if result.is_err()
        && !retained_terminal_failure
        && let Some(child_thread_id) = retained_runtime_action_child_id.as_deref()
    {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .expect("ordinary runtime action was validated before dispatch");
        if state.threads.get_thread(child_thread_id)?.is_some() {
            let recovered = if awaited_product_root {
                recover_awaited_root_chain_response(
                    state,
                    operation_id,
                    child_thread_id,
                    &params.action.item_id,
                    retained_effect_authority.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("awaited producer recovery lost effect authority")
                    })?,
                )
                .await
                .map_err(|error| {
                    awaited_chain_recovery_error(
                        operation_id,
                        "post-dispatch awaited-root child recovery",
                        error,
                    )
                })?
            } else {
                recover_runtime_action_child_response(
                    state,
                    operation_id,
                    child_thread_id,
                    &params.action.item_id,
                    retained_effect_authority.as_ref(),
                )
                .await
                .map_err(|error| {
                    runtime_action_recovery_error(
                        operation_id,
                        "post-dispatch runtime-action child recovery",
                        error,
                    )
                })?
            };
            result = attach_runtime_dispatch_evidence(
                recovered,
                &action_digest,
                durable_effect_requested,
            )
            .map_err(|error| {
                runtime_action_recovery_error(
                    operation_id,
                    "post-dispatch runtime-action evidence validation",
                    error,
                )
            })
            .map_err(crate::dispatch_error::DispatchError::Internal);
        }
        if state
            .state_store
            .get_launch_claim(child_thread_id)?
            .is_some()
            || state
                .state_store
                .in_process_handler_reservation(child_thread_id)?
                .is_some()
        {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "dispatch returned before retained launch/service ownership could prove a terminal child",
            ));
        }
    }
    if prepared_workspace_operation.is_some() {
        let operation_id = params
            .action
            .operation_id
            .as_deref()
            .expect("workspace action was validated with an operation id");
        if !state
            .state_store
            .begin_runtime_workspace_operation_settlement(operation_id)?
        {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "workspace child returned before its exact launcher/process authorities settled",
            ));
        }
        if let Some(quiesced) = exclusive_workspace_quiescence.take() {
            quiesced.resume_or_terminate().with_context(|| {
                format!(
                    "resume or terminate exact hosted root after exclusive workspace action `{operation_id}`"
                )
            })?;
        }
        if !state
            .state_store
            .settle_runtime_workspace_operation(operation_id)?
        {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "workspace operation remained unsettled after exact child cleanup",
            ));
        }
    }
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
        Some((dispatch_key, request_hash, identity)) => {
            let response = match result {
                Ok(response) => match serde_json::from_value::<
                    ryeos_runtime::callback_contract::CallbackDispatchResponse,
                >(response.clone())
                {
                    Ok(_) => response,
                    Err(error) => known_hook_dispatch_integrity_response(
                        format!(
                            "reserved hook dispatch `{dispatch_key}` returned an invalid callback response: {error}"
                        ),
                        &action_digest,
                    ),
                },
                Err(error) => known_hook_dispatch_integrity_response(
                    format!(
                        "reserved hook dispatch `{dispatch_key}` failed after reservation: {error:#}"
                    ),
                    &action_digest,
                ),
            };
            let completed = state
                .state_store
                .complete_hook_dispatch(&dispatch_key, &request_hash, &response)
                .map_err(|error| {
                    hook_integrity(format!(
                        "could not complete reserved hook dispatch `{dispatch_key}`: {error:#}"
                    ))
                })?;
            if identity.result_mode == ryeos_runtime::hooks_loader::HookResultMode::Observation {
                state
                    .state_store
                    .append_completed_hook_outcome(
                        &params.thread_id,
                        &identity,
                        &dispatch_key,
                        &request_hash,
                    )
                    .map_err(|error| {
                        hook_integrity(format!(
                            "could not append hook observation `{dispatch_key}`: {error:#}"
                        ))
                    })?;
            }
            Ok(completed.response)
        }
    }
}

/// Preserve the evidence returned by this dispatch attempt. Re-reading the
/// record that the attempt just published would turn Executed/Inserted into
/// EffectRecord/NotApplicable. Identity checks here bind the in-band result;
/// they never infer execution from a thread ID or from record provenance.
fn completed_awaited_root_response(
    response: &Value,
    action_digest: &str,
    expected_root_thread_id: &str,
) -> Result<Option<Value>> {
    if response.get("dispatch").is_none() {
        // A continued or incomplete managed launch has no accepted answer.
        return Ok(None);
    }
    // Managed execution also returns its result-generation coordinate to the
    // outer execute API. It is not a callback field: project only the typed
    // callback response here, without discarding its dispatch evidence.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ManagedResponse {
        thread: Value,
        result: Value,
        dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence,
        #[serde(default)]
        result_project_snapshot_hash: Option<String>,
    }
    let managed: ManagedResponse = serde_json::from_value(response.clone())?;
    if let Some(hash) = managed.result_project_snapshot_hash.as_deref()
        && !lillux::valid_hash(hash)
    {
        anyhow::bail!("managed product response has an invalid result snapshot hash");
    }
    let projected =
        serde_json::to_value(ryeos_runtime::callback_contract::CallbackDispatchResponse {
            thread: managed.thread,
            result: managed.result,
            dispatch: managed.dispatch,
        })?;
    let validated = attach_runtime_dispatch_evidence(projected, action_digest, true)?;
    let parsed: ryeos_runtime::callback_contract::CallbackDispatchResponse =
        serde_json::from_value(validated.clone())?;
    if parsed
        .dispatch
        .retained_effect_result(&parsed.result)?
        .is_none()
    {
        anyhow::bail!("awaited product response has no retained product result projection");
    }
    if parsed.thread.is_null() {
        if parsed.dispatch.source
            != ryeos_runtime::callback_contract::RuntimeDispatchSource::EffectRecord
        {
            anyhow::bail!("executed awaited product response lost its terminal thread");
        }
        return Ok(Some(validated));
    }
    if parsed.thread.get("chain_root_id").and_then(Value::as_str) != Some(expected_root_thread_id)
        || parsed.thread.get("status").and_then(Value::as_str) != Some("completed")
        || parsed
            .thread
            .get("thread_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        anyhow::bail!("awaited product response contradicts its terminal producer chain");
    }
    Ok(Some(validated))
}

fn attach_runtime_dispatch_evidence(
    mut response: Value,
    action_digest: &str,
    durable_effect_requested: bool,
) -> Result<Value> {
    let object = response
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("callback dispatch response is not an object"))?;
    match object.get("dispatch") {
        Some(value) => {
            let evidence: ryeos_runtime::callback_contract::RuntimeDispatchEvidence =
                serde_json::from_value(value.clone())
                    .context("decode daemon-owned dispatch evidence")?;
            evidence.validate()?;
            if evidence.action_digest != action_digest {
                anyhow::bail!("callback dispatch evidence contradicts the exact wire action");
            }
            if !durable_effect_requested
                && evidence.effect_class
                    != ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live
            {
                anyhow::bail!("live callback action returned durable dispatch evidence");
            }
            if durable_effect_requested
                && evidence.effect_class
                    == ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live
            {
                anyhow::bail!("durable callback action returned live dispatch evidence");
            }
        }
        None if durable_effect_requested => {
            anyhow::bail!("durable callback action returned no dispatch evidence")
        }
        None => {
            object.insert(
                "dispatch".to_owned(),
                serde_json::to_value(ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                    source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
                    effect_class:
                        ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
                    action_digest: action_digest.to_owned(),
                    effect_identity: None,
                    publication:
                        ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                    record_hash: None,
                    replayed_from: None,
                    result_projection:
                        ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
                })?,
            );
        }
    }
    let parsed: ryeos_runtime::callback_contract::CallbackDispatchResponse =
        serde_json::from_value(response.clone()).context("validate callback dispatch response")?;
    parsed.dispatch.validate()?;
    Ok(response)
}

async fn recover_runtime_action_child_response(
    state: &AppState,
    operation_id: &str,
    child_thread_id: &str,
    expected_subject_ref: &str,
    effect_authority: Option<&ryeos_effect_contract::PreparedEffectDispatchAuthority>,
) -> Result<Value> {
    let mut events = state.event_streams.subscribe(child_thread_id);
    loop {
        let thread = state.threads.get_thread(child_thread_id)?.ok_or_else(|| {
            runtime_action_outcome_unknown(
                operation_id,
                "the retained child identity no longer resolves",
            )
        })?;
        if ryeos_app::state_store::is_terminal_status(&thread.status) {
            let retained = state.threads.build_execute_result(child_thread_id)?;
            let retained_value = serde_json::to_value(&retained)?;
            let terminal_response = serde_json::json!({
                "thread": thread,
                "result": retained_value,
            });
            if let Some(effect_authority) = effect_authority {
                return crate::execution::runner::recover_terminal_dispatch_effect(
                    state,
                    child_thread_id,
                    expected_subject_ref,
                    effect_authority,
                    &terminal_response,
                );
            }
            if retained.as_ref().is_some_and(|result| {
                result
                    .result
                    .as_ref()
                    .is_some_and(digest_only_terminal_value)
                    || result
                        .error
                        .as_ref()
                        .is_some_and(digest_only_terminal_value)
            }) {
                return Err(anyhow::Error::new(
                    crate::dispatch_error::DispatchError::RuntimeActionResultUnavailable {
                        operation_id: operation_id.to_owned(),
                    },
                ));
            }
            return Ok(terminal_response);
        }
        match events.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err(runtime_action_outcome_unknown(
                    operation_id,
                    "the child event lane closed before terminal settlement",
                ));
            }
        }
    }
}

/// Await the hard terminal member of an independently owned producer chain.
/// Subscribe before the first authoritative read, then re-read after every
/// relevant event or lag signal so a continuation or terminal event cannot
/// land in a read/subscribe gap. The action intent continues to
/// name the original root; only the node-owned chain placement selects the
/// member whose retained output is eligible for acceptance.
async fn recover_awaited_root_chain_response(
    state: &AppState,
    operation_id: &str,
    root_thread_id: &str,
    expected_subject_ref: &str,
    effect_authority: &ryeos_effect_contract::PreparedEffectDispatchAuthority,
) -> Result<Value> {
    let mut events = state.event_streams.subscribe_all();
    loop {
        let root = state.threads.get_thread(root_thread_id)?.ok_or_else(|| {
            runtime_action_outcome_unknown(
                operation_id,
                "the retained producer root identity no longer resolves",
            )
        })?;
        if root.chain_root_id != root_thread_id {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "the retained producer identity is not its chain root",
            ));
        }
        let current_thread_id = state
            .state_store
            .current_chain_placement_thread_id(root_thread_id)?
            .ok_or_else(|| {
                runtime_action_outcome_unknown(
                    operation_id,
                    "the retained producer chain has no authoritative placement",
                )
            })?;
        let current = state
            .threads
            .get_thread(&current_thread_id)?
            .ok_or_else(|| {
                runtime_action_outcome_unknown(
                    operation_id,
                    "the authoritative producer chain placement no longer resolves",
                )
            })?;
        if current.chain_root_id != root_thread_id {
            return Err(runtime_action_outcome_unknown(
                operation_id,
                "the authoritative producer placement belongs to another chain",
            ));
        }
        if current.status != "continued"
            && ryeos_app::state_store::is_terminal_status(&current.status)
        {
            let retained = state.threads.build_execute_result(&current_thread_id)?;
            if current.status != "completed" {
                let retained_error = retained
                    .as_ref()
                    .and_then(|result| result.error.as_ref())
                    .map(Value::to_string)
                    .unwrap_or_else(|| "no retained error payload".to_owned());
                return Err(awaited_root_terminal_failure(
                    expected_subject_ref,
                    &current_thread_id,
                    &current.status,
                    &retained_error,
                ));
            }
            let terminal_response = serde_json::json!({
                "thread": current,
                "result": serde_json::to_value(&retained)?,
            });
            return crate::execution::runner::recover_terminal_dispatch_effect(
                state,
                &current_thread_id,
                expected_subject_ref,
                effect_authority,
                &terminal_response,
            );
        }
        match events.recv().await {
            Ok(event) if event.chain_root_id == root_thread_id => {}
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err(runtime_action_outcome_unknown(
                    operation_id,
                    "the producer event lane closed before chain terminal settlement",
                ));
            }
        }
    }
}

fn awaited_root_terminal_failure(
    item_ref: &str,
    thread_id: &str,
    status: &str,
    retained_error: &str,
) -> anyhow::Error {
    let retained_error = ryeos_runtime::workload_client::bounded_error_message(retained_error);
    anyhow::Error::new(crate::dispatch_error::DispatchError::SubprocessRunFailed {
        item_ref: item_ref.to_owned(),
        detail: format!(
            "retained producer thread {thread_id} ended with status {status}: {retained_error}"
        ),
    })
}

fn digest_only_terminal_value(value: &Value) -> bool {
    value.get("schema").and_then(Value::as_u64) == Some(1)
        && matches!(
            value.get("kind").and_then(Value::as_str),
            Some("ryeos.digest_only_result" | "ryeos.digest_only_error")
        )
}

fn known_hook_dispatch_integrity_response(message: String, action_digest: &str) -> Value {
    serde_json::to_value(ryeos_runtime::callback_contract::CallbackDispatchResponse {
        thread: Value::Null,
        result: ryeos_runtime::envelope::hook_dispatch_integrity_failure(&message),
        dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
            source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
            effect_class: ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
            action_digest: action_digest.to_owned(),
            effect_identity: None,
            publication:
                ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
            record_hash: None,
            replayed_from: None,
            result_projection: ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
        },
    })
    .expect("hook dispatch integrity response is infallibly serializable")
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
        "schema": "ryeos.hook_dispatch.v3",
        "chain_root_id": chain_root_id,
        "hook_dispatch": {
            "occurrence": &identity.occurrence,
            "hook_id": &identity.hook_id,
            "layer": identity.layer,
            "result_mode": identity.result_mode,
            "context_contract": &identity.context_contract,
        },
        "dispatch_caps": effective_caps,
        "callback_root_item_ref": callback_root_item_ref,
    });
    let canonical_dispatch_identity =
        lillux::canonical_json(&dispatch_identity).map_err(|error| {
            hook_integrity(format!(
                "hook dispatch identity cannot be represented as canonical JSON: {error}"
            ))
        })?;
    let dispatch_key = lillux::sha256_hex(canonical_dispatch_identity.as_bytes());
    let request_identity = serde_json::json!({
        "hook_dispatch": identity,
        "action": action,
        "chain_root_id": chain_root_id,
        "validated_project_path": exact_path_identity(validated_project_path),
        "acting_principal": acting_principal,
        "effective_caps": effective_caps,
        "hard_limits": hard_limits,
        "depth": depth,
        "callback_root_item_ref": callback_root_item_ref,
    });
    let canonical_request_identity =
        lillux::canonical_json(&request_identity).map_err(|error| {
            hook_integrity(format!(
                "hook dispatch request cannot be represented as canonical JSON: {error}"
            ))
        })?;
    let request_hash = lillux::sha256_hex(canonical_request_identity.as_bytes());
    let seed = ryeos_app::state_store::NewHookDispatch {
        seed_version: ryeos_app::runtime_db::HOOK_DISPATCH_SEED_VERSION,
        dispatch_key,
        chain_root_id: chain_root_id.to_string(),
        caller_thread_id: caller_thread_id.to_string(),
        event: identity.occurrence.event().to_string(),
        hook_id: identity.hook_id.clone(),
        request_hash: request_hash.clone(),
    };
    Ok((seed, request_hash))
}

fn exact_path_identity(path: &std::path::Path) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        serde_json::json!({
            "encoding": "unix_bytes_sha256",
            "bytes": bytes.len(),
            "sha256": lillux::sha256_hex(bytes),
        })
    }
    #[cfg(not(unix))]
    {
        serde_json::json!({
            "encoding": "unicode",
            "value": path.to_string_lossy(),
        })
    }
}

const RUNTIME_ACTION_REQUEST_SCHEMA: &str = "ryeos.runtime_action_request.v3";

/// The action digest deliberately normalizes receiver-local witness-source
/// proof. The recoverable occurrence request must retain that full selector so
/// the same operation cannot be re-driven through a different redemption
/// authority while preserving its behavior identity.
fn runtime_action_request_product_selections(
    action: &ryeos_runtime::callback::ActionPayload,
) -> Result<Value> {
    ryeos_state::external_content::products::composition::validate_product_selection_inputs(
        &action.product_selections,
    )?;
    Ok(serde_json::to_value(&action.product_selections)?)
}

#[allow(clippy::too_many_arguments)]
fn runtime_action_request_hash(
    action_digest: &str,
    action: &ryeos_runtime::callback::ActionPayload,
    chain_root_id: &str,
    current_site_id: &str,
    origin_site_id: &str,
    thread_auth: &ThreadAuthState,
    cap: &ryeos_app::callback_token::CallbackCapability,
    dispatch_caps: &[String],
    child_provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    prepared: Option<&PreparedCallbackDispatch>,
    workspace_operation: Option<&PreparedWorkloadWorkspaceOperation>,
) -> Result<String> {
    let product_selections = runtime_action_request_product_selections(action)?;
    let mut effective_caps = dispatch_caps.to_vec();
    effective_caps.sort();
    effective_caps.dedup();
    let handler_authority = thread_auth.handler_context().map(|context| {
        let mut scopes = context.scopes.clone();
        scopes.sort();
        scopes.dedup();
        serde_json::json!({
            "fingerprint": &context.fingerprint,
            "scopes": scopes,
            "verified": context.verified,
            "authorized_key_class": context.authorized_key_class,
            "authenticated_origin_site_id": &context.authenticated_origin_site_id,
        })
    });
    let prepared_subject = prepared.map(|prepared| {
        let verified = &prepared.preflight.requested_subject;
        serde_json::json!({
            "dispatch_class": prepared.preflight.class.as_str(),
            "subject_ref": verified.resolved.canonical_ref.to_string(),
            "raw_content_digest": &verified.resolved.raw_content_digest,
            "content_hash": &verified.resolved.content_hash,
            "signer": verified.signer.as_ref().map(|signer| signer.0.as_str()),
            "trust_class": verified.trust_class,
        })
    });
    let effect_authority = prepared
        .and_then(|prepared| prepared.effect_authority.as_ref())
        .map(|authority| {
            serde_json::json!({
                "authorization": &authority.authorization,
                "action_digest": &authority.action_digest,
                "subject_effect_class_ceiling": authority.subject_effect_class_ceiling,
            })
        });
    let workspace_authority = workspace_operation.map(|operation| {
        serde_json::json!({
            "workspace_id": &operation.workspace_id,
            "access": operation.access,
            "worker_instance_id": &operation.worker_instance_id,
            "worker_boot_epoch": operation.worker_boot_epoch,
            "worker_boot_identity_hash": &operation.worker_boot_identity_hash,
            "project_authority_digest": &operation.project_authority_digest,
            "workload_client_grant_digest": &operation.grant_digest,
            "input_base_snapshot_hash": &operation.input_base_snapshot_hash,
        })
    });
    let identity = serde_json::json!({
        "schema": RUNTIME_ACTION_REQUEST_SCHEMA,
        "action_digest": action_digest,
        "product_selections": product_selections,
        "mode": &action.thread,
        "chain_root_id": chain_root_id,
        "current_site_id": current_site_id,
        "origin_site_id": origin_site_id,
        "acting_principal": &thread_auth.acting_principal,
        "handler_authority": handler_authority,
        "effective_caps": effective_caps,
        "project_authority": child_provenance.project_authority(),
        "lifecycle_authority": lifecycle_authority,
        "hard_limits": &cap.hard_limits,
        "depth": cap.depth,
        "accounting_scope": &cap.accounting_scope,
        "callback_root_item_ref": &cap.item_ref,
        "callback_root_raw_content_digest": &cap.root_raw_content_digest,
        "callback_effective_definition_digest": &cap.effective_definition_digest,
        "prepared_subject": prepared_subject,
        "effect_authority": effect_authority,
        "workspace_authority": workspace_authority,
    });
    let canonical = lillux::canonical_json(&identity)
        .context("canonicalize exact runtime action request authority")?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn parent_execution_context_from_capability(
    cap: &ryeos_app::callback_token::CallbackCapability,
) -> crate::dispatch::ParentExecutionContext {
    crate::dispatch::ParentExecutionContext {
        parent_thread_id: cap.thread_id.clone(),
        hard_limits: cap.hard_limits.clone(),
        depth: cap.depth,
        accounting_scope: cap.accounting_scope.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::time::{Duration, MonotonicDeadline};
    use std::sync::Arc;

    #[test]
    fn workload_grant_binds_owner_not_downgraded_child_publication() {
        use ryeos_state::objects::{
            EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
            PinnedTerminalPublication,
        };
        for terminal_publication in [
            PinnedTerminalPublication::RetainResult,
            PinnedTerminalPublication::RetainCurrentHead {
                principal_key: "b".repeat(64),
                project_hash: "c".repeat(64),
                expected_hash: "a".repeat(64),
            },
        ] {
            let owner = ExecutionProjectAuthority::pinned(
                "site:fixture:project".into(),
                Some(std::path::PathBuf::from("/fixture")),
                "a".repeat(64),
                PinnedProjectRealization::Cow {
                    terminal_publication,
                },
                EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap();
            let child = owner.clone().for_child().unwrap();
            let owner_digest = canonical_authority_digest(&owner).unwrap();
            let child_digest = canonical_authority_digest(&child).unwrap();
            assert_ne!(owner_digest, child_digest);
            assert_eq!(
                verify_workload_owner_project_authority(&owner, &owner_digest).unwrap(),
                owner_digest
            );
            assert!(verify_workload_owner_project_authority(&owner, &child_digest).is_err());
            assert!(verify_workload_owner_project_authority(&child, &owner_digest).is_err());
            assert!(matches!(
                child,
                ExecutionProjectAuthority::PinnedGeneration {
                    realization: PinnedProjectRealization::Cow {
                        terminal_publication: PinnedTerminalPublication::Discard
                    },
                    ..
                }
            ));
            let mut changed = owner.clone();
            if let ExecutionProjectAuthority::PinnedGeneration { snapshot_hash, .. } = &mut changed
            {
                *snapshot_hash = "d".repeat(64);
            }
            assert!(verify_workload_owner_project_authority(&changed, &owner_digest).is_err());
        }
    }

    // ── V5.5 P2: enforce_callback_caps ──────────────────────────────

    fn test_auth() -> ryeos_runtime::authorizer::Authorizer {
        ryeos_runtime::authorizer::Authorizer::new()
    }

    #[test]
    fn runtime_action_request_selector_identity_retains_witness_source() {
        let mut local = ryeos_runtime::callback::ActionPayload {
            operation_id: Some("1".repeat(64)),
            item_id: "tool:test/consume".to_owned(),
            ref_bindings: std::collections::BTreeMap::new(),
            product_selections: serde_json::from_value(serde_json::json!([{
                "target":{"kind":"root"},
                "selection":{
                    "declaration_id":"runtime",
                    "witness_hash":"a".repeat(64),
                    "witness_source":{"kind":"local_capture"},
                    "qualification_hash":null
                }
            }]))
            .unwrap(),
            params: serde_json::json!({}),
            thread: "inline".to_owned(),
            call: None,
            facets: None,
            launch_window: None,
        };
        let local_digest = ryeos_runtime::callback::dispatch_action_digest(&local).unwrap();
        let local_request_selection = runtime_action_request_product_selections(&local).unwrap();

        local.product_selections[0].selection.witness_source =
            ryeos_state::external_content::products::transfer::ProductWitnessSource::Received {
                acceptance_hash: "b".repeat(64),
            };
        assert_eq!(
            ryeos_runtime::callback::dispatch_action_digest(&local).unwrap(),
            local_digest,
            "action behavior must normalize only the receiver-local proof"
        );
        assert_ne!(
            runtime_action_request_product_selections(&local).unwrap(),
            local_request_selection,
            "recoverable request identity must retain the exact witness source"
        );
        assert_eq!(
            RUNTIME_ACTION_REQUEST_SCHEMA,
            "ryeos.runtime_action_request.v3"
        );
    }

    fn enforce_test_callback_caps(
        item_id: &str,
        effective_caps: &[String],
        authorizer: &ryeos_runtime::authorizer::Authorizer,
    ) -> std::result::Result<(), crate::dispatch_error::DispatchError> {
        let kinds = ryeos_engine::test_support::load_live_kind_registry();
        enforce_callback_caps(item_id, effective_caps, authorizer, &kinds)
    }

    #[test]
    fn detached_callback_body_is_completed_with_strict_live_dispatch_evidence() {
        let action_digest = "a".repeat(64);
        let response = serde_json::json!({
            "thread": {
                "thread_id": "T-detached",
                "status": "running",
                "detached": true,
            },
            "result": {
                "detached": true,
                "child_thread_id": "T-detached",
                "queued": false,
            },
        });
        let completed = attach_runtime_dispatch_evidence(response, &action_digest, false).unwrap();
        let decoded: ryeos_runtime::callback_contract::CallbackDispatchResponse =
            serde_json::from_value(completed).unwrap();
        assert_eq!(decoded.dispatch.action_digest, action_digest);
        assert_eq!(
            decoded.dispatch.source,
            ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed
        );
        assert_eq!(
            decoded.dispatch.effect_class,
            ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live
        );
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

    fn live_provenance() -> (
        tempfile::TempDir,
        ryeos_app::execution_provenance::ExecutionProvenance,
    ) {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(ryeos_engine::AI_DIR)).unwrap();
        let authority = ryeos_app::execution_policy::resolve_standard_local_live_authority(
            project.path(),
            vec![ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
            &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
        )
        .unwrap()
        .project;
        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project.path().canonicalize().unwrap(),
            minimal_engine(),
            authority,
        )
        .unwrap();
        (project, provenance)
    }

    fn test_context_contract(root: &str) -> ryeos_engine::hooks::HookContextContract {
        ryeos_engine::hooks::HookContextContract {
            schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
            allowed_roots: std::collections::BTreeSet::from([root.to_string()]),
        }
    }

    fn test_authorization(
        owner_kind: &str,
        event: &str,
        layer: ryeos_runtime::hooks_loader::HookLayer,
        result_mode: ryeos_runtime::hooks_loader::HookResultMode,
    ) -> ryeos_app::callback_token::HookDispatchAuthorization {
        ryeos_app::callback_token::HookDispatchAuthorization {
            owner_kind: owner_kind.to_string(),
            hook_id: "audit".to_string(),
            event: event.to_string(),
            layer,
            result_mode,
            context_contract: test_context_contract("event"),
            dispatch_caps: vec!["ryeos.execute.tool.test/audit".to_string()],
        }
    }

    fn test_graph_step_occurrence() -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::new(
            "graph",
            "graph_step_completed",
            "graph:test/fixture",
            "d".repeat(64),
            "e".repeat(64),
        )
        .with_text_coordinate("graph_run_id", "run-1")
        .with_counter_coordinate("step", 3)
        .with_text_coordinate("node", "audit")
    }

    fn test_directive_occurrence(event: &str) -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::new(
            "directive",
            event,
            "directive:test/fixture",
            "a".repeat(64),
            "b".repeat(64),
        )
        .with_counter_coordinate("turn", 1)
    }

    fn test_dispatch_params(
        operation_id: Option<String>,
        hook_dispatch: Option<ryeos_runtime::callback::HookDispatchIdentity>,
    ) -> DispatchActionParams {
        DispatchActionParams {
            callback_token: "cbt-test".to_string(),
            thread_id: "T-parent".to_string(),
            thread_auth_token: "tat-test".to_string(),
            action: ryeos_runtime::callback::ActionPayload {
                operation_id,
                product_selections: Vec::new(),
                item_id: "tool:test/audit".to_string(),
                ref_bindings: std::collections::BTreeMap::new(),
                params: serde_json::json!({}),
                thread: "inline".to_string(),
                call: None,
                facets: None,
                launch_window: None,
            },
            hook_dispatch,
            effect_dispatch: None,
            workload_invocation: None,
        }
    }

    #[test]
    fn ordinary_and_hook_occurrence_contracts_are_mutually_exclusive() {
        assert!(
            validate_action_occurrence_contract(&test_dispatch_params(Some("1".repeat(64)), None,))
                .is_ok()
        );
        assert!(validate_action_occurrence_contract(&test_dispatch_params(None, None)).is_err());
        assert!(
            validate_action_occurrence_contract(&test_dispatch_params(Some("A".repeat(64)), None,))
                .is_err()
        );

        let hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_directive_occurrence("continuation"),
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Discard,
            context_contract: test_context_contract("event"),
            context_hash: "c".repeat(64),
        };
        assert!(
            validate_action_occurrence_contract(&test_dispatch_params(None, Some(hook.clone()),))
                .is_ok()
        );
        assert!(
            validate_action_occurrence_contract(&test_dispatch_params(
                Some("1".repeat(64)),
                Some(hook),
            ))
            .is_err()
        );
    }

    #[test]
    fn digest_only_inline_result_is_refused_during_preflight() {
        let error = enforce_inline_result_retention(
            "service:test/secret",
            ryeos_engine::history_policy::ThreadResultRetention::DigestOnly,
        )
        .unwrap_err();
        let dispatch = error
            .downcast_ref::<crate::dispatch_error::DispatchError>()
            .expect("preflight refusal remains a typed dispatch error");
        assert!(matches!(
            dispatch,
            crate::dispatch_error::DispatchError::LaunchPolicyForbidden { code, .. }
                if code == "inline_result_not_replayable"
        ));
        assert!(!dispatch.retryable());

        enforce_inline_result_retention(
            "service:test/public",
            ryeos_engine::history_policy::ThreadResultRetention::Full,
        )
        .unwrap();
    }

    #[test]
    fn inline_thread_run_gate_consumes_the_exact_preflight_class() {
        use crate::dispatch::RootDispatchClass;

        for class in [RootDispatchClass::ManagedSubprocess] {
            let error = enforce_inline_dispatch_class("alias:test/run", class, false).unwrap_err();
            assert!(error.to_string().contains("exact admitted route"));
            assert!(error.to_string().contains("managed thread run"));
            enforce_inline_dispatch_class("graph:test/producer", class, true).unwrap();
        }
        for class in [
            RootDispatchClass::TerminalSubprocess,
            RootDispatchClass::MethodDispatch,
            RootDispatchClass::InProcess,
        ] {
            enforce_inline_dispatch_class("alias:test/leaf", class, false).unwrap();
        }
    }

    #[test]
    fn awaited_root_preserves_in_band_publication_and_actual_replay_evidence() {
        use ryeos_runtime::callback_contract::{RuntimeDispatchPublication, RuntimeDispatchSource};

        let action_digest = "a".repeat(64);
        let accepted = serde_json::json!({"fixture":"accepted product"});
        let accepted_hash = ryeos_effect_contract::canonical_value_digest(&accepted).unwrap();
        for (source, publication) in [
            (
                RuntimeDispatchSource::Executed,
                RuntimeDispatchPublication::Inserted,
            ),
            (
                RuntimeDispatchSource::Executed,
                RuntimeDispatchPublication::Folded,
            ),
            (
                RuntimeDispatchSource::EffectRecord,
                RuntimeDispatchPublication::NotApplicable,
            ),
        ] {
            let replayed_from =
                (source == RuntimeDispatchSource::EffectRecord).then(|| "c".repeat(64));
            let thread = if replayed_from.is_some() {
                Value::Null
            } else {
                serde_json::json!({
                    "thread_id":"T-producer", "chain_root_id":"T-producer", "status":"completed"
                })
            };
            let evidence = ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                source,
                effect_class: ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Recorded,
                action_digest: action_digest.clone(),
                effect_identity: Some("b".repeat(64)),
                publication,
                record_hash: Some("c".repeat(64)),
                replayed_from: replayed_from.clone(),
                result_projection: ryeos_effect_contract::DispatchResultProjection::RetainedEffect {
                    retained_result: ryeos_effect_contract::RetainedEffectResult::ProductBuildAcceptedResult {
                        object_hash: accepted_hash.clone(),
                    },
                },
            };
            let response = serde_json::json!({
                "thread":thread,
                "result":{"outcome_code":null,"result":accepted,"error":null,"artifacts":[],
                    "replayed_from":replayed_from},
                "dispatch":evidence,
                "result_project_snapshot_hash":"d".repeat(64),
            });
            let projected =
                completed_awaited_root_response(&response, &action_digest, "T-producer")
                    .unwrap()
                    .unwrap();
            assert_eq!(projected["dispatch"], response["dispatch"]);
            assert_eq!(projected["result"], response["result"]);
            assert!(projected.get("result_project_snapshot_hash").is_none());
            assert!(
                completed_awaited_root_response(&response, &"f".repeat(64), "T-producer").is_err()
            );

            let mut wrong_result = response.clone();
            wrong_result["result"]["result"] = serde_json::json!({"fixture":"different"});
            assert!(
                completed_awaited_root_response(&wrong_result, &action_digest, "T-producer")
                    .is_err()
            );
            if source == RuntimeDispatchSource::Executed {
                assert!(
                    completed_awaited_root_response(&response, &action_digest, "T-other").is_err()
                );
                for status in ["continued", "running", "failed", "cancelled", "killed"] {
                    let mut incomplete = response.clone();
                    incomplete["thread"]["status"] = Value::String(status.into());
                    assert!(
                        completed_awaited_root_response(&incomplete, &action_digest, "T-producer")
                            .is_err()
                    );
                }
                let mut no_thread = response.clone();
                no_thread["thread"] = Value::Null;
                assert!(
                    completed_awaited_root_response(&no_thread, &action_digest, "T-producer")
                        .is_err()
                );
            }
        }
        // A real suspension has no accepted dispatch proof and must await its
        // terminal successor instead of being mistaken for in-band success.
        assert!(
            completed_awaited_root_response(
                &serde_json::json!({"thread":{"status":"continued"},"result":{}}),
                &action_digest,
                "T-producer",
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn awaited_product_terminal_failure_is_known_nonretryable_and_not_published() {
        for status in ["failed", "cancelled", "killed"] {
            let error = awaited_root_terminal_failure(
                "graph:test/producer",
                "T-producer",
                status,
                r#"{"code":"fixture_failure"}"#,
            );
            let dispatch = error
                .downcast_ref::<crate::dispatch_error::DispatchError>()
                .expect("typed dispatch failure");
            assert_eq!(dispatch.code(), "subprocess_run_failed");
            assert!(!dispatch.retryable());
            assert!(dispatch.to_string().contains(status));
            assert!(dispatch.to_string().contains("fixture_failure"));
        }

        let oversized =
            "x".repeat(ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES * 2);
        let error = awaited_root_terminal_failure(
            "graph:test/producer",
            "T-producer",
            "failed",
            &oversized,
        );
        assert!(
            error.to_string().len()
                < ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_ERROR_MESSAGE_BYTES + 256
        );
    }

    #[test]
    fn recovery_errors_are_typed_unknown_without_erasing_existing_unknown() {
        let operation_id = "1".repeat(64);
        let wrapped = runtime_action_recovery_error(
            &operation_id,
            "terminal child reconstruction",
            anyhow::anyhow!("terminal evidence drift"),
        );
        let typed = wrapped
            .downcast_ref::<crate::dispatch_error::DispatchError>()
            .unwrap();
        assert!(matches!(
            typed,
            crate::dispatch_error::DispatchError::RuntimeActionOutcomeUnknown {
                operation_id: retained,
                ..
            } if retained == &operation_id
        ));
        assert!(typed.to_string().contains(
            "terminal child reconstruction could not produce a replay-safe terminal response"
        ));

        let existing = runtime_action_outcome_unknown(&operation_id, "contact already ambiguous");
        let preserved = runtime_action_recovery_error(&operation_id, "unused boundary", existing);
        assert_eq!(
            preserved
                .downcast_ref::<crate::dispatch_error::DispatchError>()
                .unwrap()
                .to_string(),
            format!(
                "runtime action '{operation_id}' outcome is unknown: contact already ambiguous"
            )
        );

        let unavailable = anyhow::Error::new(
            crate::dispatch_error::DispatchError::RuntimeActionResultUnavailable {
                operation_id: operation_id.clone(),
            },
        );
        let preserved =
            runtime_action_recovery_error(&operation_id, "unused boundary", unavailable);
        assert!(matches!(
            preserved.downcast_ref::<crate::dispatch_error::DispatchError>(),
            Some(crate::dispatch_error::DispatchError::RuntimeActionResultUnavailable {
                operation_id: retained,
            }) if retained == &operation_id
        ));
    }

    #[test]
    fn retained_detached_child_converts_post_birth_failure_to_unknown() {
        let operation_id = "2".repeat(64);
        let pre_birth = retained_detached_child_error(
            &operation_id,
            None,
            anyhow::anyhow!("launch preparation refused"),
        );
        assert!(
            pre_birth
                .downcast_ref::<crate::dispatch_error::DispatchError>()
                .is_none()
        );

        let post_birth = retained_detached_child_error(
            &operation_id,
            Some(("T-child", "running")),
            anyhow::anyhow!("launch handoff channel closed"),
        );
        let typed = post_birth
            .downcast_ref::<crate::dispatch_error::DispatchError>()
            .expect("retained child requires a typed unknown outcome");
        assert!(matches!(
            typed,
            crate::dispatch_error::DispatchError::RuntimeActionOutcomeUnknown {
                operation_id: retained,
                ..
            } if retained == &operation_id
        ));
        assert!(typed.to_string().contains("T-child"));
        assert!(typed.to_string().contains("running"));
    }

    #[test]
    fn callback_capability_maps_to_parent_execution_context_without_kind_checks() {
        let (project, provenance) = live_provenance();
        let cap = ryeos_app::callback_token::CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-parent".to_string(),
            launch_owner: None,
            runtime_method_surface:
                ryeos_app::callback_token::CallbackRuntimeMethodSurface::complete_runtime_protocol(),
            chain_root_id: "T-parent".to_string(),
            project_path: project.path().to_path_buf(),
            expires_at: MonotonicDeadline::after(Duration::from_secs(300)),
            effective_caps: vec!["ryeos.*".to_string()],
            provenance,
            effective_bundle_id: None,
            item_ref: Some("graph:team/parent".to_string()),
            root_raw_content_digest: "0".repeat(64),
            effective_definition_digest: Some("1".repeat(64)),
            hook_dispatch_authorizations: Vec::new(),
            effect_dispatch_authorizations: Vec::new(),
            hard_limits: serde_json::json!({"turns": 6, "tokens": 1000}),
            depth: 4,
            accounting_scope: None,
            workload_client_grant: None,
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
    fn hook_ledger_key_is_chain_occurrence_scoped_and_request_hash_binds_action() {
        let identity = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_graph_step_occurrence(),
            hook_id: "audit-hook".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Control,
            context_contract: test_context_contract("state"),
            context_hash: "c".repeat(64),
        };
        let action = ryeos_runtime::callback::ActionPayload {
            product_selections: Vec::new(),
            operation_id: None,
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
        assert_eq!(
            seed_a.dispatch_key,
            "e6e8222a29ca454c492dcce28a3bad0a8ea85b983f19d0f0e21908176d2445d6"
        );
        assert_eq!(
            request_a,
            "83b2f7a909d64356d2b61615745568f97f1e4f88c60d04fbdbdb02e64157ba96"
        );

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

        let seed_for = |identity: &ryeos_runtime::callback::HookDispatchIdentity,
                        chain_root: &str| {
            hook_dispatch_ledger_seed(
                identity,
                &changed_action,
                chain_root,
                "T-segment-a",
                std::path::Path::new("/project"),
                "principal",
                &["cap:a".to_string(), "cap:b".to_string()],
                &serde_json::json!({"turns": 4}),
                2,
                "graph:test/fixture",
            )
            .unwrap()
        };
        let mut changed_context = identity.clone();
        changed_context.context_hash = "e".repeat(64);
        let (context_seed, context_request) = seed_for(&changed_context, "T-root");
        assert_eq!(seed_a.dispatch_key, context_seed.dispatch_key);
        assert_ne!(changed_request, context_request);

        let mut changed_layer = identity.clone();
        changed_layer.layer = ryeos_runtime::hooks_loader::HookLayer::Project;
        assert_ne!(
            seed_a.dispatch_key,
            seed_for(&changed_layer, "T-root").0.dispatch_key
        );
        let mut changed_mode = identity.clone();
        changed_mode.result_mode = ryeos_runtime::hooks_loader::HookResultMode::Observation;
        assert_ne!(
            seed_a.dispatch_key,
            seed_for(&changed_mode, "T-root").0.dispatch_key
        );
        let mut changed_hook = identity.clone();
        changed_hook.hook_id = "different-hook".to_string();
        assert_ne!(
            seed_a.dispatch_key,
            seed_for(&changed_hook, "T-root").0.dispatch_key
        );
        let mut changed_occurrence = identity.clone();
        changed_occurrence.occurrence = changed_occurrence
            .occurrence
            .clone()
            .with_counter_coordinate("step", 4);
        assert_ne!(
            seed_a.dispatch_key,
            seed_for(&changed_occurrence, "T-root").0.dispatch_key
        );
        assert_ne!(
            seed_a.dispatch_key,
            seed_for(&identity, "T-other").0.dispatch_key
        );
    }

    #[test]
    fn hook_identity_must_match_launch_captured_root_authority() {
        let hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_directive_occurrence("after_step"),
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Discard,
            context_contract: test_context_contract("event"),
            context_hash: "c".repeat(64),
        };
        let authorization = test_authorization(
            "directive",
            "after_step",
            ryeos_runtime::hooks_loader::HookLayer::Operator,
            ryeos_runtime::hooks_loader::HookResultMode::Discard,
        );
        let action = ryeos_runtime::callback::ActionPayload {
            product_selections: Vec::new(),
            operation_id: None,
            item_id: "tool:test/audit".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: serde_json::json!({}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        assert!(
            validate_hook_identity_authority(
                &hook,
                &action,
                "directive:test/fixture",
                &"a".repeat(64),
                Some(&"b".repeat(64)),
                &authorization,
            )
            .is_ok()
        );
        assert!(
            validate_hook_identity_authority(
                &hook,
                &action,
                "directive:test/other",
                &"a".repeat(64),
                Some(&"b".repeat(64)),
                &authorization,
            )
            .is_err()
        );
        assert!(
            validate_hook_identity_authority(
                &hook,
                &action,
                "directive:test/fixture",
                &"d".repeat(64),
                Some(&"b".repeat(64)),
                &authorization,
            )
            .is_err()
        );
        assert!(
            validate_hook_identity_authority(
                &hook,
                &action,
                "directive:test/fixture",
                &"a".repeat(64),
                Some(&"d".repeat(64)),
                &authorization,
            )
            .is_err()
        );
    }

    #[test]
    fn hook_identity_must_exist_in_the_launch_captured_hook_set() {
        let hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_directive_occurrence("after_step"),
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Observation,
            context_contract: test_context_contract("event"),
            context_hash: "c".repeat(64),
        };
        let admitted = test_authorization(
            "directive",
            "after_step",
            ryeos_runtime::hooks_loader::HookLayer::Operator,
            ryeos_runtime::hooks_loader::HookResultMode::Observation,
        );
        assert!(select_hook_admission(&hook, std::slice::from_ref(&admitted)).is_ok());

        let mut forged = hook.clone();
        forged.hook_id = "forged".to_string();
        assert!(select_hook_admission(&forged, std::slice::from_ref(&admitted)).is_err());
        forged = hook.clone();
        forged.layer = ryeos_runtime::hooks_loader::HookLayer::Project;
        assert!(select_hook_admission(&forged, std::slice::from_ref(&admitted)).is_err());
        forged = hook.clone();
        forged.result_mode = ryeos_runtime::hooks_loader::HookResultMode::Discard;
        assert!(select_hook_admission(&forged, std::slice::from_ref(&admitted)).is_err());
        forged = hook.clone();
        forged.occurrence = test_directive_occurrence("continuation");
        assert!(select_hook_admission(&forged, &[admitted]).is_err());
    }

    #[test]
    fn hook_dispatch_uses_only_the_selected_source_grants() {
        let hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_directive_occurrence("after_step"),
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Operator,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Observation,
            context_contract: test_context_contract("event"),
            context_hash: "c".repeat(64),
        };
        let admitted = test_authorization(
            "directive",
            "after_step",
            ryeos_runtime::hooks_loader::HookLayer::Operator,
            ryeos_runtime::hooks_loader::HookResultMode::Observation,
        );
        let admitted_hooks = [admitted];
        let selected = select_hook_admission(&hook, &admitted_hooks).unwrap();
        let root_grants = vec!["ryeos.execute.tool.test/*".to_string()];
        let authorizer = test_auth();

        assert!(
            enforce_test_callback_caps("tool:test/audit", &selected.dispatch_caps, &authorizer)
                .is_ok()
        );
        assert!(
            enforce_test_callback_caps("tool:test/privileged", &root_grants, &authorizer).is_ok()
        );
        assert!(
            enforce_test_callback_caps(
                "tool:test/privileged",
                &selected.dispatch_caps,
                &authorizer,
            )
            .is_err()
        );
    }

    #[test]
    fn hook_result_policy_is_rechecked_at_the_daemon_boundary() {
        let mut hook = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: test_directive_occurrence("after_step"),
            hook_id: "audit".to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Infrastructure,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Control,
            context_contract: test_context_contract("event"),
            context_hash: "b".repeat(64),
        };
        let mut action = ryeos_runtime::callback::ActionPayload {
            product_selections: Vec::new(),
            operation_id: None,
            item_id: "tool:test/audit".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: serde_json::json!({}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        assert!(
            validate_hook_result_policy(&hook, &action)
                .unwrap_err()
                .to_string()
                .contains("cannot declare result `control`")
        );

        hook.result_mode = ryeos_runtime::hooks_loader::HookResultMode::Observation;
        validate_hook_result_policy(&hook, &action).unwrap();
        action.params = serde_json::json!({
            "body": "x".repeat(ryeos_runtime::envelope::MAX_HOOK_OBSERVATION_ACTION_BYTES)
        });
        assert!(
            validate_hook_result_policy(&hook, &action)
                .unwrap_err()
                .to_string()
                .contains("maximum")
        );
    }

    #[test]
    fn caps_full_wildcard_allows_everything() {
        let auth = test_auth();
        // The `ryeos.*` cap (or expansion) covers all kinds.
        let caps = vec!["ryeos.*".to_string()];
        assert!(enforce_test_callback_caps("tool:any/thing", &caps, &auth).is_ok());
        assert!(enforce_test_callback_caps("directive:any/thing", &caps, &auth).is_ok());
    }

    #[test]
    fn executable_kind_callbacks_use_signed_capability_projections() {
        let auth = test_auth();
        for kind in [
            "tool",
            "graph",
            "directive",
            "knowledge",
            "client",
            "service",
            "worker_execution",
            "runtime",
            "worker",
        ] {
            let item = format!("{kind}:test/subject");
            let required = format!("ryeos.execute.{kind}.test/subject");
            assert!(
                enforce_test_callback_caps(&item, &[required.clone()], &auth).is_ok(),
                "{kind}"
            );
            assert!(
                matches!(enforce_test_callback_caps(&item, &[], &auth), Err(crate::dispatch_error::DispatchError::MissingCap { required: actual }) if actual == required),
                "{kind}"
            );
            assert!(
                matches!(
                    enforce_test_callback_caps(
                        &item,
                        &[format!("ryeos.execute.{kind}.test/other")],
                        &auth
                    ),
                    Err(crate::dispatch_error::DispatchError::MissingCap { .. })
                ),
                "{kind}"
            );
        }
    }

    #[test]
    fn caps_empty_denies_everything() {
        let auth = test_auth();
        let caps: Vec<String> = vec![];
        let err = enforce_test_callback_caps("tool:foo/bar", &caps, &auth).unwrap_err();
        assert_eq!(err.code(), "missing_cap");
        assert!(err.to_string().contains("ryeos.execute.tool.foo/bar"));
    }

    #[test]
    fn caps_kind_wildcard_matches_any_id_in_kind() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.*".to_string()];
        assert!(enforce_test_callback_caps("tool:any/echo", &caps, &auth).is_ok());
        assert!(enforce_test_callback_caps("tool:other/foo", &caps, &auth).is_ok());
        // Different kind — denied.
        let err = enforce_test_callback_caps("directive:foo/bar", &caps, &auth).unwrap_err();
        assert_eq!(err.code(), "missing_cap");
    }

    #[test]
    fn caps_exact_match_with_slash_subject() {
        let auth = test_auth();
        // `tool:foo/bar` → required cap `ryeos.execute.tool.foo/bar`.
        // Slash is preserved in subject, matching the canonical format.
        let caps = vec!["ryeos.execute.tool.foo/bar".to_string()];
        assert!(enforce_test_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        let err = enforce_test_callback_caps("tool:foo/baz", &caps, &auth).unwrap_err();
        assert_eq!(err.code(), "missing_cap");
    }

    #[test]
    fn caps_invalid_item_id_rejected() {
        let auth = test_auth();
        let caps = vec!["ryeos.execute.tool.foo".to_string()];
        let err = enforce_test_callback_caps("not-a-canonical-ref", &caps, &auth).unwrap_err();
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
        assert!(enforce_test_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        // A sibling `tool:foobar` requires `ryeos.execute.tool.foobar`,
        // which does NOT match `ryeos.execute.tool.foo/*` — the `/`
        // separator is required.
        let err = enforce_test_callback_caps("tool:foobar", &caps, &auth).unwrap_err();
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
        assert!(enforce_test_callback_caps("tool:foo/bar", &caps, &auth).is_ok());
        assert!(enforce_test_callback_caps("tool:baz/qux/deep", &caps, &auth).is_ok());
    }
}
