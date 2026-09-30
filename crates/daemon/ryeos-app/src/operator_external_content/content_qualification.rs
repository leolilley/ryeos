//! Authenticated activated-content inputs for the shared qualification owner.
//!
//! This adapter owns source preparation and delegates settled execution proof
//! to the existing qualification owner. It does not create a second verifier
//! workflow, publish claims, or turn an activation receipt into product authority.

use anyhow::Result;
use ryeos_state::external_content::{
    products::qualification::{
        ProductProducerRecipeSourceIdentity, ProductQualificationPolicySource,
    },
    qualification_allowance::ContentQualificationAllowance,
    qualification_purpose::{QualificationLaunchPurpose, QualificationSubject},
    qualification_subject::ContentQualificationSubject,
};
use ryeos_state::objects::ExternalContentRealization;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::managed_external_content_operation::{
    AcquisitionMode, load_current_runtime_qualification_inputs,
    require_current_runtime_qualification_inputs,
};
use crate::{handler_context::HandlerContext, state::AppState};

pub(crate) use super::product_qualification::content_proof::load_current_qualified_content;
pub use super::product_qualification::content_proof::{
    ContentQualificationProofRequest, ContentQualificationResponse, prove, qualify,
};

/// Caller-owned coordinates only. Policy, parameters, claims, subject bytes
/// and verifier selection must come from the authenticated consumer source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationLaunchRequest {
    pub launch_id: String,
    pub activation_ref: String,
    pub declaration_id: String,
}

impl ContentQualificationLaunchRequest {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            crate::state_store::is_canonical_launch_id(&self.launch_id),
            "content qualification requires a canonical caller-retained launch id"
        );
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.activation_ref)?;
        anyhow::ensure!(
            canonical.to_string() == self.activation_ref
                && canonical.kind == "config"
                && !self.activation_ref.contains('@')
                && self.activation_ref.len() <= 2048,
            "content qualification requires a canonical activation config ref"
        );
        ryeos_state::external_content::products::validate_name(&self.declaration_id)
    }
}

/// Retained preparation coordinates, not an execution grant. The accepted
/// root must still pin recipes/parameters, match dispatch D1/D2, recheck this
/// source and seal the shared purpose before a callback can start execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContentQualificationSource {
    pub declaration_authority: ryeos_state::external_content::products::qualification::QualificationConsumerDeclarationAuthority,
    pub allowance: ContentQualificationAllowance,
    pub policy: ProductQualificationPolicySource,
    pub subject: ContentQualificationSubject,
    pub verifier_admitted_definition_digest: String,
    pub verifier_realized_definition_digest: String,
    pub producer_recipe_sources: BTreeMap<String, ProductProducerRecipeSourceIdentity>,
}

/// Opaque staged consumer inputs. Only the accepted launch owner may transfer
/// their publication after retaining the complete identity in its capsule.
pub struct PreparedContentQualificationConsumer {
    inputs: super::product_qualification::PreparedBundleConsumerContentInputs,
}

impl PreparedContentQualificationConsumer {
    pub fn prepare(
        state: &AppState,
        operator: &HandlerContext,
        source: &PreparedContentQualificationSource,
        acquisition_mode: AcquisitionMode,
    ) -> Result<Self> {
        Ok(Self {
            inputs: super::product_qualification::prepare_activated_bundle_consumer_content_inputs(
                state,
                operator,
                source,
                acquisition_mode,
            )?,
        })
    }

    pub fn retained_identity(&self) -> Result<ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity>{
        self.inputs.retained_identity()
    }

    pub fn into_publication(self) -> Result<ryeos_state::PendingCasPublication> {
        self.inputs.into_publication()
    }

    pub fn require_current(
        &self,
        state: &AppState,
        operator: &HandlerContext,
        source: &PreparedContentQualificationSource,
        acquisition_mode: AcquisitionMode,
    ) -> Result<()> {
        let current = Self::prepare(state, operator, source, acquisition_mode)?;
        anyhow::ensure!(
            current.retained_identity()? == self.retained_identity()?,
            "activated consumer inputs changed before accepted launch"
        );
        Ok(())
    }
}

impl PreparedContentQualificationSource {
    /// Compare only after a fresh authenticated preparation. This pure check
    /// binds retained intent to preparation; it cannot authenticate either
    /// value or replace accepted-root/verifier-consumer admission.
    pub fn validate_purpose(&self, purpose: &QualificationLaunchPurpose) -> Result<()> {
        purpose.validate()?;
        self.allowance.validate_policy_source(&self.policy)?;
        self.subject.validate()?;
        if let Some(consumer) = &purpose.consumer_content {
            anyhow::ensure!(
                consumer.declaration_authority == self.declaration_authority,
                "qualification consumer differs from authenticated activation definition"
            );
        }
        let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
            anyhow::bail!("content qualification cannot attach a product subject");
        };
        anyhow::ensure!(
            content == &self.subject
                && purpose.policy_source == self.policy
                && purpose.required_claims == self.allowance.required_claims
                && purpose.producer_recipe_sources == self.producer_recipe_sources
                && purpose.verifier_effective_definition_digest
                    == self.verifier_admitted_definition_digest
                && purpose.verifier_realized_definition_digest
                    == self.verifier_realized_definition_digest,
            "qualification purpose differs from authenticated content preparation"
        );
        Ok(())
    }

    /// Preparation is data, so admission must compare it to a fresh complete
    /// preparation rather than treating public fields as a source capability.
    pub fn require_current(
        &self,
        state: &AppState,
        context: &HandlerContext,
        acquisition_mode: AcquisitionMode,
    ) -> Result<()> {
        let current = prepare_source(
            state,
            context,
            &self.allowance.activation_ref,
            acquisition_mode,
            &self.subject.realization,
        )?;
        anyhow::ensure!(
            &current == self,
            "content qualification preparation changed before admission"
        );
        Ok(())
    }
}

pub fn prepare_source(
    state: &AppState,
    context: &HandlerContext,
    activation_ref: &str,
    acquisition_mode: AcquisitionMode,
    admitted_realization: &ExternalContentRealization,
) -> Result<PreparedContentQualificationSource> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let (allowance, policy, records, declaration_authority) =
        load_current_runtime_qualification_inputs(
            state,
            activation_ref,
            acquisition_mode,
            admitted_realization,
        )?;
    let subject = records.retained_subject()?;
    let (d1, d2) = super::product_qualification::resolve_content_fixed_pin_verifier(
        state, context, &policy, &subject,
    )?;
    let producer_recipe_sources =
        super::product_qualification::resolve_current_bundle_producer_recipes_for_policy(
            state,
            &policy,
            &subject.manifest_hash,
        )?;
    // Verifier resolution is a separate checked-generation operation. Rejoin
    // the complete signed selection/source after it rather than assuming the
    // first policy lookup stayed current across preparation.
    require_current_runtime_qualification_inputs(
        state,
        acquisition_mode,
        &allowance,
        &policy,
        &subject,
        &declaration_authority,
    )?;
    Ok(PreparedContentQualificationSource {
        declaration_authority,
        allowance,
        policy,
        subject,
        verifier_admitted_definition_digest: d1,
        verifier_realized_definition_digest: d2,
        producer_recipe_sources,
    })
}

/// Called by the accepted-launch owner only after reserving the retained
/// request coordinate. This prepares data; it does not create a root or claim.
pub fn prepare_after_reservation(
    state: &AppState,
    context: &HandlerContext,
    request: &ContentQualificationLaunchRequest,
    acquisition_mode: AcquisitionMode,
) -> Result<PreparedContentQualificationSource> {
    request.validate()?;
    crate::operator_authority::require_admitted_operator(state, context)?;
    let (activation, _) = crate::managed_external_content::resolve_activation_for_qualification(
        state,
        &request.activation_ref,
        acquisition_mode,
        &request.declaration_id,
    )?;
    let component = activation.component(&request.declaration_id)?;
    anyhow::ensure!(
        component.declaration_kind == ryeos_engine::external_content::ExternalContentKind::Tree
            && component.expected_manifest_kind
                == ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        "content qualification source is not an exact large-content runtime tree"
    );
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let limits = state
        .node_policy
        .require::<crate::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &component.expected_manifest_hash,
        (ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64)
            .min(limits.max_object_bytes),
    )?;
    let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?;
    let realization = ExternalContentRealization {
        id: request.declaration_id.clone(),
        kind: component.declaration_kind,
        mode: ryeos_state::objects::ExternalContentMode::Pinned,
        manifest_hash: component.expected_manifest_hash.clone(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
        mount_root: component.declaration_mount_root,
        mount: component.declaration_mount.clone(),
    };
    // Derivation does not itself authenticate activation/binding authority.
    // The source adapter performs that complete current join before returning.
    prepare_source(
        state,
        context,
        &request.activation_ref,
        acquisition_mode,
        &realization,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_cannot_supply_verifier_policy_claims_or_subject_bytes() {
        let value = serde_json::json!({
            "launch_id": "L-0123456789abcdef0123456789abcdef",
            "activation_ref": "config:fixture/activation",
            "declaration_id": "runtime"
        });
        let request: ContentQualificationLaunchRequest =
            serde_json::from_value(value.clone()).unwrap();
        request.validate().unwrap();
        for key in [
            "verifier_ref",
            "policy_ref",
            "parameters",
            "required_claims",
            "manifest_hash",
            "owner_fingerprint",
            "project_path",
        ] {
            let mut changed = value.clone();
            changed[key] = serde_json::json!("caller-selected");
            assert!(serde_json::from_value::<ContentQualificationLaunchRequest>(changed).is_err());
        }
        let mut request = request;
        request.activation_ref = "config:fixture/activation@hash".into();
        assert!(request.validate().is_err());
    }
}
