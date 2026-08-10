//! Admission and recovery checks for kind-neutral execution realizations.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use ryeos_app::state::AppState;
use ryeos_state::objects::{
    ADMITTED_EXECUTION_REALIZATION_KIND, AdmittedExecutionRealization,
    EXECUTION_REALIZATION_SCHEMA_VERSION, ExecutionComponentReference, ExecutionComponentStorage,
};

pub(crate) struct ExecutionRealizationAdmission {
    pub(crate) hash: String,
    pub(crate) publication: Option<ryeos_state::PendingCasPublication>,
}

pub(crate) fn admit_or_verify(
    state: &AppState,
    metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: &str,
    contract_ref: &str,
    contract_digest: &str,
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    let launch_authority = metadata
        .admitted_launch_authority()?
        .ok_or_else(|| anyhow::anyhow!("subprocess launch has no admitted launch authority"))?;
    let components = external_components(state, resolution)?;
    let launch_authority_digest = launch_authority.digest()?;
    let artifact_identity_digest = launch_authority.artifact_identity_digest()?;
    let execution_closure_digest = launch_authority.execution_closure_digest()?;
    let properties = execution_properties(
        state,
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
    )?;

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
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
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
    store_new_realization(
        state,
        authority.digest()?,
        effective_definition_digest,
        authority.artifact_identity_digest()?,
        authority.execution_closure_digest()?,
        contract_ref,
        contract_digest,
        external_components(state, resolution)?,
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::Isolated,
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
    let properties = execution_properties(
        state,
        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::Isolated,
    )?;
    if existing.launch_authority_digest != authority.digest()?
        || existing.effective_definition_digest != effective_definition_digest
        || existing.artifact_identity_digest != authority.artifact_identity_digest()?
        || existing.execution_closure_digest != authority.execution_closure_digest()?
        || existing.contract_ref != contract_ref
        || existing.contract_digest != contract_digest
        || existing.components != external_components(state, resolution)?
        || existing.properties != properties
    {
        anyhow::bail!("persistent-session execution realization contradicts its admitted capsule");
    }
    verify_realization_node_evidence(state, &existing)?;
    verify_realization_components(state, &existing)
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
    filesystem_authority_ceiling: ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling,
    network_authority_ceiling: ryeos_engine::isolation::IsolationNetworkAuthorityCeiling,
    staged_publication: Option<&mut ryeos_state::PendingCasPublication>,
) -> Result<ExecutionRealizationAdmission> {
    let node = state
        .extensions
        .get::<ryeos_app::execution_identity_probe::NodeExecutionIdentity>()
        .ok_or_else(|| anyhow::anyhow!("node execution substrate evidence is unavailable"))?;
    verify_node_evidence(state, &node)?;
    let properties = execution_properties(
        state,
        filesystem_authority_ceiling,
        network_authority_ceiling,
    )?;
    let candidate = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
        substrate_identity_hash: node.identity_hash.clone(),
        substrate_attestation_hash: node.attestation_hash.clone(),
        launch_authority_digest,
        effective_definition_digest: effective_definition_digest.to_owned(),
        artifact_identity_digest,
        execution_closure_digest,
        contract_ref: contract_ref.to_owned(),
        contract_digest: contract_digest.to_owned(),
        components,
        properties,
    };
    candidate.validate()?;
    verify_realization_components(state, &candidate)?;
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
            let authority = state
                .state_store
                .with_state_db(|db| db.pinned_authority())?;
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
        publication,
    })
}

fn execution_properties(
    state: &AppState,
    filesystem_authority_ceiling: ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling,
    network_authority_ceiling: ryeos_engine::isolation::IsolationNetworkAuthorityCeiling,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let inspection = state.isolation.inspection();
    let mut properties = BTreeMap::new();
    properties.insert(
        "isolation_enforced".to_owned(),
        serde_json::Value::Bool(state.isolation.is_enforced()),
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
    properties.extend(authority_ceiling_properties(
        filesystem_authority_ceiling,
        network_authority_ceiling,
    ));
    Ok(properties)
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
            "isolation_filesystem_authority_ceiling".to_owned(),
            serde_json::Value::String(filesystem.to_owned()),
        ),
        (
            "isolation_network_authority_ceiling".to_owned(),
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
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    realization
        .verify_retained_components(&authority.cas_store()?, &authority.large_object_store()?)
}

fn load_realization(state: &AppState, hash: &str) -> Result<AdmittedExecutionRealization> {
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
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
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
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
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
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

fn external_components(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<Vec<ExecutionComponentReference>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(Vec::new());
    };
    let set = ryeos_state::objects::ExternalContentRealizationSet::from_value(value)?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas = authority.cas_store()?;
    let mut components = Vec::with_capacity(set.iter().len());
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
