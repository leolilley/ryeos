//! Admission and recovery checks for kind-neutral execution realizations.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use ryeos_app::state::AppState;
use ryeos_engine::isolation::{
    IsolationFilesystemAuthorityCeiling, IsolationNetworkAuthorityCeiling,
};
use ryeos_state::objects::{
    ADMITTED_EXECUTION_REALIZATION_KIND, AdmittedExecutionRealization,
    EXECUTION_REALIZATION_SCHEMA_VERSION, ExecutionComponentReference, ExecutionComponentStorage,
};

pub(crate) struct ExecutionRealizationAdmission {
    pub(crate) hash: String,
    pub(crate) launch_authority_digest: String,
    pub(crate) publication: Option<ryeos_state::PendingCasPublication>,
}

/// Compile the signed subject projections and narrow them by the existing
/// parent launch authority. Used by item-owned subprocess routes that do not
/// use the ordinary executor-chain plan builder.
pub(crate) fn project_launch_isolation_ceilings(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: Option<&ryeos_engine::resolution::ResolutionOutput>,
    parent_thread_id: Option<&str>,
) -> Result<(
    IsolationFilesystemAuthorityCeiling,
    IsolationNetworkAuthorityCeiling,
)> {
    let execution = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!("execution kind `{kind}` has no registered execution contract")
        })?;
    if resolution.is_none()
        && (execution.filesystem_authority_ceiling.is_some()
            || execution.network_authority_ceiling.is_some())
    {
        anyhow::bail!(
            "kind `{kind}` requires an admitted composed subject for isolation projection"
        );
    }
    let empty = serde_json::Value::Null;
    let composed = resolution
        .map(|resolution| &resolution.composed.composed)
        .unwrap_or(&empty);
    let mut filesystem = execution.project_filesystem_authority_ceiling(composed)?;
    let mut network = execution.project_network_authority_ceiling(composed)?;
    if let Some(parent) = parent_thread_id {
        let (parent_filesystem, parent_network) =
            admitted_parent_isolation_ceilings(state, parent)?;
        filesystem = filesystem.intersect(parent_filesystem);
        network = network.intersect(parent_network);
    }
    Ok((filesystem, network))
}

/// Parent restrictions are recovered from the existing immutable capsule and
/// realization. Neither a callback argument nor a mutable runtime projection
/// may nominate a replacement ceiling for a borrowed child.
pub(crate) fn admitted_parent_isolation_ceilings(
    state: &AppState,
    parent_thread_id: &str,
) -> Result<(
    IsolationFilesystemAuthorityCeiling,
    IsolationNetworkAuthorityCeiling,
)> {
    let capsule = state
        .state_store
        .admitted_launch_capsule(parent_thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "parent {parent_thread_id} has no admitted capsule for child isolation authority"
            )
        })?;
    let authority = state.state_store.pinned_state_authority()?;
    let realization = capsule.verify_retained_execution_realization(
        &authority.cas_store()?,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let filesystem = realization
        .properties
        .get(IsolationFilesystemAuthorityCeiling::REALIZATION_PROPERTY)
        .context("parent realization has no filesystem ceiling")?;
    let network = realization
        .properties
        .get(IsolationNetworkAuthorityCeiling::REALIZATION_PROPERTY)
        .context("parent realization has no network ceiling")?;
    Ok((
        serde_json::from_value(filesystem.clone()).context("decode parent filesystem ceiling")?,
        serde_json::from_value(network.clone()).context("decode parent network ceiling")?,
    ))
}

pub(crate) fn project_launch_resource_authority_ceiling(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: Option<&ryeos_engine::resolution::ResolutionOutput>,
    parent_thread_id: Option<&str>,
) -> Result<ryeos_engine::contracts::ExecutionResourceAuthorityCeiling> {
    let execution = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .ok_or_else(|| anyhow::anyhow!("execution kind `{kind}` has no registered contract"))?;
    if resolution.is_none() && execution.resource_authority_ceiling.is_some() {
        anyhow::bail!(
            "kind `{kind}` requires an admitted composed subject for resource-authority projection"
        );
    }
    let empty = serde_json::Value::Null;
    let composed = resolution
        .map(|resolution| &resolution.composed.composed)
        .unwrap_or(&empty);
    let mut ceiling = execution.project_resource_authority_ceiling(composed)?;
    if let Some(parent) = parent_thread_id {
        ceiling = ceiling.intersect(admitted_parent_resource_authority_ceiling(state, parent)?);
    }
    let target = execution.project_target_requirement(composed)?;
    ceiling.admits(target.as_ref())?;
    Ok(ceiling)
}

pub(crate) fn admitted_parent_resource_authority_ceiling(
    state: &AppState,
    parent_thread_id: &str,
) -> Result<ryeos_engine::contracts::ExecutionResourceAuthorityCeiling> {
    let capsule = state
        .state_store
        .admitted_launch_capsule(parent_thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "parent {parent_thread_id} has no admitted capsule for child resource authority"
            )
        })?;
    let authority = state.state_store.pinned_state_authority()?;
    let realization = capsule.verify_retained_execution_realization(
        &authority.cas_store()?,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let value = realization
        .properties
        .get(ryeos_engine::contracts::ExecutionResourceAuthorityCeiling::REALIZATION_PROPERTY)
        .ok_or_else(|| anyhow::anyhow!("parent realization has no resource-authority ceiling"))?;
    Ok(serde_json::from_value(value.clone())?)
}

pub(crate) fn admit_or_verify(
    state: &AppState,
    metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
    selected_resources: &[ryeos_engine::contracts::ExecutionResourceSelection],
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    let launch_authority = metadata
        .admitted_launch_authority()?
        .ok_or_else(|| anyhow::anyhow!("subprocess launch has no admitted launch authority"))?;
    let components = execution_components(state, resolution)?;
    let launch_authority_digest = launch_authority.digest()?;
    let artifact_identity_digest = launch_authority.artifact_identity_digest()?;
    let execution_closure_digest = launch_authority.execution_closure_digest()?;
    let properties = match metadata
        .admitted_execution_closure
        .as_ref()
        .context("execution realization has no admitted closure")?
    {
        ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            ..
        } => {
            let plan: ryeos_engine::contracts::ExecutionPlan =
                serde_json::from_value(execution_plan.clone())
                    .context("decode execution-realization admitted direct plan")?;
            direct_execution_properties(
                state.isolation.inspection(),
                state.isolation.is_enforced(),
                &plan,
                selected_resources,
            )?
        }
        ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
            prepared_runtime_launch,
            ..
        } => {
            let prepared: super::launch_preparation::PreparedRuntimeLaunch =
                serde_json::from_value(prepared_runtime_launch.clone())
                    .context("decode execution-realization admitted managed launch")?;
            execution_properties(
                state,
                prepared.filesystem_authority_ceiling,
                prepared.network_authority_ceiling,
                prepared.target_requirement.as_ref(),
                prepared.resource_authority_ceiling,
                selected_resources,
            )?
        }
    };

    if let Some(existing_hash) = metadata.execution_realization_hash.as_deref() {
        let existing = load_realization(state, existing_hash)?;
        if existing.launch_authority_digest != launch_authority_digest
            || existing.effective_definition_digest != effective_definition_digest
            || existing.artifact_identity_digest != artifact_identity_digest
            || existing.execution_closure_digest != execution_closure_digest
            || existing.contract_ref != contract_ref
            || existing.contract_digest != contract_digest
            || existing.components != components
            || existing.properties != properties
        {
            anyhow::bail!(
                "recovered execution realization {existing_hash} contradicts the admitted launch"
            );
        }
        verify_realization_node_evidence(state, &existing)?;
        verify_realization_components(state, &existing)?;
        return Ok(ExecutionRealizationAdmission {
            hash: existing_hash.to_owned(),
            launch_authority_digest,
            publication: None,
        });
    }

    store_new_realization(
        state,
        launch_authority_digest,
        effective_definition_digest,
        artifact_identity_digest,
        execution_closure_digest,
        contract_ref,
        contract_digest,
        components,
        properties,
        staged_publication,
    )
}

pub(crate) fn admit_persistent_session(
    state: &AppState,
    authority: &ryeos_state::objects::PersistentSessionAuthority,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    authority.validate()?;
    let (filesystem, network, target, resource_authority) =
        persistent_session_execution_authority(&authority.execution_closure)?;
    let selected_resources = state.execution_resources.select(target.as_ref())?;
    let properties = execution_properties(
        state,
        filesystem,
        network,
        target.as_ref(),
        resource_authority,
        selected_resources.selections(),
    )?;
    store_new_realization(
        state,
        authority.digest()?,
        effective_definition_digest,
        authority.artifact_identity_digest()?,
        authority.execution_closure_digest()?,
        contract_ref,
        contract_digest,
        execution_components(state, resolution)?,
        properties,
        staged_publication,
    )
}

pub(crate) fn verify_persistent_session(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
) -> Result<()> {
    capsule.validate()?;
    let authority = capsule.authority();
    let existing = load_realization(state, &capsule.execution_realization_hash)?;
    let (filesystem, network, target, resource_authority) =
        persistent_session_execution_authority(&authority.execution_closure)?;
    let selected_resources = state.execution_resources.select(target.as_ref())?;
    let properties = execution_properties(
        state,
        filesystem,
        network,
        target.as_ref(),
        resource_authority,
        selected_resources.selections(),
    )?;
    if existing.launch_authority_digest != authority.digest()?
        || existing.effective_definition_digest != effective_definition_digest
        || existing.artifact_identity_digest != authority.artifact_identity_digest()?
        || existing.execution_closure_digest != authority.execution_closure_digest()?
        || existing.contract_ref != contract_ref
        || existing.contract_digest != contract_digest
        || existing.components != execution_components(state, resolution)?
        || existing.properties != properties
    {
        anyhow::bail!("persistent-session execution realization contradicts its admitted capsule");
    }
    verify_realization_node_evidence(state, &existing)?;
    verify_realization_components(state, &existing)
}

fn persistent_session_execution_authority(
    closure: &ryeos_state::objects::AdmittedExecutionClosure,
) -> Result<(
    IsolationFilesystemAuthorityCeiling,
    IsolationNetworkAuthorityCeiling,
    Option<ryeos_engine::contracts::ExecutionTargetRequirement>,
    ryeos_engine::contracts::ExecutionResourceAuthorityCeiling,
)> {
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        execution_plan, ..
    } = closure
    else {
        anyhow::bail!("persistent session requires an admitted direct execution plan");
    };
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())
            .context("decode persistent-session retained isolation ceilings")?;
    // This owner is the existing controller-local persistent process. An
    // external candidate workload is not permission to relocate that process.
    plan.require_local_endpoint_for_dispatch()?;
    Ok((
        plan.filesystem_authority_ceiling,
        plan.network_authority_ceiling,
        plan.target_requirement,
        plan.resource_authority_ceiling,
    ))
}

/// Rebind an exact retained execution realization to the one authority field
/// a continuation is allowed to advance: its admitted project generation.
/// Every other launch-authority component remains byte-identical to the
/// source segment, and the retained substrate/components are copied from the
/// already-verified source realization rather than re-resolved.
pub(crate) fn transition_continuation_project_authority(
    state: &AppState,
    source: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    successor: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
) -> Result<ExecutionRealizationAdmission> {
    let source_capsule = source
        .admitted_launch_capsule()?
        .ok_or_else(|| anyhow::anyhow!("continuation source has no admitted launch capsule"))?;
    let successor_authority = successor.admitted_launch_authority()?.ok_or_else(|| {
        anyhow::anyhow!("continuation successor has no admitted launch authority")
    })?;
    let mut permitted = source_capsule.launch_authority();
    permitted.project_authority = successor_authority.project_authority.clone();
    if permitted != successor_authority {
        anyhow::bail!(
            "continuation changed launch authority outside its admitted project generation"
        );
    }

    let authority = state
        .state_store
        .with_state_db(|database| database.pinned_authority())?;
    let source_realization = source_capsule.verify_retained_execution_realization(
        &authority.cas_store()?,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let successor_digest = successor_authority.digest()?;
    if source_realization.launch_authority_digest == successor_digest {
        return Ok(ExecutionRealizationAdmission {
            hash: source_capsule.execution_realization_hash,
            launch_authority_digest: successor_digest,
            publication: None,
        });
    }

    let mut candidate = source_realization;
    candidate.launch_authority_digest = successor_digest.clone();
    store_realization_candidate(state, candidate, None)
}

#[allow(clippy::too_many_arguments)]
fn store_new_realization(
    state: &AppState,
    launch_authority_digest: String,
    effective_definition_digest: &str,
    artifact_identity_digest: String,
    execution_closure_digest: String,
    contract_ref: &str,
    contract_digest: &str,
    components: Vec<ExecutionComponentReference>,
    properties: BTreeMap<String, serde_json::Value>,
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    let node = state
        .extensions
        .get::<ryeos_app::execution_identity_probe::NodeExecutionIdentity>()
        .ok_or_else(|| anyhow::anyhow!("node execution substrate evidence is unavailable"))?;
    verify_node_evidence(state, &node)?;
    let candidate = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
        substrate_identity_hash: node.identity_hash.clone(),
        substrate_attestation_hash: node.attestation_hash.clone(),
        launch_authority_digest: launch_authority_digest.clone(),
        effective_definition_digest: effective_definition_digest.to_owned(),
        artifact_identity_digest,
        execution_closure_digest,
        contract_ref: contract_ref.to_owned(),
        contract_digest: contract_digest.to_owned(),
        components,
        properties,
    };
    store_realization_candidate(state, candidate, staged_publication)
}

fn store_realization_candidate(
    state: &AppState,
    candidate: AdmittedExecutionRealization,
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    candidate.validate()?;
    verify_realization_node_evidence(state, &candidate)?;
    verify_realization_components(state, &candidate)?;
    let launch_authority_digest = candidate.launch_authority_digest.clone();
    let expected = candidate.content_hash()?;
    let value = candidate.to_value()?;
    let (stored, publication) = match staged_publication {
        Some(publication) => {
            let guard = publication.authority().acquire_shared_guard()?;
            publication.authority().ensure_guard(&guard)?;
            let _permit = state
                .write_barrier
                .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
                .map_err(|error| {
                    anyhow::anyhow!("cannot acquire realization write permit: {error}")
                })?;
            let cas = publication.authority().cas_store()?;
            let stored = publication
                .staged_roots_mut()
                .store_object_admitted(&guard, &cas, &value)
                .context("store admitted execution realization in existing stage")?;
            (stored, None)
        }
        None => {
            let authority = state.state_store.pinned_state_authority()?;
            let guard = authority.acquire_shared_guard()?;
            authority.ensure_guard(&guard)?;
            let _permit = state
                .write_barrier
                .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
                .map_err(|error| {
                    anyhow::anyhow!("cannot acquire realization write permit: {error}")
                })?;
            let cas = authority.cas_store()?;
            let mut staged = authority
                .require_recovery()?
                .begin_staged_cas_roots_admitted(&guard, "execution-realization")?;
            let stored = staged
                .store_object_admitted(&guard, &cas, &value)
                .context("store admitted execution realization")?;
            (
                stored,
                Some(ryeos_state::PendingCasPublication::new(authority, staged)),
            )
        }
    };
    if stored != expected {
        anyhow::bail!(
            "admitted execution realization CAS hash mismatch: expected {expected}, stored {stored}"
        );
    }
    Ok(ExecutionRealizationAdmission {
        hash: stored,
        launch_authority_digest,
        publication,
    })
}

fn execution_properties(
    state: &AppState,
    filesystem_authority_ceiling: ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling,
    network_authority_ceiling: ryeos_engine::isolation::IsolationNetworkAuthorityCeiling,
    target_requirement: Option<&ryeos_engine::contracts::ExecutionTargetRequirement>,
    resource_authority_ceiling: ryeos_engine::contracts::ExecutionResourceAuthorityCeiling,
    selected_resources: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let inspection = state.isolation.inspection();
    execution_properties_from_inspection(
        inspection,
        state.isolation.is_enforced(),
        filesystem_authority_ceiling,
        network_authority_ceiling,
        target_requirement,
        resource_authority_ceiling,
        selected_resources,
    )
}

/// Select evidence from the already sealed ordinary plan. External endpoint
/// properties are admitted requirements and controller provenance ONLY. They
/// are not guest isolation qualification, target readiness, or permission to
/// contact a placement provider. Those observations retain their own owners.
fn direct_execution_properties(
    controller_inspection: &ryeos_engine::isolation::IsolationInspection,
    controller_isolation_enforced: bool,
    plan: &ryeos_engine::contracts::ExecutionPlan,
    selected_resources: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<BTreeMap<String, serde_json::Value>> {
    plan.validate_endpoint_for_sealing()?;
    match &plan.endpoint_requirement {
        ryeos_engine::contracts::ExecutionEndpointRequirement::Local {} => {
            execution_properties_from_inspection(
                controller_inspection,
                controller_isolation_enforced,
                plan.filesystem_authority_ceiling,
                plan.network_authority_ceiling,
                plan.target_requirement.as_ref(),
                plan.resource_authority_ceiling,
                selected_resources,
            )
        }
        ryeos_engine::contracts::ExecutionEndpointRequirement::External { .. } => {
            anyhow::ensure!(
                selected_resources.is_empty(),
                "external endpoint cannot claim controller-local resource selections"
            );
            let binding = plan
                .external_endpoint_binding
                .as_ref()
                .context("sealed external endpoint lost its binding identity")?;
            let mut properties = execution_authority_properties(
                plan.filesystem_authority_ceiling,
                plan.network_authority_ceiling,
                plan.target_requirement.as_ref(),
                plan.resource_authority_ceiling,
                selected_resources,
            )?;
            // The realization's existing substrate identity/attestation still
            // names the admitting controller, never the as-yet-unobserved guest.
            properties.insert("execution_substrate_role".into(), "controller".into());
            properties.insert(
                "execution_endpoint_requirement".into(),
                serde_json::Value::String(lillux::canonical_json(&serde_json::to_value(
                    &plan.endpoint_requirement,
                )?)?),
            );
            properties.insert(
                "external_endpoint_binding".into(),
                serde_json::Value::String(lillux::canonical_json(&serde_json::to_value(binding)?)?),
            );
            // Deliberately no isolation_enforced/policy/backend observation:
            // a controller observation cannot qualify the external target.
            Ok(properties)
        }
    }
}

fn execution_properties_from_inspection(
    inspection: &ryeos_engine::isolation::IsolationInspection,
    isolation_enforced: bool,
    filesystem_authority_ceiling: IsolationFilesystemAuthorityCeiling,
    network_authority_ceiling: IsolationNetworkAuthorityCeiling,
    target_requirement: Option<&ryeos_engine::contracts::ExecutionTargetRequirement>,
    resource_authority_ceiling: ryeos_engine::contracts::ExecutionResourceAuthorityCeiling,
    selected_resources: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut properties = execution_authority_properties(
        filesystem_authority_ceiling,
        network_authority_ceiling,
        target_requirement,
        resource_authority_ceiling,
        selected_resources,
    )?;
    properties.insert(
        "isolation_enforced".to_owned(),
        serde_json::Value::Bool(isolation_enforced),
    );
    properties.insert(
        "isolation_policy_digest".to_owned(),
        inspection
            .digest
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );
    properties.insert(
        "isolation_backend_inspection_digest".to_owned(),
        serde_json::Value::String(isolation_backend_inspection_digest(&inspection.backend)?),
    );
    Ok(properties)
}

/// Parent/workload ceilings and requested suitability are requirements, not
/// evidence of a particular host's enforcement. Preserve their existing keys
/// so continuation and borrowed-child narrowing keep the same authoritative
/// read path for either endpoint.
fn execution_authority_properties(
    filesystem_authority_ceiling: IsolationFilesystemAuthorityCeiling,
    network_authority_ceiling: IsolationNetworkAuthorityCeiling,
    target_requirement: Option<&ryeos_engine::contracts::ExecutionTargetRequirement>,
    resource_authority_ceiling: ryeos_engine::contracts::ExecutionResourceAuthorityCeiling,
    selected_resources: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut properties =
        authority_ceiling_properties(filesystem_authority_ceiling, network_authority_ceiling);
    properties.insert(
        ryeos_engine::contracts::ExecutionResourceAuthorityCeiling::REALIZATION_PROPERTY.to_owned(),
        serde_json::to_value(resource_authority_ceiling)?,
    );
    properties.insert(
        ryeos_engine::contracts::ExecutionTargetRequirement::REALIZATION_PROPERTY.to_owned(),
        target_requirement
            .map(|target| {
                lillux::canonical_json(&serde_json::to_value(target)?).map_err(anyhow::Error::from)
            })
            .transpose()?
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );
    properties.insert(
        ryeos_engine::contracts::ExecutionResourceSelection::REALIZATION_PROPERTY.to_owned(),
        resource_selection_property(selected_resources)?,
    );
    Ok(properties)
}

// Keep structured evidence in the owning execution contract, using the same
// canonical scalar encoding as the target requirement. Preserve list order:
// admission and recovery must compare the exact selected resource generation.
fn resource_selection_property(
    selections: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<serde_json::Value> {
    for selection in selections {
        selection.validate()?;
    }
    Ok(serde_json::Value::String(lillux::canonical_json(
        &serde_json::to_value(selections)?,
    )?))
}

fn authority_ceiling_properties(
    filesystem: ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling,
    network: ryeos_engine::isolation::IsolationNetworkAuthorityCeiling,
) -> BTreeMap<String, serde_json::Value> {
    let filesystem = match filesystem {
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy => "node_policy",
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution => {
            "captured_execution"
        }
    };
    let network = match network {
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy => "node_policy",
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::Isolated => "isolated",
    };
    [
        (
            IsolationFilesystemAuthorityCeiling::REALIZATION_PROPERTY.to_owned(),
            serde_json::Value::String(filesystem.to_owned()),
        ),
        (
            IsolationNetworkAuthorityCeiling::REALIZATION_PROPERTY.to_owned(),
            serde_json::Value::String(network.to_owned()),
        ),
    ]
    .into_iter()
    .collect()
}

/// Path-free identity of the complete backend observation that can affect a
/// launch. A declaration/adapter-only identity is insufficient: replacing an
/// inspected launcher payload must move the execution realization too.
fn isolation_backend_inspection_digest(
    inspection: &ryeos_engine::isolation::IsolationBackendInspection,
) -> Result<String> {
    let value = serde_json::to_value(inspection)?;
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&value)?.as_bytes(),
    ))
}

fn verify_realization_components(
    state: &AppState,
    realization: &AdmittedExecutionRealization,
) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    realization
        .verify_retained_components(&authority.cas_store()?, &authority.large_object_store()?)
}

fn load_realization(state: &AppState, hash: &str) -> Result<AdmittedExecutionRealization> {
    let authority = state.state_store.pinned_state_authority()?;
    let value = authority
        .cas_store()?
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("admitted execution realization {hash} is missing"))?;
    let realization = AdmittedExecutionRealization::from_current_value(&value)?;
    if realization.content_hash()? != hash {
        anyhow::bail!("admitted execution realization {hash} has the wrong content hash");
    }
    Ok(realization)
}

fn verify_node_evidence(
    state: &AppState,
    node: &ryeos_app::execution_identity_probe::NodeExecutionIdentity,
) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let identity_value = cas
        .get_object(&node.identity_hash)?
        .ok_or_else(|| anyhow::anyhow!("node execution substrate identity is missing"))?;
    let identity = ryeos_state::objects::ExecutionIdentity::from_current_value(&identity_value)?;
    if identity != node.identity || identity.identity_digest()? != node.digest {
        anyhow::bail!("published node execution substrate identity contradicts boot evidence");
    }
    verify_attestation(&authority, &node.attestation_hash, &node.identity_hash)?;
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                ryeos_app::execution_identity_probe::EXECUTION_IDENTITY_HEAD_NAMESPACE,
                ryeos_app::execution_identity_probe::EXECUTION_IDENTITY_HEAD_NAME,
            )
        })?
        .ok_or_else(|| anyhow::anyhow!("node execution substrate head is absent"))?;
    if head.target_hash != node.attestation_hash {
        anyhow::bail!("node execution substrate head does not root the boot attestation");
    }
    Ok(())
}

fn verify_realization_node_evidence(
    state: &AppState,
    realization: &AdmittedExecutionRealization,
) -> Result<()> {
    let current = state
        .extensions
        .get::<ryeos_app::execution_identity_probe::NodeExecutionIdentity>()
        .ok_or_else(|| anyhow::anyhow!("current node execution substrate is unavailable"))?;
    verify_node_evidence(state, &current)?;
    if realization.substrate_identity_hash != current.identity_hash
        || realization.substrate_attestation_hash != current.attestation_hash
    {
        anyhow::bail!(
            "admitted execution realization belongs to a different node execution substrate"
        );
    }
    let authority = state.state_store.pinned_state_authority()?;
    let value = authority
        .cas_store()?
        .get_object(&realization.substrate_identity_hash)?
        .ok_or_else(|| anyhow::anyhow!("realization substrate identity is missing"))?;
    ryeos_state::objects::ExecutionIdentity::from_current_value(&value)?;
    verify_attestation(
        &authority,
        &realization.substrate_attestation_hash,
        &realization.substrate_identity_hash,
    )
}

fn verify_attestation(
    authority: &ryeos_state::PinnedStateAuthority,
    attestation_hash: &str,
    identity_hash: &str,
) -> Result<()> {
    let value = authority
        .cas_store()?
        .get_object(attestation_hash)?
        .ok_or_else(|| anyhow::anyhow!("execution substrate attestation is missing"))?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    if attestation.subject_hash != identity_hash
        || attestation.claim != ryeos_app::execution_identity_probe::EXECUTION_IDENTITY_CLAIM
        || attestation.policy != ryeos_app::execution_identity_probe::EXECUTION_IDENTITY_POLICY
    {
        anyhow::bail!("execution substrate attestation has the wrong subject or policy");
    }
    attestation.verify_with_trust_store(authority.trust_store())?;
    if attestation.is_expired_at(&lillux::time::iso8601_now())? {
        anyhow::bail!("execution substrate attestation is expired");
    }
    Ok(())
}

fn execution_components(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<Vec<ExecutionComponentReference>> {
    let external = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .map(ryeos_state::objects::ExternalContentRealizationSet::from_value)
        .transpose()?;
    let source = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
        .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
        .transpose()?;
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let mut components = Vec::new();
    if let Some(set) = external.as_ref() {
        for external in set.iter() {
            let object = cas.get_object(&external.manifest_hash)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "external realization `{}` manifest {} is missing",
                    external.id,
                    external.manifest_hash
                )
            })?;
            let kind = object
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("external realization manifest has no kind"))?;
            if !ryeos_state::object_closure::current_object_kinds().contains(&kind) {
                anyhow::bail!("external realization manifest kind `{kind}` is unsupported");
            }
            components.push(ExecutionComponentReference {
                role: format!("external/{}", external.id),
                content_digest: external.manifest_hash.clone(),
                material: ExecutionComponentStorage::CasObject {
                    hash: external.manifest_hash.clone(),
                    expected_kind: kind.to_owned(),
                },
            });
        }
    }
    if let Some(source) = source {
        let value = cas
            .get_object(&source.binding_hash)?
            .ok_or_else(|| anyhow::anyhow!("admitted source binding is missing"))?;
        let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&value)?;
        if binding.digest()? != source.binding_hash || binding.owner_key()? != source.owner_key {
            anyhow::bail!("admitted source binding contradicts its effective projection");
        }
        components.push(ExecutionComponentReference {
            role: format!("source/{}", source.owner_key),
            content_digest: source.binding_hash.clone(),
            material: ExecutionComponentStorage::CasObject {
                hash: source.binding_hash,
                expected_kind: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
            },
        });
    }
    components.sort_by(|left, right| left.role.cmp(&right.role));
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::isolation::{IsolationBackendInspection, IsolationBackendStatus};
    use ryeos_isolation_protocol::{
        InspectedArtifact, IsolationArtifactRole, IsolationBackendSelection,
    };

    // Property projection fixture only: an empty plan is not executable
    // admission and these tests do not qualify a guest or contact a provider.
    fn endpoint_plan(external: bool) -> ryeos_engine::contracts::ExecutionPlan {
        serde_json::from_value(serde_json::json!({
            "plan_id":"plan:properties", "root_executor_id":"tool:test/runtime",
            "root_ref":"tool:test/evaluate", "item_kind":"tool", "nodes":[],
            "entrypoint":"entry", "capabilities":{
                "requires_model":false, "requires_subprocess":true,
                "requires_network":false, "custom":[]
            },
            "materialization_requirements":[],
            "filesystem_authority_ceiling":"captured_execution",
            "network_authority_ceiling":"isolated",
            "target_requirement":null,
            "endpoint_requirement": if external { serde_json::json!({
                "kind":"external", "binding_id":"evaluate", "stdout_max_bytes":1024,
                "stderr_max_bytes":1024
            }) } else { serde_json::json!({"kind":"local"}) },
            "external_endpoint_binding": if external { serde_json::json!({
                "binding_id":"evaluate", "binding_digest":"9".repeat(64)
            }) } else { serde_json::Value::Null },
            "resource_authority_ceiling":"node_policy", "cache_key":"properties",
            "executor_authorities":[]
        }))
        .unwrap()
    }

    fn resource_selection_fixture() -> ryeos_engine::contracts::ExecutionResourceSelection {
        serde_json::from_value(serde_json::json!({
            "stable_id": "gpu-0", "class": "gpu", "matched_facts": {"memory": 1024, "vendor": "fixture"},
            "observation_contract_digest": "2".repeat(64),
            "device_binding_digest": "3".repeat(64),
            "access": "execution_restricted", "enforcement": "character_device_grant",
            "character_devices": [{"role": "compute", "destination": "/dev/test-gpu",
                "access": "read_write", "major": 195, "minor": 0}]
        })).unwrap()
    }

    #[test]
    fn local_endpoint_preserves_exact_controller_isolation_properties() {
        let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
        let plan = endpoint_plan(false);
        let properties = direct_execution_properties(
            isolation.inspection(),
            isolation.is_enforced(),
            &plan,
            &[],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(properties).unwrap(),
            serde_json::json!({
                "isolation_enforced":false,
                "isolation_policy_digest":isolation.inspection().digest,
                "isolation_backend_inspection_digest":isolation_backend_inspection_digest(&isolation.inspection().backend).unwrap(),
                "isolation_filesystem_authority_ceiling":"captured_execution",
                "isolation_network_authority_ceiling":"isolated",
                "execution_resource_authority_ceiling":"node_policy",
                "execution_target_requirement":null,
                "execution_resource_selections":"[]"
            })
        );
    }

    #[test]
    fn external_endpoint_records_requirements_not_guest_isolation_testimony() {
        let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
        let mut plan = endpoint_plan(true);
        plan.target_requirement = Some(
            serde_json::from_value(serde_json::json!({
                "os":"linux", "arch":"x86_64", "resources":[]
            }))
            .unwrap(),
        );
        let properties =
            direct_execution_properties(isolation.inspection(), false, &plan, &[]).unwrap();
        assert_eq!(properties["execution_substrate_role"], "controller");
        for observed in [
            "isolation_enforced",
            "isolation_policy_digest",
            "isolation_backend_inspection_digest",
        ] {
            assert!(
                !properties.contains_key(observed),
                "controller evidence must not become guest {observed}"
            );
        }
        // An enforcement verifier requiring observed true has no testimony to
        // consume here. This is not a qualification or readiness result.
        assert_ne!(
            properties.get("isolation_enforced"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            properties["isolation_filesystem_authority_ceiling"],
            "captured_execution"
        );
        assert_eq!(
            properties["isolation_network_authority_ceiling"],
            "isolated"
        );
        assert_eq!(
            properties["execution_resource_authority_ceiling"],
            "node_policy"
        );
        assert_eq!(properties["execution_resource_selections"], "[]");
        assert_eq!(
            properties["execution_target_requirement"],
            lillux::canonical_json(&serde_json::to_value(&plan.target_requirement).unwrap())
                .unwrap()
        );
        assert_eq!(
            serde_json::from_str::<ryeos_engine::contracts::ExecutionEndpointRequirement>(
                properties["execution_endpoint_requirement"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            plan.endpoint_requirement
        );
        assert_eq!(
            serde_json::from_str::<ryeos_engine::contracts::ExternalEndpointBindingIdentity>(
                properties["external_endpoint_binding"].as_str().unwrap()
            )
            .unwrap(),
            plan.external_endpoint_binding.clone().unwrap()
        );
        // Even stronger controller enforcement cannot turn this requirement
        // projection into an observation about the external guest.
        assert_eq!(
            properties,
            direct_execution_properties(isolation.inspection(), true, &plan, &[]).unwrap()
        );

        let mut realization = realization_with_resources(&[]);
        realization.properties = properties.clone();
        let hash = realization.content_hash().unwrap();
        let round_trip =
            AdmittedExecutionRealization::from_current_value(&realization.to_value().unwrap())
                .unwrap();
        assert_eq!(
            round_trip.properties,
            direct_execution_properties(isolation.inspection(), false, &plan.clone(), &[]).unwrap()
        );
        assert_eq!(round_trip.content_hash().unwrap(), hash);
        plan.external_endpoint_binding
            .as_mut()
            .unwrap()
            .binding_digest = "8".repeat(64);
        realization.properties =
            direct_execution_properties(isolation.inspection(), false, &plan, &[]).unwrap();
        assert_ne!(realization.properties, properties);
        assert_ne!(realization.content_hash().unwrap(), hash);
    }

    #[test]
    fn external_endpoint_refuses_local_resources_and_unsealed_selection() {
        let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
        let mut plan = endpoint_plan(true);
        let selection = resource_selection_fixture();
        selection.validate().unwrap();
        let error = direct_execution_properties(isolation.inspection(), false, &plan, &[selection])
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("controller-local resource selections")
        );
        plan.external_endpoint_binding = None;
        assert!(direct_execution_properties(isolation.inspection(), false, &plan, &[]).is_err());
        plan = endpoint_plan(true);
        plan.external_endpoint_binding.as_mut().unwrap().binding_id = "other".into();
        assert!(direct_execution_properties(isolation.inspection(), false, &plan, &[]).is_err());
    }

    #[test]
    fn external_endpoint_cannot_reuse_controller_persistent_session_realization() {
        let closure = |plan: &ryeos_engine::contracts::ExecutionPlan| {
            ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: serde_json::to_value(plan).unwrap(),
                protocol_descriptor_document: String::new(),
                command: ryeos_state::objects::AdmittedDirectCommandClosure::NodePolicy,
                admitted_project_root: None,
            }
        };
        persistent_session_execution_authority(&closure(&endpoint_plan(false))).unwrap();
        let error =
            persistent_session_execution_authority(&closure(&endpoint_plan(true))).unwrap_err();
        assert!(error.to_string().contains("external launch owner"));
    }

    fn realization_with_resources(
        selections: &[ryeos_engine::contracts::ExecutionResourceSelection],
    ) -> AdmittedExecutionRealization {
        let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
        let properties = execution_properties_from_inspection(
            isolation.inspection(),
            isolation.is_enforced(),
            IsolationFilesystemAuthorityCeiling::CapturedExecution,
            IsolationNetworkAuthorityCeiling::Isolated,
            None,
            ryeos_engine::contracts::ExecutionResourceAuthorityCeiling::NodePolicy,
            selections,
        )
        .unwrap();
        AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "a".repeat(64),
            substrate_attestation_hash: "b".repeat(64),
            launch_authority_digest: "c".repeat(64),
            effective_definition_digest: "d".repeat(64),
            artifact_identity_digest: "e".repeat(64),
            execution_closure_digest: "f".repeat(64),
            contract_ref: "execution:test/fixture".to_owned(),
            contract_digest: "1".repeat(64),
            components: vec![],
            properties,
        }
    }

    #[test]
    fn resource_evidence_validates_round_trips_and_changes_realization_identity() {
        use ryeos_engine::contracts::ExecutionResourceSelection;
        let key = ExecutionResourceSelection::REALIZATION_PROPERTY;
        let empty = realization_with_resources(&[]);
        empty.validate().unwrap();
        assert_eq!(empty.properties[key], serde_json::json!("[]"));
        let mut selection = resource_selection_fixture();
        let admitted = realization_with_resources(&[selection.clone()]);
        let encoded = admitted.to_value().unwrap();
        assert_eq!(
            AdmittedExecutionRealization::from_current_value(&encoded).unwrap(),
            admitted
        );
        let decoded: Vec<ExecutionResourceSelection> =
            serde_json::from_str(admitted.properties[key].as_str().unwrap()).unwrap();
        assert_eq!(decoded, vec![selection.clone()]);
        assert_ne!(
            empty.content_hash().unwrap(),
            admitted.content_hash().unwrap()
        );
        assert_eq!(admitted, realization_with_resources(&[selection.clone()]));
        selection.device_binding_digest = "4".repeat(64);
        let changed = realization_with_resources(&[selection]);
        assert_ne!(admitted.properties, changed.properties);
        assert_ne!(
            admitted.content_hash().unwrap(),
            changed.content_hash().unwrap()
        );
        for invalid in [serde_json::json!([]), serde_json::json!({})] {
            let mut malformed = admitted.clone();
            malformed.properties.insert(key.to_owned(), invalid);
            assert!(malformed.validate().is_err());
        }
    }

    fn backend_with_launcher(digest: &str) -> IsolationBackendInspection {
        IsolationBackendInspection {
            selection: Some(IsolationBackendSelection {
                bundle: "sandbox-fixture".to_owned(),
                implementation: "fixture".to_owned(),
            }),
            status: IsolationBackendStatus::Available,
            bundle_manifest_digest: Some("1".repeat(64)),
            signer_fingerprint: Some("2".repeat(64)),
            adapter_digest: Some("3".repeat(64)),
            adapter_build: Some("fixture-build".to_owned()),
            declared_capabilities: Default::default(),
            effective_capabilities: Default::default(),
            artifacts: [(
                IsolationArtifactRole::Launcher,
                InspectedArtifact {
                    version: "1.0.0".to_owned(),
                    digest: digest.to_owned(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn launcher_payload_moves_execution_realization_backend_identity() {
        let first = backend_with_launcher(&"4".repeat(64));
        let second = backend_with_launcher(&"5".repeat(64));
        assert_ne!(
            isolation_backend_inspection_digest(&first).unwrap(),
            isolation_backend_inspection_digest(&second).unwrap()
        );
    }

    #[test]
    fn captured_launch_ceilings_move_execution_realization_identity() {
        let ordinary = authority_ceiling_properties(
            ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
        );
        let captured = authority_ceiling_properties(
            ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
            ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::Isolated,
        );
        assert_ne!(ordinary, captured);
        assert_eq!(
            captured["isolation_filesystem_authority_ceiling"],
            serde_json::json!("captured_execution")
        );
        assert_eq!(
            captured["isolation_network_authority_ceiling"],
            serde_json::json!("isolated")
        );
    }
}
