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
        "bundles/standard/.ai/config/ryeos/environments/qualification",
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
            let mut predecessor = wire.clone();
            predecessor["schema"] = "ryeos.product_qualification_policy.v1".into();
            assert!(ProductQualificationPolicy::from_value(&predecessor).is_err());
        }
    }
    assert!(policies > 0, "no authored qualification policies checked");
}
