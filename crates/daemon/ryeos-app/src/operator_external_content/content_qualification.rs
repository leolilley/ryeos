//! Authenticated activated-content inputs for the shared qualification owner.
//!
//! This adapter owns source preparation and delegates settled execution proof
//! to the existing qualification owner. It does not create a second verifier
//! workflow, publish claims, or turn an activation receipt into product authority.

use anyhow::{Context as _, Result};
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

use super::qualification_testimony::RemoteContentTestimonyRefusal;
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
    pub(super) fn publication_mut(&mut self) -> Result<&mut ryeos_state::PendingCasPublication> {
        self.inputs.publication_mut()
    }

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
    /// Fresh eligibility must rederive the consumer from current signed
    /// definitions and exact admitted content, then match the sealed purpose.
    /// This comparison alone does not grant a qualification claim.
    pub fn require_current_consumer(
        &self,
        state: &AppState,
        operator: &HandlerContext,
        acquisition_mode: AcquisitionMode,
        purpose: &QualificationLaunchPurpose,
    ) -> Result<()> {
        self.validate_purpose(purpose)?;
        if self.policy.policy.consumer_execution_context.is_some() {
            let retained = purpose.consumer_content.as_ref().ok_or_else(|| {
                anyhow::anyhow!("activated qualification has no retained consumer")
            })?;
            let current = PreparedContentQualificationConsumer::prepare(
                state,
                operator,
                self,
                acquisition_mode,
            )?;
            anyhow::ensure!(
                current.retained_identity()? == *retained,
                "activated qualification consumer differs from current signed content"
            );
        }
        Ok(())
    }

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

    /// Remote use of a content testimony. `self` is this node's fresh,
    /// authenticated preparation of its own activation and `purpose` is the
    /// origin's sealed purpose. The subject is compared in portable form only:
    /// the origin's receipt, binding and node fingerprint are its own records,
    /// and this node's equivalents were authenticated by the preparation and
    /// must name this node. Producer recipes are compared without their
    /// process-local Bundle generation. Each mismatch is a typed refusal that
    /// names what differs. Local launch and admission keep
    /// [`Self::validate_purpose`], which compares the whole subject.
    pub fn validate_remote_purpose(
        &self,
        purpose: &QualificationLaunchPurpose,
        node_fingerprint: &str,
    ) -> Result<()> {
        purpose.validate()?;
        self.allowance.validate_policy_source(&self.policy)?;
        self.subject.validate()?;
        anyhow::ensure!(
            self.policy.policy.consumer_execution_context.is_none()
                && purpose.consumer_content.is_none()
                && purpose.consumer_definitions.is_none(),
            "a remote content testimony carries no consumer context"
        );
        anyhow::ensure!(
            self.subject.target_node_fingerprint == node_fingerprint,
            "content preparation is not joined on this node"
        );
        let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
            anyhow::bail!("content qualification cannot attach a product subject");
        };
        let origin = content.portable();
        let receiver = self.subject.portable();
        let fields = origin.differing_fields(&receiver);
        if !fields.is_empty() {
            return Err(RemoteContentTestimonyRefusal::IdentityDiffers {
                fields,
                origin_activation_program_digest: origin.activation_program_digest,
                receiver_activation_program_digest: receiver.activation_program_digest,
                origin_manifest_hash: origin.manifest_hash,
                receiver_manifest_hash: receiver.manifest_hash,
            }
            .into());
        }
        if purpose.policy_source != self.policy
            || purpose.required_claims != self.allowance.required_claims
        {
            return Err(RemoteContentTestimonyRefusal::policy_differs(
                &purpose.policy_source,
                &purpose.required_claims,
                &self.policy,
                &self.allowance.required_claims,
            )
            .into());
        }
        require_same_producer_recipes(
            &purpose.producer_recipe_sources,
            &self.producer_recipe_sources,
        )?;
        if purpose.verifier_effective_definition_digest != self.verifier_admitted_definition_digest
            || purpose.verifier_realized_definition_digest
                != self.verifier_realized_definition_digest
        {
            return Err(RemoteContentTestimonyRefusal::VerifierDiffers {
                origin_admitted: purpose.verifier_effective_definition_digest.clone(),
                receiver_admitted: self.verifier_admitted_definition_digest.clone(),
                origin_realized: purpose.verifier_realized_definition_digest.clone(),
                receiver_realized: self.verifier_realized_definition_digest.clone(),
            }
            .into());
        }
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

/// Signed-content identity of one producer recipe source, without its
/// process-local Bundle generation, as a digest both nodes can compare.
fn producer_recipe_content_digest(source: &ProductProducerRecipeSourceIdentity) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::to_value(source.content_identity())?)
}

/// The origin's and this node's producer recipes name the same scenarios with
/// the same signed content. The Bundle generation identity is node-local cache
/// identity and is never compared between nodes, as `portable()` drops it for
/// the product lane.
fn require_same_producer_recipes(
    origin: &BTreeMap<String, ProductProducerRecipeSourceIdentity>,
    receiver: &BTreeMap<String, ProductProducerRecipeSourceIdentity>,
) -> Result<()> {
    let scenarios = origin
        .keys()
        .chain(receiver.keys())
        .collect::<std::collections::BTreeSet<_>>();
    for scenario in scenarios {
        let origin_recipe = origin
            .get(scenario)
            .map(producer_recipe_content_digest)
            .transpose()?;
        let receiver_recipe = receiver
            .get(scenario)
            .map(producer_recipe_content_digest)
            .transpose()?;
        if origin_recipe != receiver_recipe {
            return Err(RemoteContentTestimonyRefusal::ProducerDiffers {
                scenario: scenario.clone(),
                origin_recipe,
                receiver_recipe,
            }
            .into());
        }
    }
    Ok(())
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
    let realization = derive_admitted_realization(
        state,
        &request.activation_ref,
        acquisition_mode,
        &request.declaration_id,
    )?
    .context("current managed activation has no completed head")?;
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

/// Receiver side of a remote content testimony: prepare this node's own
/// activation of the declaration the origin qualified, from this node's own
/// signed component and resident manifest. The origin's realization is never
/// used here, so a differing mount or metric reaches the typed portable
/// comparison instead of failing inside the record join. A node that holds no
/// completed activation of the program it resolves is refused with
/// `remote_content_no_local_activation`.
pub(crate) fn prepare_receiver_source(
    state: &AppState,
    context: &HandlerContext,
    activation_ref: &str,
    declaration_id: &str,
) -> Result<PreparedContentQualificationSource> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let realization = derive_admitted_realization(
        state,
        activation_ref,
        AcquisitionMode::Offline,
        declaration_id,
    )?
    .ok_or_else(|| RemoteContentTestimonyRefusal::NoLocalActivation {
        activation_ref: activation_ref.to_owned(),
    })?;
    prepare_source(
        state,
        context,
        activation_ref,
        AcquisitionMode::Offline,
        &realization,
    )
}

/// This node's admitted realization of one activated declaration, derived
/// from its current signed activation component and the exact resident
/// manifest; `None` when this node holds no completed activation of the
/// program it currently resolves. Derivation authenticates nothing: callers
/// must still run the complete current join in [`prepare_source`].
fn derive_admitted_realization(
    state: &AppState,
    activation_ref: &str,
    acquisition_mode: AcquisitionMode,
    declaration_id: &str,
) -> Result<Option<ExternalContentRealization>> {
    let (activation, _) = crate::managed_external_content::resolve_activation_for_qualification(
        state,
        activation_ref,
        acquisition_mode,
        declaration_id,
    )?;
    let component = activation.component(declaration_id)?;
    anyhow::ensure!(
        component.declaration_kind == ryeos_engine::external_content::ExternalContentKind::Tree
            && component.expected_manifest_kind
                == ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        "content qualification source is not an exact large-content runtime tree"
    );
    if !crate::managed_external_content_operation::has_completed_activation_head(
        state,
        &activation,
    )? {
        return Ok(None);
    }
    let authority = state.state_store.pinned_state_authority()?;
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    // Only this exact-hash manifest read is guarded. Source preparation, which
    // the caller runs next, resolves the verifier and starts handler
    // processes, so it must run after the guard drops; it re-derives
    // everything it relies on itself.
    let value = super::product_qualification::with_state_guard(&authority, |_guard| {
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &authority.cas_store()?,
            &component.expected_manifest_hash,
            (ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64)
                .min(limits.max_object_bytes),
        )
    })?;
    let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?;
    Ok(Some(ExternalContentRealization {
        id: declaration_id.to_owned(),
        kind: component.declaration_kind,
        mode: ryeos_state::objects::ExternalContentMode::Pinned,
        manifest_hash: component.expected_manifest_hash.clone(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
        mount_root: component.declaration_mount_root,
        mount: component.declaration_mount.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::qualification::{
        ProductQualificationBundleDefinitionIdentity, QualificationConsumerDeclarationAuthority,
    };

    const RECEIVER: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    /// The origin's sealed purpose and this node's preparation of the same
    /// signed content: equal in everything but the receipt, binding and node.
    fn remote_fixture() -> (
        PreparedContentQualificationSource,
        QualificationLaunchPurpose,
    ) {
        let capsule =
            ryeos_state::external_execution::admission::test_support::content_candidate_capsule(
                &"a".repeat(64),
            )
            .unwrap();
        let purpose = capsule
            .retained_external_runtime_content_qualification
            .unwrap()
            .evidence
            .purpose;
        let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
            panic!("content fixture has no activated subject");
        };
        let allowance = ContentQualificationAllowance {
            activation_ref: "config:fixtures/runtime-activation".into(),
            policy_ref: purpose.policy_source.canonical_ref.clone(),
            required_claims: purpose.required_claims.clone(),
        };
        let mut subject = content.clone();
        subject.activation_receipt_hash = "b".repeat(64);
        subject.binding_hash = "c".repeat(64);
        subject.target_node_fingerprint = RECEIVER.into();
        let source = PreparedContentQualificationSource {
            declaration_authority: QualificationConsumerDeclarationAuthority::ActivatedContent {
                activation_definition: ProductQualificationBundleDefinitionIdentity {
                    canonical_ref: allowance.activation_ref.clone(),
                    raw_content_digest: "6".repeat(64),
                    effective_definition_digest: "7".repeat(64),
                    publisher_fingerprint: "8".repeat(64),
                },
                declaration_id: content.declaration_id.clone(),
                allowance: allowance.clone(),
            },
            allowance,
            policy: purpose.policy_source.clone(),
            subject,
            verifier_admitted_definition_digest: purpose
                .verifier_effective_definition_digest
                .clone(),
            verifier_realized_definition_digest: purpose
                .verifier_realized_definition_digest
                .clone(),
            producer_recipe_sources: purpose.producer_recipe_sources.clone(),
        };
        (source, purpose)
    }

    fn refusal(error: &anyhow::Error) -> &RemoteContentTestimonyRefusal {
        error
            .downcast_ref::<RemoteContentTestimonyRefusal>()
            .unwrap_or_else(|| panic!("expected a typed refusal, got {error:#}"))
    }

    fn recipe(generation: &str, recipe_digest: &str) -> ProductProducerRecipeSourceIdentity {
        ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: generation.into(),
            canonical_ref: "config:fixtures/runtime-recipe".into(),
            raw_content_digest: "1".repeat(64),
            effective_definition_digest: "2".repeat(64),
            publisher_fingerprint: "3".repeat(64),
            recipe_digest: recipe_digest.into(),
        }
    }

    #[test]
    fn remote_purpose_admits_another_nodes_receipt_binding_and_node() {
        let (source, purpose) = remote_fixture();
        let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
            unreachable!()
        };
        assert_ne!(source.subject.local_join(), content.local_join());
        assert_eq!(source.subject.portable(), content.portable());
        source.validate_remote_purpose(&purpose, RECEIVER).unwrap();
    }

    #[test]
    fn remote_purpose_refuses_different_content_naming_the_field() {
        let changes: [(&[&str], fn(&mut ContentQualificationSubject)); 5] = [
            (&["activation_program_digest"], |subject| {
                subject.activation_program_digest = "9".repeat(64)
            }),
            (&["manifest_hash", "realization"], |subject| {
                subject.manifest_hash = "e".repeat(64);
                subject.realization.manifest_hash = "e".repeat(64);
            }),
            (&["realization"], |subject| {
                subject.realization.mount = "elsewhere".into()
            }),
            (&["consumer_ref"], |subject| {
                subject.consumer_ref = "worker:fixtures/other-consumer".into()
            }),
            (&["declaration_id", "realization"], |subject| {
                subject.declaration_id = "other-runtime".into();
                subject.realization.id = "other-runtime".into();
            }),
        ];
        for (fields, change) in changes {
            let (mut source, purpose) = remote_fixture();
            change(&mut source.subject);
            let error = source
                .validate_remote_purpose(&purpose, RECEIVER)
                .unwrap_err();
            let refusal = refusal(&error);
            assert_eq!(refusal.code(), "remote_content_identity_differs");
            let RemoteContentTestimonyRefusal::IdentityDiffers {
                fields: named,
                origin_activation_program_digest,
                receiver_activation_program_digest,
                ..
            } = refusal
            else {
                panic!("expected an identity refusal");
            };
            assert_eq!(named.as_slice(), fields);
            assert!(refusal.fix().contains(origin_activation_program_digest));
            assert!(refusal.fix().contains(receiver_activation_program_digest));
            assert!(error.to_string().contains(fields[0]));
        }
    }

    #[test]
    fn remote_purpose_refuses_a_different_policy_claims_producer_or_verifier() {
        let cases: [(&str, fn(&mut PreparedContentQualificationSource)); 4] = [
            ("remote_content_policy_differs", |source| {
                source.policy.effective_definition_digest = "e".repeat(64)
            }),
            ("remote_content_policy_differs", |source| {
                source.allowance.required_claims.truncate(1)
            }),
            ("remote_content_producer_differs", |source| {
                source
                    .producer_recipe_sources
                    .insert("probe".into(), recipe("generation-b", &"4".repeat(64)));
            }),
            ("remote_content_verifier_differs", |source| {
                source.verifier_realized_definition_digest = "f".repeat(64)
            }),
        ];
        for (code, change) in cases {
            let (mut source, purpose) = remote_fixture();
            change(&mut source);
            let error = source
                .validate_remote_purpose(&purpose, RECEIVER)
                .unwrap_err();
            assert_eq!(refusal(&error).code(), code, "{error:#}");
            assert!(!refusal(&error).fix().is_empty());
        }
    }

    #[test]
    fn producer_recipes_compare_without_their_bundle_generation() {
        let origin =
            BTreeMap::from([("probe".to_owned(), recipe("generation-a", &"4".repeat(64)))]);
        let same_content =
            BTreeMap::from([("probe".to_owned(), recipe("generation-b", &"4".repeat(64)))]);
        require_same_producer_recipes(&origin, &same_content).unwrap();
        let other_content =
            BTreeMap::from([("probe".to_owned(), recipe("generation-a", &"5".repeat(64)))]);
        let error = require_same_producer_recipes(&origin, &other_content).unwrap_err();
        let RemoteContentTestimonyRefusal::ProducerDiffers {
            scenario,
            origin_recipe,
            receiver_recipe,
        } = refusal(&error)
        else {
            panic!("expected a producer refusal");
        };
        assert_eq!(scenario, "probe");
        assert!(origin_recipe.is_some() && receiver_recipe.is_some());
        assert_ne!(origin_recipe, receiver_recipe);
        let error = require_same_producer_recipes(&origin, &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            refusal(&error),
            RemoteContentTestimonyRefusal::ProducerDiffers {
                receiver_recipe: None,
                ..
            }
        ));
    }

    #[test]
    fn remote_purpose_refuses_a_subject_not_joined_on_this_node() {
        let (source, purpose) = remote_fixture();
        let error = source
            .validate_remote_purpose(&purpose, &"0".repeat(64))
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<RemoteContentTestimonyRefusal>()
                .is_none()
        );
        assert!(error.to_string().contains("not joined on this node"));
    }

    #[test]
    fn local_purpose_still_requires_the_whole_subject() {
        let (mut source, purpose) = remote_fixture();
        // Another node's receipt, binding and fingerprint never satisfy the
        // local check, which compares the whole subject.
        assert!(source.validate_purpose(&purpose).is_err());
        let QualificationSubject::ActivatedContent { content } = &purpose.subject else {
            unreachable!()
        };
        source.subject = content.clone();
        source.validate_purpose(&purpose).unwrap();
        source.subject.binding_hash = "c".repeat(64);
        assert!(source.validate_purpose(&purpose).is_err());
    }

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
