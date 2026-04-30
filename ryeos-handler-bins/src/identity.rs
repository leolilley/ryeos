use std::collections::HashMap;

use ryeos_handler_protocol::{ComposeRequest, ComposeSuccess, ResolutionStepNameWire};
use serde_json::Value;

pub fn validate_config(config: &Value) -> Result<(), String> {
    match config {
        Value::Null => Ok(()),
        Value::Object(obj) if obj.is_empty() => Ok(()),
        Value::Object(_) => Err(
            "identity composer takes no config: composer_config must be null or {}"
                .to_string(),
        ),
        other => Err(format!(
            "identity composer takes no config: composer_config must be null or {{}} \
             (got {})",
            value_type(other)
        )),
    }
}

pub fn compose(
    _config: &Value,
    request: &ComposeRequest,
) -> Result<ComposeSuccess, ResolutionStepNameWire> {
    Ok(ComposeSuccess {
        composed: request.root.parsed.clone(),
        derived: HashMap::new(),
        policy_facts: HashMap::new(),
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
                trust_class: TrustClassWire::TrustedSystem,
            },
            parsed,
        }
    }

    #[test]
    fn returns_root_parsed_verbatim() {
        let view = compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(json!({ "k": 1 })),
                ancestors: vec![],
            },
        )
        .unwrap();
        assert_eq!(view.composed, json!({ "k": 1 }));
        assert!(view.derived.is_empty());
        assert!(view.policy_facts.is_empty());
    }

    #[test]
    fn ignores_ancestors() {
        let view = compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(json!({ "x": 1 })),
                ancestors: vec![root_input(json!({ "anything": 1 }))],
            },
        )
        .unwrap();
        assert_eq!(view.composed, json!({ "x": 1 }));
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
        let err = validate_config(&json!({ "anything": 1 })).unwrap_err();
        assert!(err.contains("identity composer takes no config"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_array() {
        let err = validate_config(&json!([1, 2, 3])).unwrap_err();
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_string() {
        let err = validate_config(&json!("nope")).unwrap_err();
        assert!(err.contains("string"), "got: {err}");
    }
}
