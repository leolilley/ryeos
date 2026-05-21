//! Tool-argument JSON parser used by the dispatcher. Historically
//! this module also held a non-streaming HTTP provider adapter; the
//! live launch path now uses `provider_adapter::call_provider_streaming`
//! exclusively, so the only surface left here is the typed-fail-loud
//! arg parser shared with the dispatcher.

use serde_json::Value;

/// Typed-fail-loud tool-argument parser. The previous implementation
/// silently fell back to `{}` on bad JSON, masking malformed
/// LLM-emitted tool calls. Now any parse failure (after the
/// `\"` / `\n` unescape repair pass) is propagated as a typed
/// error so the dispatcher can refuse the call rather than dispatch
/// with empty args.
pub fn parse_tool_arguments(args_str: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(args_str) {
        return Ok(repair_json_string_values(v));
    }
    let fixed = fix_json_string(args_str);
    serde_json::from_str::<Value>(&fixed)
        .map(repair_json_string_values)
        .map_err(|e| {
            format!(
                "malformed tool-call arguments JSON ({e}); raw={}",
                args_str.chars().take(200).collect::<String>()
            )
        })
}

fn fix_json_string(s: &str) -> String {
    let s = s.replace("\\\"", "\"");

    s.replace("\\n", "\n")
}

fn repair_json_string_values(value: Value) -> Value {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    return repair_json_string_values(parsed);
                }
            }
            Value::String(s)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(repair_json_string_values).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, repair_json_string_values(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_arguments_valid_json() {
        let args = parse_tool_arguments("{\"key\": \"value\"}").unwrap();
        assert_eq!(args["key"], "value");
    }

    #[test]
    fn parse_tool_arguments_invalid_errors_typed() {
        let err = parse_tool_arguments("not json").unwrap_err();
        assert!(
            err.contains("malformed tool-call arguments JSON"),
            "expected typed error message, got: {err}"
        );
    }

    #[test]
    fn parse_tool_arguments_escaped_quotes() {
        let args = parse_tool_arguments(r#"{\"cmd\": \"ls\"}"#).unwrap();
        assert_eq!(args["cmd"], "ls");
    }

    #[test]
    fn parse_tool_arguments_repairs_nested_json_string_objects() {
        let args = parse_tool_arguments(
            r#"{"grammar":"{\"schema_version\":1,\"chart_kind\":\"callout\"}"}"#,
        )
        .unwrap();
        assert_eq!(args["grammar"]["schema_version"], 1);
        assert_eq!(args["grammar"]["chart_kind"], "callout");
    }

    #[test]
    fn parse_tool_arguments_empty_string_errors_typed() {
        assert!(parse_tool_arguments("").is_err());
    }

    #[test]
    fn fix_json_string_handles_escapes() {
        assert_eq!(fix_json_string(r#"hello\nworld"#), "hello\nworld");
        assert_eq!(fix_json_string(r#"say \"hi\""#), "say \"hi\"");
    }
}
