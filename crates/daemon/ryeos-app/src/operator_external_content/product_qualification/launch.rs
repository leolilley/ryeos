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
use serde::{Deserialize, Deserializer, Serialize};
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
    /// Required-nullable so omission cannot silently select the legacy
    /// projectless lane. A present snapshot selects the pinned read-only
    /// verifier lane; no path or authority is caller-authored.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub project_context: Option<ProductQualificationProjectContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationProjectContext {
    pub snapshot_hash: String,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl ProductQualificationLaunchRequest {
    pub fn validate(&self) -> Result<()> {
        if !crate::state_store::is_canonical_launch_id(&self.launch_id) {
            bail!("product qualification requires a canonical caller-retained launch id");
        }
        require_canonical_hash("product witness", &self.witness_hash)?;
        self.witness_source.validate()?;
        require_bounded_name("product relationship", &self.relationship_name)?;
        if let Some(project) = &self.project_context
            && (!lillux::valid_hash(&project.snapshot_hash)
                || project
                    .snapshot_hash
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase()))
        {
            bail!("qualification project context requires an exact canonical snapshot");
        }
        Ok(())
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
    pub verifier_admitted_definition_digest: Option<String>,
    /// Finalized, realized verifier definition (D2). The accepted root's
    /// pre-realization D1 is computed independently from dispatch preflight.
    pub verifier_realized_definition_digest: Option<String>,
    /// Pinned selected-slot admission resolves both verifier identities only
    /// after normal root preflight has authenticated the exact slot binding.
    pub pinned_snapshot_hash: Option<String>,
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

fn selected_root_product_selection(
    declaration_id: &str,
    witness_hash: &str,
    witness_source: &ProductWitnessSource,
) -> ryeos_state::external_content::products::composition::ProductSelectionInput {
    use ryeos_state::external_content::products::composition::{
        ProductSelection, ProductSelectionInput, ProductSelectionTarget,
    };
    ProductSelectionInput {
        target: ProductSelectionTarget::Root {},
        selection: ProductSelection {
            declaration_id: declaration_id.to_owned(),
            witness_hash: witness_hash.to_owned(),
            witness_source: witness_source.clone(),
            qualification_hash: None,
        },
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
    // Projectless qualification retains the signed fixed-pin lane. The pinned
    // read-only lane instead selects the policy's inner subject slot and lets
    // normal request-engine preflight authenticate its current binding.
    let (
        verifier_admitted_definition_digest,
        verifier_realized_definition_digest,
        product_selections,
    ) = if request.project_context.is_some() {
        // The pinned lane selects an outer-relationship-authorized product
        // slot. The ordinary request Engine preflight authenticates the
        // selected inner slot and current pinned binding before D1/D2 are
        // finalized below; do not resolve it projectlessly here.
        // The signed policy selects the verifier's inner subject slot; it is
        // distinct from the outer relationship's qualified-product consumer.
        let selection = selected_root_product_selection(
            &policy_source.policy.subject_declaration_id,
            &product.attestation_hash,
            &request.witness_source,
        );
        (None, None, vec![selection])
    } else {
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
                sealed_request: None,
                project_context_resolver: None,
                pinned_admission: None,
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
        (
            Some(current_verifier.admitted_definition_digest),
            Some(current_verifier.effective_definition_digest),
            Vec::new(),
        )
    };
    Ok(PreparedProductQualificationLaunch {
        launch_id: request.launch_id.clone(),
        owner_fingerprint: context.fingerprint.clone(),
        product_witness_hash: product.attestation_hash,
        witness_source: request.witness_source.clone(),
        relationship,
        policy_source: policy_source.clone(),
        subject_manifest_hash: product.evidence.manifest_hash,
        verifier_ref: policy_source.policy.verifier_ref.clone(),
        verifier_admitted_definition_digest,
        verifier_realized_definition_digest,
        verifier_parameters: policy_source.policy.verifier_parameters.clone(),
        // In the projectless lane, the signed verifier declares an exact fixed
        // pin, so adding a Root selector would invent a different definition.
        // In the pinned lane, derive the Root selector from the signed policy
        // subject and authenticated witness; dispatch preflight resolves its
        // inner signed slot.
        product_selections,
        pinned_snapshot_hash: request
            .project_context
            .as_ref()
            .map(|project| project.snapshot_hash.clone()),
        consumer_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::composition::ProductSelectionTarget;
    use serde_json::json;

    #[test]
    fn caller_cannot_choose_verifier_policy_parameters_or_claims() {
        let valid = json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier",
            "project_context":null
        });
        let request: ProductQualificationLaunchRequest =
            serde_json::from_value(valid.clone()).unwrap();
        request.validate().unwrap();
        let mut omitted_nullable = valid.clone();
        omitted_nullable
            .as_object_mut()
            .unwrap()
            .remove("project_context");
        assert!(
            serde_json::from_value::<ProductQualificationLaunchRequest>(omitted_nullable).is_err()
        );
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

    #[test]
    fn pinned_selector_uses_policy_inner_slot_not_outer_consumer_slot() {
        use std::fs;

        let outer_relationship_name = "runtime_to_qualified_runtime";
        let outer_consumer_declaration = "runtime";
        let workspace = ryeos_engine::test_support::workspace_root();
        let standard = ryeos_engine::test_support::standard_bundle_root();
        let policy: serde_yaml::Value = serde_yaml::from_slice(
            &fs::read(standard.join(".ai/config/ryeos/environments/qualification/gnu-python.yaml"))
                .unwrap(),
        )
        .unwrap();
        let policy_inner_subject_declaration =
            policy["product_qualification_policy"]["subject_declaration_id"]
                .as_str()
                .unwrap();
        let relationships: serde_yaml::Value = serde_yaml::from_slice(
            &fs::read(workspace.join(".ai/config/development/ryeos/gnu-python-products.yaml"))
                .unwrap(),
        )
        .unwrap();
        let relationships = relationships["product_relationships"]["relationships"]
            .as_sequence()
            .unwrap();
        let outer = relationships
            .iter()
            .find(|relationship| relationship["name"].as_str() == Some(outer_relationship_name))
            .unwrap();
        let inner = relationships
            .iter()
            .find(|relationship| {
                relationship["name"].as_str() == Some("runtime_to_qualification_verifier")
            })
            .unwrap();
        assert_eq!(
            outer["consumer"]["declaration_id"].as_str(),
            Some(outer_consumer_declaration)
        );
        assert_eq!(
            inner["consumer"]["declaration_id"].as_str(),
            Some(policy_inner_subject_declaration)
        );
        assert!(inner["qualification"]["policy_ref"].is_null());

        let verifier: serde_yaml::Value = serde_yaml::from_slice(
            &fs::read(standard.join(".ai/tools/ryeos/environments/qualification/gnu-python.yaml"))
                .unwrap(),
        )
        .unwrap();
        assert!(
            verifier["external_product_slots"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|slot| {
                    slot["id"].as_str() == Some(policy_inner_subject_declaration)
                        && slot["relationship"].as_str()
                            == Some("runtime_to_qualification_verifier")
                })
        );

        let source = ProductWitnessSource::LocalCapture {};
        let selection = selected_root_product_selection(
            policy_inner_subject_declaration,
            &"a".repeat(64),
            &source,
        );
        assert!(matches!(selection.target, ProductSelectionTarget::Root {}));
        assert_eq!(selection.selection.declaration_id, "subject");
        assert_eq!(selection.selection.witness_source, source);
        assert!(selection.selection.qualification_hash.is_none());
    }
}
