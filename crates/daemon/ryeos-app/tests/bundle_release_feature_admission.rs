use ryeos_app::bundle_publication::admitted_build::AdmittedReleaseInput;
use serde_json::{Value, json};

const HANDLERS: &str = "ryeos-handler-bins";
const TRIPLE: &str = "x86_64-unknown-linux-gnu";

fn payload(bundle: &str, binary: &str, features: &[&str]) -> Value {
    json!({
        "bundle": bundle,
        "binary": binary,
        "cargo_package": HANDLERS,
        "build_class": "release",
        "bundle_sets": ["full"],
        "required_features": features,
    })
}

fn input(bundle: &str, payloads: Vec<Value>, features: &[&str]) -> Value {
    let packages = if payloads.is_empty() {
        vec![]
    } else {
        vec![HANDLERS]
    };
    let selections = if features.is_empty() {
        vec![]
    } else {
        vec![json!({"cargo_package": HANDLERS, "features": features})]
    };
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
        "payload_ownership_content_hash": "b".repeat(64),
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
fn parser_only_admission_selects_basic_parsers() {
    let value = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );
    let admitted = AdmittedReleaseInput::from_value(&value).expect("parser input is valid");
    assert_eq!(admitted.payloads.len(), 1);
    assert_eq!(admitted.payloads[0].required_features, ["basic-parsers"]);
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["basic-parsers"]
    );
}

#[test]
fn full_and_mixed_admission_select_full_handlers() {
    let full = input(
        "web",
        vec![payload(
            "web",
            "ryeos-direct-execution-evidence",
            &["full-handlers"],
        )],
        &["full-handlers"],
    );
    let admitted = AdmittedReleaseInput::from_value(&full).expect("full input is valid");
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["full-handlers"]
    );

    // This exact binary pair is also exercised through the Python Core builder
    // contract test. Full handlers subsume the parser composition.
    let mixed = input(
        "core",
        vec![
            payload("core", "rye-parser-yaml-document", &["basic-parsers"]),
            payload(
                "core",
                "ryeos-direct-execution-evidence",
                &["full-handlers"],
            ),
        ],
        &["full-handlers"],
    );
    assert!(AdmittedReleaseInput::from_value(&mixed).is_err());
    let admitted =
        AdmittedReleaseInput::from_core_seed_value(&mixed).expect("mixed Core seed input is valid");
    assert_eq!(
        admitted.cargo_feature_selections[0].features,
        ["full-handlers"]
    );
}

#[test]
fn payload_and_aggregate_feature_tampering_are_rejected() {
    let valid = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );

    let mut tampered_payload = valid.clone();
    tampered_payload["payloads"][0]["required_features"] = json!(["full-handlers"]);
    assert!(AdmittedReleaseInput::from_value(&tampered_payload).is_err());

    let mut tampered_aggregate = valid;
    tampered_aggregate["cargo_feature_selections"][0]["features"] = json!(["full-handlers"]);
    assert!(AdmittedReleaseInput::from_value(&tampered_aggregate).is_err());

    let mut wrong_package_selection = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );
    wrong_package_selection["cargo_feature_selections"][0]["cargo_package"] =
        json!("ryeos-web-tools");
    assert!(AdmittedReleaseInput::from_value(&wrong_package_selection).is_err());
}

#[test]
fn wrong_package_unknown_binary_and_unknown_features_are_rejected() {
    let mut wrong_package = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );
    wrong_package["payloads"][0]["cargo_package"] = json!("ryeos-web-tools");
    wrong_package["cargo_packages"] = json!(["ryeos-web-tools"]);
    wrong_package["cargo_feature_selections"] = json!([
        {"cargo_package": "ryeos-web-tools", "features": ["basic-parsers"]}
    ]);
    assert!(AdmittedReleaseInput::from_value(&wrong_package).is_err());

    let unknown_binary = input(
        "web",
        vec![payload("web", "ryeos-unlisted-handler", &[])],
        &[],
    );
    assert!(AdmittedReleaseInput::from_value(&unknown_binary).is_err());

    let mut unknown_feature = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );
    unknown_feature["cargo_feature_selections"][0]["features"] =
        json!(["basic-parsers", "unlisted"]);
    assert!(AdmittedReleaseInput::from_value(&unknown_feature).is_err());
}

#[test]
fn duplicate_selections_stale_schema_and_unknown_fields_are_rejected() {
    let valid = input(
        "web",
        vec![payload(
            "web",
            "rye-parser-yaml-document",
            &["basic-parsers"],
        )],
        &["basic-parsers"],
    );

    let mut duplicate = valid.clone();
    duplicate["cargo_feature_selections"] = json!([
        {"cargo_package": HANDLERS, "features": ["basic-parsers"]},
        {"cargo_package": HANDLERS, "features": ["basic-parsers"]}
    ]);
    assert!(AdmittedReleaseInput::from_value(&duplicate).is_err());

    let mut stale_schema = valid.clone();
    stale_schema["schema"] = json!("ryeos.bundle_release_input_plan.v1");
    assert!(AdmittedReleaseInput::from_value(&stale_schema).is_err());

    let mut unknown_field = valid;
    unknown_field["unrecognized"] = json!(true);
    assert!(AdmittedReleaseInput::from_value(&unknown_field).is_err());
}
