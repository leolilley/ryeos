//! Protected, pre-execution inputs for a product-qualification verifier.
//!
//! This module authenticates the product and its signed policy. It does not
//! launch a Tool or grant a callback: the accepted-root owner must reserve the
//! caller-retained launch coordinate, preflight the exact Bundle verifier,
//! and seal a qualification purpose before execution can begin.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection;
use ryeos_state::external_content::products::composition::{
    ProductRelationship, ProductSelectionInputs, ProductSelectionTarget,
    canonicalize_product_selection_inputs, validate_product_selection_inputs,
};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity;
use ryeos_state::external_content::products::qualification::ProductQualificationPolicySource;
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::state::AppState;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;

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
    /// Actual occurrence selectors, separately authenticated and sealed. They
    /// never override signed verifier parameters or choose a new executable.
    #[serde(default)]
    pub consumer_invocation: Option<ConsumerVerificationInputSelection>,
    /// Required-nullable so omission cannot silently select the legacy
    /// projectless lane. A present snapshot selects the pinned read-only
    /// verifier lane; no path or authority is caller-authored.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub project_context: Option<ProductQualificationProjectContext>,
    /// Exact coordinates for the verifier's other signed Root slots. Empty
    /// preserves single-subject callers; it never discovers dependencies.
    /// The signed policy alone selects the subject and checker parameters.
    #[serde(default)]
    pub auxiliary_product_selections: ProductSelectionInputs,
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
        if let Some(invocation) = &self.consumer_invocation {
            invocation.validate()?;
        }
        validate_product_selection_inputs(&self.auxiliary_product_selections)?;
        if self
            .auxiliary_product_selections
            .iter()
            .any(|input| !matches!(input.target, ProductSelectionTarget::Root {}))
        {
            bail!("qualification auxiliary selections must target signed Root slots");
        }
        if self.project_context.is_none() && !self.auxiliary_product_selections.is_empty() {
            bail!("projectless fixed-pin qualification does not accept product selections");
        }
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
    pub consumer_invocation: Option<ConsumerVerificationInputSelection>,
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
    /// Staged-root lease of a Worker-less remote probe. Present exactly when
    /// the signed policy has no consumer context and names a probe scenario,
    /// so its lifetime never depends on consumer content.
    probe_publication: Option<ryeos_state::PendingCasPublication>,
}

impl PreparedProductQualificationLaunch {
    pub(in crate::operator_external_content) fn consumer_publication_mut(
        &mut self,
    ) -> Result<&mut ryeos_state::PendingCasPublication> {
        match (&mut self.consumer_content, &mut self.probe_publication) {
            (Some(content), None) => content.publication_mut(),
            (None, Some(publication)) => Ok(publication),
            _ => bail!("remote verifier requires exactly one staged publication"),
        }
    }

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
        ensure!(
            self.consumer_content.is_none() || self.probe_publication.is_none(),
            "qualification launch holds two verifier publications"
        );
        if let Some(publication) = self.probe_publication.take() {
            return Ok(Some(publication));
        }
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
            remote_testimony: None,
        },
    }
}

/// Join caller coordinates to the daemon-authored subject. Normal product
/// composition subsequently authenticates exact coverage and each slot's
/// signed relationship, binding and required qualification.
fn selected_root_product_inputs(
    declaration_id: &str,
    witness_hash: &str,
    witness_source: &ProductWitnessSource,
    auxiliary: &ProductSelectionInputs,
) -> Result<ProductSelectionInputs> {
    validate_product_selection_inputs(auxiliary)?;
    if auxiliary.iter().any(|input| {
        !matches!(input.target, ProductSelectionTarget::Root {})
            || input.selection.declaration_id == declaration_id
    }) {
        bail!(
            "qualification auxiliary selections cannot override the signed subject or target dependencies"
        );
    }
    let mut inputs = auxiliary.clone();
    inputs.push(selected_root_product_selection(
        declaration_id,
        witness_hash,
        witness_source,
    ));
    // Apply the existing aggregate bounds including the daemon-owned subject.
    canonicalize_product_selection_inputs(inputs)
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
    require_current_consumer_invocation(
        state,
        &prepared.owner_fingerprint,
        &prepared.policy_source,
        &prepared.subject_manifest_hash,
        prepared.consumer_invocation.as_ref(),
    )
}

pub(crate) fn require_invocation_lane(
    policy: &ProductQualificationPolicySource,
    invocation: Option<&ConsumerVerificationInputSelection>,
) -> Result<()> {
    let probe_scenarios = policy
        .policy
        .producer_scenarios
        .values()
        .any(|scenario| scenario.remote_probe_input.is_some());
    match (&policy.policy.consumer_execution_context, invocation) {
        (None, None) => {
            ensure!(
                !probe_scenarios,
                "remote probe qualification requires an explicit invocation"
            );
            Ok(())
        }
        (Some(_), Some(selection)) => {
            selection.validate()?;
            let scenario = policy
                .policy
                .producer_scenarios
                .get(&selection.scenario_id)
                .context("consumer invocation scenario is not signed in policy")?;
            ensure!(
                scenario.remote_verifier.is_some()
                    && scenario.consumer_worker_request.is_some()
                    && scenario.remote_probe_input.is_none(),
                "consumer invocation requires the implemented remote Worker scenario"
            );
            Ok(())
        }
        (None, Some(selection)) => {
            selection.validate()?;
            let scenario = policy
                .policy
                .producer_scenarios
                .get(&selection.scenario_id)
                .context("probe invocation scenario is not signed in policy")?;
            ensure!(
                scenario.remote_verifier.is_some()
                    && scenario.remote_probe_input.is_some()
                    && scenario.consumer_worker_request.is_none(),
                "probe invocation requires a remote probe scenario"
            );
            Ok(())
        }
        (Some(_), None) => bail!("consumer qualification requires an explicit invocation"),
    }
}

/// Authenticate actual retained selectors without constructing pre-born root
/// coordinates. This check grants no contact and never extends guest lifetime.
fn require_current_consumer_invocation(
    state: &AppState,
    owner: &str,
    policy: &ProductQualificationPolicySource,
    subject_manifest_hash: &str,
    invocation: Option<&ConsumerVerificationInputSelection>,
) -> Result<()> {
    require_invocation_lane(policy, invocation)?;
    state.engine.with_checked_bundle_generation(|generation| {
        let current = super::resolve_current_bundle_qualification_policy_in_generation(
            generation,
            &policy.canonical_ref,
        )?;
        ensure!(
            &current == policy,
            "qualification policy changed after verifier preparation"
        );
        let Some(invocation) = invocation else {
            return Ok(());
        };
        let qualification = state
            .state_store
            .observed_consumer_invocation_prerequisite(owner, invocation)?;
        let binding = state
            .node_config
            .runtime_snapshot_qualification
            .iter()
            .find(|binding| binding.digest() == qualification.intent.qualification_profile_digest)
            .context("consumer invocation lost its current qualification profile")?;
        let producer = state
            .node_config
            .runtime_snapshot_production
            .iter()
            .find(|producer| {
                producer.id() == binding.production_binding_id()
                    && producer.digest() == binding.production_binding_digest()
            })
            .context("consumer invocation lost its exact producer profile")?;
        let source = state
            .state_store
            .runtime_snapshot_operation(&qualification.intent.snapshot_operation_id)?
            .context("consumer invocation source snapshot is absent")?;
        ensure!(
            source.intent.production_profile_digest == producer.digest()
                && source.intent.provider_id == producer.backend()
                && source.intent.adapter_artifact_hash == producer.adapter_artifact_hash()
                && source.intent.provider_group_id == producer.provider_group_id()
                && qualification.intent.adapter_artifact_hash == producer.adapter_artifact_hash()
                && qualification.intent.provider_spec_digest == binding.provider_spec_digest()
                && qualification.intent.settings_digest == binding.settings_digest()
                && qualification.intent.verifier_artifact_hash == binding.verifier_artifact_hash(),
            "consumer invocation changed its exact current occurrence authority"
        );
        let scenario = &policy.policy.producer_scenarios[&invocation.scenario_id];
        // A probe scenario has no producer recipe, so nothing is pinned for it.
        let sources = match scenario.recipe_ref.as_deref() {
            Some(recipe_ref) => std::collections::BTreeMap::from([(
                invocation.scenario_id.clone(),
                super::resolve_bundle_producer_recipe_in_generation(generation, recipe_ref)?
                    .source_identity()?,
            )]),
            None => std::collections::BTreeMap::new(),
        };
        let verifier = super::prepare_remote_consumer_verifier_from_admitted_sources_in_generation(
            state,
            generation,
            policy,
            &sources,
            subject_manifest_hash,
            &invocation.scenario_id,
        )?;
        let selected = binding.consumer_verifier_for_source(
            &invocation.scenario_id,
            &verifier.scenario_source_digest,
        )?;
        verifier.require_protected_selection(selected)?;
        // The potentially lengthy source capture above must not turn an
        // earlier live-occurrence read into permission after termination.
        state
            .state_store
            .observed_consumer_invocation_prerequisite(owner, invocation)?;
        Ok(())
    })
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
    // Policy, consumer and verifier resolution below start handler processes,
    // so no CAS guard spans this preparation. The fresh product witness and
    // the subject manifest are each read in their own guarded segment. This
    // function returns only data, never a guard, so no guarantee held by a
    // guard ever outlived it; proof and publication re-authenticate the
    // witness and terminal against current state under their own guards.
    super::require_unguarded_resolution("product qualification launch preparation")?;
    let authority = state.state_store.pinned_state_authority()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let product = super::with_state_guard(&authority, |guard| {
        super::super::product_receipt::load_product_source(
            state,
            &authority,
            guard,
            limits,
            &context.fingerprint,
            &request.witness_hash,
            &request.witness_source,
            super::super::product_receipt::ProductSourceVerification::Fresh,
        )
    })?;
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
    require_current_consumer_invocation(
        state,
        &context.fingerprint,
        &policy_source,
        &product.evidence.manifest_hash,
        request.consumer_invocation.as_ref(),
    )?;
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
        let selections = selected_root_product_inputs(
            &policy_source.policy.subject_declaration_id,
            &product.attestation_hash,
            &request.witness_source,
            &request.auxiliary_product_selections,
        )?;
        (None, None, selections)
    } else {
        let current_verifier = super::resolve_current_bundle_verifier_identity_against_admitted(
            state,
            &authority,
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
    let probe_publication = if policy_source.policy.consumer_execution_context.is_none()
        && policy_source
            .policy
            .producer_scenarios
            .values()
            .any(|scenario| scenario.remote_probe_input.is_some())
    {
        Some(super::with_state_guard(&authority, |guard| {
            Ok(ryeos_state::PendingCasPublication::new(
                authority.try_clone()?,
                authority
                    .require_recovery()?
                    .begin_staged_cas_roots_admitted(guard, "qualification-probe-verifier")?,
            ))
        })?)
    } else {
        None
    };
    Ok(PreparedProductQualificationLaunch {
        launch_id: request.launch_id.clone(),
        owner_fingerprint: context.fingerprint.clone(),
        product_witness_hash: product.attestation_hash,
        witness_source: request.witness_source.clone(),
        relationship,
        consumer_invocation: request.consumer_invocation.clone(),
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
        probe_publication,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::composition::ProductSelectionTarget;
    use ryeos_state::external_content::products::qualification::{
        PRODUCT_QUALIFICATION_POLICY_SCHEMA, ProductQualificationPolicy,
        ProductQualificationProducerScenario, ProductQualificationRemoteVerifierSelection,
        VerifierProcessSettlementAuthority,
    };
    use serde_json::json;

    fn lane_policy(
        scenarios: Vec<(&str, ProductQualificationProducerScenario)>,
    ) -> ProductQualificationPolicySource {
        ProductQualificationPolicySource {
            canonical_ref: "config:fixtures/policy".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            policy: ProductQualificationPolicy {
                schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.into(),
                verifier_ref: "tool:fixtures/verify".into(),
                subject_declaration_id: "subject".into(),
                allowed_claims: vec!["claim".into()],
                minimum_verifier_process_settlement: VerifierProcessSettlementAuthority::ScopeEmpty,
                verifier_parameters: json!({}),
                consumer_execution_context: None,
                producer_scenarios: scenarios
                    .into_iter()
                    .map(|(name, scenario)| (name.to_owned(), scenario))
                    .collect(),
            },
        }
    }

    fn remote_selection() -> ProductQualificationRemoteVerifierSelection {
        ProductQualificationRemoteVerifierSelection {
            binary_ref: "bin:fixtures/verifier".into(),
            guest_target_triple: "x86_64-unknown-linux-musl".into(),
            archive_members: Default::default(),
        }
    }

    fn probe_scenario() -> ProductQualificationProducerScenario {
        ProductQualificationProducerScenario {
            recipe_ref: None,
            remote_verifier: Some(remote_selection()),
            consumer_worker_request: None,
            remote_probe_input: Some(json!({"probe": "a"})),
        }
    }

    fn producer_scenario() -> ProductQualificationProducerScenario {
        ProductQualificationProducerScenario {
            recipe_ref: Some("config:fixtures/recipe".into()),
            remote_verifier: None,
            consumer_worker_request: None,
            remote_probe_input: None,
        }
    }

    fn invocation(scenario_id: &str) -> ConsumerVerificationInputSelection {
        ConsumerVerificationInputSelection {
            scenario_id: scenario_id.into(),
            qualification_operation_id: "a".repeat(64),
            prerequisite_measurement_attempt_id: "b".repeat(64),
        }
    }

    #[test]
    fn invocation_lane_admits_a_probe_invocation_of_a_probe_scenario_only() {
        let policy = lane_policy(vec![
            ("probe", probe_scenario()),
            ("producer", producer_scenario()),
        ]);
        require_invocation_lane(&policy, Some(&invocation("probe"))).unwrap();
        // A probe policy cannot be launched without naming its attempt.
        assert!(require_invocation_lane(&policy, None).is_err());
        // The invocation must name a probe scenario, never a plain producer
        // scenario or an unsigned one.
        assert!(require_invocation_lane(&policy, Some(&invocation("producer"))).is_err());
        assert!(require_invocation_lane(&policy, Some(&invocation("absent"))).is_err());
        // A probe scenario that also carries a Worker request is not a probe.
        let mut hybrid = probe_scenario();
        hybrid.consumer_worker_request = Some(
            ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest {
                schema: ryeos_state::external_content::products::qualification::PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA.into(),
                controller_canary: None,
                credential_profile_id: None,
                fixture: None,
                session_setup: json!({}),
                body: json!({}),
            },
        );
        let hybrid = lane_policy(vec![("probe", hybrid)]);
        assert!(require_invocation_lane(&hybrid, Some(&invocation("probe"))).is_err());
    }

    #[test]
    fn invocation_lane_keeps_nonprobe_policies_invocation_free_and_workers_explicit() {
        let plain = lane_policy(vec![("producer", producer_scenario())]);
        require_invocation_lane(&plain, None).unwrap();
        assert!(require_invocation_lane(&plain, Some(&invocation("producer"))).is_err());
        // A Worker consumer policy still requires an explicit invocation and
        // refuses a probe scenario as its target.
        let mut worker = lane_policy(vec![("probe", probe_scenario())]);
        worker.policy.consumer_execution_context = Some(
            ryeos_state::external_content::products::qualification::ProductQualificationConsumerExecutionContext {
                worker_ref: "worker:fixtures/w".into(),
                product_declaration_id: "runtime".into(),
                environment_ref: "config:fixtures/env".into(),
                worker_execution_ref: "worker_execution:fixtures/x".into(),
                environment_binding: "environment".into(),
            },
        );
        assert!(require_invocation_lane(&worker, None).is_err());
        assert!(require_invocation_lane(&worker, Some(&invocation("probe"))).is_err());
    }

    #[test]
    fn auxiliary_coordinates_preserve_exact_proof_and_cannot_replace_subject() {
        let source = ProductWitnessSource::LocalCapture {};
        let mut auxiliary = selected_root_product_selection("python", &"b".repeat(64), &source);
        auxiliary.selection.qualification_hash = Some("c".repeat(64));
        let inputs = selected_root_product_inputs(
            "subject",
            &"a".repeat(64),
            &source,
            &vec![auxiliary.clone()],
        )
        .unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], auxiliary);
        assert_eq!(
            inputs[1],
            selected_root_product_selection("subject", &"a".repeat(64), &source)
        );
        assert!(
            selected_root_product_inputs(
                "python",
                &"a".repeat(64),
                &source,
                &vec![auxiliary.clone()]
            )
            .is_err()
        );
        assert!(
            selected_root_product_inputs(
                "subject",
                &"a".repeat(64),
                &source,
                &vec![auxiliary.clone(), auxiliary.clone()]
            )
            .is_err()
        );
        auxiliary.target = ProductSelectionTarget::ContentDependency {
            binding: "python".into(),
        };
        assert!(
            selected_root_product_inputs("subject", &"a".repeat(64), &source, &vec![auxiliary])
                .is_err()
        );
    }

    #[test]
    fn auxiliary_request_requires_pinned_context_and_aggregate_selection_bounds() {
        let auxiliary = selected_root_product_selection(
            "python",
            &"b".repeat(64),
            &ProductWitnessSource::LocalCapture {},
        );
        let mut value = json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier",
            "project_context":null,
            "auxiliary_product_selections":[auxiliary]
        });
        let request: ProductQualificationLaunchRequest =
            serde_json::from_value(value.clone()).unwrap();
        assert!(request.validate().is_err());
        value["project_context"] = json!({"snapshot_hash":"d".repeat(64)});
        let request: ProductQualificationLaunchRequest = serde_json::from_value(value).unwrap();
        request.validate().unwrap();
        let source = ProductWitnessSource::LocalCapture {};
        let full: ProductSelectionInputs = (0
            ..ryeos_state::external_content::products::composition::MAX_PRODUCT_SELECTIONS)
            .map(|index| {
                selected_root_product_selection(
                    &format!("slot_{index:02}"),
                    &"b".repeat(64),
                    &source,
                )
            })
            .collect();
        validate_product_selection_inputs(&full).unwrap();
        assert!(selected_root_product_inputs("subject", &"a".repeat(64), &source, &full).is_err());
    }

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
    fn consumer_invocation_is_closed_typed_data_not_parameter_overrides() {
        let mut value = json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier",
            "project_context":null,
            "consumer_invocation": {
                "scenario_id":"codex-runtime",
                "qualification_operation_id":"b".repeat(64),
                "prerequisite_measurement_attempt_id":"c".repeat(64)
            }
        });
        let request: ProductQualificationLaunchRequest =
            serde_json::from_value(value.clone()).unwrap();
        request.validate().unwrap();
        assert_eq!(
            request.consumer_invocation.unwrap().scenario_id,
            "codex-runtime"
        );
        for field in [
            "parameters",
            "verifier_ref",
            "accepted_root_id",
            "maximum_duration_ms",
        ] {
            let mut forged = value.clone();
            forged["consumer_invocation"][field] = json!("caller-controlled");
            assert!(serde_json::from_value::<ProductQualificationLaunchRequest>(forged).is_err());
        }
        for field in [
            "scenario_id",
            "qualification_operation_id",
            "prerequisite_measurement_attempt_id",
        ] {
            let mut omitted = value.clone();
            omitted["consumer_invocation"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(serde_json::from_value::<ProductQualificationLaunchRequest>(omitted).is_err());
        }
        value["consumer_invocation"]["scenario_id"] = json!("../foreign");
        let request: ProductQualificationLaunchRequest = serde_json::from_value(value).unwrap();
        assert!(request.validate().is_err());
    }

    #[test]
    fn pinned_selector_uses_policy_inner_slot_not_outer_consumer_slot() {
        use std::fs;

        let outer_relationship_name = "runtime_to_qualified_runtime";
        let outer_consumer_declaration = "runtime";
        let workspace = ryeos_engine::test_support::workspace_root();
        let qualification = workspace.join("bundles/environment-qualification");
        let policy: serde_yaml::Value = serde_yaml::from_slice(
            &fs::read(
                qualification.join(".ai/config/ryeos/environments/qualification/gnu-python.yaml"),
            )
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
            &fs::read(
                qualification.join(".ai/tools/ryeos/environments/qualification/gnu-python.yaml"),
            )
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
