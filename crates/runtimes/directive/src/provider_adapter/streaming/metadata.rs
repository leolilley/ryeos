use serde_json::Value;

use crate::directive::StreamPaths;
use crate::provider_adapter::http::TokenUsage;

/// Pull provider-side metadata (usage totals, finish_reason) out of a
/// single SSE event block. Stream events and response accounting are kept
/// separate: this normalizes the provider totals used for cost accounting and
/// response routing without affecting emitted event ordering.
///
/// When `stream_paths` is provided, schema-declared paths are authoritative.
/// Otherwise OpenAI/Anthropic-style paths are tried.
pub(super) fn harvest_chunk_meta(
    block: &str,
    last_usage: &mut Option<TokenUsage>,
    last_finish: &mut Option<String>,
    stream_paths: Option<&StreamPaths>,
) {
    use ryeos_runtime::template::resolve_path;

    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.trim();
        if rest == "[DONE]" || rest.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(rest) {
            Ok(v) => v,
            Err(e) => {
                tracing::trace!(
                    "skipping malformed SSE data JSON: {e}; raw={}",
                    &rest[..rest.len().min(120)]
                );
                continue;
            }
        };

        if let Some(sp) = stream_paths {
            if let Some(ref usage_path) = sp.usage_path {
                if let Some(u) = resolve_path(&parsed, usage_path) {
                    let new_input = sp
                        .input_tokens_field
                        .as_ref()
                        .and_then(|f| u.get(f))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let new_output = sp
                        .output_tokens_field
                        .as_ref()
                        .and_then(|f| u.get(f))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    update_usage_max(last_usage, new_input, new_output);
                }
            }
            if let Some(ref fr_path) = sp.finish_reason_path {
                if let Some(reason) = resolve_path(&parsed, fr_path).and_then(|v| v.as_str()) {
                    if !reason.is_empty() {
                        *last_finish = Some(reason.to_string());
                    }
                }
            }
        } else {
            if let Some(u) = parsed.get("usage") {
                let new_input = u
                    .get("prompt_tokens")
                    .or_else(|| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let new_output = u
                    .get("completion_tokens")
                    .or_else(|| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                update_usage_max(last_usage, new_input, new_output);
            }

            if let Some(reason) = parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("finish_reason"))
                .and_then(|f| f.as_str())
            {
                if !reason.is_empty() {
                    *last_finish = Some(reason.to_string());
                }
            }
        }
    }
}

fn update_usage_max(last_usage: &mut Option<TokenUsage>, new_input: u64, new_output: u64) {
    if new_input == 0 && new_output == 0 {
        return;
    }
    let prev_input = last_usage.as_ref().map(|u| u.input_tokens).unwrap_or(0);
    let prev_output = last_usage.as_ref().map(|u| u.output_tokens).unwrap_or(0);
    *last_usage = Some(TokenUsage {
        input_tokens: new_input.max(prev_input),
        output_tokens: new_output.max(prev_output),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_usage_totals_never_regress_and_latest_finish_wins() {
        let block = concat!(
            "data: {\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8},",
            "\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n",
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":9},",
            "\"choices\":[{\"finish_reason\":\"stop\"}]}\n",
        );
        let mut usage = None;
        let mut finish = None;

        harvest_chunk_meta(block, &mut usage, &mut finish, None);

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn configured_paths_are_authoritative_for_usage_and_finish() {
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".into(),
            text_field: "text".into(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: Some("usageMetadata".into()),
            input_tokens_field: Some("promptTokenCount".into()),
            output_tokens_field: Some("candidatesTokenCount".into()),
            finish_reason_path: Some("candidates.0.finishReason".into()),
        };
        let block = concat!(
            "data: {\"usageMetadata\":{\"promptTokenCount\":7,",
            "\"candidatesTokenCount\":5},\"candidates\":[{\"finishReason\":\"MAX_TOKENS\"}],",
            "\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":99}}\n",
        );
        let mut usage = None;
        let mut finish = None;

        harvest_chunk_meta(block, &mut usage, &mut finish, Some(&paths));

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(finish.as_deref(), Some("MAX_TOKENS"));
    }
}
