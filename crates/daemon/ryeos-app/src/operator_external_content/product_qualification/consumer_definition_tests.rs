use std::{fs, sync::Arc};

use anyhow::{Result, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{TrustStore, TrustedSigner};
use ryeos_state::external_content::products::composition::ProductRelationships;
use ryeos_state::external_content::products::qualification::{
    ProductQualificationConsumerDefinitionIdentity, ProductQualificationConsumerExecutionContext,
};

use super::{
    admit_current_bundle_consumer_worker, consumer_definition_identity,
    prepare_current_bundle_consumer_environment, require_consumer_worker_matches_definitions,
    resolve_consumer_definition_in_generation, resolve_current_bundle_consumer_definitions,
    resolve_current_bundle_consumer_environment_definition,
    resolve_current_bundle_qualification_policy,
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
    let environment = resolve_current_bundle_consumer_environment_definition(
        &state,
        &policy_source,
        &relationship,
    )?
    .ok_or_else(|| anyhow::anyhow!("signed consumer environment definition absent"))?;
    ensure!(
        environment.definitions == definitions
            && environment.declarations.len() == 1
            && environment.declarations[0].id == "authoring-tools"
            && environment.executable_search.len() == 1
            && environment.executable_search[0].realization_id == "authoring-tools"
            && environment.executable_search[0].relative_directory == "bin"
            && environment.process_environment.contains_key("TMPDIR"),
        "signed consumer environment definition did not match its executable closure"
    );
    let unavailable =
        prepare_current_bundle_consumer_environment(&state, &policy_source, &relationship)
            .err()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "uninstalled authoring-tools content was admitted as an exact realization"
                )
            })?;
    ensure!(
        unavailable.to_string().contains("no retained manifest"),
        "consumer environment refused before its external-content admission boundary: {unavailable}"
    );
    let (joined, worker) =
        admit_current_bundle_consumer_worker(&state, &policy_source, &relationship)?
            .ok_or_else(|| anyhow::anyhow!("signed consumer Worker admission absent"))?;
    ensure!(
        joined == definitions
            && worker.bundle_generation_identity == joined.bundle_generation_identity
            && worker.signed_effective_definition_digest
                == joined.worker.effective_definition_digest
            && worker.preselection_effective_definition_digest
                != worker.signed_effective_definition_digest
            && lillux::valid_hash(&worker.source.binding_hash)
            && lillux::valid_hash(&worker.source.content_manifest_hash),
        "signed consumer Worker source did not join its definitions"
    );
    let mutations: [fn(&mut ProductQualificationConsumerDefinitionIdentity); 4] = [
        |identity: &mut ProductQualificationConsumerDefinitionIdentity| {
            identity.bundle_generation_identity = "a".repeat(64)
        },
        |identity: &mut ProductQualificationConsumerDefinitionIdentity| {
            identity.worker.raw_content_digest = "a".repeat(64)
        },
        |identity: &mut ProductQualificationConsumerDefinitionIdentity| {
            identity.worker.publisher_fingerprint = "a".repeat(64)
        },
        |identity: &mut ProductQualificationConsumerDefinitionIdentity| {
            identity.worker.effective_definition_digest = "a".repeat(64)
        },
    ];
    for mutate in mutations {
        let mut mismatch = joined.clone();
        mutate(&mut mismatch);
        ensure!(
            require_consumer_worker_matches_definitions(&mismatch, &worker).is_err(),
            "changed consumer Worker authority was accepted"
        );
    }
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
    ensure!(
        resolve_current_bundle_consumer_environment_definition(&state, &changed, &relationship)
            .is_err(),
        "changed policy admitted consumer environment intent"
    );
    let mut wrong_relationship = relationship.clone();
    wrong_relationship.consumer.declaration_id = "other-runtime".into();
    ensure!(
        resolve_current_bundle_consumer_definitions(&state, &policy_source, &wrong_relationship)
            .is_err(),
        "wrong consumer slot was admitted against the signed qualification policy"
    );
    ensure!(
        resolve_current_bundle_consumer_environment_definition(
            &state,
            &policy_source,
            &wrong_relationship,
        )
        .is_err(),
        "wrong consumer slot admitted consumer environment intent"
    );
    Ok(())
}

#[test]
fn signed_consumer_environment_requires_its_exact_retained_binding() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut state = crate::state::test_support::build(root.path())?;
    state.node_policy = Arc::new(crate::node_policy::NodePolicySnapshot::from_test_records(
        vec![Arc::new(
            crate::node_policy::sections::object_closure::NodeObjectClosurePolicy {
                schema: 1,
                max_roots: 256,
                max_objects: 32_768,
                max_blobs: 32_768,
                max_object_bytes: 32 * 1024 * 1024,
                max_total_object_bytes: 64 * 1024 * 1024,
                max_blob_bytes: 128 * 1024 * 1024,
                max_total_blob_bytes: 128 * 1024 * 1024,
                max_response_bytes: 256 * 1024 * 1024,
                max_links_per_object: 100_000,
                local_verification: None,
            },
        )],
    ));
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let blob = cas.store_blob(b"tool fixture")?;
    let manifest =
        ryeos_state::objects::ExternalContentManifestObject::from_value(&serde_json::json!({
            "schema": ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
            "kind": ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            "entry_count": 2,
            "total_bytes": 12,
            "entries": [
                {"path": "bin", "kind": "dir"},
                {"path": "bin/tool", "kind": "file", "mode": 493,
                 "blob_hash": blob, "size": 12}
            ]
        }))?;
    let manifest_hash = cas.store_object(&serde_json::to_value(&manifest)?)?;

    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let codex = ryeos_engine::test_support::workspace_root().join("bundles/codex");
    let test_bundle = root.path().join("test-bundle");
    let signer = SigningKey::from_bytes(&[7u8; 32]);
    let environment_path = test_bundle.join(".ai/config/test/external-authoring.yaml");
    fs::create_dir_all(environment_path.parent().unwrap())?;
    let environment = format!(
        r#"category: test
version: "1.0.0"
schema: ryeos.worker_environment.v6
external_product_slots: []
worker_ref: worker:codex/external-hosted-authoring
external_content:
  - id: authoring-tools
    kind: tree
    mode: pinned
    digest: {manifest_hash}
    mount_root: execution_runtime
    mount: authoring-tools
configuration:
  executable_search:
    - realization_id: authoring-tools
      relative_directory: bin
  process_environment:
    TZ: {{kind: literal, value: UTC}}
credential_requirement:
  workload_family: codex
  required_state: active
  subject_projection_contract: codex.account.v1
portable_state_contract: ryeos.worker_session.restore.v1
workload_client: null
"#
    );
    fs::write(
        &environment_path,
        lillux::signature::sign_content(&environment, &signer, "#", None),
    )?;
    let policy_path = test_bundle.join(".ai/config/test/consumer-qualification.yaml");
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
    environment_ref: config:test/external-authoring
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
        label: Some("consumer environment fixture".into()),
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
    let relationship = state.engine.with_checked_bundle_generation(|generation| {
        let authored = resolve_consumer_definition_in_generation(
            generation,
            "config:codex/guest-runtime-products",
            "config",
        )?;
        let relationships = ProductRelationships::from_value(
            authored.composed.composed["product_relationships"].clone(),
        )?;
        Ok::<_, anyhow::Error>(
            relationships
                .select("runtime_to_external_authoring_worker")?
                .clone(),
        )
    })?;
    let policy_source =
        resolve_current_bundle_qualification_policy(&state, "config:test/consumer-qualification")?;
    let consumer = state.engine.with_checked_bundle_generation(|generation| {
        let resolution = resolve_consumer_definition_in_generation(
            generation,
            "config:test/external-authoring",
            "config",
        )?;
        crate::external_content_admission::consumer_authority(
            &resolution,
            &ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        )
    })?;
    let operator = crate::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
    crate::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &["ryeos.execute.service.node/status".into()],
        "fixture operator",
        "fixture",
        "2026-01-01T00:00:00Z",
        state.identity.signing_key(),
        crate::identity::WildcardPolicy::Reject,
    )?;
    let grant = crate::operator_authority::admitted_operator_authority_digest(
        &state,
        operator.fingerprint(),
    )?;
    let wrong_consumer = ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
        "config:test/other".into(),
        consumer.publisher_fingerprint().into(),
    )?;
    let wrong = ryeos_state::objects::ExternalContentBinding::active(
        manifest_hash.clone(),
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        wrong_consumer,
        state.identity.fingerprint().into(),
        operator.fingerprint().into(),
        grant.clone(),
    )?;
    let guard = authority.acquire_exclusive_guard(true)?;
    let wrong_hash = cas.store_object(&wrong.to_value()?)?;
    let node_signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.ensure_current_external_content_binding_epoch(&guard)?;
        db.advance_generic_head_ref(
            ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
            &wrong.binding_subject_id,
            &wrong_hash,
            None,
            &node_signer,
            &guard,
        )
    })?;
    drop(guard);
    let wrong_binding =
        prepare_current_bundle_consumer_environment(&state, &policy_source, &relationship)
            .err()
            .ok_or_else(|| anyhow::anyhow!("wrong consumer binding admitted the environment"))?;
    ensure!(
        wrong_binding
            .downcast_ref::<crate::external_content_admission::ExternalContentBindingUnavailable>()
            .is_some(),
        "wrong consumer binding did not fail at binding admission: {wrong_binding}"
    );
    let binding = ryeos_state::objects::ExternalContentBinding::active(
        manifest_hash.clone(),
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        consumer,
        state.identity.fingerprint().into(),
        operator.fingerprint().into(),
        grant,
    )?;
    let guard = authority.acquire_exclusive_guard(true)?;
    let binding_hash = cas.store_object(&binding.to_value()?)?;
    state.state_store.with_state_db(|db| {
        db.advance_generic_head_ref(
            ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
            &binding.binding_subject_id,
            &binding_hash,
            None,
            &node_signer,
            &guard,
        )
    })?;
    drop(guard);
    let prepared =
        prepare_current_bundle_consumer_environment(&state, &policy_source, &relationship)?
            .ok_or_else(|| anyhow::anyhow!("signed consumer environment was absent"))?;
    ensure!(
        prepared.admitted().realizations.iter().len() == 1
            && prepared
                .admitted()
                .realizations
                .iter()
                .next()
                .unwrap()
                .manifest_hash
                == manifest_hash
            && prepared
                .admitted()
                .definition
                .definitions
                .environment
                .canonical_ref
                == "config:test/external-authoring"
            && lillux::valid_hash(&prepared.admitted().realized_effective_definition_digest),
        "exact signed consumer environment did not reach staged admission"
    );
    // No launch purpose owns these roots yet: dropping the staging is correct.
    drop(prepared);
    Ok(())
}
