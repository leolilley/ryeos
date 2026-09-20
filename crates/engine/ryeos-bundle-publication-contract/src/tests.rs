use serde_json::{Value, json};

use super::*;

fn h(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn generation_value() -> Value {
    json!({
        "schema": BUNDLE_GENERATION_SCHEMA,
        "kind": BUNDLE_GENERATION_KIND,
        "bundle_name": "standard",
        "authored_version": "1.2.3",
        "content_manifest_hash": h('a'),
        "manifest_item_hash": h('b'),
        "target": {"kind":"triple","triple":"x86_64-unknown-linux-gnu"},
        "build_profile": "release",
        "substrate_protocol": 1,
        "bundle_manifest_format": "ryeos.bundle-manifest/v1",
        "accepted_product_result_hash": h('c'),
        "selected_product_identity": "bundle",
        "selected_product_witness": h('d'),
        "publisher_materialization_result_hash": h('e'),
        "accepted_capture_result_hash": h('f'),
        "selected_signed_product_identity": "signed-bundle",
        "selected_signed_product_witness": h('9'),
        "source_snapshot_hash": null,
        "qualification_evidence_hashes": [h('1'), h('2')],
        "provenance_hash": null,
        "sbom_hash": null
    })
}

fn materialization_value() -> Value {
    json!({
        "schema": PUBLISHER_MATERIALIZATION_RESULT_SCHEMA,
        "kind": PUBLISHER_MATERIALIZATION_RESULT_KIND,
        "accepted_product_result_hash": h('1'),
        "selected_product_identity": "bundle",
        "selected_product_witness": h('2'),
        "input_content_manifest_hash": h('3'),
        "output_content_manifest_hash": h('4'),
        "output_manifest_item_hash": h('5'),
        "publisher_fingerprint": h('6'),
        "publisher_tool_effective_definition_digest": h('7'),
        "publisher_tool_artifact_identity_hash": h('8'),
        "mutation_contract": "ryeos_bundle_sign_v1"
    })
}

fn set_value() -> Value {
    json!({
        "schema": BUNDLE_SET_SCHEMA,
        "kind": BUNDLE_SET_KIND,
        "set_name": "stable",
        "target": {"kind":"portable"},
        "substrate_protocol": 1,
        "substrate_release_attestation_hash": h('5'),
        "entries": [
            {"bundle_name":"core","generation_hash":h('1'),"publisher_attestation_hash":h('2')},
            {"bundle_name":"standard","generation_hash":h('3'),"publisher_attestation_hash":h('4')}
        ],
        "migration_requirement": "none"
    })
}

fn substrate_release_value() -> Value {
    json!({
        "schema": SUBSTRATE_RELEASE_SCHEMA,
        "kind": SUBSTRATE_RELEASE_KIND,
        "catalog_namespace": "official",
        "bundle_publication_policy_section_digest": h('8'),
        "trust_epoch": 1,
        "substrate_image_digest": format!("sha256:{}", h('a')),
        "substrate_protocol": 1,
        "target": {"kind":"portable"},
        "substrate_build_accepted_result_hash": h('b'),
        "substrate_build_receipt_hash": h('e'),
        "selected_substrate_product_identity": "substrate",
        "selected_substrate_product_witness": h('c'),
        "qualification_evidence_hashes": [h('d')],
        "core_generation_hash": h('1'),
        "core_generation_attestation_hash": h('2')
    })
}

fn substrate_build_receipt_value() -> Value {
    json!({
        "schema": SUBSTRATE_BUILD_RECEIPT_SCHEMA,
        "kind": SUBSTRATE_BUILD_RECEIPT_KIND,
        "substrate_image_digest": format!("sha256:{}", h('a')),
        "substrate_protocol": 1,
        "target": {"kind":"portable"},
        "core_generation_hash": h('1')
    })
}

fn selection_value() -> Value {
    json!({
        "schema": NODE_BUNDLE_SELECTION_SCHEMA,
        "kind": NODE_BUNDLE_SELECTION_KIND,
        "target_node_or_app_root_identity": "node:local/app:root",
        "substrate_image_digest": format!("sha256:{}", h('1')),
        "substrate_protocol": 1,
        "bundle_set_hash": h('2'),
        "curated_set_attestation_hash": h('3'),
        "bundle_publication_policy_section_digest": h('4'),
        "node_policy_generation_digest": h('5'),
        "expected_active_selection": null,
        "migration_decision": "none"
    })
}

fn snapshot_value() -> Value {
    json!({
        "schema": BUNDLE_CATALOG_SNAPSHOT_SCHEMA,
        "kind": BUNDLE_CATALOG_SNAPSHOT_KIND,
        "publisher": format!("fp:{}", h('1')),
        "bundle_channels": [
            {"bundle_name":"core","channel":"stable","generation_attestation_hash":h('2')},
            {"bundle_name":"standard","channel":"stable","generation_attestation_hash":h('3')}
        ],
        "set_channels": [
            {"set_name":"complete","channel":"stable","set_attestation_hash":h('4')}
        ]
    })
}

fn publication_value() -> Value {
    json!({
        "schema": BUNDLE_CATALOG_PUBLICATION_SCHEMA,
        "kind": BUNDLE_CATALOG_PUBLICATION_KIND,
        "catalog_namespace": "ryeos",
        "snapshot_hash": h('1'),
        "previous_publication_attestation_hash": null,
        "sequence": 0
    })
}

fn payload_ownership_value() -> Value {
    json!({
        "schema": BUNDLE_PAYLOAD_OWNERSHIP_SCHEMA,
        "kind": BUNDLE_PAYLOAD_OWNERSHIP_KIND,
        "bundles": [
            {
                "bundle_name": "browser",
                "bundle_sets": ["full", "release-artifacts"],
                "payloads": [
                    {"binary":"ryeos-browser-tools","cargo_package":"ryeos-browser-tools","build_class":"release"}
                ]
            },
            {
                "bundle_name": "web",
                "bundle_sets": ["central-host", "full", "release-artifacts"],
                "payloads": [
                    {"binary":"ryeos-web-tools","cargo_package":"ryeos-web-tools","build_class":"release"}
                ]
            }
        ]
    })
}

#[test]
fn all_current_wires_round_trip_and_hash_canonically() {
    macro_rules! check {
        ($ty:ty, $value:expr) => {{
            let value = $value;
            let wire = <$ty>::from_current_value(&value).unwrap();
            assert_eq!(wire.to_value().unwrap(), value);
            assert_eq!(
                wire.content_hash().unwrap(),
                lillux::cas::sha256_hex(lillux::canonical_json(&value).unwrap().as_bytes())
            );
        }};
    }
    check!(BundleGeneration, generation_value());
    check!(PublisherMaterializationResult, materialization_value());
    check!(BundleSet, set_value());
    check!(SubstrateRelease, substrate_release_value());
    check!(SubstrateBuildReceipt, substrate_build_receipt_value());
    check!(NodeBundleSelection, selection_value());
    check!(BundleCatalogSnapshot, snapshot_value());
    check!(BundleCatalogPublication, publication_value());
    check!(BundlePayloadOwnership, payload_ownership_value());
}

#[test]
fn payload_ownership_is_closed_sorted_and_globally_unique() {
    let ownership = BundlePayloadOwnership::from_current_value(&payload_ownership_value()).unwrap();
    assert_eq!(
        ownership.owner("web").unwrap().payloads[0].binary,
        "ryeos-web-tools"
    );
    assert!(ownership.owner("central-auth").is_none());

    let mut value = payload_ownership_value();
    value["bundles"].as_array_mut().unwrap().reverse();
    assert!(BundlePayloadOwnership::from_current_value(&value).is_err());

    let mut value = payload_ownership_value();
    value["bundles"][1]["payloads"][0]["binary"] = json!("ryeos-browser-tools");
    assert!(BundlePayloadOwnership::from_current_value(&value).is_err());

    let mut value = payload_ownership_value();
    value["bundles"][0]["payloads"][0]["build_class"] = json!("debug");
    assert!(BundlePayloadOwnership::from_current_value(&value).is_err());
}

#[test]
fn unknown_fields_and_old_schemas_fail_closed() {
    let mut value = generation_value();
    value["legacy"] = json!(true);
    assert!(BundleGeneration::from_current_value(&value).is_err());
    let mut value = generation_value();
    value["schema"] = json!("ryeos.bundle_generation.v0");
    assert!(BundleGeneration::from_current_value(&value).is_err());
}

#[test]
fn required_nullable_fields_must_be_present() {
    let mut generation = generation_value();
    generation
        .as_object_mut()
        .unwrap()
        .remove("source_snapshot_hash");
    assert!(BundleGeneration::from_current_value(&generation).is_err());
    let mut selection = selection_value();
    selection
        .as_object_mut()
        .unwrap()
        .remove("expected_active_selection");
    assert!(NodeBundleSelection::from_current_value(&selection).is_err());
    let mut publication = publication_value();
    publication
        .as_object_mut()
        .unwrap()
        .remove("previous_publication_attestation_hash");
    assert!(BundleCatalogPublication::from_current_value(&publication).is_err());
}

#[test]
fn hashes_are_exact_lowercase_hex() {
    for bad in ["a".repeat(63), "A".repeat(64), "z".repeat(64)] {
        let mut value = generation_value();
        value["content_manifest_hash"] = json!(bad);
        assert!(BundleGeneration::from_current_value(&value).is_err());
    }
}

#[test]
fn evidence_and_entry_lists_are_strictly_sorted_and_unique() {
    let mut value = generation_value();
    value["qualification_evidence_hashes"] = json!([h('2'), h('1')]);
    assert!(BundleGeneration::from_current_value(&value).is_err());
    let mut value = set_value();
    value["entries"].as_array_mut().unwrap().reverse();
    assert!(BundleSet::from_current_value(&value).is_err());
    let mut value = snapshot_value();
    value["bundle_channels"][1]["bundle_name"] = json!("core");
    assert!(BundleCatalogSnapshot::from_current_value(&value).is_err());
}

#[test]
fn enums_are_closed_and_target_triples_are_canonical() {
    let mut value = generation_value();
    value["build_profile"] = json!("debug");
    assert!(BundleGeneration::from_current_value(&value).is_err());
    let mut value = generation_value();
    value["target"] = json!({"kind":"triple","triple":"../../host"});
    assert!(BundleGeneration::from_current_value(&value).is_err());
    let mut value = set_value();
    value["migration_requirement"] = json!("run_scripts");
    assert!(BundleSet::from_current_value(&value).is_err());
}

#[test]
fn materialization_requires_distinct_input_and_output() {
    let mut value = materialization_value();
    value["output_content_manifest_hash"] = value["input_content_manifest_hash"].clone();
    assert!(PublisherMaterializationResult::from_current_value(&value).is_err());
}

#[test]
fn catalog_sequence_has_clean_genesis_and_successor_shapes() {
    let mut value = publication_value();
    value["sequence"] = json!(1);
    assert!(BundleCatalogPublication::from_current_value(&value).is_err());
    let mut value = publication_value();
    value["previous_publication_attestation_hash"] = json!(h('2'));
    assert!(BundleCatalogPublication::from_current_value(&value).is_err());
    value["sequence"] = json!(1);
    assert!(BundleCatalogPublication::from_current_value(&value).is_ok());
}

#[test]
fn substrate_release_is_exactly_bound_to_image_protocol_and_core() {
    let mut value = substrate_release_value();
    value["substrate_protocol"] = json!(0);
    assert!(SubstrateRelease::from_current_value(&value).is_err());

    let mut value = substrate_release_value();
    value["substrate_image_digest"] = json!(h('a'));
    assert!(SubstrateRelease::from_current_value(&value).is_err());

    let mut value = substrate_release_value();
    value["core_generation_hash"] = json!(h('A'));
    assert!(SubstrateRelease::from_current_value(&value).is_err());
}

#[test]
fn wire_byte_bound_is_checked_before_deserialization() {
    let value = json!({"padding": "x".repeat(MAX_WIRE_BYTES + 1)});
    let error = BundleGeneration::from_current_value(&value).unwrap_err();
    assert!(error.to_string().contains("wire byte bound"));
}
