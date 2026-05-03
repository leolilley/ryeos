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

    // Whole-expression fast path: if the entire template is a single ${...},
    // resolve it and preserve the native type (number, bool, etc.) instead
    // of stringifying.
    if let Some(caps) = WHOLE_EXPR_RE.captures(template) {
        let expr = &caps[1];
        if let Some(val) = resolve_expression(expr, context) {
            return Ok(val);
        }
        // Whole expression didn't resolve — fall through to error collection
        // in the scanning loop below (will report "unresolved expression").
    }

    // If nothing looks like an interpolation, return the template as-is.
    if !INTERP_RE.is_match(template) && !INPUT_RE.is_match(template) {
        return Ok(Value::String(template.to_string()));
    }

    // Scan the template, resolving ${...} expressions and {input:...}
    // references. Both must resolve — missing inputs and unresolved paths
    // are errors unless the author explicitly opts out:
    //   - ${a || b}  → explicit fallback (try a, use b if null)
    //   - {input:x?} → optional input (empty if missing)
    //   - {input:x:default} → default value if missing
    //   - {input:x|default} → default value if missing
    let mut result = String::new();
    let mut remaining = template;

    while !remaining.is_empty() {
        // Try ${...} expression
        if let Some(caps) = INTERP_RE.captures(remaining) {
            let full_match = caps.get(0).unwrap();
            let match_start = full_match.start();

            // Try {input:...} reference (takes precedence if it appears
            // before or at the same position as the next ${...})
            if let Some(input_caps) = INPUT_RE.captures(remaining) {
                let input_start = input_caps.get(0).unwrap().start();
                if input_start <= match_start {
                    let input_match = input_caps.get(0).unwrap();
                    let name = &input_caps[1];
                    let modifier = input_caps.get(2).map(|m| m.as_str()).unwrap_or("");

                    result.push_str(&remaining[..input_start]);
                    result.push_str(&resolve_input(name, modifier, context)?);
                    remaining = &remaining[input_match.end()..];
                    continue;
                }
            }

            let expr = &caps[1];
            result.push_str(&remaining[..match_start]);
            match resolve_expression(expr, context) {
                Some(val) => {
                    match stringify_value(&val) {
                        Some(s) => result.push_str(&s),
                        None => {
                            anyhow::bail!(
                                "interpolation: ${{{}}} resolved to {} which cannot be stringified",
                                expr, val
                            );
                        }
                    }
                }
                None => {
                    anyhow::bail!(
                        "interpolation: ${{{}}} could not be resolved — \
                         no matching path in context and no || fallback provided",
                        expr
                    );
                }
            }
            remaining = &remaining[full_match.end()..];
            continue;
        }

        // Try {input:...} reference (only remaining pattern)
        if let Some(caps) = INPUT_RE.captures(remaining) {
            let full_match = caps.get(0).unwrap();
            let name = &caps[1];
            let modifier = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            result.push_str(&remaining[..full_match.start()]);
            result.push_str(&resolve_input(name, modifier, context)?);
            remaining = &remaining[full_match.end()..];
            continue;
        }

        // No more interpolation patterns — append the rest verbatim.
        result.push_str(remaining);
        break;
    }

    Ok(Value::String(result))
}

/// Resolve an `{input:NAME}` or `{input:NAMEMODIFIER}` reference.
///
/// Modifier semantics:
///   - `?`        → optional: return empty string if missing
///   - `:default` → return `default` if missing
///   - `|default` → return `default` if missing
///   - (none)     → **required**: return error if missing
fn resolve_input(name: &str, modifier: &str, context: &Value) -> anyhow::Result<String> {
    let inputs = context.get("inputs");

    match inputs.and_then(|i| i.get(name)) {
        Some(val) => {
            // Input values must be strings — numeric/boolean inputs would
            // produce confusing results in template expansion. Operators
            // who need non-string inputs should use ${...} expressions with
            // the inputs path instead.
            val.as_str()
                .map(|s| Ok(s.to_string()))
                .unwrap_or_else(|| {
                    anyhow::bail!(
                        "interpolation: {{input:{name}}} resolved to {val} which is not a string — \
                         use ${{inputs.{name}}} for non-string input access"
                    )
                })
        }
        None => match modifier {
            "?" => Ok(String::new()),
            m if m.starts_with(':') => Ok(m[1..].to_string()),
            m if m.starts_with('|') => Ok(m[1..].to_string()),
            _ => anyhow::bail!(
                "interpolation: {{input:{name}}} is missing from inputs and \
                 has no fallback modifier. Use {{input:{name}?}} for optional \
                 or {{input:{name}:default}} for a default value."
            ),
        },
    }
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
            "type" => Value::String(match &result {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
            }.to_string()),
            _ => return None, // Unknown pipe → expression unresolvable
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
    fn missing_required_input_errors() {
        let ctx = json!({"inputs": {"name": "Rye"}});
        let result = interpolate(&json!("{input:nonexistent}"), &ctx);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("nonexistent") && msg.contains("missing from inputs"),
            "error message should name the missing input: {msg}"
        );
    }

    #[test]
    fn unresolved_expression_errors() {
        let ctx = json!({"inputs": {}});
        let result = interpolate(&json!("Hello ${state.nonexistent}"), &ctx);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("state.nonexistent") && msg.contains("no || fallback"),
            "error message should name the path and suggest fallback: {msg}"
        );
    }

    #[test]
    fn unresolved_expression_with_fallback_is_ok() {
        let ctx = json!({"backup": {"name": "alice"}});
        assert_eq!(
            interpolate(&json!("${state.missing || backup.name}"), &ctx).unwrap(),
            json!("alice")
        );
    }

    #[test]
    fn optional_input_with_question_mark_is_ok() {
        let ctx = json!({"inputs": {}});
        assert_eq!(
            interpolate(&json!("Hello {input:name?}!"), &ctx).unwrap(),
            json!("Hello !")
        );
    }

    #[test]
    fn optional_input_with_default_is_ok() {
        let ctx = json!({"inputs": {}});
        assert_eq!(
            interpolate(&json!("Hello {input:name:World}!"), &ctx).unwrap(),
            json!("Hello World!")
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
