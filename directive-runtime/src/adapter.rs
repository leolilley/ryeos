use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::directive::{ProviderConfig, ProviderMessage, ToolCall};

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
    model: &str,
    messages: &[ProviderMessage],
    tools: &[crate::directive::ToolSchema],
) -> Result<AdapterResponse> {
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    let mut body = json!({
        "model": model,
        "messages": serialize_messages(messages),
    });

    if !tools.is_empty() {
        body["tools"] = serialize_tools(tools);
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

    let resp = req
        .json(&body)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        bail!("provider returned {}: {}", status, text.chars().take(500).collect::<String>());
    }

    let resp_json: Value = resp.json().await?;

    parse_response(&resp_json)
}

fn serialize_messages(messages: &[ProviderMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut obj = json!({
                "role": msg.role,
            });

            if let Some(ref content) = msg.content {
                obj["content"] = content.clone();
            } else {
                obj["content"] = Value::Null;
            }

            if let Some(ref calls) = msg.tool_calls {
                obj["tool_calls"] = json!(calls.iter().map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                }).collect::<Vec<_>>());
            }

            if let Some(ref id) = msg.tool_call_id {
                obj["tool_call_id"] = json!(id);
            }

            obj
        })
        .collect()
}

fn serialize_tools(tools: &[crate::directive::ToolSchema]) -> Value {
    json!(tools.iter().map(|t| {
        json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }
        })
    }).collect::<Vec<_>>())
}

fn parse_response(resp: &Value) -> Result<AdapterResponse> {
    let choice = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| anyhow::anyhow!("no choices in response"))?;

    let message_val = choice
        .get("message")
        .ok_or_else(|| anyhow::anyhow!("no message in choice"))?;

    let role = message_val
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant")
        .to_string();

    let content = message_val.get("content").cloned();
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(String::from);

    let tool_calls = message_val.get("tool_calls").and_then(|tc| tc.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|tc| {
                let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                let func = tc.get("function")?;
                let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let arguments = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
                Some(ToolCall { id, name, arguments: parse_tool_arguments(&arguments) })
            })
            .collect::<Vec<_>>()
    });

    let usage = resp.get("usage").map(|u| TokenUsage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    });

    Ok(AdapterResponse {
        message: ProviderMessage {
            role,
            content,
            tool_calls,
            tool_call_id: None,
        },
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

    #[test]
    fn parse_response_basic() {
        let resp = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let result = parse_response(&resp).unwrap();
        assert_eq!(result.message.role, "assistant");
        assert_eq!(result.message.content.unwrap(), "Hello!");
        assert_eq!(result.finish_reason.unwrap(), "stop");
        assert_eq!(result.usage.unwrap().input_tokens, 10);
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
        let result = parse_response(&resp).unwrap();
        let calls = result.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].id.as_deref(), Some("call_123"));
    }

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
    fn serialize_messages_format() {
        let msgs = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            tool_calls: None,
            tool_call_id: None,
        }];
        let serialized = serialize_messages(&msgs);
        assert_eq!(serialized[0]["role"], "user");
        assert_eq!(serialized[0]["content"], "hello");
    }
}
