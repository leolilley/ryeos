//! Protected, pre-execution inputs for a product-qualification verifier.
//!
//! This module authenticates the product and its signed policy. It does not
//! launch a Tool or grant a callback: the accepted-root owner must reserve the
//! caller-retained launch coordinate, preflight the exact Bundle verifier,
//! and seal a qualification purpose before execution can begin.

use anyhow::{Context as _, Result, bail};
use ryeos_state::external_content::products::composition::{
    ProductRelationship, ProductSelectionInputs,
};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity;
use ryeos_state::external_content::products::qualification::ProductQualificationPolicySource;
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

use super::{require_bounded_name, require_canonical_hash};

/// Caller coordinates only. The witness relationship and trusted Bundle
/// policy, never caller parameters or a caller Tool ref, choose the verifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationLaunchRequest {
    pub launch_id: String,
    pub witness_hash: String,
    pub witness_source: ProductWitnessSource,
    pub relationship_name: String,
}

impl ProductQualificationLaunchRequest {
    pub fn validate(&self) -> Result<()> {
        if !crate::state_store::is_canonical_launch_id(&self.launch_id) {
            bail!("product qualification requires a canonical caller-retained launch id");
        }
        require_canonical_hash("product witness", &self.witness_hash)?;
        self.witness_source.validate()?;
        require_bounded_name("product relationship", &self.relationship_name)
    }
}

/// Authenticated inputs to accepted-root preflight. This is not a grant: the
/// dispatcher must still prove the exact root route, signed source, selected
/// fixed pin, effective definition, and sealed purpose at its launch cut.
pub struct PreparedProductQualificationLaunch {
    pub launch_id: String,
    pub owner_fingerprint: String,
    pub product_witness_hash: String,
    pub witness_source: ProductWitnessSource,
    pub relationship: ProductRelationship,
    pub policy_source: ProductQualificationPolicySource,
    pub subject_manifest_hash: String,
    pub verifier_ref: String,
    /// Pre-realization D1 from the first current-Bundle admission. The later
    /// dispatch preflight must match it before any verifier executes.
    pub verifier_admitted_definition_digest: String,
    /// Finalized, realized verifier definition (D2). The accepted root's
    /// pre-realization D1 is computed independently from dispatch preflight.
    pub verifier_realized_definition_digest: String,
    pub verifier_parameters: Value,
    pub product_selections: ProductSelectionInputs,
    consumer_content: Option<super::PreparedBundleConsumerContentInputs>,
}

impl PreparedProductQualificationLaunch {
    pub fn consumer_content_identity(
        &self,
    ) -> Result<Option<ProductQualificationConsumerContentIdentity>> {
        self.consumer_content
            .as_ref()
            .map(super::PreparedBundleConsumerContentInputs::retained_identity)
            .transpose()
    }

    /// Transfer the one staged lease to the accepted-root owner. Releasing
    /// it before the durable capsule owns its typed CAS edges is forbidden.
    pub fn take_consumer_content_publication(
        &mut self,
    ) -> Result<Option<ryeos_state::PendingCasPublication>> {
        self.consumer_content
            .take()
            .map(super::PreparedBundleConsumerContentInputs::into_publication)
            .transpose()
    }
}

/// Recheck the exact current signed Bundle policy just before dispatch. A
/// verifier D1 match alone does not bind the separate policy Config; this
/// comparison prevents silently launching under a changed claim/parameter
/// contract after preparation. The later irreversible nested-producer cut
/// must make its own current-policy check as well.
pub fn require_current_policy_matches_prepared(
    state: &AppState,
    prepared: &PreparedProductQualificationLaunch,
) -> Result<()> {
    prepared.policy_source.validate()?;
    if prepared.relationship.qualification.policy_ref.as_deref()
        != Some(prepared.policy_source.canonical_ref.as_str())
    {
        bail!("prepared product relationship differs from signed qualification policy");
    }
    let current = state.engine.with_checked_bundle_generation(|_| {
        super::resolve_current_bundle_qualification_policy(
            state,
            &prepared.policy_source.canonical_ref,
        )
    })?;
    if current != prepared.policy_source {
        bail!("qualification policy changed after verifier preparation");
    }
    Ok(())
}

/// Run only after the accepted-launch owner has reserved `launch_id`. A lost
/// acknowledgement must resolve that reservation; it must not call this again
/// to invent a second verifier root.
pub fn prepare_after_reservation(
    state: &AppState,
    context: &HandlerContext,
    request: &ProductQualificationLaunchRequest,
) -> Result<PreparedProductQualificationLaunch> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let product = super::super::product_receipt::load_product_source(
        state,
        &authority,
        &guard,
        limits,
        &context.fingerprint,
        &request.witness_hash,
        &request.witness_source,
        super::super::product_receipt::ProductSourceVerification::Fresh,
    )?;
    let relationship = product
        .evidence
        .relationships
        .select(&request.relationship_name)?
        .clone();
    relationship.validate_product_evidence(&product.evidence)?;
    let policy_ref = relationship
        .qualification
        .policy_ref
        .as_deref()
        .context("selected product relationship has no qualification policy")?;
    let policy_source = super::resolve_current_bundle_qualification_policy(state, policy_ref)?;
    let consumer_content =
        if let Some(consumer_context) = &policy_source.policy.consumer_execution_context {
            consumer_context.validate_relationship_consumer(&relationship.consumer)?;
            let mut prepared = super::prepare_current_bundle_consumer_content_inputs(
                state,
                &policy_source,
                &relationship,
                &product.evidence.recipe_ref,
            )?
            .context("signed consumer content is absent")?;
            prepared.require_external_runtime_member_alignment(
                state,
                &authority,
                &guard,
                limits,
                &product.evidence.manifest_hash,
            )?;
            Some(prepared)
        } else {
            None
        };
    for claim in &relationship.qualification.required_claims {
        if policy_source
            .policy
            .allowed_claims
            .binary_search(claim)
            .is_err()
        {
            bail!("product relationship requires a claim outside its signed policy");
        }
    }
    // This profile uses a signed fixed-pin verifier, not an operator-selected
    // product slot. Re-admit the exact current Bundle Tool and independently
    // compare its pinned subject realization with the authenticated witness
    // before any root or nested producer can execute.
    let current_verifier = super::resolve_current_bundle_verifier_identity_against_admitted(
        state,
        &authority,
        &guard,
        context,
        &policy_source.policy.verifier_ref,
        &policy_source.policy.verifier_parameters,
        super::CurrentVerifierContext {
            content: super::CurrentVerifierContent::Root(None),
            logical_project_root: None,
            binding_subject_authority: None,
        },
        None,
    )?;
    super::require_exact_pinned_subject(
        &current_verifier.realizations,
        &policy_source.policy.subject_declaration_id,
        &product.evidence.manifest_hash,
        product.evidence.declaration.shape,
        product.evidence.entry_count,
        product.evidence.total_bytes,
    )?;
    Ok(PreparedProductQualificationLaunch {
        launch_id: request.launch_id.clone(),
        owner_fingerprint: context.fingerprint.clone(),
        product_witness_hash: product.attestation_hash,
        witness_source: request.witness_source.clone(),
        relationship,
        policy_source: policy_source.clone(),
        subject_manifest_hash: product.evidence.manifest_hash,
        verifier_ref: policy_source.policy.verifier_ref.clone(),
        verifier_admitted_definition_digest: current_verifier.admitted_definition_digest,
        verifier_realized_definition_digest: current_verifier.effective_definition_digest,
        verifier_parameters: policy_source.policy.verifier_parameters.clone(),
        // The current signed verifier declares exact fixed pins. A Root
        // ProductSelection would invent a different execution definition and
        // is invalid when no signed product slot exists. Dispatch preflight
        // must verify the fixed subject declaration and exact witness bytes.
        // A selected-slot verifier needs a separate signed-contract branch;
        // it must never be inferred from the policy subject id alone.
        product_selections: Vec::new(),
        consumer_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn caller_cannot_choose_verifier_policy_parameters_or_claims() {
        let valid = json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier"
        });
        let request: ProductQualificationLaunchRequest =
            serde_json::from_value(valid.clone()).unwrap();
        request.validate().unwrap();
        for field in [
            "verifier_ref",
            "policy_ref",
            "parameters",
            "claims",
            "subject_manifest_hash",
        ] {
            let mut forged = valid.clone();
            forged[field] = json!("caller-controlled");
            assert!(serde_json::from_value::<ProductQualificationLaunchRequest>(forged).is_err());
        }
    }
}
