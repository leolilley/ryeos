use std::collections::HashMap;

use serde_json::{json, Value};

use crate::directive::{MessageSchemas, ProviderMessage, ToolCall};

#[tracing::instrument(level = "debug", name = "provider:build_messages", skip(messages, schemas), fields(count = messages.len()))]
pub fn convert_messages(
    messages: &[ProviderMessage],
    schemas: &Option<MessageSchemas>,
) -> (Vec<Value>, Option<String>) {
    match schemas {
        None => convert_openai(messages),
        Some(s) => convert_with_schemas(messages, s),
    }
}

pub fn convert_response_message(
    provider_response: &Value,
    schemas: &Option<MessageSchemas>,
) -> ProviderMessage {
    match schemas {
        None => parse_openai_response(provider_response),
        Some(s) => parse_response_with_schemas(provider_response, s),
    }
}

fn convert_openai(messages: &[ProviderMessage]) -> (Vec<Value>, Option<String>) {
    let converted: Vec<Value> = messages
        .iter()
        .map(|msg| {
            let mut obj = json!({ "role": msg.role });
            match &msg.content {
                Some(content) => obj["content"] = content.clone(),
                None => obj["content"] = Value::Null,
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
        .collect();
    (converted, None)
}

fn convert_with_schemas(
    messages: &[ProviderMessage],
    schemas: &MessageSchemas,
) -> (Vec<Value>, Option<String>) {
    let role_map = schemas.role_map.as_ref();
    let content_key = schemas.content_key.as_deref().unwrap_or("content");
    let content_wrap = schemas.content_wrap.as_deref();
    let system_mode = schemas
        .system_message
        .as_ref()
        .and_then(|s| s.mode.as_deref())
        .unwrap_or("body_field");
    let tool_result_wrap = schemas
        .tool_result
        .as_ref()
        .and_then(|t| t.wrap_key.as_deref());

    let mut extracted_system: Option<String> = None;
    let mut converted = Vec::new();

    for msg in messages {
        if msg.role == "system" && system_mode == "body_inject" {
            if let Some(ref content) = msg.content {
                let text = match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                extracted_system = Some(text);
            }
            continue;
        }

        let mapped_role = match role_map {
            Some(rm) => rm.get(&msg.role).cloned().unwrap_or_else(|| msg.role.clone()),
            None => msg.role.clone(),
        };

        let mut obj = json!({ "role": mapped_role });

        if msg.role == "system" && system_mode == "message_role" {
            match &msg.content {
                Some(content) => obj[content_key] = content.clone(),
                None => obj[content_key] = Value::Null,
            }
        } else if msg.tool_call_id.is_some() {
            let result_content = match &msg.content {
                Some(content) => content.clone(),
                None => json!(null),
            };
            if let Some(wk) = tool_result_wrap {
                obj[content_key] = json!({ wk: result_content });
            } else {
                obj["content"] = result_content;
            }
            if let Some(ref id) = msg.tool_call_id {
                obj["tool_call_id"] = json!(id);
            }
        } else {
            let nested_content = match &msg.content {
                Some(content) => content.clone(),
                None => Value::Null,
            };
            if content_wrap.is_some() {
                let wrapped = wrap_content(nested_content, content_key, content_wrap.unwrap());
                obj[content_wrap.unwrap()] = wrapped;
            } else {
                obj[content_key] = nested_content;
            }
        }

        if let Some(ref calls) = msg.tool_calls {
            let formatted_calls: Vec<Value> = calls
                .iter()
                .map(|tc| {
                    let mut call_obj = json!({
                        "id": tc.id,
                        "type": "function",
                        "name": tc.name,
                        "arguments": tc.arguments,
                    });
                    if let Some(wrap) = content_wrap {
                        let args_val = match &tc.arguments {
                            Value::Object(_) => tc.arguments.clone(),
                            _ => json!({ "input": tc.arguments }),
                        };
                        call_obj[wrap] = json!([{ content_key: args_val }]);
                    }
                    call_obj
                })
                .collect();
            obj["tool_calls"] = json!(formatted_calls);
        }

        converted.push(obj);
    }

    (converted, extracted_system)
}

fn wrap_content(content: Value, content_key: &str, wrap_key: &str) -> Value {
    match &content {
        Value::String(s) => json!([{ content_key: s }]),
        Value::Null => json!([{ content_key: null }]),
        Value::Array(arr) => {
            if arr.is_empty() {
                json!([{ content_key: null }])
            } else {
                let parts: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        if v.is_string() {
                            json!({ content_key: v })
                        } else {
                            v.clone()
                        }
                    })
                    .collect();
                json!(parts)
            }
        }
        other => json!([{ content_key: other }]),
    }
}

fn unwrap_content(val: &Value, content_key: &str, content_wrap: &str) -> Value {
    if let Some(parts) = val.get(content_wrap).and_then(|v| v.as_array()) {
        let strings: Vec<String> = parts
            .iter()
            .filter_map(|p| p.get(content_key).and_then(|v| v.as_str()).map(String::from))
            .collect();
        if strings.len() == 1 {
            json!(strings[0])
        } else if strings.is_empty() {
            Value::Null
        } else {
            json!(strings.join(""))
        }
    } else if let Some(s) = val.get(content_key) {
        s.clone()
    } else {
        Value::Null
    }
}

fn parse_openai_response(resp: &Value) -> ProviderMessage {
    let choice = resp
        .get("choices")
        .and_then(|c| c.get(0));
    let message_val = choice
        .and_then(|c| c.get("message"))
        .unwrap_or(resp);

    let role = message_val
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant")
        .to_string();

    let content = message_val.get("content").cloned();

    let tool_calls = message_val.get("tool_calls").and_then(|tc| tc.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|tc| {
                let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                let func = tc.get("function")?;
                let name = func
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = func
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();
                Some(ToolCall {
                    id,
                    name,
                    arguments: crate::provider_adapter::http::parse_tool_arguments(&arguments),
                })
            })
            .collect::<Vec<_>>()
    });

    ProviderMessage {
        role,
        content,
        tool_calls,
        tool_call_id: None,
    }
}

fn parse_response_with_schemas(
    resp: &Value,
    schemas: &MessageSchemas,
) -> ProviderMessage {
    let content_key = schemas.content_key.as_deref().unwrap_or("content");
    let content_wrap = schemas.content_wrap.as_deref();

    let inverse_role_map: HashMap<String, String> = schemas
        .role_map
        .as_ref()
        .map(|rm| {
            rm.iter()
                .map(|(k, v)| (v.clone(), k.clone()))
                .collect()
        })
        .unwrap_or_default();

    let message_val = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .unwrap_or(resp);

    let raw_role = message_val
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");
    let role = inverse_role_map
        .get(raw_role)
        .cloned()
        .unwrap_or_else(|| raw_role.to_string());

    let content = if let Some(wrap) = content_wrap {
        Some(unwrap_content(message_val, content_key, wrap))
    } else {
        message_val.get(content_key).cloned()
    };

    let tool_calls = message_val
        .get("tool_calls")
        .or_else(|| message_val.get("function_call"))
        .and_then(|tc| {
            if tc.is_array() {
                Some(tc.as_array().unwrap().clone())
            } else {
                Some(vec![tc.clone()])
            }
        })
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                    let name = tc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| tc.get("function").and_then(|f| f.get("name").and_then(|v| v.as_str())))
                        .unwrap_or("")
                        .to_string();
                    let arguments = tc
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            tc.get("function")
                                .and_then(|f| f.get("arguments").and_then(|v| v.as_str()))
                        })
                        .unwrap_or("{}");
                    Some(ToolCall {
                        id,
                        name,
                        arguments: crate::provider_adapter::http::parse_tool_arguments(arguments),
                    })
                })
                .collect::<Vec<_>>()
        });

    ProviderMessage {
        role,
        content,
        tool_calls,
        tool_call_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::{MessageSchemas, ProviderMessage, SystemMessageConfig, ToolResultConfig};
    use ryeos_tracing::test as trace_test;

    fn sample_messages() -> Vec<ProviderMessage> {
        vec![
            ProviderMessage {
                role: "system".to_string(),
                content: Some(json!("You are helpful.")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("Hello")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Hi there!")),
                tool_calls: None,
                tool_call_id: None,
            },
        ]
    }

    #[test]
    fn default_openai_format() {
        let msgs = sample_messages();
        let (converted, system) = convert_messages(&msgs, &None);
        assert_eq!(system, None);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[1]["content"], "Hello");
        assert_eq!(converted[2]["role"], "assistant");
    }

    #[test]
    fn role_map_gemini_style() {
        let msgs = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("Hello")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Hi!")),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let schemas = MessageSchemas {
            role_map: Some(
                vec![("assistant".to_string(), "model".to_string())]
                    .into_iter()
                    .collect(),
            ),
            content_key: None,
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted[1]["role"], "model");
    }

    #[test]
    fn system_message_body_inject() {
        let msgs = sample_messages();
        let schemas = MessageSchemas {
            role_map: None,
            content_key: None,
            content_wrap: None,
            system_message: Some(SystemMessageConfig {
                mode: Some("body_inject".to_string()),
            }),
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, system) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(converted.len(), 2);
        assert!(converted.iter().all(|m| m.get("role").and_then(|r| r.as_str()) != Some("system")));
    }

    #[test]
    fn system_message_as_message_role() {
        let msgs = sample_messages();
        let schemas = MessageSchemas {
            role_map: Some(
                vec![("system".to_string(), "my_system".to_string())]
                    .into_iter()
                    .collect(),
            ),
            content_key: None,
            content_wrap: None,
            system_message: Some(SystemMessageConfig {
                mode: Some("message_role".to_string()),
            }),
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, system) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(system, None);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "my_system");
    }

    #[test]
    fn tool_result_with_wrap_key() {
        let msgs = vec![ProviderMessage {
            role: "tool".to_string(),
            content: Some(json!("result data")),
            tool_calls: None,
            tool_call_id: Some("call_123".to_string()),
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: None,
            content_wrap: None,
            system_message: None,
            tool_result: Some(ToolResultConfig {
                wrap_key: Some("result".to_string()),
            }),
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted[0]["content"]["result"], "result data");
        assert_eq!(converted[0]["tool_call_id"], "call_123");
    }

    #[test]
    fn content_wrap_parts() {
        let msgs = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("Hello world")),
            tool_calls: None,
            tool_call_id: None,
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: None,
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas.clone()));
        assert_eq!(converted[0]["content"], "Hello world");

        let schemas_wrap = MessageSchemas {
            content_wrap: Some("parts".to_string()),
            ..schemas.clone()
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas_wrap));
        assert_eq!(converted[0]["parts"][0]["content"], "Hello world");
    }

    #[test]
    fn roundtrip_openai() {
        let msgs = vec![ProviderMessage {
            role: "assistant".to_string(),
            content: Some(json!("Hello!")),
            tool_calls: Some(vec![ToolCall {
                id: Some("call_1".to_string()),
                name: "bash".to_string(),
                arguments: json!({"cmd": "ls"}),
            }]),
            tool_call_id: None,
        }];
        let (converted, _) = convert_messages(&msgs, &None);
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"cmd\":\"ls\"}"
                        }
                    }]
                },
                "finish_reason": "stop"
            }]
        });
        let parsed = convert_response_message(&resp, &None);
        assert_eq!(parsed.role, "assistant");
        assert_eq!(parsed.content.unwrap(), "Hello!");
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["cmd"], "ls");
    }

    #[test]
    fn content_key_custom() {
        let msgs = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("test")),
            tool_calls: None,
            tool_call_id: None,
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("text".to_string()),
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted[0]["text"], "test");
    }

    #[test]
    fn response_parsing_with_inverse_role_map() {
        let schemas = MessageSchemas {
            role_map: Some(
                vec![("assistant".to_string(), "model".to_string())]
                    .into_iter()
                    .collect(),
            ),
            content_key: None,
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let resp = json!({
            "choices": [{
                "message": { "role": "model", "content": "Hi!" },
                "finish_reason": "stop"
            }]
        });
        let parsed = convert_response_message(&resp, &Some(schemas));
        assert_eq!(parsed.role, "assistant");
        assert_eq!(parsed.content.unwrap(), "Hi!");
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn convert_messages_emits_span() {
        let msgs = sample_messages();
        let (_, spans) = trace_test::capture_traces(|| {
            convert_messages(&msgs, &None);
        });

        let span = trace_test::find_span(&spans, "provider:build_messages");
        assert!(span.is_some(), "expected provider:build_messages span, got: {:?}", spans.iter().map(|s: &ryeos_tracing::test::RecordedSpan| &s.name).collect::<Vec<_>>());

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("count"), Some(&msgs.len().to_string()));
    }
}
