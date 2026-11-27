//! Boot-local hosted-workload client authority.
//!
//! The protected target channel is transport adaptation only. This module
//! retains the ordinary callback/thread-auth pair in the daemon and turns a
//! validated frame into exactly one existing `runtime.dispatch_action` call.
//! It owns no alternate identity, dispatcher, child launcher, or operation
//! ledger.

use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use ryeos_app::callback_token::{
    AdmittedWorkloadClientGrant, CallbackCapability, CallbackRuntimeMethodSurface,
};
use ryeos_app::runtime_db::{WorkerProcessState, daemon_generation_id};
use ryeos_app::state::AppState;
use ryeos_executor::execution::persistent_session::ExclusivePersistentSessionIdentity;
use ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_runtime::workload_client::{
    WORKLOAD_CLIENT_CHANNEL_ENV, WORKLOAD_CLIENT_CHANNEL_TARGET_FD, WORKLOAD_CLIENT_PROTOCOL,
    WorkloadClientBootFrame, WorkloadClientCollectedChild, WorkloadClientCollectedResult,
    WorkloadClientDispatchFrame, WorkloadClientOperation, WorkloadClientOutcome,
    WorkloadClientReadyFrame, WorkloadClientRequestContract, WorkloadClientResponseFrame,
};

const WORKLOAD_PRESENTATION_EVENT: &str = "hosted_session.workload_client_admitted";
const WORKLOAD_PRESENTATION_OPERATION: &str = "workload-client-presentation";

/// Compile and start one boot-local workload-client authority when the exact
/// admitted project environment requested it. The returned child endpoint is
/// already owned by the generic isolation target-channel contract.
pub(super) fn prepare_for_dedicated_boot(
    state: &AppState,
    root_capability: &CallbackCapability,
    identity: &ExclusivePersistentSessionIdentity,
) -> Result<Option<ryeos_engine::isolation::IsolationTargetChannelAuthority>> {
    let root_thread = state
        .threads
        .get_thread(&identity.placement_thread_id)?
        .ok_or_else(|| anyhow!("workload-client root thread disappeared"))?;
    let root_launch_capsule_hash = root_thread
        .admitted_launch_capsule_hash
        .as_deref()
        .ok_or_else(|| anyhow!("workload-client root has no admitted launch capsule"))?;
    let session = state
        .state_store
        .dedicated_session(&identity.placement_thread_id)?
        .ok_or_else(|| anyhow!("workload-client dedicated session disappeared"))?;
    if root_thread.status != "running"
        || root_capability.thread_id != root_thread.thread_id
        || root_capability.chain_root_id != root_thread.chain_root_id
        || session.placement_thread_id != root_thread.thread_id
        || session.chain_root_id != root_thread.chain_root_id
        || session.owner_principal != root_thread.requested_by.as_deref().unwrap_or_default()
    {
        bail!("workload-client boot contradicts the admitted root/session state");
    }
    require_reserved_boot(&session, identity)?;
    let owner_principal = session.owner_principal.as_str();
    let session_capsule_hash = session.admitted_capsule_hash.as_str();
    let capsule = state
        .state_store
        .admitted_launch_capsule(&identity.placement_thread_id)?
        .ok_or_else(|| anyhow!("workload-client root has no admitted launch capsule"))?;
    if capsule.content_hash()? != root_launch_capsule_hash {
        bail!("workload-client root capsule changed after session admission");
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &capsule,
        )?;
    if sealed.project_authority() != root_capability.provenance.project_authority()
        || root_capability.thread_id != identity.placement_thread_id
    {
        bail!("workload-client root callback contradicts sealed project authority");
    }
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &capsule.execution_closure
    else {
        bail!("workload-client root is not a managed runtime launch");
    };
    let prepared: ryeos_executor::execution::launch_preparation::PreparedRuntimeLaunch =
        serde_json::from_value(prepared_runtime_launch.clone())
            .context("decode workload-client retained launch authority")?;
    let request = prepared
        .runtime_facts
        .get(ryeos_runtime::workload_client::WORKLOAD_CLIENT_REQUEST_FACT)
        .cloned()
        .map(serde_json::from_value::<WorkloadClientRequestContract>)
        .transpose()
        .context("decode admitted workload-client project request")?;
    let Some(request) = request else {
        return Ok(None);
    };
    request.validate()?;
    let profile_hash =
        ryeos_executor::execution::persistent_session::validate_workload_client_profile(
            state,
            session_capsule_hash,
            &request,
        )?;

    require_private_workload_client_isolation(state)?;
    let execution_policy = state
        .node_policy
        .require::<NodeExecutionAdmissionPolicy>()?;
    let node_policy = execution_policy
        .workload_client
        .as_ref()
        .ok_or_else(|| anyhow!("node execution policy disables workload-client admission"))?;
    node_policy.admit_request(&request)?;

    let root_config = prepared
        .runtime_data
        .get("worker_execution")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("workload-client root has no admitted worker-execution config"))?;
    let root_delegation_caps = root_config
        .get("workload_client_delegation_caps")
        .cloned()
        .map(serde_json::from_value::<Vec<String>>)
        .transpose()
        .context("decode workload-client root delegation ceiling")?
        .ok_or_else(|| {
            anyhow!("worker-execution config has no workload-client delegation ceiling")
        })?;
    validate_delegation_ceiling("worker root", &root_delegation_caps)?;

    let ingress = sealed
        .handler_context()
        .cloned()
        .ok_or_else(|| anyhow!("workload-client admission requires retained ingress authority"))?;
    if !ingress.verified || ingress.fingerprint != owner_principal {
        bail!("workload-client ingress authority is not the verified session owner");
    }
    ingress.validate_execution_authority(
        owner_principal,
        &ingress.scopes,
        &root_thread.current_site_id,
        &root_thread.origin_site_id,
    )?;
    let current_operator = ryeos_app::operator_authority::retained_admitted_operator_authority(
        state,
        owner_principal,
        &root_thread.origin_site_id,
    )?;

    let mut effective_caps = BTreeSet::new();
    for execution in &request.executions {
        effective_caps.insert(required_execute_capability(
            root_capability.provenance.request_engine(),
            &execution.item_ref,
        )?);
        for values in execution.ref_bindings.values() {
            for item_ref in values {
                effective_caps.insert(required_execute_capability(
                    root_capability.provenance.request_engine(),
                    item_ref,
                )?);
            }
        }
    }
    let effective_caps = effective_caps.into_iter().collect::<Vec<_>>();
    if effective_caps.is_empty() {
        bail!("workload-client admission produced no exact execution capabilities");
    }
    for capability in &effective_caps {
        require_capability("calling operator", &ingress.scopes, capability, state)?;
        require_capability(
            "current target operator grant",
            &current_operator.scopes,
            capability,
            state,
        )?;
        require_capability(
            "worker root delegation",
            &root_delegation_caps,
            capability,
            state,
        )?;
        require_capability(
            "node workload-client policy",
            &node_policy.delegation_cap_ceiling,
            capability,
            state,
        )?;
    }

    let project_authority_digest = digest_value(sealed.project_authority())?;
    let request_digest = digest_value(&request)?;
    let caller_scope_digest = digest_value(&canonical_strings(ingress.scopes.clone()))?;
    let root_delegation_digest = digest_value(&root_delegation_caps)?;
    let execution_product_selections =
        ryeos_runtime::workload_client::admitted_execution_product_selections(
            sealed.product_selections(),
            Some(&request),
        )?;
    let execution_presentation = present_admitted_executions(
        state,
        root_capability,
        &request,
        &effective_caps,
        owner_principal,
        &root_thread.current_site_id,
        &root_thread.origin_site_id,
        &execution_product_selections,
    )?;
    let grant = AdmittedWorkloadClientGrant {
        schema: AdmittedWorkloadClientGrant::SCHEMA,
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        chain_root_id: root_capability.chain_root_id.clone(),
        placement_thread_id: identity.placement_thread_id.clone(),
        owner_principal: owner_principal.to_owned(),
        origin_site_id: root_thread.origin_site_id.clone(),
        worker_instance_id: identity.worker_instance_id.clone(),
        worker_boot_epoch: identity.boot_epoch,
        worker_boot_identity_hash: identity.boot_identity_hash.clone(),
        root_launch_capsule_hash: root_launch_capsule_hash.to_owned(),
        session_capsule_hash: session_capsule_hash.to_owned(),
        project_authority_digest,
        request_digest,
        caller_scope_digest,
        operator_grant_digest: current_operator.grant_digest,
        root_delegation_digest,
        node_policy_generation_digest: state.node_policy.generation_digest().to_owned(),
        ingresses: request
            .bindings
            .iter()
            .map(|binding| binding.ingress())
            .collect(),
        executions: request.executions.clone(),
        execution_product_selections,
        execution_presentation,
        effective_caps: effective_caps.clone(),
        max_in_flight: request.max_in_flight,
        max_invocations_per_boot: request.max_invocations_per_boot,
        max_lifetime_seconds: request.max_lifetime_seconds,
        max_request_bytes: node_policy.max_request_bytes,
    };
    let grant_digest = grant.digest()?;

    // One immutable placement fact binds the exact mechanical registration
    // recipe across boot rotation. A different presentation under the same
    // placement conflicts before contact; it cannot silently replace tools
    // restored by the pinned protocol implementation from its captured state.
    let presentation_fact = json!({
        "placement_thread_id":identity.placement_thread_id,
        "session_capsule_hash":session_capsule_hash,
        "request_digest":grant.request_digest,
        "profile_hash":profile_hash,
        "presentation_digest":digest_value(&grant.execution_presentation)?,
        "execution_product_selections_digest":digest_value(&grant.execution_product_selections)?,
        "ingresses":grant.ingresses,
    });
    if identity.boot_epoch > 1 {
        let retained = ryeos_app::authoritative_root_fact::lookup(
            state,
            &identity.placement_thread_id,
            WORKLOAD_PRESENTATION_EVENT,
            WORKLOAD_PRESENTATION_OPERATION,
        )?;
        if retained.count != 1 {
            bail!("recovered workload boot lacks its original presentation testimony");
        }
    }
    ryeos_app::authoritative_root_fact::append_once(
        state,
        &identity.placement_thread_id,
        WORKLOAD_PRESENTATION_EVENT,
        WORKLOAD_PRESENTATION_OPERATION,
        presentation_fact,
    )?;

    state
        .state_store
        .assert_no_active_runtime_workspace_operation_for_chain(&root_capability.chain_root_id)?;

    let ttl = lillux::time::Duration::from_secs(request.max_lifetime_seconds);
    let callback = state.callback_tokens.generate_with_context(
        &identity.placement_thread_id,
        root_capability.project_path.clone(),
        ttl,
        effective_caps.clone(),
        root_capability.provenance.clone(),
        root_capability.effective_bundle_id.clone(),
        root_capability.item_ref.clone(),
        root_capability.root_raw_content_digest.clone(),
        root_capability.effective_definition_digest.clone(),
        root_capability.hard_limits.clone(),
        root_capability.depth,
        // The worker session is a child of its root thread: it can hold no
        // wider ceiling than the root bearer it is derived from.
        root_capability.thread_ceiling,
    );
    let callback_token = callback.token.clone();
    let mut minted_thread_auth_token = None;
    let setup = (|| {
        if !state
            .callback_tokens
            .set_chain_root(&callback_token, &root_capability.chain_root_id)
            || !state.callback_tokens.set_launch_owner(
                &callback_token,
                root_capability
                    .launch_owner
                    .clone()
                    .ok_or_else(|| anyhow!("workload-client root callback has no launch owner"))?,
            )
        {
            bail!("workload-client callback disappeared during boot admission");
        }
        if !state.callback_tokens.set_evaluator_origin_chain(
            &callback_token,
            root_capability.evaluator_origin_chain.clone(),
        ) {
            bail!("workload-client callback disappeared during origin binding");
        }
        if !state.callback_tokens.restrict_runtime_methods(
            &callback_token,
            CallbackRuntimeMethodSurface::exact(vec![
                ryeos_runtime::RUNTIME_DISPATCH_ACTION_METHOD.to_owned(),
            ])?,
        )? || !state
            .callback_tokens
            .set_workload_client_grant(&callback_token, grant.clone())?
        {
            bail!("workload-client callback disappeared during grant binding");
        }
        if let Some(scope) = root_capability.accounting_scope.clone()
            && !state
                .callback_tokens
                .set_accounting_scope(&callback_token, scope)
        {
            bail!("workload-client callback disappeared during accounting binding");
        }

        let narrowed_ingress = ingress.narrowed_for_execution(
            effective_caps.clone(),
            &root_thread.current_site_id,
            &root_thread.origin_site_id,
        )?;
        let thread_auth = state.thread_auth.mint(
            &identity.placement_thread_id,
            owner_principal.to_owned(),
            effective_caps,
            Some(narrowed_ingress),
            &root_thread.current_site_id,
            &root_thread.origin_site_id,
            ttl,
        )?;
        let thread_auth_token = thread_auth.token.clone();
        minted_thread_auth_token = Some(thread_auth_token.clone());
        let channels = lillux::inherited_duplex_channel_pair()
            .map_err(anyhow::Error::msg)
            .context("create protected workload-client channel")?;
        let target = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
            channels.1,
            WORKLOAD_CLIENT_CHANNEL_TARGET_FD,
            WORKLOAD_CLIENT_CHANNEL_ENV,
        )?;
        spawn_daemon_broker(
            state.clone(),
            grant,
            grant_digest,
            callback_token.clone(),
            thread_auth_token,
            channels.0,
        )?;
        Ok(target)
    })();
    if setup.is_err() {
        state.callback_tokens.invalidate(&callback_token);
        if let Some(thread_auth_token) = minted_thread_auth_token.as_deref() {
            state.thread_auth.invalidate(thread_auth_token);
        }
    }
    setup.map(Some)
}

/// Admission already reserves the pending boot before any process exists.
/// Require that exact tuple, not empty worker fields: clearing it would erase
/// the pre-contact cleanup fence established by ordinary session admission and
/// recovery. `worker_process` attachment later proves liveness separately.
fn require_reserved_boot(
    session: &ryeos_app::runtime_db::DedicatedSessionRecord,
    identity: &ExclusivePersistentSessionIdentity,
) -> Result<()> {
    if session.state != "admitted"
        || session.send_boundary != "none"
        || session.placement_thread_id != identity.placement_thread_id
        || session.worker_instance_id.as_deref() != Some(identity.worker_instance_id.as_str())
        || session.worker_boot_epoch != Some(identity.boot_epoch)
        || identity.boot_epoch == 0
        || session.credential_generation != identity.lifecycle_generation
    {
        bail!("workload-client boot differs from the admitted pending worker reservation");
    }
    Ok(())
}

fn require_private_workload_client_isolation(state: &AppState) -> Result<()> {
    use ryeos_isolation_protocol::IsolationCapability;
    let capabilities = &state.isolation.inspection().backend.effective_capabilities;
    if !state.isolation.is_enforced()
        || !capabilities.contains(&IsolationCapability::FilesystemPrivateTmp)
        || !capabilities.contains(&IsolationCapability::ProcessIsolatedPidNamespace)
        || !capabilities.contains(&IsolationCapability::IpcTargetUnixStream)
    {
        bail!(
            "workload-client admission requires enforced private-tmp, PID-namespace, and target-channel isolation"
        );
    }
    Ok(())
}

fn validate_delegation_ceiling(label: &str, capabilities: &[String]) -> Result<()> {
    if capabilities.is_empty()
        || capabilities.len() > ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_DELEGATION_CAPS
        || capabilities.windows(2).any(|pair| pair[0] >= pair[1])
    {
        bail!("{label} workload-client delegation ceiling is not finite and canonical");
    }
    for capability in capabilities {
        if !capability.starts_with("ryeos.execute.")
            || ryeos_runtime::authorizer::validate_scope_pattern(capability).is_err()
        {
            bail!("{label} workload-client delegation ceiling is not canonical");
        }
    }
    Ok(())
}

fn required_execute_capability(
    engine: &ryeos_engine::engine::Engine,
    item_ref: &str,
) -> Result<String> {
    // This registered kind projection is the single capability constructor
    // for both workload-client admission and the downstream callback action.
    // Do not reconstruct `ryeos.execute.{kind}.{id}` here or branch on a kind:
    // a kind schema may own different signed capability vocabulary.
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
        .with_context(|| format!("parse workload-client item ref `{item_ref}`"))?;
    let schema = engine.kinds.get(&canonical.kind).ok_or_else(|| {
        anyhow!(
            "workload-client item kind `{}` is not registered",
            canonical.kind
        )
    })?;
    let admission = schema.inventory_policy.admission.as_ref().ok_or_else(|| {
        anyhow!(
            "workload-client item kind `{}` has no signed execution-capability projection",
            canonical.kind
        )
    })?;
    let capability = admission.required_capability(&canonical);
    if capability.contains('*')
        || capability.contains('?')
        || ryeos_runtime::authorizer::validate_scope_pattern(&capability).is_err()
    {
        bail!("workload-client item produced a non-exact execution capability");
    }
    Ok(capability)
}

fn require_capability(
    label: &str,
    grants: &[String],
    required: &str,
    state: &AppState,
) -> Result<()> {
    state
        .authorizer
        .authorize(grants, &AuthorizationPolicy::require(required))
        .map_err(|error| anyhow!("{label} does not authorize `{required}`: {error}"))
}

fn canonical_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn present_admitted_executions(
    state: &AppState,
    root: &CallbackCapability,
    request: &WorkloadClientRequestContract,
    scopes: &[String],
    owner: &str,
    current_site: &str,
    origin_site: &str,
    product_selections: &std::collections::BTreeMap<
        String,
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
    >,
) -> Result<Value> {
    use ryeos_engine::contracts::{
        EffectivePrincipal, PlanContext, Principal, ProjectContext, SubjectResolutionAuthority,
    };
    let provenance = &root.provenance;
    let subject = provenance.subject_resolution_authority();
    let context = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: owner.to_owned(),
        caller_scopes: scopes.to_vec(),
        engine: provenance.request_engine().clone(),
        requested_call: None,
        // A validate-only presentation of what a worker may launch; no
        // handler runs.
        disclosure_class: ryeos_node_runtime::disclosure::DisclosureClass::Public,
        plan_ctx: PlanContext {
            logical_project: provenance.signer_project(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: owner.to_owned(),
                scopes: scopes.to_vec(),
            }),
            project_context: if matches!(subject, SubjectResolutionAuthority::Projectless) {
                ProjectContext::None
            } else {
                ProjectContext::LocalPath {
                    path: provenance.subject_effective_path().to_path_buf(),
                }
            },
            subject_resolution_authority: subject,
            current_site_id: current_site.to_owned(),
            origin_site_id: origin_site.to_owned(),
            execution_hints: Default::default(),
            scheduled_fire: None,
            validate_only: true,
        },
    };
    let binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &context.engine,
        &context.plan_ctx,
        provenance,
    )?;
    let presentation = request
        .executions
        .iter()
        .map(|ceiling| {
            let selections = product_selections
                .get(&ceiling.item_ref)
                .cloned()
                .unwrap_or_default();
            ryeos_executor::dispatch::present_workload_execution(
                ceiling,
                &selections,
                &binding,
                &context,
                state,
            )
            .map_err(anyhow::Error::new)
        })
        .collect::<Result<Vec<_>>>()?;
    let presentation = Value::Array(presentation);
    ryeos_runtime::workload_client::validate_execution_presentation(&presentation)?;
    Ok(presentation)
}

fn digest_value(value: &impl serde::Serialize) -> Result<String> {
    let value = serde_json::to_value(value)?;
    let canonical = lillux::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn spawn_daemon_broker(
    state: AppState,
    grant: AdmittedWorkloadClientGrant,
    grant_digest: String,
    callback_token: String,
    thread_auth_token: String,
    channel: lillux::InheritedDuplexChannel,
) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("ryeos-workload-client-daemon".to_owned())
        .spawn(move || {
            let _credentials = WorkloadClientCredentialLease {
                state: state.clone(),
                callback_token: callback_token.clone(),
                thread_auth_token: thread_auth_token.clone(),
            };
            if let Err(error) = serve_daemon_broker(
                &state,
                &runtime,
                &grant,
                &grant_digest,
                &callback_token,
                &thread_auth_token,
                channel,
            ) {
                tracing::debug!(
                    placement_thread_id = %grant.placement_thread_id,
                    worker_instance_id = %grant.worker_instance_id,
                    %error,
                    "workload-client boot channel closed"
                );
            }
        })
        .context("start daemon workload-client broker")?;
    Ok(())
}

struct WorkloadClientCredentialLease {
    state: AppState,
    callback_token: String,
    thread_auth_token: String,
}

impl Drop for WorkloadClientCredentialLease {
    fn drop(&mut self) {
        self.state.callback_tokens.invalidate(&self.callback_token);
        self.state.thread_auth.invalidate(&self.thread_auth_token);
    }
}

fn serve_daemon_broker(
    state: &AppState,
    runtime: &tokio::runtime::Handle,
    grant: &AdmittedWorkloadClientGrant,
    grant_digest: &str,
    callback_token: &str,
    thread_auth_token: &str,
    mut channel: lillux::InheritedDuplexChannel,
) -> Result<()> {
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        grant.max_lifetime_seconds,
    ));
    let mut channel = channel.with_deadline(deadline);
    let boot = WorkloadClientBootFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        grant_digest: grant_digest.to_owned(),
        ingresses: grant.ingresses.clone(),
        execution_presentation: grant.execution_presentation.clone(),
        max_lifetime_seconds: grant.max_lifetime_seconds,
        max_in_flight: grant.max_in_flight,
        max_request_bytes: grant.max_request_bytes,
    };
    boot.validate()?;
    ryeos_runtime::workload_client::write_frame_bounded(
        &mut channel,
        &boot,
        ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES,
    )?;
    let ready: WorkloadClientReadyFrame = ryeos_runtime::workload_client::read_frame_bounded(
        &mut channel,
        ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES,
    )?;
    ready.validate()?;
    if ready.grant_digest != grant_digest {
        bail!("workload-client bridge acknowledged a different boot grant");
    }

    for invocation in 0..grant.max_invocations_per_boot {
        if deadline.has_elapsed() {
            bail!("workload-client boot authority expired");
        }
        let request: WorkloadClientDispatchFrame =
            ryeos_runtime::workload_client::read_frame_bounded(
                &mut channel,
                grant.max_request_bytes as usize,
            )?;
        if deadline.has_elapsed() {
            bail!("workload-client authority expired while receiving a request");
        }
        let response = dispatch_request(
            state,
            runtime,
            grant,
            grant_digest,
            callback_token,
            thread_auth_token,
            request,
        );
        ryeos_runtime::workload_client::write_frame(&mut channel, &response)?;
        if invocation + 1 == grant.max_invocations_per_boot {
            break;
        }
    }
    Ok(())
}

/// Route one validated frame by its operation. Collect is answered here from
/// the daemon's own records; it is not a runtime callback method, so it adds
/// nothing to any callback method surface or thread-ceiling list.
fn dispatch_request(
    state: &AppState,
    runtime: &tokio::runtime::Handle,
    grant: &AdmittedWorkloadClientGrant,
    grant_digest: &str,
    callback_token: &str,
    thread_auth_token: &str,
    dispatch: WorkloadClientDispatchFrame,
) -> WorkloadClientResponseFrame {
    if matches!(
        dispatch.request.operation,
        WorkloadClientOperation::Collect(_)
    ) {
        return collect_request(
            state,
            grant,
            grant_digest,
            callback_token,
            thread_auth_token,
            dispatch,
        );
    }
    execute_request(
        state,
        runtime,
        grant,
        grant_digest,
        callback_token,
        thread_auth_token,
        dispatch,
    )
}

fn execute_request(
    state: &AppState,
    runtime: &tokio::runtime::Handle,
    grant: &AdmittedWorkloadClientGrant,
    grant_digest: &str,
    callback_token: &str,
    thread_auth_token: &str,
    dispatch: WorkloadClientDispatchFrame,
) -> WorkloadClientResponseFrame {
    let request_id = dispatch.request.request_id.clone();
    let outcome = (|| -> Result<Value> {
        dispatch.validate()?;
        grant.validate()?;
        if !grant.ingresses.contains(&dispatch.source.ingress()) {
            bail!("workload invocation selected an ungranted ingress");
        }
        validate_live_boot(
            state,
            grant,
            grant_digest,
            callback_token,
            thread_auth_token,
        )?;
        let WorkloadClientOperation::Execute(execute) = dispatch.request.operation else {
            bail!("workload operation is not an execute");
        };
        // The request id is the runtime-asserted occurrence coordinate. Keep
        // behavior out of this identity: runtime.dispatch_action separately
        // retains the canonical action digest, so replaying the same
        // coordinate with different behavior fails instead of minting a
        // second child operation.
        let operation_id = dispatch.source.runtime_operation_id(grant_digest)?;
        let action = ryeos_runtime::callback::ActionPayload {
            product_selections: grant.product_selections_for_action(&execute.item_ref),
            operation_id: Some(operation_id),
            item_id: execute.item_ref,
            ref_bindings: execute.ref_bindings,
            params: execute.params,
            thread: "inline".to_owned(),
            call: execute.call,
            facets: None,
            launch_window: None,
        };
        grant.authorize_action(&action)?;
        // Internal transport retains the same callback admission boundary as
        // UDS: aggregate deadlines and stop fencing precede the ordinary
        // child dispatcher. This method uses callback/thread proofs, not the
        // kernel peer proof reserved for runtime.attach_process.
        runtime.block_on(super::dispatch_runtime_method(
            ryeos_runtime::RUNTIME_DISPATCH_ACTION_METHOD,
            &json!({
                "callback_token": callback_token,
                "thread_id": grant.placement_thread_id,
                "thread_auth_token": thread_auth_token,
                "action": action,
                "workload_invocation": dispatch.source,
            }),
            state,
            None,
        ))
    })();
    let outcome = match outcome {
        Ok(value) => match serde_json::from_value::<
            ryeos_runtime::callback_contract::CallbackDispatchResponse,
        >(value)
        {
            Ok(response) => WorkloadClientOutcome::Dispatched { response },
            // A malformed response after contact must not become a retryable
            // failure-before-execution or a fabricated successful child.
            Err(_) => WorkloadClientOutcome::Failed {
                code: ryeos_runtime::callback::RUNTIME_ACTION_OUTCOME_UNKNOWN_CODE.to_owned(),
                message: "dispatch returned an invalid response after possible child contact"
                    .to_owned(),
                retryable: false,
            },
        },
        Err(error) => classify_workload_dispatch_error(&error),
    };
    WorkloadClientResponseFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        request_id,
        outcome,
    }
}

/// Answer one collect request. The frame's only input is a child id; whether
/// this session may read that child is decided from the daemon's own link and
/// thread records, never from the request.
fn collect_request(
    state: &AppState,
    grant: &AdmittedWorkloadClientGrant,
    grant_digest: &str,
    callback_token: &str,
    thread_auth_token: &str,
    dispatch: WorkloadClientDispatchFrame,
) -> WorkloadClientResponseFrame {
    let request_id = dispatch.request.request_id.clone();
    let outcome = (|| -> Result<WorkloadClientOutcome> {
        dispatch.validate()?;
        grant.validate()?;
        if !grant.ingresses.contains(&dispatch.source.ingress()) {
            bail!("workload invocation selected an ungranted ingress");
        }
        validate_live_boot(
            state,
            grant,
            grant_digest,
            callback_token,
            thread_auth_token,
        )?;
        let WorkloadClientOperation::Collect(collect) = &dispatch.request.operation else {
            bail!("workload operation is not a collect");
        };
        let child_id = collect.child_thread_id.as_str();
        let link = collect_link_facts(state, child_id)?;
        let thread = state.threads.get_thread(child_id)?;
        collect_outcome(
            grant,
            link.as_ref().map(|(parent_chain_root, relation)| {
                (parent_chain_root.as_str(), relation.as_str())
            }),
            thread
                .as_ref()
                .and_then(|thread| thread.requested_by.as_deref()),
            thread.as_ref().map(collected_child_from_thread),
            || {
                Ok(state.threads.get_thread_result(child_id)?.map(|record| {
                    WorkloadClientCollectedResult {
                        outcome_code: record.outcome_code,
                        result: record.result,
                        error: record.error,
                    }
                }))
            },
            || {
                Ok(ryeos_app::vault::node_held_secret_values(
                    &state.secret_sources(),
                )?)
            },
        )
    })();
    WorkloadClientResponseFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        request_id,
        outcome: outcome.unwrap_or_else(|error| classify_workload_dispatch_error(&error)),
    }
}

/// The facts of a child's link that decide a collect, read from the daemon's
/// own records: the chain root of the link's parent thread and the link's
/// relation. The parent's chain root is its thread row's, never a request
/// field. `None` when the child has no link or its parent has no thread row.
fn collect_link_facts(state: &AppState, child_id: &str) -> Result<Option<(String, String)>> {
    let Some((parent, relation)) = state.state_store.child_link_parent_and_relation(child_id)?
    else {
        return Ok(None);
    };
    Ok(state
        .threads
        .get_thread(&parent)?
        .map(|parent| (parent.chain_root_id, relation)))
}

/// The fixed allow-list of a thread row a session may read of its child.
fn collected_child_from_thread(
    thread: &ryeos_app::state_store::ThreadDetail,
) -> WorkloadClientCollectedChild {
    WorkloadClientCollectedChild {
        thread_id: thread.thread_id.clone(),
        status: thread.status.clone(),
        kind: thread.kind.clone(),
        item_ref: thread.item_ref.clone(),
        created_at: thread.created_at.clone(),
        started_at: thread.started_at.clone(),
        finished_at: thread.finished_at.clone(),
        result: None,
    }
}

fn collect_not_permitted() -> WorkloadClientOutcome {
    WorkloadClientOutcome::Failed {
        code: ryeos_runtime::workload_client::WORKLOAD_COLLECT_NOT_PERMITTED_CODE.to_owned(),
        message: ryeos_runtime::workload_client::WORKLOAD_COLLECT_NOT_PERMITTED_MESSAGE.to_owned(),
        retryable: false,
    }
}

/// Daemon-authored error reasons a collect passes through as the error code.
/// Every other error becomes the generic code.
const COLLECT_KNOWN_ERROR_REASONS: [&str; 2] =
    [ryeos_app::cascade::PARENT_END_REASON, "durable_stop_intent"];

const COLLECT_GENERIC_ERROR_CODE: &str = "child_error";
const COLLECT_ERROR_MESSAGE: &str =
    "the child ended with an error; the node does not return error text to a worker session";

/// The failure a worker session sees in place of a child's recorded error: a
/// code and one fixed sentence. The child's own error text is never carried,
/// because it can name paths, hosts and values of the node. The code is
/// chosen from the daemon's own vocabulary, never copied from the child.
fn redacted_failure(error: &Value) -> Value {
    let code = error
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| COLLECT_KNOWN_ERROR_REASONS.contains(reason))
        .unwrap_or(COLLECT_GENERIC_ERROR_CODE);
    json!({ "code": code, "message": COLLECT_ERROR_MESSAGE })
}

fn collect_result_withheld() -> WorkloadClientOutcome {
    WorkloadClientOutcome::Failed {
        code: ryeos_runtime::workload_client::WORKLOAD_COLLECT_RESULT_WITHHELD_CODE.to_owned(),
        message: ryeos_runtime::workload_client::WORKLOAD_COLLECT_RESULT_WITHHELD_MESSAGE
            .to_owned(),
        retryable: false,
    }
}

/// Decide one collect from the facts the daemon holds. `child` is the thread
/// row's allow-list projection and is `None` when no such thread exists;
/// `link` is the child's link as `(chain root of the link's parent thread,
/// relation)`. `read_result` is called only for a permitted, terminal child.
/// A refused request yields one outcome whatever the reason.
///
/// The child's output is the disclosure surface: whatever the child printed,
/// the session reads. Two floors stand under it. The recorded error never
/// leaves as text (see [`redacted_failure`]). The result passes an
/// exact-match secret floor: when `node_secrets` yields a value of at least
/// the minimum length that appears in the output, byte for byte (raw,
/// JSON-escaped or ASCII-escaped), the whole output is withheld with a typed
/// failure, never cut or masked. `node_secrets` is called only when there is
/// output to check, and a failure to enumerate withholds it. The floor is
/// exact-match only: it does not see an encoded, hashed, split or shortened
/// secret, nor one that was never in the set it was given.
fn collect_outcome<S: AsRef<str>>(
    grant: &AdmittedWorkloadClientGrant,
    link: Option<(&str, &str)>,
    child_requested_by: Option<&str>,
    child: Option<WorkloadClientCollectedChild>,
    read_result: impl FnOnce() -> Result<Option<WorkloadClientCollectedResult>>,
    node_secrets: impl FnOnce() -> Result<Vec<S>>,
) -> Result<WorkloadClientOutcome> {
    if grant.authorize_collect(link, child_requested_by).is_err() {
        return Ok(collect_not_permitted());
    }
    let Some(mut child) = child else {
        return Ok(collect_not_permitted());
    };
    if ryeos_app::state_store::is_terminal_status(&child.status) {
        child.result = read_result()?.map(|mut record| {
            record.error = record.error.as_ref().map(redacted_failure);
            record
        });
    }
    if !child.result_within_cap()? {
        return Ok(WorkloadClientOutcome::Failed {
            code: ryeos_runtime::workload_client::WORKLOAD_COLLECT_RESULT_TOO_LARGE_CODE.to_owned(),
            message: "the child's result exceeds the per-call collect byte cap".to_owned(),
            retryable: false,
        });
    }
    if let Some(output) = child.result.as_ref() {
        let Ok(secrets) = node_secrets() else {
            return Ok(collect_result_withheld());
        };
        if ryeos_runtime::workload_client::value_contains_secret(
            &serde_json::to_value(output)?,
            &secrets,
        ) {
            return Ok(collect_result_withheld());
        }
    }
    Ok(WorkloadClientOutcome::Collected { child })
}

fn classify_workload_dispatch_error(error: &anyhow::Error) -> WorkloadClientOutcome {
    use ryeos_executor::dispatch_error::DispatchError;
    match error.downcast_ref::<DispatchError>() {
        Some(dispatch) => WorkloadClientOutcome::Failed {
            code: dispatch.code().to_owned(),
            message: ryeos_runtime::workload_client::bounded_error_message(&dispatch.to_string()),
            retryable: dispatch.retryable(),
        },
        None => WorkloadClientOutcome::Failed {
            code: "execution-failed".to_owned(),
            message: bounded_error(error),
            retryable: false,
        },
    }
}

fn validate_live_boot(
    state: &AppState,
    grant: &AdmittedWorkloadClientGrant,
    grant_digest: &str,
    callback_token: &str,
    thread_auth_token: &str,
) -> Result<()> {
    if state.node_policy.generation_digest() != grant.node_policy_generation_digest {
        bail!("workload-client node policy generation changed");
    }
    let callback = state
        .callback_tokens
        .validate_token_and_thread(callback_token, &grant.placement_thread_id)?;
    let retained_grant = callback
        .workload_client_grant
        .as_ref()
        .ok_or_else(|| anyhow!("workload-client callback lost its admitted grant"))?;
    if retained_grant.digest()? != grant_digest || retained_grant != grant {
        bail!("workload-client callback grant changed after boot");
    }
    let thread_auth = state
        .thread_auth
        .validate(thread_auth_token, &grant.placement_thread_id)?;
    if thread_auth.acting_principal != grant.owner_principal
        || thread_auth.caller_scopes != grant.effective_caps
    {
        bail!("workload-client thread authority changed after boot");
    }
    let retained_ingress = thread_auth
        .handler_context()
        .ok_or_else(|| anyhow!("workload-client thread authority lost retained ingress proof"))?;
    let worker = state
        .state_store
        .worker_process(&grant.worker_instance_id)?
        .ok_or_else(|| anyhow!("workload-client worker process disappeared"))?;
    if worker.state != WorkerProcessState::Live
        || worker.cleanup_state != "owned"
        || worker.daemon_generation_id != daemon_generation_id()
        || worker.placement_thread_id != grant.placement_thread_id
        || worker.boot_epoch != grant.worker_boot_epoch
        || worker.boot_identity_hash != grant.worker_boot_identity_hash
        || worker.session_capsule_hash != grant.session_capsule_hash
    {
        bail!("workload-client worker is not the exact live admitted boot");
    }
    let session = state
        .state_store
        .dedicated_session(&grant.placement_thread_id)?
        .ok_or_else(|| anyhow!("workload-client dedicated session disappeared"))?;
    if session.chain_root_id != grant.chain_root_id
        || session.admitted_capsule_hash != grant.session_capsule_hash
        || session.worker_instance_id.as_deref() != Some(grant.worker_instance_id.as_str())
        || session.worker_boot_epoch != Some(grant.worker_boot_epoch)
        || !matches!(
            session.state.as_str(),
            "idle" | "turn_running" | "awaiting_approval"
        )
    {
        bail!("workload-client session is not owned by the exact live worker boot");
    }
    let thread = state
        .threads
        .get_thread(&grant.placement_thread_id)?
        .ok_or_else(|| anyhow!("workload-client placement thread disappeared"))?;
    if thread.status != "running"
        || thread.chain_root_id != grant.chain_root_id
        || thread.requested_by.as_deref() != Some(grant.owner_principal.as_str())
        || thread.origin_site_id != grant.origin_site_id
        || thread.admitted_launch_capsule_hash.as_deref()
            != Some(grant.root_launch_capsule_hash.as_str())
        || state
            .state_store
            .current_chain_placement_thread_id(&grant.chain_root_id)?
            .as_deref()
            != Some(grant.placement_thread_id.as_str())
        || digest_value(
            thread
                .project_authority
                .as_ref()
                .ok_or_else(|| anyhow!("workload-client thread lost project authority"))?,
        )? != grant.project_authority_digest
    {
        bail!("workload-client placement is no longer the authoritative chain head");
    }
    retained_ingress.validate_execution_authority(
        &grant.owner_principal,
        &grant.effective_caps,
        &thread.current_site_id,
        &thread.origin_site_id,
    )?;
    let current_operator = ryeos_app::operator_authority::retained_admitted_operator_authority(
        state,
        &grant.owner_principal,
        &grant.origin_site_id,
    )?;
    if current_operator.grant_digest != grant.operator_grant_digest {
        bail!("workload-client operator grant generation changed");
    }
    current_operator.require_covers(&grant.effective_caps)?;
    Ok(())
}

fn bounded_error(error: &anyhow::Error) -> String {
    let normalized = format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    ryeos_runtime::workload_client::bounded_error_message(&normalized)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use ryeos_app::runtime_db::{
        DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession,
    };

    fn test_execution(
        item_ref: &str,
    ) -> ryeos_runtime::workload_client::WorkloadClientExecutionCeiling {
        ryeos_runtime::workload_client::WorkloadClientExecutionCeiling {
            item_ref: item_ref.to_owned(),
            ref_bindings: BTreeMap::new(),
            calls: vec![ryeos_runtime::workload_client::WorkloadClientCallCeiling::Default],
            effect_classes: vec!["live".to_owned()],
            workspace_access:
                ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            detached: false,
            collect: false,
        }
    }

    const SESSION: &str = "T-00000000-0000-4000-8000-0000000000aa";
    const CHILD: &str = "T-00000000-0000-4000-8000-0000000000c1";

    fn collect_grant(detached: bool, collect: bool) -> AdmittedWorkloadClientGrant {
        let mut execution = test_execution("tool:project/check");
        execution.detached = detached;
        execution.collect = collect;
        AdmittedWorkloadClientGrant {
            schema: AdmittedWorkloadClientGrant::SCHEMA,
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            chain_root_id: SESSION.to_owned(),
            placement_thread_id: SESSION.to_owned(),
            owner_principal: format!("fp:{}", "a".repeat(64)),
            origin_site_id: "site:source".to_owned(),
            worker_instance_id: "worker-1".to_owned(),
            worker_boot_epoch: 1,
            worker_boot_identity_hash: "b".repeat(64),
            root_launch_capsule_hash: "c".repeat(64),
            session_capsule_hash: "d".repeat(64),
            project_authority_digest: "e".repeat(64),
            request_digest: "f".repeat(64),
            caller_scope_digest: "1".repeat(64),
            operator_grant_digest: "2".repeat(64),
            root_delegation_digest: "3".repeat(64),
            node_policy_generation_digest: "4".repeat(64),
            ingresses: vec![ryeos_runtime::workload_client::WorkloadClientIngress::Cli],
            executions: vec![execution],
            execution_product_selections: BTreeMap::new(),
            execution_presentation: Value::Null,
            effective_caps: vec!["ryeos.execute.tool.project/check".to_owned()],
            max_in_flight: 2,
            max_invocations_per_boot: 8,
            max_lifetime_seconds: 300,
            max_request_bytes: 4096,
        }
    }

    fn child_row(status: &str) -> WorkloadClientCollectedChild {
        WorkloadClientCollectedChild {
            thread_id: CHILD.to_owned(),
            status: status.to_owned(),
            kind: "tool".to_owned(),
            item_ref: "tool:project/check".to_owned(),
            created_at: "2026-11-25T00:00:00Z".to_owned(),
            started_at: Some("2026-11-25T00:00:01Z".to_owned()),
            finished_at: None,
            result: None,
        }
    }

    fn recorded_result(value: Value) -> Result<Option<WorkloadClientCollectedResult>> {
        Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(value),
            error: None,
        }))
    }

    fn unreachable_result() -> Result<Option<WorkloadClientCollectedResult>> {
        panic!("a result is read only for a permitted, terminal child")
    }

    fn no_secrets() -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    #[test]
    fn collect_returns_status_for_a_running_child_and_the_result_once_terminal() {
        let grant = collect_grant(true, true);
        let owner = grant.owner_principal.clone();
        let link = Some((SESSION, "dispatch"));

        let running = collect_outcome(
            &grant,
            link,
            Some(owner.as_str()),
            Some(child_row("running")),
            unreachable_result,
            no_secrets,
        )
        .unwrap();
        let WorkloadClientOutcome::Collected { child } = running else {
            panic!("a running child is collected as status");
        };
        assert_eq!(child.status, "running");
        assert!(child.result.is_none());

        let done = collect_outcome(
            &grant,
            link,
            Some(owner.as_str()),
            Some(child_row("completed")),
            || recorded_result(json!({"answer": 42})),
            no_secrets,
        )
        .unwrap();
        let WorkloadClientOutcome::Collected { child } = done else {
            panic!("a terminal child is collected with its result");
        };
        assert_eq!(child.status, "completed");
        assert_eq!(child.result.unwrap().result, Some(json!({"answer": 42})));
        // A collected child is data, never an execution success.
        let frame = WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            outcome: WorkloadClientOutcome::Collected {
                child: child_row("running"),
            },
        };
        frame.validate().unwrap();
        assert!(!frame.outcome.succeeded());
    }

    #[test]
    fn every_refused_collect_is_the_same_outcome() {
        let grant = collect_grant(true, true);
        let owner = grant.owner_principal.clone();
        let row = || Some(child_row("completed"));
        let refused = |link: Option<(&str, &str)>,
                       requested_by: Option<&str>,
                       child: Option<WorkloadClientCollectedChild>| {
            collect_outcome(
                &grant,
                link,
                requested_by,
                child,
                unreachable_result,
                no_secrets,
            )
            .unwrap()
        };
        let outcomes = [
            // A thread id that does not exist.
            refused(None, None, None),
            // A thread that exists and is the owner's but this session was
            // never given.
            refused(None, Some(owner.as_str()), row()),
            // A sibling session's child.
            refused(
                Some(("T-00000000-0000-4000-8000-0000000000bb", "dispatch")),
                Some(owner.as_str()),
                row(),
            ),
            // A grandchild: its link names the child.
            refused(
                Some(("T-00000000-0000-4000-8000-0000000000c2", "dispatch")),
                Some(owner.as_str()),
                row(),
            ),
            // A direct child launched by a different key.
            refused(Some((SESSION, "dispatch")), Some("fp:someone-else"), row()),
            // A direct link under another relation.
            refused(Some((SESSION, "continuation")), Some(owner.as_str()), row()),
        ];
        for outcome in &outcomes {
            let WorkloadClientOutcome::Failed {
                code, retryable, ..
            } = outcome
            else {
                panic!("a refused collect is a typed failure");
            };
            assert_eq!(
                code,
                ryeos_runtime::workload_client::WORKLOAD_COLLECT_NOT_PERMITTED_CODE
            );
            assert!(!retryable);
            assert_eq!(outcome, &outcomes[0], "no existence oracle");
        }
        // The refusal is itself a valid wire answer.
        WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            outcome: outcomes[0].clone(),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn a_grant_that_does_not_allow_collect_refuses_it_identically() {
        let owner = collect_grant(true, true).owner_principal;
        let attempt = |grant: &AdmittedWorkloadClientGrant| {
            collect_outcome(
                grant,
                Some((SESSION, "dispatch")),
                Some(owner.as_str()),
                Some(child_row("completed")),
                unreachable_result,
                no_secrets,
            )
            .unwrap()
        };
        // The permitted case reads the terminal child's result; the refusals
        // below must never reach that read.
        let permitted = collect_outcome(
            &collect_grant(true, true),
            Some((SESSION, "dispatch")),
            Some(owner.as_str()),
            Some(child_row("completed")),
            || Ok(None),
            no_secrets,
        )
        .unwrap();
        assert!(matches!(permitted, WorkloadClientOutcome::Collected { .. }));
        let no_collect = attempt(&collect_grant(true, false));
        let no_detached = attempt(&collect_grant(false, false));
        let collect_without_detached = attempt(&collect_grant(false, true));
        for outcome in [&no_collect, &no_detached, &collect_without_detached] {
            let WorkloadClientOutcome::Failed { code, .. } = outcome else {
                panic!("a grant without collect refuses it");
            };
            assert_eq!(
                code,
                ryeos_runtime::workload_client::WORKLOAD_COLLECT_NOT_PERMITTED_CODE
            );
            assert_eq!(outcome, &no_collect);
        }
    }

    #[test]
    fn a_result_over_the_cap_is_refused_typed_and_not_cut() {
        let grant = collect_grant(true, true);
        let owner = grant.owner_principal.clone();
        let outcome = collect_outcome(
            &grant,
            Some((SESSION, "dispatch")),
            Some(owner.as_str()),
            Some(child_row("completed")),
            || {
                recorded_result(json!("x".repeat(
                    ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_COLLECT_RESULT_BYTES
                )))
            },
            no_secrets,
        )
        .unwrap();
        let WorkloadClientOutcome::Failed {
            code, retryable, ..
        } = &outcome
        else {
            panic!("an oversize result is refused");
        };
        assert_eq!(
            code,
            ryeos_runtime::workload_client::WORKLOAD_COLLECT_RESULT_TOO_LARGE_CODE
        );
        assert!(!retryable);
    }

    #[test]
    fn a_collected_child_is_a_fixed_allow_list_of_the_thread_row() {
        let value = serde_json::to_value(child_row("completed")).unwrap();
        let mut keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        // No project path, authority, capsule, lineage, requester or runtime
        // detail crosses the worker boundary.
        assert_eq!(
            keys,
            [
                "created_at",
                "finished_at",
                "item_ref",
                "kind",
                "started_at",
                "status",
                "thread_id"
            ]
        );
    }

    #[test]
    fn collect_adds_no_callback_method_and_detach_stays_unreachable() {
        // Collect is answered by the broker from its own records. The session's
        // callback surface is still exactly the one dispatch method, and the
        // production inline surface still refuses a detached action.
        let source = include_str!("workload_client.rs");
        let (production, _tests) = source
            .split_once("#[cfg(test)]")
            .expect("module has a test section");
        assert_eq!(
            production
                .matches("CallbackRuntimeMethodSurface::exact(vec![")
                .count(),
            1
        );
        assert!(production.contains("thread: \"inline\".to_owned()"));
        assert!(!production.contains("thread: \"detached\""));
        assert!(!production.contains("authorize_detached_ceiling"));
        let grant = collect_grant(true, true);
        let mut action = ryeos_runtime::callback::ActionPayload {
            product_selections: Default::default(),
            operation_id: None,
            item_id: "tool:project/check".to_owned(),
            ref_bindings: BTreeMap::new(),
            params: json!({}),
            thread: "detached".to_owned(),
            call: None,
            facets: None,
            launch_window: None,
        };
        assert!(grant.authorize_action(&action).is_err());
        action.thread = "inline".to_owned();
        assert!(grant.authorize_action(&action).is_ok());
    }

    #[test]
    fn launch_selections_are_normalized_for_only_the_exact_admitted_child() {
        use ryeos_state::external_content::products::composition::{
            ProductSelection, ProductSelectionInput, ProductSelectionTarget,
        };
        let request = WorkloadClientRequestContract {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            bindings: vec![
                ryeos_runtime::workload_client::WorkloadClientBinding::StructuredSession {},
            ],
            executions: vec![test_execution("tool:project/check")],
            max_in_flight: 1,
            max_invocations_per_boot: 2,
            max_lifetime_seconds: 60,
        };
        let selected = ProductSelection {
            declaration_id: "platform".to_owned(),
            witness_hash: "a".repeat(64),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: Some("b".repeat(64)),
            remote_testimony: None,
        };
        let inputs = vec![ProductSelectionInput {
            target: ProductSelectionTarget::WorkloadExecution {
                item_ref: "tool:project/check".to_owned(),
            },
            selection: selected.clone(),
        }];
        let admitted = ryeos_runtime::workload_client::admitted_execution_product_selections(
            &inputs,
            Some(&request),
        )
        .unwrap();
        assert_eq!(
            admitted["tool:project/check"],
            vec![ProductSelectionInput {
                target: ProductSelectionTarget::Root {},
                selection: selected.clone(),
            }]
        );

        let outside = vec![ProductSelectionInput {
            target: ProductSelectionTarget::WorkloadExecution {
                item_ref: "tool:project/other".to_owned(),
            },
            selection: selected,
        }];
        assert!(
            ryeos_runtime::workload_client::admitted_execution_product_selections(
                &outside,
                Some(&request)
            )
            .is_err()
        );
    }

    #[test]
    fn workload_client_requires_the_real_pending_session_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let db = ryeos_app::test_support::TestRuntime::open(&temp.path().join("runtime.sqlite3"));
        db.create_credential_profile(NewCredentialProfile {
            activation: ryeos_app::runtime_db::CredentialActivation::Ceremony,
            profile_id: "P-test",
            owner_principal: "fp:operator",
            home_id: "home-test",
        })
        .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: "T-test",
            chain_root_id: "T-test",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-test",
            candidate_required: false,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "P-test",
            credential_generation: 1,
            credential_lock_owner: "worker-test",
        })
        .unwrap();
        let session = db.dedicated_session("T-test").unwrap().unwrap();
        let identity = ExclusivePersistentSessionIdentity {
            placement_thread_id: "T-test".to_owned(),
            worker_instance_id: "worker-test".to_owned(),
            boot_identity_hash: "b".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            control_channel_identity: "channel-test".to_owned(),
            accounting_scope: None,
        };
        // The production transaction supplies the tuple before worker_process
        // exists. Empty fields cannot stand in for this pending authority.
        assert!(db.worker_process("worker-test").unwrap().is_none());
        require_reserved_boot(&session, &identity).unwrap();
        let mut missing = session.clone();
        missing.worker_instance_id = None;
        missing.worker_boot_epoch = None;
        assert!(require_reserved_boot(&missing, &identity).is_err());
        for field in ["placement", "worker", "epoch", "generation"] {
            let mut wrong = identity.clone();
            match field {
                "placement" => wrong.placement_thread_id = "T-other".to_owned(),
                "worker" => wrong.worker_instance_id = "worker-other".to_owned(),
                "epoch" => wrong.boot_epoch += 1,
                "generation" => wrong.lifecycle_generation += 1,
                _ => unreachable!(),
            }
            assert!(require_reserved_boot(&session, &wrong).is_err(), "{field}");
        }
        let mut advanced = session.clone();
        advanced.state = "binding".to_owned();
        assert!(require_reserved_boot(&advanced, &identity).is_err());
        let mut contacting = session.clone();
        contacting.send_boundary = "contacting".to_owned();
        assert!(require_reserved_boot(&contacting, &identity).is_err());
        let mut successor = session;
        let mut next = identity;
        successor.worker_instance_id = Some("worker-recovered".to_owned());
        successor.worker_boot_epoch = Some(2);
        assert!(require_reserved_boot(&successor, &next).is_err());
        next.worker_instance_id = "worker-recovered".to_owned();
        next.boot_epoch = 2;
        require_reserved_boot(&successor, &next).unwrap();
    }

    // ── Collect: error shaping, the secret floor, and the session chain ─────

    fn collect_with(
        child: WorkloadClientCollectedChild,
        record: Result<Option<WorkloadClientCollectedResult>>,
        secrets: Result<Vec<String>>,
    ) -> WorkloadClientOutcome {
        let grant = collect_grant(true, true);
        let owner = grant.owner_principal.clone();
        collect_outcome(
            &grant,
            Some((SESSION, "dispatch")),
            Some(owner.as_str()),
            Some(child),
            || record,
            || secrets,
        )
        .unwrap()
    }

    fn collected_result(outcome: WorkloadClientOutcome) -> WorkloadClientCollectedResult {
        let WorkloadClientOutcome::Collected { child } = outcome else {
            panic!("the collect was permitted and returned");
        };
        child.result.expect("a terminal child carries its result")
    }

    fn assert_withheld(outcome: &WorkloadClientOutcome) {
        let WorkloadClientOutcome::Failed {
            code, retryable, ..
        } = outcome
        else {
            panic!("the output is withheld as a typed failure");
        };
        assert_eq!(
            code,
            ryeos_runtime::workload_client::WORKLOAD_COLLECT_RESULT_WITHHELD_CODE
        );
        assert!(!retryable);
    }

    #[test]
    fn a_collected_error_is_a_code_and_a_fixed_sentence_never_the_nodes_text() {
        let raw = "failed to open /home/operator/.local/share/node/secrets.db: permission denied";
        let record = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("failed".to_owned()),
            result: None,
            error: Some(json!({"code": "ENOENT", "message": raw, "path": "/home/operator/x"})),
        }));
        let outcome = collect_with(child_row("failed"), record, no_secrets());
        let result = collected_result(outcome.clone());
        let error = result.error.expect("an error is still reported");
        assert_eq!(
            error,
            json!({"code": "child_error", "message": COLLECT_ERROR_MESSAGE})
        );
        let wire = serde_json::to_string(&outcome).unwrap();
        assert!(!wire.contains("/home/operator"), "{wire}");
        assert!(!wire.contains("permission denied"), "{wire}");
        assert!(!wire.contains("ENOENT"), "{wire}");
        // A daemon-authored stop reason keeps its own code.
        let stopped = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("cancelled_by_parent_end".to_owned()),
            result: None,
            error: Some(json!({"reason": "parent_session_ended", "intent": "cancel"})),
        }));
        let error = collected_result(collect_with(child_row("cancelled"), stopped, no_secrets()))
            .error
            .unwrap();
        assert_eq!(
            error,
            json!({"code": "parent_session_ended", "message": COLLECT_ERROR_MESSAGE})
        );
        // No recorded error stays absent.
        let clean = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(json!({"answer": 42})),
            error: None,
        }));
        assert_eq!(
            collected_result(collect_with(child_row("completed"), clean, no_secrets())).error,
            None
        );
    }

    #[test]
    fn a_result_holding_a_node_secret_is_withheld_typed_and_one_without_is_returned() {
        let secret = "provider-key-0123456789".to_owned();
        let leaking = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(json!({"log": format!("auth header was {secret}")})),
            error: None,
        }));
        let withheld = collect_with(child_row("completed"), leaking, Ok(vec![secret.clone()]));
        assert_withheld(&withheld);
        // The refusal itself carries nothing of the output.
        assert!(
            !serde_json::to_string(&withheld)
                .unwrap()
                .contains("auth header")
        );
        WorkloadClientResponseFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "collect-1".to_owned(),
            outcome: withheld,
        }
        .validate()
        .unwrap();

        let clean = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(json!({"answer": 42})),
            error: None,
        }));
        let returned = collected_result(collect_with(
            child_row("completed"),
            clean,
            Ok(vec![secret.clone()]),
        ));
        assert_eq!(returned.result, Some(json!({"answer": 42})));
    }

    #[test]
    fn the_secret_floor_has_a_minimum_length_and_matches_the_escaped_form() {
        let record = |text: &str| {
            Ok(Some(WorkloadClientCollectedResult {
                outcome_code: Some("ok".to_owned()),
                result: Some(json!({"log": text})),
                error: None,
            }))
        };
        // A value below the minimum length is not checked.
        let short = "abc1234".to_owned();
        assert!(matches!(
            collect_with(
                child_row("completed"),
                record("contains abc1234 here"),
                Ok(vec![short])
            ),
            WorkloadClientOutcome::Collected { .. }
        ));
        // A secret with a quote in it, printed as an already-serialized
        // document, is found in its escaped form.
        let quoted = "pa\"ss-word-0123".to_owned();
        let printed = serde_json::to_string(&quoted).unwrap();
        assert_withheld(&collect_with(
            child_row("completed"),
            record(&printed),
            Ok(vec![quoted]),
        ));
        // A non-ASCII secret printed in its \uXXXX form is found.
        let accented = "clé-secrète-été".to_owned();
        assert_withheld(&collect_with(
            child_row("completed"),
            record("cl\\u00e9-secr\\u00e8te-\\u00e9t\\u00e9"),
            Ok(vec![accented]),
        ));
    }

    #[test]
    fn output_is_withheld_when_the_nodes_secrets_cannot_be_enumerated() {
        let record = Ok(Some(WorkloadClientCollectedResult {
            outcome_code: Some("ok".to_owned()),
            result: Some(json!({"answer": 42})),
            error: None,
        }));
        assert_withheld(&collect_with(
            child_row("completed"),
            record,
            Err(anyhow!("sealed store unreadable")),
        ));
        // A running child has no output, so no secret is ever read for it.
        let grant = collect_grant(true, true);
        let owner = grant.owner_principal.clone();
        let running = collect_outcome(
            &grant,
            Some((SESSION, "dispatch")),
            Some(owner.as_str()),
            Some(child_row("running")),
            unreachable_result,
            || -> Result<Vec<String>> { panic!("no output to check") },
        )
        .unwrap();
        assert!(matches!(running, WorkloadClientOutcome::Collected { .. }));
    }

    fn session_thread(thread_id: &str) -> ryeos_app::state_store::NewThreadRecord {
        let hash = "a".repeat(64);
        ryeos_app::state_store::NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: format!("directive:test/{thread_id}"),
            executor_ref: "native:test".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: format!("directive:test/{thread_id}"),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        }
        owner: None,
    }

    /// Real: the StateStore with its runtime and state databases, the child
    /// links, thread rows and continuation chain, the broker's link-fact
    /// resolution and the grant's authorization. Substituted: the live boot
    /// validation and the wire frame, which need a hosted worker.
    #[test]
    fn a_child_of_an_earlier_segment_is_collected_from_the_successor_and_no_other_is() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).expect("compose");
        let store = &state.state_store;
        let owner = collect_grant(true, true).owner_principal;
        let make_running = |id: &str| {
            store.create_thread_for_test(&session_thread(id)).unwrap();
            store.mark_thread_running(id, None).unwrap();
        };
        // The session: segment 1 launches a child, then continues as segment 2.
        make_running("T-seg-1");
        make_running("T-seg-1-child");
        store
            .record_child_link("T-seg-1", "T-seg-1-child", "dispatch")
            .unwrap();
        let mut successor = session_thread("T-seg-2");
        successor.chain_root_id = "T-seg-1".to_string();
        successor.upstream_thread_id = Some("T-seg-1".to_string());
        successor.captured_history_policy = None;
        store
            .create_continuation_for_test(&successor, "T-seg-1", "T-seg-1", Some("continue"))
            .unwrap();
        // The child's own child, and another session with its own child.
        make_running("T-seg-1-grandchild");
        store
            .record_child_link("T-seg-1-child", "T-seg-1-grandchild", "dispatch")
            .unwrap();
        make_running("T-other");
        make_running("T-other-child");
        store
            .record_child_link("T-other", "T-other-child", "dispatch")
            .unwrap();

        // The grant is the successor's: placement is segment 2, chain root the
        // session's.
        let mut grant = collect_grant(true, true);
        grant.chain_root_id = "T-seg-1".to_owned();
        grant.placement_thread_id = "T-seg-2".to_owned();
        let attempt = |child: &str| {
            let facts = collect_link_facts(&state, child).unwrap();
            collect_outcome(
                &grant,
                facts
                    .as_ref()
                    .map(|(chain, relation)| (chain.as_str(), relation.as_str())),
                Some(owner.as_str()),
                Some(child_row("completed")),
                || recorded_result(json!({"answer": 42})),
                no_secrets,
            )
            .unwrap()
        };
        assert!(
            matches!(
                attempt("T-seg-1-child"),
                WorkloadClientOutcome::Collected { .. }
            ),
            "the successor collects what the earlier segment launched"
        );
        let unknown = attempt("T-00000000-0000-4000-8000-0000000000ff");
        assert!(matches!(unknown, WorkloadClientOutcome::Failed { .. }));
        for refused in ["T-other-child", "T-seg-1-grandchild"] {
            assert_eq!(attempt(refused), unknown, "{refused}: no oracle");
        }
    }
}
