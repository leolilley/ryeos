//! Current verification of exact retained persistent-session bytes.
//!
//! These shared checks are not a complete admitted session owner, launch
//! permit, resource reservation or cleanup proof. Mechanical plan recovery
//! still cannot mint trusted authority. Original exact-program, attachment,
//! realization verification remains read-only; custody and executable admission
//! remain with their existing owners.

use anyhow::{Context as _, Result, anyhow, bail};
use ryeos_engine::protocols::validate_persistent_session_protocol;
use ryeos_state::objects::AdmittedPersistentSessionCapsule;
use serde::{Deserialize, Serialize};

use crate::prepared_evidence_attachment::PreparedEvidenceAttachment;

pub(crate) mod cleanup_channel;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionExactProgram {
    pub effective_definition_digest: String,
    pub resolution_output: ryeos_engine::resolution::RetainedResolutionOutput,
    pub evidence_attachments: Vec<PreparedEvidenceAttachment>,
}

pub fn validate_exact_evidence_attachments(exact: &PersistentSessionExactProgram) -> Result<()> {
    let mut previous: Option<&str> = None;
    for binding in &exact.evidence_attachments {
        binding.validate()?;
        if previous.is_some_and(|value| value >= binding.binding_id.as_str()) {
            bail!("persistent-session evidence attachments are not canonically ordered");
        }
        previous = Some(&binding.binding_id);
    }
    Ok(())
}

pub fn retained_exact_program(
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<PersistentSessionExactProgram> {
    serde_json::from_value(capsule.retained_exact_program()?)
        .context("decode full retained persistent-session exact program")
}

pub fn retained_session_protocol(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<ryeos_engine::protocols::descriptor::PersistentSessionProtocol> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session protocol has no direct artifact identity");
    };
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        execution_plan,
        ..
    } = &capsule.execution_closure
    else {
        bail!("persistent-session protocol has no retained direct closure");
    };
    // The current trust store may revoke the retained signer. It must never
    // replace admitted behavior with a newer descriptor at the same ref.
    let descriptor = verify_and_decode_retained_protocol_document(
        protocol_descriptor_document,
        protocol_content_hash,
        protocol_signer_fingerprint,
        &engine.node_trust_store,
    )
    .map_err(|error| anyhow!("verify retained session protocol: {error}"))?;
    ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &descriptor)?;
    let session =
        validate_persistent_session_protocol(&descriptor).map_err(|error| anyhow!(error))?;
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())?;
    if plan
        .filesystem_authority_ceiling
        .intersect(session.workspace_authority.filesystem_ceiling())
        != plan.filesystem_authority_ceiling
        || plan
            .network_authority_ceiling
            .intersect(session.network_authority.network_ceiling())
            != plan.network_authority_ceiling
    {
        bail!("persistent-session plan widens its retained protocol ceilings");
    }
    Ok(session.clone())
}

fn verify_and_decode_retained_protocol_document(
    document: &str,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let body = crate::admitted_program_trust::verify_admitted_signed_descriptor_document(
        document,
        expected_content_hash,
        expected_signer,
        trust_store,
    )?;
    decode_retained_protocol_descriptor(&body)
}

/// Decode already-admitted predecessor protocol bytes without weakening
/// current authoring. Before cleanup authority became explicit, pooled
/// sessions had no dedicated cleanup owner and exclusive sessions always
/// required the local Lillux process scope. The signed original bytes are
/// verified before this narrow historical interpretation is applied.
fn decode_retained_protocol_descriptor(
    body: &str,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let mut value: serde_json::Value = serde_yaml::from_str(body)?;
    if let Some(session) = value
        .get_mut("session")
        .and_then(serde_json::Value::as_object_mut)
        && !session.contains_key("cleanup_authority")
    {
        let historical = match session
            .get("process_mode")
            .and_then(serde_json::Value::as_str)
        {
            Some("pooled_requests") => "not_required",
            Some("exclusive_session") => "local_process_scope",
            _ => bail!("retained session protocol has no interpretable cleanup authority"),
        };
        session.insert(
            "cleanup_authority".to_owned(),
            serde_json::Value::String(historical.to_owned()),
        );
    }
    serde_json::from_value(value).context("decode retained session protocol descriptor")
}

pub fn validate_capsule_current_trust(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<()> {
    retained_session_protocol(engine, capsule)?;
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        root_subject_signer_fingerprint,
        root_subject_source_identity,
        protocol_signer_fingerprint,
        executable_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session capsule has a non-direct artifact identity");
    };
    let mut signers = Vec::new();
    if let Some(signer) = root_subject_signer_fingerprint.as_deref() {
        signers.push(("root subject", signer));
    }
    signers.push(("protocol", protocol_signer_fingerprint.as_str()));
    signers.push((
        "runtime",
        runtime_identity.runtime_signer_fingerprint.as_str(),
    ));
    if let Some(signer) = runtime_identity
        .runtime_bundle_signer_fingerprint
        .as_deref()
    {
        signers.push(("runtime bundle", signer));
    }
    if let ryeos_state::objects::DirectRootSourceIdentity::Bundle {
        manifest_signer_fingerprint,
        ..
    } = root_subject_source_identity
    {
        signers.push(("root bundle", manifest_signer_fingerprint));
    }
    if let ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        executor_manifest_signer_fingerprint,
        ..
    } = executable_identity
    {
        signers.push(("executable bundle", executor_manifest_signer_fingerprint));
    }
    for (label, signer) in signers {
        if !engine.node_trust_store.is_trusted(signer) {
            bail!("persistent-session {label} signer is no longer trusted: {signer}");
        }
    }
    Ok(())
}

/// Exact read-only retained inputs for a local resource-bearing trusted pool.
/// This is NOT executable admission, a spawn permit, financial prebinding or
/// driver retirement. Private fields and no Clone/serde constructor prevent a
/// raw capsule, selected-resource DTO or caller flag from creating this proof.
pub struct VerifiedTrustedPooledSessionInputs {
    state_store: std::sync::Arc<crate::state_store::StateStore>,
    state_authority: ryeos_state::PinnedStateAuthority,
    capsule: AdmittedPersistentSessionCapsule,
    exact: PersistentSessionExactProgram,
    selected: crate::execution_resources::SelectedExecutionResources,
    cleanup_contract_digest: String,
}

impl VerifiedTrustedPooledSessionInputs {
    /// Verify current retained bytes and independent node/product agreement.
    /// Existing executable prerequisite refusals remain authoritative.
    pub fn verify(state: &crate::state::AppState, capsule_hash: &str) -> Result<Self> {
        let capsule = state
            .state_store
            .admitted_persistent_session_capsule(capsule_hash)?;
        validate_capsule_current_trust(&state.engine, &capsule)?;
        let session = retained_session_protocol(&state.engine, &capsule)?;
        use ryeos_engine::protocols::descriptor::{
            PersistentSessionCleanupAuthority, PersistentSessionProcessMode,
        };
        anyhow::ensure!(
            session.process_mode == PersistentSessionProcessMode::PooledRequests
                && session.cleanup_authority
                    == PersistentSessionCleanupAuthority::TrustedProcessGroup
                && capsule.external_candidate.is_none(),
            "verified trusted pooled inputs require the exact local pooled product"
        );
        ryeos_engine::protocols::descriptor::validate_session_resource_cleanup_contract(&session)
            .map_err(|error| anyhow!(error))?;
        let product = session
            .resource_cleanup_contract
            .as_ref()
            .context("verified trusted pooled inputs have no product cleanup contract")?;
        let node = state
            .node_policy
            .require::<crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy>(
        )?;
        let resources = node
            .resource_authority
            .as_ref()
            .context("verified trusted pooled inputs have no node resource authority")?;
        resources.validate()?;
        anyhow::ensure!(
            resources.trusted_cleanup_contract_digest.as_ref() == Some(product),
            "verified trusted pooled node/product cleanup contracts differ"
        );
        let exact = retained_exact_program(&capsule)?;
        validate_exact_evidence_attachments(&exact)?;
        anyhow::ensure!(
            exact
                .resolution_output
                .effective_definition_digest()?
                .as_str()
                == exact.effective_definition_digest,
            "verified trusted pooled exact program digest does not reproduce"
        );
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } = &capsule.artifact_identity
        else {
            bail!("verified trusted pooled capsule has a non-direct artifact identity");
        };
        let selected = crate::execution_realization::verify_persistent_session_selected(
            state,
            &capsule,
            &exact.resolution_output.restore(),
            &exact.effective_definition_digest,
            protocol_ref,
            protocol_content_hash,
        )?;
        anyhow::ensure!(
            !selected.selections().is_empty(),
            "resource-free sessions do not acquire trusted resource input proof"
        );
        let state_authority = state.state_store.pinned_state_authority()?;
        state_authority.runtime_directory().ensure_path_binding()?;
        let inputs = Self {
            state_store: std::sync::Arc::clone(&state.state_store),
            state_authority,
            capsule,
            exact,
            selected,
            cleanup_contract_digest: product.clone(),
        };
        inputs.require_same_state(state)?;
        Ok(inputs)
    }

    /// Read-only retained inputs; none of these borrows grants execution.
    pub fn capsule(&self) -> &AdmittedPersistentSessionCapsule {
        &self.capsule
    }
    pub fn exact(&self) -> &PersistentSessionExactProgram {
        &self.exact
    }
    pub fn selected(&self) -> &crate::execution_resources::SelectedExecutionResources {
        &self.selected
    }
    pub fn cleanup_contract_digest(&self) -> &str {
        &self.cleanup_contract_digest
    }

    pub fn require_same_state(&self, state: &crate::state::AppState) -> Result<()> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.state_store, &state.state_store),
            "verified trusted pooled inputs belong to another StateStore owner"
        );
        let current = state.state_store.pinned_state_authority()?;
        let original = self.state_authority.runtime_directory();
        original.ensure_path_binding()?;
        current.runtime_directory().ensure_path_binding()?;
        anyhow::ensure!(
            original.identity()? == current.runtime_directory().identity()?,
            "verified trusted pooled inputs belong to another runtime incarnation"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::protocols::descriptor::PersistentSessionCleanupAuthority;

    #[test]
    fn retained_predecessor_protocol_gets_only_its_historical_cleanup_meaning() {
        let exclusive = r#"
kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let descriptor = decode_retained_protocol_descriptor(exclusive).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let pooled = exclusive
            .replace("exclusive_session", "pooled_requests")
            .replace("runtime_workspace", "ephemeral_scratch")
            .replace("node_policy", "isolated")
            .replace(
                "  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY\n",
                "",
            );
        let descriptor = decode_retained_protocol_descriptor(&pooled).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::NotRequired
        );

        // Current authoring never receives the retained-document bridge.
        assert!(
            serde_yaml::from_str::<ryeos_engine::protocols::ProtocolDescriptor>(exclusive).is_err()
        );
    }

    #[test]
    fn retained_predecessor_protocol_is_verified_before_historical_interpretation() {
        let key = lillux::crypto::SigningKey::from_bytes(&[91u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let body = r#"kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let document = lillux::signature::sign_content(body, &key, "#", None);
        let content_hash = lillux::signature::content_hash(body);
        let descriptor = verify_and_decode_retained_protocol_document(
            &document,
            &content_hash,
            &fingerprint,
            &trust,
        )
        .unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let tampered = document.replace("exclusive_session", "pooled_requests");
        assert!(
            verify_and_decode_retained_protocol_document(
                &tampered,
                &content_hash,
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &"0".repeat(64),
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        let revoked = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &content_hash,
                &fingerprint,
                &revoked,
            )
            .is_err()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "persistent_session_admission/factory_controls.rs"]
mod factory_controls;
