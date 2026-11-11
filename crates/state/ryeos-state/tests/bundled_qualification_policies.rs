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

fn node(held: &[(&str, ProofSource)]) -> HostCapabilities {
    let mut view = HostCapabilities::default();
    for (name, source) in held {
        view.provide(name, source.clone(), "test", "held");
    }
    for name in [
        "isolation:enforced",
        "filesystem:captured_execution",
        "callback:captured_compatible",
        "termination:scope",
        "recovery:scope",
        "termination:pid_namespace",
        "recovery:pid_namespace",
        "sessions:trusted_process_group",
    ] {
        view.refuse(name, format!("{name} is not held"));
    }
    view
}

fn contained_node() -> HostCapabilities {
    node(&[
        ("isolation:enforced", ProofSource::Measured),
        ("filesystem:captured_execution", ProofSource::Measured),
        ("callback:captured_compatible", ProofSource::Measured),
        ("termination:pid_namespace", ProofSource::Measured),
        ("recovery:pid_namespace", ProofSource::Measured),
    ])
}

#[test]
fn shipped_codex_variants_select_by_held_guarantees_and_agree_with_their_policies() {
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::qualification::VerifierProcessSettlementAuthority;
    use ryeos_state::external_content::products::qualification_variants::{
        NodeGuarantees, QualificationVariants, require_policy_guarantees,
    };

    let variants = QualificationVariants::from_value(&codex_config(
        "qualification-variants.yaml",
        "qualification_variants",
    ))
    .unwrap();
    let relationships = ProductRelationships::from_value(codex_config(
        "guest-runtime-products.yaml",
        "product_relationships",
    ))
    .unwrap();
    let contained = contained_node();
    let trusted = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
    for (node, settlement) in [
        (
            &contained,
            VerifierProcessSettlementAuthority::ContainmentEmpty,
        ),
        (
            &trusted,
            VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent,
        ),
    ] {
        let variant = variants.select(NodeGuarantees::new(node, &[])).unwrap();
        let file = format!(
            "{}.yaml",
            variant.policy_ref.strip_prefix("config:codex/").unwrap()
        );
        let policy = ProductQualificationPolicy::from_value(&codex_config(
            &file,
            "product_qualification_policy",
        ))
        .unwrap();
        // The policy repeats exactly the guarantees its variant requires, so
        // the node that selected it also admits it, and no other node does.
        assert_eq!(policy.requires_guarantees, variant.requires_guarantees);
        assert_eq!(policy.minimum_verifier_process_settlement, settlement);
        require_policy_guarantees(
            &policy.requires_guarantees,
            NodeGuarantees::new(node, &[]),
            &variant.policy_ref,
        )
        .unwrap();
        let other = if std::ptr::eq(node, &contained) {
            &trusted
        } else {
            &contained
        };
        assert!(
            require_policy_guarantees(
                &policy.requires_guarantees,
                NodeGuarantees::new(other, &[]),
                &variant.policy_ref
            )
            .is_err()
        );
        let worker = relationships.select(&variant.relationship_name).unwrap();
        assert_eq!(
            worker.qualification.policy_ref.as_deref(),
            Some(variant.policy_ref.as_str())
        );
        policy
            .consumer_execution_context
            .as_ref()
            .unwrap()
            .validate_relationship_consumer(&worker.consumer)
            .unwrap();
    }
    assert_eq!(
        variants
            .select(NodeGuarantees::new(&contained, &[]))
            .unwrap()
            .relationship_name,
        "runtime_to_external_authoring_worker"
    );
    assert_eq!(
        variants
            .select(NodeGuarantees::new(&trusted, &[]))
            .unwrap()
            .relationship_name,
        "runtime_to_external_authoring_worker_trusted_lane"
    );
}

#[test]
fn shipped_codex_variants_refuse_a_node_holding_neither_and_an_ambiguous_one() {
    use ryeos_state::external_content::products::qualification_variants::{
        NodeGuarantees, QualificationVariants,
    };

    let variants = QualificationVariants::from_value(&codex_config(
        "qualification-variants.yaml",
        "qualification_variants",
    ))
    .unwrap();
    // Enforced isolation without a qualified containment mechanism, and no
    // trusted-session acceptance, holds neither variant's guarantees.
    let uncontained = node(&[
        ("isolation:enforced", ProofSource::Measured),
        ("filesystem:captured_execution", ProofSource::Measured),
        ("callback:captured_compatible", ProofSource::Measured),
    ]);
    let error = variants
        .select(NodeGuarantees::new(&uncontained, &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("no qualification variant"), "{error}");
    assert!(
        error.contains("config:codex/consumer-qualification-policy "),
        "{error}"
    );
    assert!(
        error.contains("any_of(termination:scope, termination:pid_namespace)"),
        "{error}"
    );
    assert!(
        error.contains("sessions:trusted_process_group is not held"),
        "{error}"
    );
    // A node that holds both sets of guarantees cannot choose.
    let mut both = contained_node();
    both.provide(
        "sessions:trusted_process_group",
        ProofSource::Trusted,
        "test",
        "held",
    );
    let error = variants
        .select(NodeGuarantees::new(&both, &[]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("more than one"), "{error}");
    assert!(
        error.contains("consumer-qualification-policy-trusted-lane"),
        "{error}"
    );
    // A weaker grade of the same name does not stand in.
    let mut attested = contained_node();
    attested.provide(
        "isolation:enforced",
        ProofSource::ProviderAttested,
        "test",
        "held",
    );
    assert!(
        variants
            .select(NodeGuarantees::new(&attested, &[]))
            .is_err()
    );
}

#[test]
fn each_shipped_variant_requires_one_settlement_through_its_whole_chain() {
    use ryeos_state::external_content::products::qualification::VerifierProcessSettlementAuthority::{
        self as Settlement, ContainmentEmpty, ScopeEmpty, TrustedProcessGroupAbsent,
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let tool = |name: &str| -> Value {
        let path = root
            .join("bundles/codex/.ai/tools/codex/qualification")
            .join(name);
        let document: Value =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        serde_json::from_str(document["config"]["args"][2].as_str().unwrap()).unwrap()
    };
    // (policy, verifier Tool, Worker, the settlements its node can reach)
    let chains: [(&str, &str, &str, &[Settlement]); 2] = [
        (
            "consumer-qualification-policy.yaml",
            "consumer-runtime-verifier.yaml",
            "worker:codex/external-hosted-authoring",
            &[ContainmentEmpty, ScopeEmpty],
        ),
        (
            "consumer-qualification-policy-trusted-lane.yaml",
            "consumer-runtime-verifier-trusted-lane.yaml",
            "worker:codex/external-hosted-authoring-trusted-lane",
            &[TrustedProcessGroupAbsent],
        ),
    ];
    let parse = |value: &Value| -> Settlement { serde_json::from_value(value.clone()).unwrap() };
    let mut minimums = Vec::new();
    for (policy_file, verifier, worker, reachable) in chains {
        let policy = codex_config(policy_file, "product_qualification_policy");
        let contract = tool(verifier);
        assert_eq!(contract["target"]["worker_ref"], worker);
        let policy_minimum = parse(&policy["minimum_verifier_process_settlement"]);
        let checker_minimum = parse(&contract["semantic_checker"]["required_root_settlement"]);
        // The policy and the checker root require the same settlement, and
        // every settlement the variant's node can reach meets both.
        assert_eq!(policy_minimum, checker_minimum, "{policy_file}");
        for reached in reachable {
            assert!(
                reached.satisfies(policy_minimum),
                "{policy_file}: {reached:?}"
            );
            assert!(
                reached.satisfies(checker_minimum),
                "{policy_file}: {reached:?}"
            );
        }
        minimums.push((policy_minimum, reachable));
    }
    // A settlement from the other variant's mechanism is refused.
    let (contained_minimum, contained) = &minimums[0];
    let (trusted_minimum, trusted) = &minimums[1];
    for reached in *trusted {
        assert!(!reached.satisfies(*contained_minimum), "{reached:?}");
    }
    let _ = contained;
    assert_ne!(contained_minimum, trusted_minimum);
}
