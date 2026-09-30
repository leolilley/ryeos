//! Activated-source adapter for the existing qualification proof owner.
//! No product witness, publication, or runtime readiness is manufactured here.

use anyhow::{Context as _, bail};
use ryeos_state::external_content::qualification_evidence::{
    CONTENT_QUALIFICATION_EVIDENCE_SCHEMA, ContentQualificationEvidence,
};
use ryeos_state::external_content::qualification_purpose::QualificationSubject;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{
    CurrentVerifierContent, CurrentVerifierContext, ProductQualificationResult,
    ProductQualificationVerifier, authorize_qualification_terminal, execution_evidence,
    require_reproducible_current_verifier_lane,
    resolve_current_bundle_verifier_identity_against_admitted,
};
use crate::handler_context::HandlerContext;
use crate::managed_external_content_operation::AcquisitionMode;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::operator_external_content::content_qualification::{
    ContentQualificationLaunchRequest, prepare_source,
};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationProofRequest {
    pub launch: ContentQualificationLaunchRequest,
    pub verifier_chain_root_id: String,
    pub verifier_thread_id: String,
}

impl ContentQualificationProofRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.launch.validate()?;
        ryeos_runtime::validate_runtime_thread_id(&self.verifier_chain_root_id)?;
        ryeos_runtime::validate_runtime_thread_id(&self.verifier_thread_id)?;
        Ok(())
    }
}

/// Compute testimony from authoritative retained execution, not caller evidence.
/// The caller must still use an authenticated publication owner to bank claims.
pub async fn prove(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ContentQualificationProofRequest,
) -> anyhow::Result<ContentQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || prove_blocking(&state, &context, &request))
        .await
        .context("content qualification proof task stopped")?
}

fn prove_blocking(
    state: &AppState,
    context: &HandlerContext,
    request: &ContentQualificationProofRequest,
) -> anyhow::Result<ContentQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&request.verifier_chain_root_id)?
        .context("content qualification verifier root does not exist")?;
    let (terminal, has_continuation, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_continuation_presence(
            &request.verifier_chain_root_id,
            &request.verifier_thread_id,
        )?
        .context("content qualification terminal does not exist")?;
    authorize_qualification_terminal(
        &root,
        &terminal,
        has_continuation,
        &request.verifier_chain_root_id,
        &request.verifier_thread_id,
        &context.fingerprint,
    )?;
    let capsule_hash = terminal
        .admitted_launch_capsule_hash
        .as_deref()
        .context("content qualification terminal has no capsule")?;
    let cas = authority.cas_store()?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            capsule_hash,
            limits.max_object_bytes,
        )?,
    )?;
    let realization = capsule.verify_retained_execution_realization(
        &cas,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let purpose = sealed
        .qualification_purpose()
        .context("content verifier has no sealed qualification purpose")?;
    let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
        bail!("content qualification cannot consume a product launch");
    };
    let planning = state
        .state_store
        .launch_planning_record_for_owner(&request.launch.launch_id, &context.fingerprint)?
        .context("content verifier has no owner-bound launch reservation")?;
    if planning.state != "bound"
        || planning.reserved_thread_id != root.thread_id
        || planning.bound_thread_id.as_deref() != Some(root.thread_id.as_str())
        || purpose.launch_id != request.launch.launch_id
        || purpose.owner_fingerprint != context.fingerprint
        || content.declaration_id != request.launch.declaration_id
        || !sealed.product_selections().is_empty()
        || !matches!(
            &capsule.project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        )
    {
        bail!("content verifier differs from its accepted source launch");
    }
    // Rejoin current receipt, signed consumer allowance, grant, exact payload,
    // policy and recipes. No product receipt is synthesized for acquired bytes.
    let prepared = prepare_source(
        state,
        context,
        &request.launch.activation_ref,
        AcquisitionMode::Offline,
        &content.realization,
    )?;
    prepared.validate_purpose(purpose)?;
    let admitted = sealed.admitted_effective_resolution()?;
    require_reproducible_current_verifier_lane(&admitted.composed.derived, true)?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        state,
        &authority,
        &guard,
        context,
        &purpose.verifier_ref,
        &purpose.policy_source.policy.verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(None),
            logical_project_root: None,
            binding_subject_authority: Some(sealed.resolution_subject_authority()),
            sealed_request: Some(&sealed),
            project_context_resolver: None,
            pinned_admission: None,
        },
        Some(&admitted),
    )?;
    let _source = crate::source_closure_admission::recover_source_closure(
        state,
        current.request_engine.as_ref(),
        &admitted,
    )?;
    let d2 = sealed.effective_definition_digest().as_str();
    if current.artifact_identity != capsule.artifact_identity
        || current.effective_definition_digest != d2
        || purpose.verifier_realized_definition_digest != d2
        || realization.effective_definition_digest != d2
        || realization.launch_authority_digest != capsule.launch_authority_digest()?
        || realization.artifact_identity_digest
            != capsule.launch_authority().artifact_identity_digest()?
        || realization.content_hash()? != capsule.execution_realization_hash
    {
        bail!("content verifier current identity contradicts retained execution");
    }
    content.verify_verifier_realizations(
        &capsule
            .external_realization_set()?
            .context("content verifier has no realization set")?,
        &purpose.subject_declaration_id,
    )?;
    let (projected, execution_proof, settlement) = execution_evidence::prove(
        state,
        &authority,
        &guard,
        limits,
        context,
        &terminal,
        &capsule,
        &admitted,
        &current,
        &purpose.execution_view()?,
        None,
    )?;
    let result = ProductQualificationResult::from_value(&projected)?;
    let evidence = ContentQualificationEvidence {
        schema: CONTENT_QUALIFICATION_EVIDENCE_SCHEMA.to_owned(),
        purpose: purpose.clone(),
        verifier: ProductQualificationVerifier {
            chain_root_id: root.thread_id,
            thread_id: terminal.thread_id.clone(),
            admitted_launch_capsule_hash: capsule_hash.to_owned(),
            canonical_ref: sealed.item_ref().to_owned(),
            effective_definition_digest: d2.to_owned(),
            exact_program_hash: capsule.exact_program_hash.clone(),
            admitted_parameters_digest: sealed.admitted_parameters_digest()?,
            launch_authority_digest: capsule.launch_authority_digest()?,
            execution_realization_hash: capsule.execution_realization_hash.clone(),
            artifact_identity: capsule.artifact_identity.clone(),
            admitted_project_root: ProductQualificationVerifier::project_root_from_closure(
                &capsule.execution_closure,
            )?,
            substrate_identity_hash: realization.substrate_identity_hash,
            subject_declaration_id: purpose.subject_declaration_id.clone(),
            subject_manifest_hash: purpose.subject_manifest_hash.clone(),
            terminal_snapshot_hash: ryeos_state::objects::thread_snapshot::hash_snapshot(
                &terminal,
            )?,
            process_settlement_witness_digest: settlement
                .as_ref()
                .map(|(digest, _)| digest.clone()),
            process_settlement_authority: settlement.map(|(_, authority)| authority),
            result_digest: result.digest()?,
        },
        execution_proof,
        result,
    };
    evidence.validate()?;
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::ContentQualificationProofRequest;
    use serde_json::json;

    #[test]
    fn proof_request_accepts_coordinates_not_caller_testimony() {
        let request = json!({
            "launch": {
                "launch_id": format!("L-{}", "a".repeat(32)),
                "activation_ref": "config:test/runtime",
                "declaration_id": "runtime"
            },
            "verifier_chain_root_id": "T-11111111-1111-1111-1111-111111111111",
            "verifier_thread_id": "T-22222222-2222-2222-2222-222222222222"
        });
        let parsed: ContentQualificationProofRequest =
            serde_json::from_value(request.clone()).unwrap();
        parsed.validate().unwrap();
        for field in [
            "evidence",
            "claims",
            "purpose",
            "owner_fingerprint",
            "expires_at",
        ] {
            let mut forged = request.clone();
            forged
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), json!({}));
            assert!(serde_json::from_value::<ContentQualificationProofRequest>(forged).is_err());
        }
    }
}
