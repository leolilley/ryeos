use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::condition::resolve_path;

static WHOLE_EXPR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\$\{([^}]+)\}$").unwrap());

static INTERP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([^}]+)\}").unwrap());

static INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{input:(\w+)(\?|[:|][^}]*)?\}").unwrap());

pub fn interpolate(template: &Value, context: &Value) -> anyhow::Result<Value> {
    match template {
        Value::String(s) => interpolate_string(s, context),
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), interpolate(v, context)?);
            }
            Ok(Value::Object(result))
        }
        Value::Array(arr) => {
            let result: Vec<Value> = arr
                .iter()
                .map(|v| interpolate(v, context))
                .collect::<anyhow::Result<_>>()?;
            Ok(Value::Array(result))
        }
        other => Ok(other.clone()),
    }
}

fn interpolate_string(template: &str, context: &Value) -> anyhow::Result<Value> {
    tracing::trace!(template = %template, "interpolating template");
    if let Some(caps) = WHOLE_EXPR_RE.captures(template) {
        let expr = &caps[1];
        let resolved = resolve_expression(expr, context);
        if let Some(val) = resolved {
            return Ok(val);
        }
    }

    if !INTERP_RE.is_match(template) && !INPUT_RE.is_match(template) {
        return Ok(Value::String(template.to_string()));
    }

    let mut result = template.to_string();

    result = INTERP_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let expr = &caps[1];
            resolve_expression(expr, context)
                .and_then(|v| stringify_value(&v))
                .unwrap_or_default()
        })
        .to_string();

    result = INPUT_RE
        .replace_all(&result, |caps: &regex::Captures| {
            let name = &caps[1];
            let modifier = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let inputs = context.get("inputs");

            match inputs.and_then(|i| i.get(name)) {
                Some(val) => val.as_str().unwrap_or("").to_string(),
                None => match modifier {
                    "?" => String::new(),
                    m if m.starts_with(':') => m[1..].to_string(),
                    m if m.starts_with('|') => m[1..].to_string(),
                    _ => String::new(),
                },
            }
        })
        .to_string();

    Ok(Value::String(result))
}

fn resolve_expression(expr: &str, context: &Value) -> Option<Value> {
    let (path_part, pipes) = parse_pipes(expr);

    let fallbacks: Vec<&str> = path_part.split("||").map(|s| s.trim()).collect();

    let mut resolved: Option<Value> = None;
    let mut matched_fallback: Option<&str> = None;
    for fallback in &fallbacks {
        let val = resolve_path(context, fallback);
        if let Some(v) = val {
            if !v.is_null() {
                resolved = Some(v.clone());
                matched_fallback = Some(fallback);
                break;
            }
        }
    }

    let resolved = match resolved {
        Some(v) => v,
        None => {
            tracing::trace!(expr = %expr, "expression unresolved (no fallback matched)");
            return None;
        }
    };
    tracing::trace!(expr = %expr, matched = matched_fallback.unwrap_or(""), pipe_count = pipes.len(), "expression resolved");

    apply_pipes(resolved, &pipes)
}

fn parse_pipes(expr: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '|' {
            if i + 1 < chars.len() && chars[i + 1] == '|' {
                current.push_str("||");
                i += 2;
            } else {
                parts.push(std::mem::take(&mut current));
                i += 1;
            }
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    if parts.len() <= 1 {
        return (expr.to_string(), Vec::new());
    }
    let path = parts[0].clone();
    let pipes = parts[1..].iter().map(|s| s.trim().to_string()).collect();
    (path, pipes)
}

fn apply_pipes(val: Value, pipes: &[String]) -> Option<Value> {
    let mut result = val;
    for pipe in pipes {
        result = match pipe.as_str() {
            "json" => {
                let s = stringify_value(&result)?;
                Value::String(s)
            }
            "from_json" => {
                let s = result.as_str()?;
                serde_json::from_str(s).ok()?
            }
            "length" => match &result {
                Value::Array(a) => Value::Number(a.len().into()),
                Value::Object(o) => Value::Number(o.len().into()),
                Value::String(s) => Value::Number(s.len().into()),
                _ => return None,
            },
            "keys" => match &result {
                Value::Object(o) => {
                    Value::Array(o.keys().map(|k| Value::String(k.clone())).collect())
                }
                _ => return None,
            },
            "upper" => {
                let s = result.as_str()?;
                Value::String(s.to_uppercase())
            }
            "lower" => {
                let s = result.as_str()?;
                Value::String(s.to_lowercase())
            }
            _ => result,
        };
    }
    Some(result)
}

fn stringify_value(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

pub fn interpolate_action(action: &Value, context: &Value) -> anyhow::Result<Value> {
    match action {
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                if k == "item_id" || k == "params" {
                    result.insert(k.clone(), interpolate(v, context)?);
                } else {
                    result.insert(k.clone(), v.clone());
                }
            }
            Ok(Value::Object(result))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_expression_preserves_type() {
        let ctx = json!({"state": {"count": 3}});
        assert_eq!(
            interpolate(&json!("${state.count}"), &ctx).unwrap(),
            json!(3)
        );
    }

    #[test]
    fn partial_string_stringifies() {
        let ctx = json!({"user": {"name": "Alice"}, "count": 3});
        assert_eq!(
            interpolate(&json!("Hello ${user.name} (${count})"), &ctx).unwrap(),
            json!("Hello Alice (3)")
        );
    }

    #[test]
    fn fallback_and_pipe() {
        let ctx = json!({"backup": {"name": "alice"}});
        assert_eq!(
            interpolate(&json!("${state.missing || backup.name | upper}"), &ctx).unwrap(),
            json!("ALICE")
        );
    }

    #[test]
    fn input_refs() {
        let ctx = json!({"inputs": {"name": "Rye"}});
        assert_eq!(
            interpolate(
                &json!("A={input:name};B={input:missing?};C={input:missing:default}"),
                &ctx
            )
            .unwrap(),
            json!("A=Rye;B=;C=default")
        );
    }

    #[test]
    fn interpolate_action_only_touches_item_id_and_params() {
        let action = json!({
            "item_id": "${tool.id}",
            "params": {"n": "${x}"},
            "other": "${y}"
        });
        let ctx = json!({"tool": {"id": "rye/tool"}, "x": 1, "y": 2});
        let result = interpolate_action(&action, &ctx).unwrap();
        assert_eq!(result["item_id"], "rye/tool");
        assert_eq!(result["params"]["n"], 1);
        assert_eq!(result["other"], "${y}");
    }

    #[test]
    fn no_interpolation_passthrough() {
        let ctx = json!({});
        assert_eq!(
            interpolate(&json!("plain text"), &ctx).unwrap(),
            json!("plain text")
        );
    }

    #[test]
    fn nested_object_interpolation() {
        let ctx = json!({"a": 1});
        let template = json!({"x": "${a}", "nested": {"y": "${a}"}});
        let result = interpolate(&template, &ctx).unwrap();
        assert_eq!(result["x"], 1);
        assert_eq!(result["nested"]["y"], 1);
    }

    #[test]
    fn pipe_length() {
        let ctx = json!({"items": [1, 2, 3]});
        assert_eq!(
            interpolate(&json!("${items | length}"), &ctx).unwrap(),
            json!(3)
        );
    }
}
