use std::{fs, sync::Arc};

use anyhow::{Result, ensure};
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{TrustStore, TrustedSigner};
use ryeos_state::external_content::products::composition::ProductRelationships;
use ryeos_state::external_content::products::qualification::{
    ProductQualificationConsumerDefinitionIdentity, ProductQualificationConsumerExecutionContext,
};

use super::{
    consumer_definition_identity, resolve_consumer_definition_in_generation,
    resolve_current_bundle_consumer_definitions, resolve_current_bundle_qualification_policy,
};

#[test]
fn signed_codex_consumer_definitions_share_one_checked_bundle_generation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut state = crate::state::test_support::build(root.path())?;
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let codex = ryeos_engine::test_support::workspace_root().join("bundles/codex");
    let test_bundle = root.path().join("test-bundle");
    let policy_path = test_bundle.join(".ai/config/test/consumer-qualification.yaml");
    fs::create_dir_all(policy_path.parent().unwrap())?;
    let signer = SigningKey::from_bytes(&[7u8; 32]);
    let policy = r#"category: test
version: "1.0.0"
product_qualification_policy:
  schema: ryeos.product_qualification_policy.v2
  verifier_ref: tool:fixtures/qualify_runtime
  subject_declaration_id: guest-runtime
  allowed_claims: [runtime_qualified]
  minimum_verifier_process_settlement: scope_empty
  verifier_parameters: {}
  consumer_execution_context:
    worker_ref: worker:codex/external-hosted-authoring
    product_declaration_id: guest-runtime
    environment_ref: config:codex/environments/external-authoring
    worker_execution_ref: worker_execution:codex/bounded-turn
    environment_binding: environment
"#;
    fs::write(
        &policy_path,
        lillux::signature::sign_content(policy, &signer, "#", None),
    )?;
    let roots = vec![core.clone(), standard.clone(), codex, test_bundle];
    let mut trust = ryeos_engine::test_support::live_trust_store();
    let verifier = signer.verifying_key();
    trust.extend_from(&TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: lillux::signature::compute_fingerprint(&verifier),
        verifying_key: verifier,
        label: Some("consumer qualification fixture".into()),
    }]));
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )?;
    let (parsers, _) = ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let dispatcher = ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
    let registered = ["core", "standard", "codex", "test"]
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

    state.engine.with_checked_bundle_generation(|generation| {
        let worker = resolve_consumer_definition_in_generation(
            generation,
            "worker:codex/external-hosted-authoring",
            "worker",
        )?;
        let environment = resolve_consumer_definition_in_generation(
            generation,
            "config:codex/environments/external-authoring",
            "config",
        )?;
        let execution = resolve_consumer_definition_in_generation(
            generation,
            "worker_execution:codex/bounded-turn",
            "worker_execution",
        )?;
        let worker_identity = consumer_definition_identity(&worker)?;
        let environment_identity = consumer_definition_identity(&environment)?;
        let execution_identity = consumer_definition_identity(&execution)?;
        let signed_context = ProductQualificationConsumerExecutionContext {
            worker_ref: "worker:codex/external-hosted-authoring".into(),
            product_declaration_id: "guest-runtime".into(),
            environment_ref: "config:codex/environments/external-authoring".into(),
            worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
            environment_binding: "environment".into(),
        };
        let definitions = ProductQualificationConsumerDefinitionIdentity {
            bundle_generation_identity: generation.request_engine_generation_identity().into(),
            worker: worker_identity,
            environment: environment_identity,
            worker_execution: execution_identity,
        };
        definitions.validate_for(&signed_context)?;
        ensure!(
            !generation.request_engine_generation_identity().is_empty(),
            "Codex consumer definitions did not resolve from one signed Bundle generation"
        );
        ensure!(
            resolve_consumer_definition_in_generation(
                generation,
                "config:codex/environments/external-authoring",
                "worker",
            )
            .is_err()
                && resolve_consumer_definition_in_generation(
                    generation,
                    "worker:codex/missing",
                    "worker",
                )
                .is_err(),
            "wrong-kind or absent consumer definition was admitted"
        );
        Ok(())
    })?;

    let relationship = state.engine.with_checked_bundle_generation(|generation| {
        let authored = resolve_consumer_definition_in_generation(
            generation,
            "config:codex/guest-runtime-products",
            "config",
        )?;
        let relationships = ProductRelationships::from_value(
            authored
                .composed
                .composed
                .get("product_relationships")
                .ok_or_else(|| anyhow::anyhow!("signed Codex product relationships absent"))?
                .clone(),
        )?;
        Ok::<_, anyhow::Error>(
            relationships
                .select("runtime_to_external_authoring_worker")?
                .clone(),
        )
    })?;
    let policy_source =
        resolve_current_bundle_qualification_policy(&state, "config:test/consumer-qualification")?;
    let definitions =
        resolve_current_bundle_consumer_definitions(&state, &policy_source, &relationship)?
            .ok_or_else(|| anyhow::anyhow!("signed consumer definitions absent"))?;
    definitions.validate_for(
        policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .unwrap(),
    )?;
    let mut changed = policy_source.clone();
    changed
        .policy
        .consumer_execution_context
        .as_mut()
        .unwrap()
        .environment_ref = "config:codex/environments/other".into();
    ensure!(
        resolve_current_bundle_consumer_definitions(&state, &changed, &relationship).is_err(),
        "changed policy was admitted against the current signed Bundle generation"
    );
    let mut wrong_relationship = relationship.clone();
    wrong_relationship.consumer.declaration_id = "other-runtime".into();
    ensure!(
        resolve_current_bundle_consumer_definitions(&state, &policy_source, &wrong_relationship)
            .is_err(),
        "wrong consumer slot was admitted against the signed qualification policy"
    );
    Ok(())
}
