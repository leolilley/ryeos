use std::collections::HashMap;

use ryeos_handler_protocol::{ComposeRequest, ComposeSuccess, ResolutionStepNameWire};
use serde_json::Value;

pub fn validate_config(config: &Value) -> Result<(), String> {
    match config {
        Value::Null => Ok(()),
        Value::Object(obj) if obj.is_empty() => Ok(()),
        Value::Object(_) => Err(
            "graph_permissions composer takes no config: composer_config must be null or {}"
                .to_string(),
        ),
        other => Err(format!(
            "graph_permissions composer takes no config: composer_config must be null or {{}} \
             (got {})",
            value_type(other)
        )),
    }
}

pub fn compose(
    _config: &Value,
    request: &ComposeRequest,
) -> Result<ComposeSuccess, (ResolutionStepNameWire, String)> {
    let root_parsed = &request.root.parsed;

    // Strict authoring validation — fail loud at compose time with the same
    // checks the directive composer applies: reject a legacy top-level
    // `permissions:` block, reject old `callbacks`/unknown keys under
    // `requires.capabilities`, and reject a malformed `declared` list. (Graphs
    // are leaf-only — no ancestor narrowing.)
    crate::extends_chain::reject_legacy_permissions(root_parsed)?;
    crate::extends_chain::validate_requires_shape(root_parsed)?;
    if let Some(manifest) = crate::extends_chain::manifest_value(root_parsed) {
        crate::extends_chain::validate_manifest_shape(manifest)?;
    }

    let mut policy_facts = HashMap::new();

    // Self-asserted action authority is lifted from `requires.capabilities.declared`
    // into effective_caps; the `manifest` sub-tree rides through `composed` and is
    // minted against the signed manifest at launch.
    let caps = match crate::extends_chain::declared_value(root_parsed) {
        Some(declared) => {
            crate::extends_chain::validate_declared_shape(declared)?;
            declared
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    policy_facts.insert(
        "effective_caps".to_string(),
        Value::Array(caps.into_iter().map(Value::String).collect()),
    );

    Ok(ComposeSuccess {
        composed: root_parsed.clone(),
        derived: HashMap::new(),
        policy_facts,
    })
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_handler_protocol::{ComposeInput, ComposeItemContext, TrustClassWire};
    use serde_json::json;

    fn root_input(parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: "x:r".into(),
                resolved_ref: "x:r".into(),
                trust_class: TrustClassWire::TrustedBundle,
            },
            parsed,
        }
    }

    fn run(root: Value) -> ComposeSuccess {
        compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(root),
                ancestors: vec![],
            },
        )
        .unwrap()
    }

    fn policy_fact_string_seq(view: &ComposeSuccess, name: &str) -> Vec<String> {
        view.policy_facts
            .get(name)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn copies_root_parsed_verbatim() {
        let view = run(json!({ "k": 1 }));
        assert_eq!(view.composed, json!({ "k": 1 }));
        assert!(view.derived.is_empty());
    }

    fn declared(caps: Value) -> Value {
        json!({ "requires": { "capabilities": { "declared": caps } } })
    }

    #[test]
    fn lifts_declared_execute_into_effective_caps() {
        let view = run(declared(json!([
            "ryeos.execute.tool.echo",
            "ryeos.execute.tool.read"
        ])));
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.echo", "ryeos.execute.tool.read"]
        );
    }

    #[test]
    fn empty_declared_yields_empty_caps() {
        let view = run(declared(json!([])));
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn missing_requires_yields_empty_caps() {
        let view = run(json!({ "config": {} }));
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn legacy_top_level_permissions_rejected() {
        let err = compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(json!({ "permissions": ["ryeos.execute.tool.echo"] })),
                ancestors: vec![],
            },
        )
        .unwrap_err();
        assert!(
            err.1.contains("`permissions:` is removed")
                && err.1.contains("requires.capabilities.declared"),
            "got: {}",
            err.1
        );
    }

    fn compose_err(root: Value) -> String {
        compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(root),
                ancestors: vec![],
            },
        )
        .unwrap_err()
        .1
    }

    #[test]
    fn legacy_callbacks_rejected() {
        let err = compose_err(json!({ "requires": { "capabilities": { "callbacks": {
            "bundle_events": [{ "event_kind": "e", "operations": ["append"] }]
        } } } }));
        assert!(
            err.contains("unknown key `requires.capabilities.callbacks`"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_requires_key_rejected() {
        let err = compose_err(json!({ "requires": { "capabilities": { "frob": [] } } }));
        assert!(
            err.contains("unknown key `requires.capabilities.frob`"),
            "got: {err}"
        );
    }

    #[test]
    fn malformed_declared_map_rejected() {
        let err =
            compose_err(json!({ "requires": { "capabilities": { "declared": { "execute": [] } } } }));
        assert!(err.contains("must be a list"), "got: {err}");
    }

    #[test]
    fn declared_non_string_rejected() {
        let err = compose_err(json!({ "requires": { "capabilities": { "declared": [42] } } }));
        assert!(err.contains("only strings"), "got: {err}");
    }

    fn manifest(inner: Value) -> Value {
        json!({ "requires": { "capabilities": { "manifest": inner } } })
    }

    #[test]
    fn valid_manifest_passes() {
        let view = run(manifest(json!({
            "bundle_events": [{ "event_kind": "e", "operations": ["append", "scan"] }]
        })));
        // manifest is not lifted into effective_caps (it's minted at launch).
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn manifest_unknown_key_rejected() {
        let err = compose_err(manifest(json!({ "frob": [] })));
        assert!(
            err.contains("unknown key `requires.capabilities.manifest.frob`"),
            "got: {err}"
        );
    }

    #[test]
    fn manifest_invalid_operation_rejected() {
        let err = compose_err(manifest(json!({
            "bundle_events": [{ "event_kind": "e", "operations": ["frobnicate"] }]
        })));
        assert!(err.contains("invalid operation"), "got: {err}");
    }

    #[test]
    fn manifest_empty_operations_rejected() {
        let err = compose_err(manifest(json!({
            "runtime_vault": [{ "namespace": "oauth", "operations": [] }]
        })));
        assert!(err.contains("at least one operation"), "got: {err}");
    }

    #[test]
    fn manifest_empty_event_kind_rejected() {
        let err = compose_err(manifest(json!({
            "bundle_events": [{ "event_kind": "", "operations": ["append"] }]
        })));
        assert!(err.contains("empty `event_kind`"), "got: {err}");
    }

    #[test]
    fn validate_config_accepts_null() {
        validate_config(&Value::Null).unwrap();
    }

    #[test]
    fn validate_config_accepts_empty_object() {
        validate_config(&json!({})).unwrap();
    }

    #[test]
    fn validate_config_rejects_non_empty_object() {
        let err = validate_config(&json!({ "permissions_path": "custom" })).unwrap_err();
        assert!(
            err.contains("graph_permissions composer takes no config"),
            "got: {err}"
        );
    }
}
