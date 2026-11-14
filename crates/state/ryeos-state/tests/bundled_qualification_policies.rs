//! Exercise authored policy assets through the actual qualification reader.
//! A Rust-only fixture can pass while a shipped signed Config is unreadable.

use lillux::process_control::{HostCapabilities, ProofSource};
use ryeos_state::external_content::products::qualification::ProductQualificationPolicy;
use serde_json::Value;
use std::path::Path;

#[test]
fn bundled_qualification_policies_match_the_current_reader() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut policies = 0;
    for directory in [
        "bundles/environment-qualification/.ai/config/ryeos/environments/qualification",
        "bundles/bundle-release/.ai/config/bundle-release",
    ] {
        for entry in std::fs::read_dir(root.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let document: Value = serde_yaml::from_str(&source).unwrap();
            let Some(wire) = document.get("product_qualification_policy") else {
                continue;
            };
            ProductQualificationPolicy::from_value(wire)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            policies += 1;

            // Neither missing authority nor a predecessor schema acquires a
            // compatibility default when an authored Config is migrated.
            let mut missing_settlement = wire.clone();
            missing_settlement
                .as_object_mut()
                .unwrap()
                .remove("minimum_verifier_process_settlement");
            assert!(ProductQualificationPolicy::from_value(&missing_settlement).is_err());
            for predecessor_schema in [
                "ryeos.product_qualification_policy.v1",
                "ryeos.product_qualification_policy.v2",
                "ryeos.product_qualification_policy.v3",
                "ryeos.product_qualification_policy.v4",
            ] {
                let mut predecessor = wire.clone();
                predecessor["schema"] = predecessor_schema.into();
                assert!(ProductQualificationPolicy::from_value(&predecessor).is_err());
            }
        }
    }
    assert!(policies > 0, "no authored qualification policies checked");
}

fn codex_config(name: &str, key: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let path = root.join("bundles/codex/.ai/config/codex").join(name);
    let source = std::fs::read_to_string(&path).unwrap();
    let document: Value = serde_yaml::from_str(&source).unwrap();
    document
        .get(key)
        .unwrap_or_else(|| panic!("{} has no {key}", path.display()))
        .clone()
}

#[test]
fn shipped_codex_consumer_qualification_configs_agree_and_parse() {
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::producer_recipe::{
        ProducerExecutableSource, ProductProducerRecipe,
    };
    use ryeos_state::external_execution::admission::{
        QUALIFICATION_CONTEXT_PARAMETER, QUALIFICATION_CONTEXT_SCHEMA, REQUIRED_CLAIMS,
    };

    let policy = ProductQualificationPolicy::from_value(&codex_config(
        "consumer-qualification-policy.yaml",
        "product_qualification_policy",
    ))
    .unwrap();
    let relationships = ProductRelationships::from_value(codex_config(
        "guest-runtime-products.yaml",
        "product_relationships",
    ))
    .unwrap();

    // The worker edge names the populated policy and requires exactly the
    // claims program admission demands of it; the policy allows each of them
    // and carries the consumer context derived on the node that holds the
    // captured runtime.
    let worker = relationships
        .select("runtime_to_external_authoring_worker")
        .unwrap();
    assert_eq!(
        worker.qualification.policy_ref.as_deref(),
        Some("config:codex/consumer-qualification-policy")
    );
    assert_eq!(worker.qualification.required_claims, REQUIRED_CLAIMS);
    // One signed relationship serves every node: no sibling edge repeats the
    // Worker under another policy.
    assert_eq!(
        relationships
            .relationships
            .iter()
            .filter(|relationship| relationship.qualification.policy_ref.is_some())
            .count(),
        1
    );
    for claim in REQUIRED_CLAIMS {
        assert!(policy.allowed_claims.iter().any(|allowed| allowed == claim));
    }
    let context = &policy.verifier_parameters[QUALIFICATION_CONTEXT_PARAMETER];
    assert_eq!(context["schema"], QUALIFICATION_CONTEXT_SCHEMA);
    for field in [
        "profile_hash",
        "source_binding_hash",
        "source_content_manifest_hash",
        "execution_environment_digest",
        "provider_executable_manifest_hash",
        "requirement_digest",
    ] {
        assert_eq!(context[field].as_str().unwrap().len(), 64, "{field}");
    }

    // The qualification verifier's own edge stays unqualified so it can
    // inspect the subject before any qualification exists.
    let verifier = relationships
        .select("runtime_to_qualification_verifier")
        .unwrap();
    assert!(verifier.qualification.policy_ref.is_none());
    assert!(verifier.qualification.required_claims.is_empty());
    assert_eq!(verifier.consumer.canonical_ref, policy.verifier_ref);

    policy
        .consumer_execution_context
        .as_ref()
        .unwrap()
        .validate_relationship_consumer(&worker.consumer)
        .unwrap();
    assert_eq!(
        policy.subject_declaration_id,
        verifier.consumer.declaration_id
    );

    let scenario = policy.producer_scenarios.get("remote_codex").unwrap();
    assert_eq!(
        scenario.recipe_ref.as_deref(),
        Some("config:codex/consumer-qualification-recipe")
    );
    let recipe = ProductProducerRecipe::from_value(codex_config(
        "consumer-qualification-recipe.yaml",
        "product_producer_recipe",
    ))
    .unwrap();
    let ProducerExecutableSource::AdmittedRealizationMember {
        realization_id,
        relative_path,
        ..
    } = &recipe.executable_source
    else {
        panic!("consumer recipe must target the admitted subject executable");
    };
    assert_eq!(realization_id, &policy.subject_declaration_id);
    assert_eq!(relative_path, "bin/codex");
    let origin = policy.verifier_parameters["responses_origin"]
        .as_str()
        .unwrap();
    assert_eq!(
        recipe.loopback_ingress.as_ref().unwrap().address,
        origin.strip_prefix("http://").unwrap()
    );
}

fn core_protocol(name: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let path = root
        .join("bundles/core/.ai/protocols/ryeos/core")
        .join(format!("{name}.yaml"));
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn codex_item(relative: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let path = root.join("bundles/codex/.ai").join(relative);
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn shipped_consumer_policy_names_no_guarantees_and_no_settlement() {
    use ryeos_state::external_content::products::composition::ProductRelationships;

    let wire = codex_config(
        "consumer-qualification-policy.yaml",
        "product_qualification_policy",
    );
    assert!(wire.get("requires_guarantees").is_none());
    assert!(wire.get("minimum_verifier_process_settlement").is_none());
    let policy = ProductQualificationPolicy::from_value(&wire).unwrap();
    assert!(policy.minimum_verifier_process_settlement.is_none());
    let context = policy.consumer_execution_context.as_ref().unwrap();

    // A consumer policy takes its settlement from the selected protocol, so
    // naming one, or a node guarantee, is refused rather than ignored.
    let mut with_minimum = wire.clone();
    with_minimum["minimum_verifier_process_settlement"] = "containment_empty".into();
    assert!(ProductQualificationPolicy::from_value(&with_minimum).is_err());
    let mut with_guarantees = wire.clone();
    with_guarantees["requires_guarantees"] = serde_json::json!(["isolation:enforced"]);
    assert!(ProductQualificationPolicy::from_value(&with_guarantees).is_err());

    // Exactly one relationship joins the policy's Worker to the policy, which
    // is what the derivation service relies on.
    let relationships = ProductRelationships::from_value(codex_config(
        "guest-runtime-products.yaml",
        "product_relationships",
    ))
    .unwrap();
    let joined: Vec<_> = relationships
        .relationships
        .iter()
        .filter(|relationship| {
            relationship.consumer.canonical_ref == context.worker_ref
                && relationship.qualification.policy_ref.as_deref()
                    == Some("config:codex/consumer-qualification-policy")
        })
        .collect();
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].name, "runtime_to_external_authoring_worker");
}

#[test]
fn shipped_worker_and_verifier_list_their_protocol_pairs() {
    let worker = codex_item("workers/codex/external-hosted-authoring.yaml");
    assert_eq!(
        worker["execution_protocols"],
        serde_json::json!([
            "protocol:ryeos/core/contained_structured_session",
            "protocol:ryeos/core/trusted_structured_session",
        ])
    );
    // The selected protocol owns the filesystem authority; the Worker names
    // neither a single protocol nor an authority of its own.
    assert!(worker.get("execution_protocol").is_none());
    assert!(worker.get("filesystem_authority").is_none());

    let verifier = codex_item("tools/codex/qualification/consumer-runtime-verifier.yaml");
    assert_eq!(
        verifier["execution_protocols"],
        serde_json::json!([
            "protocol:ryeos/core/qualification_consumer_callback_contained",
            "protocol:ryeos/core/qualification_consumer_callback_trusted",
        ])
    );
    assert!(verifier.get("execution_protocol").is_none());
    assert!(verifier.get("filesystem_authority").is_none());
    // Neither item has a sibling for the other holding.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    assert!(
        !root
            .join("bundles/codex/.ai/workers/codex/external-hosted-authoring-trusted-lane.yaml")
            .exists()
    );
}

#[test]
fn each_shipped_protocol_holding_agrees_with_its_session_block_and_its_peer() {
    use lillux::process_control::CapabilityRequirement;
    use ryeos_state::external_content::products::qualification::VerifierProcessSettlementAuthority as Settlement;
    use ryeos_state::node_guarantees::{
        NodeGuarantees, unmet_guarantees, validate_required_guarantees,
    };

    let holding = |name: &str| -> (Vec<CapabilityRequirement>, String, Settlement) {
        let protocol = core_protocol(name);
        let holding = &protocol["holding"];
        let requires: Vec<CapabilityRequirement> =
            serde_json::from_value(holding["requires_guarantees"].clone()).unwrap();
        validate_required_guarantees(&requires).unwrap();
        (
            requires,
            holding["filesystem_authority"].as_str().unwrap().to_owned(),
            serde_json::from_value(holding["root_settlement"].clone()).unwrap(),
        )
    };
    let (contained, contained_authority, contained_settlement) =
        holding("contained_structured_session");
    let (trusted, trusted_authority, trusted_settlement) = holding("trusted_structured_session");

    // A session protocol's holding is implied by its own session block.
    let session = |name: &str| core_protocol(name)["session"].clone();
    let contained_session = session("contained_structured_session");
    assert_eq!(contained_session["cleanup_authority"], "descendant_containment");
    assert_eq!(contained_session["workspace_authority"], "ephemeral_scratch");
    assert_eq!(contained_authority, "captured_execution");
    assert_eq!(contained_settlement, Settlement::ContainmentEmpty);
    let trusted_session = session("trusted_structured_session");
    assert_eq!(trusted_session["cleanup_authority"], "trusted_process_group");
    assert_eq!(trusted_session["workspace_authority"], "runtime_workspace");
    assert_eq!(trusted_authority, "node_policy");
    assert_eq!(trusted_settlement, Settlement::TrustedProcessGroupAbsent);

    // The verifier's callback protocols carry the same holdings as the Worker's
    // session protocols, so one node selection serves both and they differ
    // exactly where the two holdings differ.
    let (callback_contained, authority, settlement) =
        holding("qualification_consumer_callback_contained");
    assert_eq!(callback_contained, contained);
    assert_eq!((authority.as_str(), settlement), ("captured_execution", contained_settlement));
    let (callback_trusted, authority, settlement) =
        holding("qualification_consumer_callback_trusted");
    assert_eq!(callback_trusted, trusted);
    assert_eq!((authority.as_str(), settlement), ("node_policy", trusted_settlement));
    assert_ne!(contained_authority, trusted_authority);
    assert!(!trusted_settlement.satisfies(contained_settlement));
    assert!(Settlement::ScopeEmpty.satisfies(contained_settlement));

    // The guarantees select between them: a contained node meets only the
    // contained holding and a trusted-session node only the trusted one.
    let mut contained_node = HostCapabilities::default();
    for name in [
        "isolation:enforced",
        "filesystem:captured_execution",
        "callback:captured_compatible",
        "termination:pid_namespace",
        "recovery:pid_namespace",
    ] {
        contained_node.provide(name, ProofSource::Measured, "test", "held");
    }
    for name in ["termination:scope", "recovery:scope", "sessions:trusted_process_group"] {
        contained_node.refuse(name, format!("{name} is not held"));
    }
    let mut trusted_node = HostCapabilities::default();
    trusted_node.provide(
        "sessions:trusted_process_group",
        ProofSource::Trusted,
        "test",
        "held",
    );
    for name in [
        "isolation:enforced",
        "filesystem:captured_execution",
        "callback:captured_compatible",
        "termination:scope",
        "recovery:scope",
        "termination:pid_namespace",
        "recovery:pid_namespace",
    ] {
        trusted_node.refuse(name, format!("{name} is not held"));
    }
    let unmet = |requires: &[CapabilityRequirement], node: &HostCapabilities| {
        unmet_guarantees(requires, NodeGuarantees::new(node, &[])).len()
    };
    assert_eq!(unmet(&contained, &contained_node), 0);
    assert!(unmet(&trusted, &contained_node) > 0);
    assert_eq!(unmet(&trusted, &trusted_node), 0);
    assert!(unmet(&contained, &trusted_node) > 0);
}

#[test]
fn shipped_verifier_contract_pins_one_worker_and_names_no_settlement() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let path = root.join("bundles/codex/.ai/tools/codex/qualification/consumer-runtime-verifier.yaml");
    let document: Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let contract: Value =
        serde_json::from_str(document["config"]["args"][2].as_str().unwrap()).unwrap();
    assert_eq!(contract["target"]["worker_ref"], "worker:codex/external-hosted-authoring");
    // The settlement the verifier root must reach comes from its selected
    // protocol's holding, not from its signed product contract.
    assert!(contract["semantic_checker"].get("required_root_settlement").is_none());
    assert!(contract["target"].get("required_root_settlement").is_none());
}
