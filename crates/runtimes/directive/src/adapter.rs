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
        return Ok(v);
    }
    let fixed = fix_json_string(args_str);
    serde_json::from_str::<Value>(&fixed).map_err(|e| {
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
    fn parse_tool_arguments_empty_string_errors_typed() {
        assert!(parse_tool_arguments("").is_err());
    }

    #[test]
    fn fix_json_string_handles_escapes() {
        assert_eq!(fix_json_string(r#"hello\nworld"#), "hello\nworld");
        assert_eq!(fix_json_string(r#"say \"hi\""#), "say \"hi\"");
    }
}
