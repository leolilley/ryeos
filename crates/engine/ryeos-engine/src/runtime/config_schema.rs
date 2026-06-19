//! `config_schema` — caller parameter validation for the **user
//! tool**.
//!
//! NOT a `RuntimeHandler`. The runtime-handler dispatch loop iterates
//! chain elements (the executor pipeline), but the user's tool
//! item — which owns the public invocation contract — is upstream of
//! the chain. Validating against any chain element's `config_schema`
//! is wrong-by-design (chain[0] is always a wrapper/runtime, not the
//! caller-facing tool).
//!
//! Instead, this module exposes a free function `validate_caller_params`
//! that runs as a pre-pass in `plan_builder` against the resolved
//! tool item BEFORE chain expansion. Wrapper / runtime YAMLs may
//! declare their own `config_schema` for documentation, but those are
//! ignored by the engine (`ignored_keys` carries `config_schema`).
//!
//! On validation failure → `EngineError::ParameterValidationFailed`
//! with structured per-error messages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use crate::error::EngineError;

/// Compiled-validator cache keyed by the schema's canonical-JSON hash.
/// Schema compilation (regex builds, keyword resolution) is far more
/// expensive than validation and tool schemas are immutable signed
/// content, so plan building reuses validators across requests. The
/// cache is cleared wholesale if it ever exceeds the cap — simpler
/// than LRU and a node's distinct tool schemas sit nowhere near it.
const VALIDATOR_CACHE_CAP: usize = 512;

fn validator_for_cached(schema: &Value) -> Result<Arc<jsonschema::Validator>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<jsonschema::Validator>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let key = lillux::sha256_hex(lillux::canonical_json(schema).as_bytes());
    if let Some(validator) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(validator);
    }

    let validator = Arc::new(jsonschema::validator_for(schema).map_err(|e| e.to_string())?);
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= VALIDATOR_CACHE_CAP {
        guard.clear();
    }
    guard.insert(key, validator.clone());
    Ok(validator)
}

/// Validate caller params against the root tool's `config_schema`, if
/// the tool item exposes one.
///
/// `tool_block` is the parsed top-level mapping of the resolved
/// item's source file (whatever the kind's parser produced). For
/// YAML/JSON tools that's the deserialized map; for Python/JS tools
/// that's whatever metadata-style mapping the parser exposes.
///
/// If `tool_block` does not contain a `config_schema` key, this is a
/// no-op (no schema declared → no validation).
/// Returns the (possibly coerced) caller params. CLI tail flags arrive
/// as strings (`--max-actions 60` → `"60"`) because arg binding is
/// schema-free; this is the one place with both schema and params, so
/// string→`integer`/`number`/`boolean` coercion happens here, against
/// the declared types, before validation. The coerced value flows
/// downstream so the runtime receives a typed `60`, not `"60"`.
pub fn validate_caller_params(
    tool_block: &Value,
    params: &Value,
    tool_id: &str,
) -> Result<Value, EngineError> {
    let Some(schema) = tool_block.as_object().and_then(|o| o.get("config_schema")) else {
        return Ok(params.clone());
    };

    let coerced = coerce_params_to_schema(schema, params);

    let validator =
        validator_for_cached(schema).map_err(|e| EngineError::InvalidRuntimeConfig {
            path: tool_id.to_owned(),
            reason: format!("invalid JSON schema in config_schema: {e}"),
        })?;

    let mut errors: Vec<String> = Vec::new();
    for err in validator.iter_errors(&coerced) {
        errors.push(format!("{}: {}", err.instance_path, err));
    }

    if !errors.is_empty() {
        return Err(EngineError::ParameterValidationFailed {
            tool: tool_id.to_owned(),
            errors,
        });
    }

    Ok(coerced)
}

/// Coerce top-level string params to the scalar type the schema declares
/// (`integer`/`number`/`boolean`). Only string values are touched, and
/// only when they parse cleanly — a non-parsing string is left as-is so
/// validation still fails with the real type-mismatch message. Nested
/// objects are left untouched (CLI flags are top-level scalars).
fn coerce_params_to_schema(schema: &Value, params: &Value) -> Value {
    let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(Value::as_object),
        params.as_object(),
    ) else {
        return params.clone();
    };

    let mut out = obj.clone();
    for (key, prop_schema) in props {
        let Some(current) = out.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some(coerced) = coerce_scalar(current, prop_schema) {
            out.insert(key.clone(), coerced);
        }
    }
    Value::Object(out)
}

fn coerce_scalar(s: &str, prop_schema: &Value) -> Option<Value> {
    let types = schema_types(prop_schema);
    // `integer` is stricter than `number`, so try it first; only fall to
    // `number` if the schema actually allows a float.
    if types.iter().any(|t| t == "integer") {
        if let Ok(n) = s.parse::<i64>() {
            return Some(Value::from(n));
        }
    }
    if types.iter().any(|t| t == "number") {
        if let Ok(f) = s.parse::<f64>() {
            return serde_json::Number::from_f64(f).map(Value::Number);
        }
    }
    if types.iter().any(|t| t == "boolean") {
        match s {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            _ => {}
        }
    }
    None
}

/// The declared JSON-Schema `type`(s) for a property — a single string
/// or an array of strings (`type: [integer, "null"]`).
fn schema_types(prop_schema: &Value) -> Vec<String> {
    match prop_schema.get("type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_schema_is_noop() {
        // Tool item has no config_schema key → always passes.
        let tool = json!({ "version": "1.0.0" });
        validate_caller_params(&tool, &Value::Null, "tool:foo").unwrap();
        validate_caller_params(&tool, &json!({"x": 1}), "tool:foo").unwrap();
    }

    #[test]
    fn valid_params_pass() {
        let tool = json!({
            "config_schema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        });
        validate_caller_params(&tool, &json!({"command": "ls"}), "tool:foo").unwrap();
    }

    #[test]
    fn missing_required_field_fails_loud() {
        let tool = json!({
            "config_schema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        });
        let err = validate_caller_params(&tool, &json!({}), "tool:foo").unwrap_err();
        match err {
            EngineError::ParameterValidationFailed { tool, errors } => {
                assert_eq!(tool, "tool:foo");
                assert!(
                    errors.iter().any(|e| e.contains("command")),
                    "expected error mentioning `command`, got {errors:?}"
                );
            }
            other => panic!("expected ParameterValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_fails_loud() {
        let tool = json!({
            "config_schema": {
                "type": "object",
                "properties": { "count": { "type": "integer" } },
                "required": ["count"]
            }
        });
        let err = validate_caller_params(&tool, &json!({"count": "nope"}), "tool:foo").unwrap_err();
        assert!(
            matches!(err, EngineError::ParameterValidationFailed { .. }),
            "expected ParameterValidationFailed, got {err:?}"
        );
    }

    #[test]
    fn malformed_schema_fails_loud() {
        let tool = json!({ "config_schema": { "type": 12345 } });
        let err = validate_caller_params(&tool, &Value::Null, "tool:foo").unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig, got {err:?}"
        );
    }

    fn coerce_schema() -> Value {
        json!({
            "config_schema": {
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "ratio": { "type": "number" },
                    "flag":  { "type": "boolean" },
                    "name":  { "type": "string" }
                }
            }
        })
    }

    #[test]
    fn coerces_string_flags_to_declared_scalar_types() {
        let tool = coerce_schema();
        let coerced = validate_caller_params(
            &tool,
            &json!({"count": "60", "ratio": "1.5", "flag": "true", "name": "x"}),
            "tool:foo",
        )
        .unwrap();
        assert_eq!(coerced["count"], json!(60));
        assert_eq!(coerced["ratio"], json!(1.5));
        assert_eq!(coerced["flag"], json!(true));
        // Strings stay strings.
        assert_eq!(coerced["name"], json!("x"));
    }

    #[test]
    fn does_not_coerce_already_typed_values() {
        let tool = coerce_schema();
        let coerced =
            validate_caller_params(&tool, &json!({"count": 60, "flag": false}), "tool:foo").unwrap();
        assert_eq!(coerced["count"], json!(60));
        assert_eq!(coerced["flag"], json!(false));
    }

    #[test]
    fn non_parsing_string_is_left_for_validation_to_reject() {
        // "nope" can't become an integer → stays a string → validation
        // fails with the real type-mismatch message.
        let tool = coerce_schema();
        let err =
            validate_caller_params(&tool, &json!({"count": "nope"}), "tool:foo").unwrap_err();
        assert!(matches!(err, EngineError::ParameterValidationFailed { .. }));
    }

    #[test]
    fn float_string_not_coerced_for_integer_field() {
        // "60.5" is not a valid i64 and the field is integer-only, so it
        // is left as a string and rejected — floats don't sneak in.
        let tool = coerce_schema();
        let err =
            validate_caller_params(&tool, &json!({"count": "60.5"}), "tool:foo").unwrap_err();
        assert!(matches!(err, EngineError::ParameterValidationFailed { .. }));
    }

    #[test]
    fn no_schema_returns_params_unchanged() {
        let tool = json!({ "version": "1.0.0" });
        let out = validate_caller_params(&tool, &json!({"x": "1"}), "tool:foo").unwrap();
        assert_eq!(out, json!({"x": "1"}));
    }
}
