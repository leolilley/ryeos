use ryeos_app::bundle_publication::admitted_build::AdmittedReleaseInput;
use ryeos_bundle_publication_contract::BundlePayloadOwnership;
use serde_json::{Value, json};

const HANDLERS: &str = "ryeos-handler-bins";
const PARSERS: &str = "ryeos-parser-bins";
const TRIPLE: &str = "x86_64-unknown-linux-gnu";

fn package_for(binary: &str) -> &'static str {
    if binary.starts_with("rye-parser-") {
        PARSERS
    } else {
        HANDLERS
    }
}

/// Signed ownership naming one bundle's payloads with their Cargo features.
/// The implication table makes each graph or directive composition cover the
/// core and extends compositions.
fn ownership(bundle: &str, payloads: &[(&str, &[&str])]) -> BundlePayloadOwnership {
    let payloads: Vec<Value> = payloads
        .iter()
        .map(|(binary, features)| {
            json!({
                "binary": binary,
                "cargo_package": package_for(binary),
                "build_class": "release",
                "cargo_features": features,
            })
        })
        .collect();
    BundlePayloadOwnership::from_current_value(&json!({
        "schema": "ryeos.bundle_payload_ownership.v2",
        "kind": "bundle_payload_ownership",
        "host_packages": [],
        "no_default_features_packages": [HANDLERS],
        "feature_implications": {
            HANDLERS: {
                "directive-handlers": ["core-handlers", "extends-composer"],
                "graph-handlers": ["core-handlers", "extends-composer"],
            }
        },
        "data_only_bundles": [],
        "host_only_bundles": [],
        "bundles": [{
            "bundle_name": bundle,
            "bundle_sets": ["full"],
            "host_features": [],
            "payloads": payloads,
        }],
    }))
    .expect("ownership fixture is valid")
}

fn payload(bundle: &str, binary: &str, features: &[&str]) -> Value {
    json!({
        "bundle": bundle,
        "binary": binary,
        "cargo_package": package_for(binary),
        "build_class": "release",
        "bundle_sets": ["full"],
        "required_features": features,
    })
}

/// `handler_features` is the expected selection for the handler package; every
/// other package in the payloads is selected with no features.
fn input(
    owner: &BundlePayloadOwnership,
    bundle: &str,
    payloads: Vec<Value>,
    handler_features: &[&str],
) -> Value {
    let mut packages: Vec<&str> = payloads
        .iter()
        .map(|payload| match payload["cargo_package"].as_str() {
            Some(HANDLERS) => HANDLERS,
            Some(PARSERS) => PARSERS,
            other => panic!("fixture payload has unexpected package {other:?}"),
        })
        .collect();
    packages.sort_unstable();
    packages.dedup();
    let selections: Vec<Value> = packages
        .iter()
        .map(|package| {
            let features: &[&str] = if *package == HANDLERS {
                handler_features
            } else {
                &[]
            };
            json!({"cargo_package": package, "features": features})
        })
        .collect();
    json!({
        "schema": "ryeos.bundle_release_input_plan.v2",
        "project_path": "/source",
        "bundle_name": bundle,
        "authored_manifest": {
            "name": bundle,
            "version": "0.1.0",
            "provides_kinds": [],
            "requires_kinds": [],
        },
        "source_snapshot_hash": "a".repeat(64),
        "predecessor_generation_hash": null,
        "target": {"kind": "triple", "triple": TRIPLE},
        "build_profile": "release",
        "payload_ownership_item_ref": "config:bundle-release/payload-ownership",
        "payload_ownership_content_hash": owner.content_hash().unwrap(),
        "payloads": payloads,
        "cargo_packages": packages,
        "cargo_feature_selections": selections,
        "build_classes": if packages.is_empty() { vec![] } else { vec!["release"] },
        "requires_binary_build": !packages.is_empty(),
        "clean_output_required": true,
        "ambient_target_reuse_allowed": false,
    })
}

#[test]
fn parser_only_admission_selects_the_parser_package_without_features() {
    let owner = ownership("web", &[("rye-parser-yaml-document", &[])]);
    let value = input(
        &owner,
        "web",
        vec![payload("web", "rye-parser-yaml-document", &[])],
        &[],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&value, &owner).expect("parser input is valid");
    assert_eq!(admitted.payloads.len(), 1);
    assert!(admitted.payloads[0].required_features.is_empty());
    assert_eq!(admitted.cargo_feature_selections.len(), 1);
    assert_eq!(admitted.cargo_feature_selections[0].cargo_package, PARSERS);
    assert!(admitted.cargo_feature_selections[0].features.is_empty());
}

#[test]
fn core_and_mixed_admission_select_core_handlers() {
    let owner = ownership("web", &[("ryeos-direct-execution-evidence", &["core-handlers"])]);
    let full = input(
        &owner,
        "web",
        vec![payload(
            "web",
            "ryeos-direct-execution-evidence",
            &["core-handlers"],
        )],
        &["core-handlers"],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&full, &owner).expect("full input is valid");
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["core-handlers"]
    );

    // The parser stays in its own package and selection.
    let owner = ownership(
        "core",
        &[
            ("rye-parser-yaml-document", &[]),
            ("ryeos-direct-execution-evidence", &["core-handlers"]),
        ],
    );
    let mixed = input(
        &owner,
        "core",
        vec![
            payload("core", "rye-parser-yaml-document", &[]),
            payload(
                "core",
                "ryeos-direct-execution-evidence",
                &["core-handlers"],
            ),
        ],
        &["core-handlers"],
    );
    assert!(AdmittedReleaseInput::from_value(&mixed).is_err());
    let admitted =
        AdmittedReleaseInput::from_core_seed_value_with_ownership(&mixed, &owner).expect("mixed Core seed input is valid");
    let mut forged = mixed.clone();
    forged["payloads"][1]["required_features"] = json!(["graph-handlers"]);
    forged["cargo_feature_selections"][0]["features"] = json!(["graph-handlers"]);
    assert!(AdmittedReleaseInput::from_core_seed_value_with_ownership(&forged, &owner).is_err());
    assert_eq!(admitted.cargo_feature_selections.len(), 2);
    assert_eq!(admitted.cargo_feature_selections[0].cargo_package, HANDLERS);
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["core-handlers"]
    );
    assert_eq!(admitted.cargo_feature_selections[1].cargo_package, PARSERS);
    assert!(admitted.cargo_feature_selections[1].features.is_empty());
}

#[test]
fn graph_admission_selects_only_the_graph_composition() {
    let owner = ownership(
        "web",
        &[
            ("rye-composer-identity", &["core-handlers"]),
            ("ryeos-graph-launch-preparer", &["graph-handlers"]),
        ],
    );
    let value = input(
        &owner,
        "web",
        vec![
            payload("web", "rye-composer-identity", &["core-handlers"]),
            payload("web", "ryeos-graph-launch-preparer", &["graph-handlers"]),
        ],
        &["graph-handlers"],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&value, &owner).expect("workflow input is valid");
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["graph-handlers"]
    );
    let mut widened_core = value.clone();
    widened_core["payloads"][0]["required_features"] = json!(["graph-handlers"]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&widened_core, &owner).is_err());
    let mut missing_workflow = value;
    missing_workflow["cargo_feature_selections"][0]["features"] = json!(["core-handlers"]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&missing_workflow, &owner).is_err());
}

#[test]
fn independent_handler_roots_are_not_collapsed_into_each_other() {
    let owner = ownership(
        "web",
        &[
            ("rye-composer-extends-chain", &["extends-composer"]),
            ("rye-composer-identity", &["core-handlers"]),
            ("ryeos-directive-launch-preparer", &["directive-handlers"]),
            ("ryeos-graph-launch-preparer", &["graph-handlers"]),
        ],
    );
    let value = input(
        &owner,
        "web",
        vec![
            payload("web", "rye-composer-extends-chain", &["extends-composer"]),
            payload("web", "rye-composer-identity", &["core-handlers"]),
            payload(
                "web",
                "ryeos-directive-launch-preparer",
                &["directive-handlers"],
            ),
            payload("web", "ryeos-graph-launch-preparer", &["graph-handlers"]),
        ],
        &["directive-handlers", "graph-handlers"],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&value, &owner).expect("both independent roots are valid");
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["directive-handlers", "graph-handlers"]
    );
    for incomplete in ["directive-handlers", "graph-handlers"] {
        let mut altered = value.clone();
        altered["cargo_feature_selections"][0]["features"] = json!([incomplete]);
        assert!(AdmittedReleaseInput::from_value_with_ownership(&altered, &owner).is_err());
    }
    let owner = ownership(
        "web",
        &[
            ("rye-composer-extends-chain", &["extends-composer"]),
            ("rye-composer-identity", &["core-handlers"]),
        ],
    );
    let core_extends = input(
        &owner,
        "web",
        vec![
            payload("web", "rye-composer-extends-chain", &["extends-composer"]),
            payload("web", "rye-composer-identity", &["core-handlers"]),
        ],
        &["core-handlers", "extends-composer"],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&core_extends, &owner).unwrap();
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["core-handlers", "extends-composer"]
    );
}

#[test]
fn payload_and_aggregate_feature_tampering_are_rejected() {
    let owner = ownership("web", &[("rye-parser-yaml-document", &[])]);
    let valid = input(
        &owner,
        "web",
        vec![payload("web", "rye-parser-yaml-document", &[])],
        &[],
    );
    let admitted = AdmittedReleaseInput::from_value_with_ownership(&valid, &owner).expect("parser input is valid");

    let mut tampered_payload = valid.clone();
    tampered_payload["payloads"][0]["required_features"] = json!(["core-handlers"]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&tampered_payload, &owner).is_err());

    let mut tampered_aggregate = valid.clone();
    tampered_aggregate["cargo_feature_selections"][0]["features"] = json!(["core-handlers"]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&tampered_aggregate, &owner).is_err());

    let mut wrong_package_selection = valid;
    wrong_package_selection["cargo_feature_selections"][0]["cargo_package"] =
        json!("ryeos-web-tools");
    assert!(AdmittedReleaseInput::from_value(&wrong_package_selection).is_err());
}

#[test]
fn wrong_package_unknown_binary_and_unknown_features_are_rejected() {
    let owner = ownership(
        "web",
        &[
            ("rye-composer-identity", &["core-handlers"]),
            ("rye-parser-yaml-document", &[]),
        ],
    );

    // A parser claimed by the handler package is not the owned selection.
    let mut parser_in_handlers = input(
        &owner,
        "web",
        vec![payload("web", "rye-parser-yaml-document", &[])],
        &[],
    );
    parser_in_handlers["payloads"][0]["cargo_package"] = json!(HANDLERS);
    parser_in_handlers["cargo_packages"] = json!([HANDLERS]);
    parser_in_handlers["cargo_feature_selections"] =
        json!([{"cargo_package": HANDLERS, "features": []}]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&parser_in_handlers, &owner).is_err());

    // A handler binary claimed by another package is rejected.
    let mut handler_elsewhere = input(
        &owner,
        "web",
        vec![payload("web", "rye-composer-identity", &["core-handlers"])],
        &["core-handlers"],
    );
    handler_elsewhere["payloads"][0]["cargo_package"] = json!(PARSERS);
    handler_elsewhere["cargo_packages"] = json!([PARSERS]);
    handler_elsewhere["cargo_feature_selections"] =
        json!([{"cargo_package": PARSERS, "features": ["core-handlers"]}]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&handler_elsewhere, &owner).is_err());

    let unknown_binary = input(
        &owner,
        "web",
        vec![payload("web", "ryeos-unlisted-handler", &[])],
        &[],
    );
    assert!(AdmittedReleaseInput::from_value_with_ownership(&unknown_binary, &owner).is_err());

    let mut unknown_feature = input(
        &owner,
        "web",
        vec![payload("web", "rye-parser-yaml-document", &[])],
        &[],
    );
    unknown_feature["cargo_feature_selections"][0]["features"] = json!(["unlisted"]);
    assert!(AdmittedReleaseInput::from_value_with_ownership(&unknown_feature, &owner).is_err());
}

#[test]
fn duplicate_selections_stale_schema_and_unknown_fields_are_rejected() {
    let owner = ownership("web", &[("rye-parser-yaml-document", &[])]);
    let valid = input(
        &owner,
        "web",
        vec![payload("web", "rye-parser-yaml-document", &[])],
        &[],
    );

    let mut duplicate = valid.clone();
    duplicate["cargo_feature_selections"] = json!([
        {"cargo_package": PARSERS, "features": []},
        {"cargo_package": PARSERS, "features": []}
    ]);
    assert!(AdmittedReleaseInput::from_value(&duplicate).is_err());

    let mut stale_schema = valid.clone();
    stale_schema["schema"] = json!("ryeos.bundle_release_input_plan.v1");
    assert!(AdmittedReleaseInput::from_value(&stale_schema).is_err());

    let mut unknown_field = valid;
    unknown_field["unrecognized"] = json!(true);
    assert!(AdmittedReleaseInput::from_value(&unknown_field).is_err());
}

#[test]
fn core_seed_request_validation_enforces_the_ownership_features() {
    use ryeos_app::bundle_publication::core_seed::CoreSeedBuildRequest;
    let owner = ownership(
        "core",
        &[("ryeos-direct-execution-evidence", &["core-handlers"])],
    );
    let release_input = input(
        &owner,
        "core",
        vec![payload(
            "core",
            "ryeos-direct-execution-evidence",
            &["core-handlers"],
        )],
        &["core-handlers"],
    );
    let request = |release_input: Value| -> CoreSeedBuildRequest {
        serde_json::from_value(json!({
            "catalog_namespace": "official",
            "bundle_publication_policy_section_digest": "a".repeat(64),
            "trust_epoch": 1,
            "release_input": release_input,
        }))
        .unwrap()
    };
    assert!(request(release_input.clone()).validate_with_ownership(&owner).is_ok());
    let mut forged = release_input;
    forged["payloads"][0]["required_features"] = json!(["graph-handlers"]);
    forged["cargo_feature_selections"][0]["features"] = json!(["graph-handlers"]);
    assert!(request(forged).validate_with_ownership(&owner).is_err());
}
