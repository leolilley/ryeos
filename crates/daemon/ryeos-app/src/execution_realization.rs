//! Read-only comparison of retained execution realization evidence.
//!
//! These existing checks do not create a realization, acquire execution
//! custody or mint admission/spawn authority. Verification of node evidence
//! reads StateStore internally and must precede a reservation mutex.

use std::collections::BTreeMap;

use crate::state::AppState;
use anyhow::{Context, Result};
use ryeos_engine::isolation::{
    IsolationFilesystemAuthorityCeiling, IsolationNetworkAuthorityCeiling,
};
use ryeos_state::objects::{
    AdmittedExecutionRealization, ExecutionComponentReference, ExecutionComponentStorage,
};

pub fn verify_persistent_session(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
) -> Result<()> {
    verify_persistent_session_selected(
        state,
        capsule,
        resolution,
        effective_definition_digest,
        contract_ref,
        contract_digest,
    )
    .map(|_| ())
}

/// SAME existing comparisons, retaining the actual selected descriptor owner.
/// This private App seam grants no spawn or cleanup permission.
pub(crate) fn verify_persistent_session_selected(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
) -> Result<crate::execution_resources::SelectedExecutionResources> {
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
    verify_realization_components(state, &existing)?;
    Ok(selected_resources)
}

pub fn persistent_session_execution_authority(
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

pub fn execution_properties(
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

pub fn execution_properties_from_inspection(
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

pub fn execution_authority_properties(
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

pub fn resource_selection_property(
    selections: &[ryeos_engine::contracts::ExecutionResourceSelection],
) -> Result<serde_json::Value> {
    for selection in selections {
        selection.validate()?;
    }
    Ok(serde_json::Value::String(lillux::canonical_json(
        &serde_json::to_value(selections)?,
    )?))
}

pub fn authority_ceiling_properties(
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

pub fn isolation_backend_inspection_digest(
    inspection: &ryeos_engine::isolation::IsolationBackendInspection,
) -> Result<String> {
    let value = serde_json::to_value(inspection)?;
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&value)?.as_bytes(),
    ))
}

pub fn execution_components(
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

pub fn load_realization(state: &AppState, hash: &str) -> Result<AdmittedExecutionRealization> {
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

pub fn verify_realization_components(
    state: &AppState,
    realization: &AdmittedExecutionRealization,
) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    realization
        .verify_retained_components(&authority.cas_store()?, &authority.large_object_store()?)
}

pub fn verify_node_evidence(
    state: &AppState,
    node: &crate::execution_identity_probe::NodeExecutionIdentity,
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
                crate::execution_identity_probe::EXECUTION_IDENTITY_HEAD_NAMESPACE,
                crate::execution_identity_probe::EXECUTION_IDENTITY_HEAD_NAME,
            )
        })?
        .ok_or_else(|| anyhow::anyhow!("node execution substrate head is absent"))?;
    if head.target_hash != node.attestation_hash {
        anyhow::bail!("node execution substrate head does not root the boot attestation");
    }
    Ok(())
}

pub fn verify_realization_node_evidence(
    state: &AppState,
    realization: &AdmittedExecutionRealization,
) -> Result<()> {
    let current = state
        .extensions
        .get::<crate::execution_identity_probe::NodeExecutionIdentity>()
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

pub fn verify_attestation(
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
        || attestation.claim != crate::execution_identity_probe::EXECUTION_IDENTITY_CLAIM
        || attestation.policy != crate::execution_identity_probe::EXECUTION_IDENTITY_POLICY
    {
        anyhow::bail!("execution substrate attestation has the wrong subject or policy");
    }
    attestation.verify_with_trust_store(authority.trust_store())?;
    if attestation.is_expired_at(&lillux::time::iso8601_now())? {
        anyhow::bail!("execution substrate attestation is expired");
    }
    Ok(())
}
