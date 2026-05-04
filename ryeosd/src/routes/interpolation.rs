//! Unified path-template interpolation for route source_config values.
//!
//! Provides compile-time validation and runtime substitution for
//! `${path.<name>}` templates in source_config. Phase 1 supports only
//! `${path.*}`; `${headers.*}`, `${body.*}`, etc. are deferred.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};

/// Validate that all `${path.<name>}` templates in `source_config` reference
/// capture names declared in the route's path pattern.
///
/// Returns the set of capture names referenced by the config.
///
/// ## What's validated
///
/// - Every string value in the JSON object is scanned for `${path.<name>}`.
/// - Each referenced `<name>` must appear in `declared_captures`.
/// - Only `${path.*}` is supported; `${headers.*}`, `${body.*}`, etc. are
///   rejected as unsupported in Phase 1.
/// - Non-template strings are allowed (literal values).
///
/// ## Compile-time only
///
/// This function is called during route table compilation. It produces
/// `RouteConfigError` so bad configs fail loudly at startup.
pub fn validate_path_templates(
    config: &Value,
    declared_captures: &[String],
    route_id: &str,
    mode: &str,
) -> Result<HashSet<String>, RouteConfigError> {
    let declared: HashSet<&str> = declared_captures.iter().map(|s| s.as_str()).collect();
    let mut referenced = HashSet::new();

    validate_value(config, &declared, &mut referenced, route_id, mode)?;

    Ok(referenced)
}

/// Interpolate `${path.<name>}` templates in `source_config` using actual
/// capture values from the route match.
///
/// ## Runtime only
///
/// This function is called during request dispatch. It produces
/// `RouteDispatchError` so missing captures result in a 500.
pub fn interpolate_path(
    config: &Value,
    captures: &HashMap<String, String>,
) -> Result<Value, RouteDispatchError> {
    match config {
        Value::String(s) => {
            if let Some(interpolated) = try_interpolate_string(s, captures)? {
                Ok(Value::String(interpolated))
            } else {
                Ok(Value::String(s.clone()))
            }
        }
        Value::Object(map) => {
            let mut result = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                result.insert(k.clone(), interpolate_path(v, captures)?);
            }
            Ok(Value::Object(result))
        }
        Value::Array(arr) => {
            let result: Vec<Value> = arr
                .iter()
                .map(|v| interpolate_path(v, captures))
                .collect::<Result<_, _>>()?;
            Ok(Value::Array(result))
        }
        other => Ok(other.clone()),
    }
}

// ── Internal helpers ──────────────────────────────────────────────

fn validate_value(
    value: &Value,
    declared: &HashSet<&str>,
    referenced: &mut HashSet<String>,
    route_id: &str,
    mode: &str,
) -> Result<(), RouteConfigError> {
    match value {
        Value::String(s) => validate_templates_in_string(s, declared, referenced, route_id, mode),
        Value::Object(map) => {
            for v in map.values() {
                validate_value(v, declared, referenced, route_id, mode)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            for v in arr {
                validate_value(v, declared, referenced, route_id, mode)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_templates_in_string(
    s: &str,
    declared: &HashSet<&str>,
    referenced: &mut HashSet<String>,
    route_id: &str,
    mode: &str,
) -> Result<(), RouteConfigError> {
    let mut remaining = s;
    while let Some(start) = remaining.find("${") {
        let end = remaining[start..]
            .find('}')
            .ok_or_else(|| {
                let preview: String = remaining[start + 2..].chars().take(20).collect();
                RouteConfigError::InvalidSourceConfig {
                    id: route_id.into(),
                    src: mode.into(),
                    reason: format!(
                        "unterminated '${{...}}' starting at '{preview}' in source_config value",
                    ),
                }
            })?;
        let inner = &remaining[start + 2..start + end];
        if let Some(name) = inner.strip_prefix("path.") {
            if !declared.contains(name) {
                return Err(RouteConfigError::InvalidSourceConfig {
                    id: route_id.into(),
                    src: mode.into(),
                    reason: format!(
                        "source_config references undeclared path capture '{name}'; \
                         route path declares: [{}]",
                        declared
                            .iter()
                            .map(|c| format!("'{c}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
            referenced.insert(name.to_string());
        } else {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: mode.into(),
                reason: format!(
                    "unsupported template '${{{inner}}}' in source_config; \
                     only ${{path.<name>}} is supported"
                ),
            });
        }
        remaining = &remaining[start + end + 1..];
    }
    Ok(())
}

fn try_interpolate_string(
    s: &str,
    captures: &HashMap<String, String>,
) -> Result<Option<String>, RouteDispatchError> {
    if !s.contains("${") {
        return Ok(None);
    }

    let mut result = String::with_capacity(s.len());
    let mut remaining = s;
    let mut changed = false;

    while let Some(start) = remaining.find("${") {
        result.push_str(&remaining[..start]);
        let end = remaining[start..]
            .find('}')
            .ok_or_else(|| RouteDispatchError::Internal(format!(
                "unterminated template in source_config: {s}"
            )))?;
        let inner = &remaining[start + 2..start + end];
        if let Some(name) = inner.strip_prefix("path.") {
            let value = captures.get(name).ok_or_else(|| {
                RouteDispatchError::Internal(format!(
                    "path capture '{name}' not found in route match"
                ))
            })?;
            result.push_str(value);
            changed = true;
        } else {
            return Err(RouteDispatchError::Internal(format!(
                "unsupported template '${{{inner}}}' at dispatch time"
            )));
        }
        remaining = &remaining[start + end + 1..];
    }

    result.push_str(remaining);

    if changed {
        Ok(Some(result))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<String> {
        vec!["thread_id".into(), "bundle_id".into()]
    }

    #[test]
    fn valid_single_template() {
        let config = serde_json::json!({"thread_id": "${path.thread_id}"});
        let refs = validate_path_templates(&config, &declared(), "r1", "json").unwrap();
        assert!(refs.contains("thread_id"));
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn valid_multiple_templates() {
        let config = serde_json::json!({
            "thread_id": "${path.thread_id}",
            "scope": "${path.bundle_id}",
        });
        let refs = validate_path_templates(&config, &declared(), "r1", "json").unwrap();
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn undeclared_capture_rejected() {
        let config = serde_json::json!({"thread_id": "${path.unknown}"});
        let err = validate_path_templates(&config, &declared(), "r1", "json").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("undeclared path capture 'unknown'"), "got: {msg}");
    }

    #[test]
    fn unsupported_template_rejected() {
        let config = serde_json::json!({"key": "${headers.x-signature}"});
        let err = validate_path_templates(&config, &declared(), "r1", "json").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported template"), "got: {msg}");
    }

    #[test]
    fn unterminated_template_rejected() {
        let config = serde_json::json!({"key": "${path.thread_id"});
        let err = validate_path_templates(&config, &declared(), "r1", "json").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unterminated"), "got: {msg}");
    }

    #[test]
    fn literal_value_ok() {
        let config = serde_json::json!({"key": "literal_value"});
        let refs = validate_path_templates(&config, &declared(), "r1", "json").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn null_value_ok() {
        let config = serde_json::json!({"key": null});
        let refs = validate_path_templates(&config, &declared(), "r1", "json").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn interpolation_substitutes() {
        let config = serde_json::json!({"thread_id": "${path.thread_id}", "limit": 10});
        let mut captures = HashMap::new();
        captures.insert("thread_id".into(), "abc-123".into());
        let result = interpolate_path(&config, &captures).unwrap();
        assert_eq!(result["thread_id"], "abc-123");
        assert_eq!(result["limit"], 10);
    }

    #[test]
    fn interpolation_missing_capture_errors() {
        let config = serde_json::json!({"thread_id": "${path.thread_id}"});
        let captures = HashMap::new();
        let err = interpolate_path(&config, &captures).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn mixed_template_and_literal() {
        let config = serde_json::json!({"prefix": "threads/${path.thread_id}/details"});
        let mut captures = HashMap::new();
        captures.insert("thread_id".into(), "xyz".into());
        let result = interpolate_path(&config, &captures).unwrap();
        assert_eq!(result["prefix"], "threads/xyz/details");
    }
}
