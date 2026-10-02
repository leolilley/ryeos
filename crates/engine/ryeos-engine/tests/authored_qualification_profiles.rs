//! Exercise shipped policy parameters at the Engine's actual pre-dispatch gate.
//! Direct Python checker tests cannot detect an incompatible public Tool schema.

use std::path::{Path, PathBuf};

use ryeos_engine::runtime::config_schema::validate_caller_params;
use ryeos_state::external_content::products::qualification::ProductQualificationPolicy;
use serde_json::{Value, json};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn python_tool(path: &Path) -> Value {
    let source = std::fs::read_to_string(path).unwrap();
    let metadata = source
        .lines()
        .take_while(|line| line.starts_with('#'))
        .filter(|line| !line.starts_with("# ryeos:signed:"))
        .filter_map(|line| line.strip_prefix("# "))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: Value = serde_yaml::from_str(&metadata).unwrap();
    parsed["ryeos-tool"].clone()
}

#[test]
fn authored_qualification_profiles_pass_the_engine_caller_gate() {
    let root = repository();
    for name in ["development-platform", "static-link-inputs"] {
        let tool = python_tool(&root.join(format!(
            "bundles/standard/.ai/tools/ryeos/environments/qualification/{name}.py"
        )));
        let policy_source: Value = serde_yaml::from_str(
            &std::fs::read_to_string(root.join(format!(
                "bundles/standard/.ai/config/ryeos/environments/qualification/{name}.yaml"
            )))
            .unwrap(),
        )
        .unwrap();
        let policy =
            ProductQualificationPolicy::from_value(&policy_source["product_qualification_policy"])
                .unwrap();
        let parameters = &policy.verifier_parameters;
        let validated = validate_caller_params(&tool, parameters, &policy.verifier_ref)
            .unwrap_or_else(|error| panic!("{name} authored profile cannot launch: {error}"));
        assert_eq!(validated, *parameters);
        assert!(validate_caller_params(&tool, &json!({}), &policy.verifier_ref).is_err());
        let mut extra = parameters.clone();
        extra["caller_override"] = json!(true);
        assert!(validate_caller_params(&tool, &extra, &policy.verifier_ref).is_err());
        let mut old = parameters.clone();
        old["schema"] = json!(parameters["schema"].as_str().unwrap().replace(".v2", ".v1"));
        assert!(validate_caller_params(&tool, &old, &policy.verifier_ref).is_err());
        let mut nested = parameters.clone();
        if name == "development-platform" {
            nested["tool_versions"]["extra"] = json!("unadmitted tool");
        } else {
            let first = nested["files"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone();
            nested["files"][&first]["caller_override"] = json!(true);
        }
        assert!(validate_caller_params(&tool, &nested, &policy.verifier_ref).is_err());
    }
}

#[test]
fn producer_intent_is_resolver_owned_and_not_a_caller_parameter() {
    let root = repository();
    for (tool_path, intent_path) in [
        (
            ".ai/tools/ryeos/development/cargo-vendor-finalize/run.py",
            "bundles/standard/.ai/config/development/ryeos/cargo-vendor-lock-intent.yaml",
        ),
        (
            ".ai/tools/ryeos/development/static-link-inputs/assemble.py",
            "bundles/standard/.ai/config/development/ryeos/static-link-contract-intent.yaml",
        ),
    ] {
        let tool = python_tool(&root.join(tool_path));
        assert_eq!(
            tool["config_resolve"],
            json!({
                "type":"strict_signed_project_bundle", "bundle_name":"standard", "config_path":intent_path
            })
        );
        assert_eq!(
            validate_caller_params(&tool, &json!({}), tool_path).unwrap(),
            json!({})
        );
        assert!(
            validate_caller_params(&tool, &json!({"resolved_config":{"value":{}}}), tool_path)
                .is_err()
        );
        assert!(
            validate_caller_params(&tool, &json!({"expected_sha256":"a".repeat(64)}), tool_path)
                .is_err()
        );
    }
}
