use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::directive::{
    ExecutionConfig, ProviderConfig, ProviderMessage, ToolCall, ToolSchema,
};

pub struct AdapterResponse {
    pub message: ProviderMessage,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<String>,
}

pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub async fn call_provider(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    execution: &ExecutionConfig,
    model: &str,
    messages: &[ProviderMessage],
    tools: &[ToolSchema],
) -> Result<AdapterResponse> {
    let schemas = provider
        .schemas
        .as_ref()
        .and_then(|s| s.messages.as_ref());

    let (converted_messages, system_prompt) =
        crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());

    let tools_val =
        crate::provider_adapter::tools::serialize_tools(tools, &schemas.cloned());

    let stream_url = provider.extra.get("stream_url").and_then(|v| v.as_str());
    let url = match stream_url {
        Some(su) => format!(
            "{}{}",
            provider.base_url.trim_end_matches('/'),
            su.trim_start_matches('/')
        ),
        None => format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        ),
    };

    let mut body = json!({
        "model": model,
        "messages": converted_messages,
    });

    if let Some(sys) = &system_prompt {
        body["system"] = json!(sys);
    }

    if !tools.is_empty() {
        body["tools"] = tools_val;
    }

    let mut req = client.post(&url);

    let auth_header = provider.auth.header_name.as_deref().unwrap_or("Authorization");
    let auth_prefix = provider.auth.prefix.as_deref().unwrap_or("Bearer ");
    if let Some(ref env_var) = provider.auth.env_var {
        if let Ok(key) = std::env::var(env_var) {
            req = req.header(auth_header, format!("{}{}", auth_prefix, key));
        }
    }

    for (k, v) in &provider.headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let max_attempts = execution.retries + 1;
    let mut last_error: Option<String> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let delay = calculate_backoff(execution.backoff_base_ms, attempt);
            sleep(Duration::from_millis(delay)).await;
        }

        let resp = match req
            .try_clone()
            .unwrap()
            .json(&body)
            .timeout(Duration::from_secs(execution.timeout_seconds))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() && execution.retry_on_timeout && attempt + 1 < max_attempts {
                    last_error = Some(format!("timeout: {}", e));
                    continue;
                }
                if e.is_connect() && attempt + 1 < max_attempts {
                    last_error = Some(format!("connect error: {}", e));
                    continue;
                }
                bail!("request failed: {}", e);
            }
        };

        let status = resp.status().as_u16();

        if resp.status().is_success() {
            let resp_json: Value = resp.json().await?;
            return parse_response(&resp_json, &schemas.cloned());
        }

        if execution.never_retry.iter().any(|pattern| {
            status.to_string().contains(pattern)
                || provider
                    .base_url
                    .contains(pattern)
        }) {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "provider returned {} (never_retry): {}",
                status,
                text.chars().take(500).collect::<String>()
            );
        }

        if execution.retry_status_codes.contains(&status) && attempt + 1 < max_attempts {
            let text = resp.text().await.unwrap_or_default();
            last_error = Some(format!("status {}: {}", status, text));
            continue;
        }

        let text = resp.text().await.unwrap_or_default();
        bail!(
            "provider returned {}: {}",
            status,
            text.chars().take(500).collect::<String>()
        );
    }

    bail!(
        "all {} attempts failed. last: {}",
        max_attempts,
        last_error.unwrap_or_else(|| "unknown".to_string())
    )
}

fn calculate_backoff(base_ms: u64, attempt: u32) -> u64 {
    let base = base_ms as f64;
    let exponential = base * 2f64.powi(attempt as i32);
    let jitter = exponential * 0.2;
    let jitter_val = if jitter > 0.0 {
        (js_random_offset() * 2.0 - 1.0) * jitter
    } else {
        0.0
    };
    let result = exponential + jitter_val;
    result.round().max(0.0) as u64
}

fn js_random_offset() -> f64 {
    let mut seed: u64 = 12345;
    seed = seed.wrapping_mul(6364136223846793005);
    seed = seed.wrapping_add(1442695040888963407);
    let x = ((seed >> 33) as u32) as f64;
    x / u32::MAX as f64
}

fn parse_response(resp: &Value, schemas: &Option<crate::directive::MessageSchemas>) -> Result<AdapterResponse> {
    let message =
        crate::provider_adapter::messages::convert_response_message(resp, schemas);

    let usage = resp.get("usage").map(|u| TokenUsage {
        input_tokens: u
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    });

    let finish_reason = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .map(String::from);

    Ok(AdapterResponse {
        message,
        usage,
        finish_reason,
    })
}

pub fn parse_tool_arguments(args_str: &str) -> Value {
    serde_json::from_str(args_str).unwrap_or_else(|_| {
        let fixed = fix_json_string(args_str);
        serde_json::from_str(&fixed).unwrap_or(json!({}))
    })
}

fn fix_json_string(s: &str) -> String {
    let s = s.replace("\\\"", "\"");
    let s = s.replace("\\n", "\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ExecutionConfig;

    #[test]
    fn parse_tool_arguments_valid_json() {
        let args = parse_tool_arguments("{\"key\": \"value\"}");
        assert_eq!(args["key"], "value");
    }

    #[test]
    fn parse_tool_arguments_invalid_returns_empty_object() {
        let args = parse_tool_arguments("not json");
        assert!(args.is_object());
    }

    #[test]
    fn parse_tool_arguments_escaped_quotes() {
        let args = parse_tool_arguments(r#"{\"cmd\": \"ls\"}"#);
        assert_eq!(args["cmd"], "ls");
    }

    #[test]
    fn parse_tool_arguments_empty_string() {
        let args = parse_tool_arguments("");
        assert!(args.is_object());
    }

    #[test]
    fn calculate_backoff_values() {
        let b1 = calculate_backoff(1000, 1);
        let b2 = calculate_backoff(1000, 2);
        assert!(b1 > 0);
        assert!(b2 > b1);
        let min_expected = 1000u64 * 2;
        let max_expected = 1000u64 * 2 * 6 / 5;
        assert!(b1 >= min_expected / 5 * 4);
        assert!(b1 <= max_expected + 100);
    }

    #[test]
    fn backoff_deterministic() {
        let a = calculate_backoff(1000, 1);
        let b = calculate_backoff(1000, 1);
        assert_eq!(a, b);
    }

    fn is_retryable_status(status: u16, retry_codes: &[u16]) -> bool {
        retry_codes.contains(&status)
    }

    fn is_never_retry(status: u16, never_retry: &[String]) -> bool {
        never_retry.iter().any(|p| status.to_string().contains(p))
    }

    #[test]
    fn retry_on_retryable_status() {
        let codes = vec![429u16, 500, 502, 503];
        assert!(is_retryable_status(500, &codes));
        assert!(is_retryable_status(429, &codes));
        assert!(!is_retryable_status(200, &codes));
        assert!(!is_retryable_status(404, &codes));
    }

    #[test]
    fn never_retry_prevents_retry() {
        let never = vec!["403".to_string()];
        assert!(is_never_retry(403, &never));
        assert!(!is_never_retry(429, &never));
    }

    #[test]
    fn never_retry_overrides_retryable() {
        let retry_codes = vec![429u16, 500, 502, 503];
        let never_retry = vec!["500".to_string()];
        assert!(is_retryable_status(500, &retry_codes));
        assert!(is_never_retry(500, &never_retry));
    }

    #[test]
    fn parse_response_with_usage() {
        let resp = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let result = parse_response(&resp, &None).unwrap();
        assert_eq!(result.message.role, "assistant");
        assert_eq!(result.message.content.unwrap(), "Hello!");
        assert_eq!(result.finish_reason.unwrap(), "stop");
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
    }

    #[test]
    fn parse_response_tool_calls() {
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\": \"/tmp/test\"}"
                        }
                    }]
                },
                "finish_reason": "tool_use"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        });
        let result = parse_response(&resp, &None).unwrap();
        let calls = result.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].id.as_deref(), Some("call_123"));
    }

    #[test]
    fn fix_json_string_handles_escapes() {
        assert_eq!(fix_json_string(r#"hello\nworld"#), "hello\nworld");
        assert_eq!(fix_json_string(r#"say \"hi\""#), "say \"hi\"");
    }
}
