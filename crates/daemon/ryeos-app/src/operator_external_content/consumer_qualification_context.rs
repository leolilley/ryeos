//! Read-only derivation of the consumer qualification context.
//!
//! A runtime-qualification policy must carry, in its verifier parameters, the
//! six digests that the admitted external candidate use will later reproduce.
//! Authoring them by hand would make the policy a selection; this operation
//! derives them from the same admitted Worker source, environment and runtime
//! product that qualification itself uses, and emits them as data. It writes no
//! node state: the staged content publication is dropped unpublished.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_state::external_content::products::composition::{
    ExternalProductSlotDeclaration, ProductRelationship, ProductRelationships,
};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerExecutionContext;
use serde::{Deserialize, Serialize};

use super::product_qualification::{
    AdmittedUseInputs, load_runtime_manifest_identity,
    prepare_current_bundle_consumer_content_inputs, realize_runtime_slot,
    require_unguarded_resolution, resolve_consumer_definition_in_generation,
    resolve_current_bundle_qualification_policy,
};
use crate::handler_context::HandlerContext;
use crate::state::AppState;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;

/// Inputs name the signed policy that carries the consumer execution context
/// (worker, environment, worker execution and runtime declaration), the signed
/// relationship that selects the runtime product, and the exact manifest of the
/// runtime product the consumer will be handed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerQualificationContextRequest {
    pub policy_ref: String,
    pub relationship_ref: String,
    pub relationship_name: String,
    pub subject_manifest_hash: String,
}

impl ConsumerQualificationContextRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if !lillux::valid_hash(&self.subject_manifest_hash) {
            bail!("subject_manifest_hash must be a 64-character lowercase hex digest");
        }
        for (field, value) in [
            ("policy_ref", &self.policy_ref),
            ("relationship_ref", &self.relationship_ref),
            ("relationship_name", &self.relationship_name),
        ] {
            if value.is_empty() {
                bail!("{field} must not be empty");
            }
        }
        Ok(())
    }
}

/// The emitted document. `verifier_parameters` is the fragment to merge into
/// the policy's `verifier_parameters`; `consumer_execution_context` is the
/// signed context the digests were derived under.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerQualificationContextResponse {
    pub policy_ref: String,
    pub relationship_name: String,
    pub subject_manifest_hash: String,
    pub bundle_generation_identity: String,
    pub verifier_parameters: serde_json::Value,
    pub consumer_execution_context: ProductQualificationConsumerExecutionContext,
}

pub async fn derive(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ConsumerQualificationContextRequest,
) -> anyhow::Result<ConsumerQualificationContextResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || derive_blocking(&state, &request))
        .await
        .context("consumer qualification context task stopped")?
}

fn derive_blocking(
    state: &AppState,
    request: &ConsumerQualificationContextRequest,
) -> anyhow::Result<ConsumerQualificationContextResponse> {
    require_unguarded_resolution("consumer qualification context derivation")?;
    let policy_source = resolve_current_bundle_qualification_policy(state, &request.policy_ref)?;
    let signed_context = policy_source
        .policy
        .consumer_execution_context
        .clone()
        .context("qualification policy has no consumer execution context")?;
    let relationship =
        resolve_relationship(state, &request.relationship_ref, &request.relationship_name)?;
    let prepared = prepare_current_bundle_consumer_content_inputs(
        state,
        &policy_source,
        &relationship,
        &request.relationship_ref,
    )?
    .context("qualification policy has no consumer execution context")?;
    let requirement = prepared
        .worker_source
        .profile
        .external_candidate_requirement()?
        .context("consumer Worker has no external candidate requirement")?;
    if requirement.runtime_product_declaration_id != signed_context.product_declaration_id {
        bail!("external Worker runtime product differs from the signed consumer context");
    }
    let slot = prepared
        .runtime_slot()
        .context("consumer Worker declares no runtime product slot")?;
    require_slot_matches_context(slot, &signed_context)?;
    let authority = state.state_store.pinned_state_authority()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let (_executable_hash, entry_count, total_bytes) = load_runtime_manifest_identity(
        &authority,
        limits,
        &request.subject_manifest_hash,
        &requirement.runtime_recipe.executable_relative_path,
    )?;
    let runtime = realize_runtime_slot(
        slot,
        &request.subject_manifest_hash,
        entry_count,
        total_bytes,
    );
    let qualification_use = AdmittedUseInputs {
        requirement: &requirement,
        profile: &prepared.worker_source.profile,
        source: &prepared.worker_source.source,
        worker_literals: &prepared.worker_literals,
        runtime,
        environment: &prepared.environment.realizations,
        executable_search: &prepared.environment.definition.executable_search,
        process_environment: &prepared.environment.definition.process_environment,
    }
    .derive()?;
    // Nothing was published: dropping the staged content leaves node state
    // exactly as it was.
    Ok(ConsumerQualificationContextResponse {
        policy_ref: request.policy_ref.clone(),
        relationship_name: request.relationship_name.clone(),
        subject_manifest_hash: request.subject_manifest_hash.clone(),
        bundle_generation_identity: prepared.definitions.bundle_generation_identity.clone(),
        verifier_parameters: qualification_use.parameters()?,
        consumer_execution_context: signed_context,
    })
}

/// The digests must come from the slot qualification itself realizes: the
/// one the signed context names as the runtime product.
fn require_slot_matches_context(
    slot: &ExternalProductSlotDeclaration,
    context: &ProductQualificationConsumerExecutionContext,
) -> anyhow::Result<()> {
    if slot.id != context.product_declaration_id {
        bail!("consumer runtime slot differs from the signed consumer context product");
    }
    Ok(())
}

fn resolve_relationship(
    state: &AppState,
    relationship_ref: &str,
    name: &str,
) -> anyhow::Result<ProductRelationship> {
    state.engine.with_checked_bundle_generation(|generation| {
        let authored =
            resolve_consumer_definition_in_generation(generation, relationship_ref, "config")?;
        let relationships = ProductRelationships::from_value(
            authored
                .composed
                .composed
                .get("product_relationships")
                .context("product relationship Config has no relationships")?
                .clone(),
        )?;
        Ok(relationships.select(name)?.clone())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use anyhow::{Result, ensure};
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{TrustStore, TrustedSigner};
    use ryeos_state::external_execution::admission::ExternalCandidateQualificationUse;
    use ryeos_state::external_execution::admission::test_support;
    use ryeos_state::objects::{
        ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization, ExternalContentRealizationSet,
    };

    use super::*;

    fn environment_realization() -> ExternalContentRealization {
        ExternalContentRealization {
            id: "environment".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "f".repeat(64),
            entry_count: 2,
            total_bytes: 12,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "environment".into(),
        }
    }

    fn derive_with(
        environment: &ExternalContentRealizationSet,
    ) -> Result<ExternalCandidateQualificationUse> {
        let requirement = test_support::fixture_requirement();
        let profile = test_support::fixture_profile(&requirement)?;
        let source = test_support::fixture_source_projection();
        let runtime = test_support::fixture_provider_realizations()?
            .iter()
            .next()
            .cloned()
            .context("fixture has no provider realization")?;
        AdmittedUseInputs {
            requirement: &requirement,
            profile: &profile,
            source: &source,
            worker_literals: &ExternalContentRealizationSet::new(vec![])?,
            runtime,
            environment,
            executable_search: &[],
            process_environment: &BTreeMap::new(),
        }
        .derive()
    }

    #[test]
    fn digests_are_deterministic_and_match_the_admitted_use() -> Result<()> {
        let empty = ExternalContentRealizationSet::new(vec![])?;
        let first = derive_with(&empty)?;
        let second = derive_with(&empty)?;
        ensure!(
            first == second,
            "same admitted inputs derived different digests"
        );
        ensure!(
            first == test_support::fixture_qualification_use(&test_support::fixture_requirement())?,
            "derivation differs from the admitted use constructor"
        );
        let parameters = first.parameters()?;
        let context = &parameters["external_candidate_qualification_context"];
        for field in [
            "requirement_digest",
            "profile_hash",
            "source_binding_hash",
            "source_content_manifest_hash",
            "provider_executable_manifest_hash",
            "execution_environment_digest",
        ] {
            ensure!(
                context[field].as_str().is_some_and(lillux::valid_hash),
                "{field} is not a 64-hex digest"
            );
        }
        let with_environment = derive_with(&ExternalContentRealizationSet::new(vec![
            environment_realization(),
        ])?)?;
        ensure!(
            with_environment.parameters()?["external_candidate_qualification_context"]["execution_environment_digest"]
                != context["execution_environment_digest"],
            "environment content did not change the environment digest"
        );
        Ok(())
    }

    #[test]
    fn environment_identity_that_contradicts_the_runtime_is_refused() -> Result<()> {
        let mut conflicting = environment_realization();
        conflicting.id = "codex".into();
        let error = derive_with(&ExternalContentRealizationSet::new(vec![conflicting])?)
            .err()
            .context("conflicting environment identity was accepted")?;
        ensure!(
            error
                .to_string()
                .contains("conflicts with inherited environment"),
            "unexpected refusal: {error}"
        );
        Ok(())
    }

    #[test]
    fn slot_that_is_not_the_signed_runtime_product_is_refused() -> Result<()> {
        let context = ProductQualificationConsumerExecutionContext {
            worker_ref: "worker:codex/external-hosted-authoring".into(),
            product_declaration_id: "guest-runtime".into(),
            environment_ref: "config:codex/environments/external-authoring".into(),
            worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
            environment_binding: "environment".into(),
        };
        let mut slot = ExternalProductSlotDeclaration {
            id: "guest-runtime".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            relationship: "runtime_to_external_authoring_worker".into(),
            kind: ExternalContentKind::Tree,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "codex-guest-runtime".into(),
        };
        require_slot_matches_context(&slot, &context)?;
        slot.id = "other-runtime".into();
        ensure!(
            require_slot_matches_context(&slot, &context).is_err(),
            "a different slot was accepted"
        );
        Ok(())
    }

    #[test]
    fn request_refuses_a_malformed_manifest_digest() {
        let request = ConsumerQualificationContextRequest {
            policy_ref: "config:test/consumer-qualification".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            relationship_name: "runtime_to_external_authoring_worker".into(),
            subject_manifest_hash: "not-a-digest".into(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn absent_environment_definition_is_refused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (_state_owner, mut state) = crate::state::test_support::build(root.path())?;
        let core = ryeos_engine::test_support::core_bundle_root();
        let workflow = ryeos_engine::test_support::workflow_bundle_roots();
        let codex = ryeos_engine::test_support::workspace_root().join("bundles/codex");
        let test_bundle = root.path().join("test-bundle");
        let policy_path = test_bundle.join(".ai/config/test/consumer-qualification.yaml");
        fs::create_dir_all(policy_path.parent().unwrap())?;
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let policy = r#"category: test
version: "1.0.0"
product_qualification_policy:
  schema: ryeos.product_qualification_policy.v4
  verifier_ref: tool:fixtures/qualify_runtime
  subject_declaration_id: guest-runtime
  allowed_claims: [runtime_qualified]
  minimum_verifier_process_settlement: scope_empty
  verifier_parameters: {}
  consumer_execution_context:
    worker_ref: worker:codex/external-hosted-authoring
    product_declaration_id: guest-runtime
    environment_ref: config:codex/environments/absent
    worker_execution_ref: worker_execution:codex/bounded-turn
    environment_binding: environment
"#;
        fs::write(
            &policy_path,
            lillux::signature::sign_content(policy, &signer, "#", None),
        )?;
        let mut roots = vec![core.clone()];
        roots.extend(workflow.iter().cloned());
        roots.push(codex);
        roots.push(test_bundle);
        let mut trust = ryeos_engine::test_support::live_trust_store();
        let verifier = signer.verifying_key();
        trust.extend_from(&TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: lillux::signature::compute_fingerprint(&verifier),
            verifying_key: verifier,
            label: Some("consumer context fixture".into()),
        }]));
        let mut kind_roots = vec![core.join(".ai/node/engine/kinds")];
        kind_roots.extend(
            workflow
                .iter()
                .map(|root| root.join(".ai/node/engine/kinds")),
        );
        let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(&kind_roots, &trust)?;
        let (parsers, _) =
            ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
        let handlers = ryeos_engine::test_support::load_live_handler_registry();
        let dispatcher =
            ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
        let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
        let registered = ["core", "knowledge", "directive", "graph", "codex", "test"]
            .into_iter()
            .zip(roots.iter().cloned())
            .map(
                |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                    name: name.to_owned(),
                    canonical_root,
                },
            )
            .collect();
        state.engine = Arc::new(
            ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
                .with_trust_store(trust.clone())
                .with_node_trust_store(trust)
                .with_composers(composers)
                .with_registered_bundle_roots(registered),
        );
        let request = ConsumerQualificationContextRequest {
            policy_ref: "config:test/consumer-qualification".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            relationship_name: "runtime_to_external_authoring_worker".into(),
            subject_manifest_hash: "a".repeat(64),
        };
        let refusal = derive_blocking(&state, &request)
            .err()
            .context("a missing environment definition was admitted")?;
        ensure!(
            refusal
                .to_string()
                .contains("config:codex/environments/absent"),
            "refusal does not name the absent environment: {refusal:#}"
        );
        Ok(())
    }
}
