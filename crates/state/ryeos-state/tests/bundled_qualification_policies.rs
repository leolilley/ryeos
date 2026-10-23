//! Exercise authored policy assets through the actual qualification reader.
//! A Rust-only fixture can pass while a shipped signed Config is unreadable.

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
    use ryeos_state::external_execution::admission::REQUIRED_CLAIMS;

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

    // The worker edge ships unqualified: it names the policy only once the
    // derived consumer context has been merged into that policy on a node
    // holding the captured runtime. The policy must already allow every
    // claim program admission will require of that edge.
    let worker = relationships
        .select("runtime_to_external_authoring_worker")
        .unwrap();
    assert!(worker.qualification.policy_ref.is_none());
    assert!(worker.qualification.required_claims.is_empty());
    for claim in REQUIRED_CLAIMS {
        assert!(policy.allowed_claims.iter().any(|allowed| allowed == claim));
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
