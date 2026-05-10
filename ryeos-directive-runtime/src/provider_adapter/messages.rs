use serde_json::{json, Value};

use crate::directive::{MessageSchemas, ProviderMessage};

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
    use std::collections::HashMap;

    let role_map = schemas.role_map.as_ref();
    let content_key = schemas.content_key.as_deref().unwrap_or("content");
    let text_placement = schemas.text_placement.as_deref();
    let tc_placement = schemas
        .assistant_tool_calls_placement
        .as_deref()
        .unwrap_or("top_level_field");
    let system_mode = schemas
        .system_message
        .as_ref()
        .and_then(|s| s.mode.as_deref())
        .unwrap_or("body_field");
    let tool_result_wrap = schemas
        .tool_result
        .as_ref()
        .and_then(|t| t.wrap_key.as_deref());

    // Tool-result buffering for `wrap_mode: content_blocks` mode.
    // Accumulates rendered blocks across consecutive tool messages;
    // flushed to `converted` when a non-tool message is seen or at end.
    let tr_role = schemas
        .tool_result
        .as_ref()
        .and_then(|t| t.role.as_deref())
        .unwrap_or("tool");
    let tr_wrap_mode = schemas
        .tool_result
        .as_ref()
        .and_then(|t| t.wrap_mode.as_deref())
        .unwrap_or("direct");
    let tr_block_template = schemas
        .tool_result
        .as_ref()
        .and_then(|t| t.block_template.as_ref());
    let tc_block_template = schemas.tool_call_block_template.as_ref();

    // Build tool_call_id → tool_name lookup so tool-result messages
    // can populate provider templates that reference {tool_name}
    // (Gemini's functionResponse.name needs this; Gemini doesn't
    // return tool_call IDs so it identifies tool results by name).
    let mut tc_id_to_name: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if let Some(ref calls) = msg.tool_calls {
            for tc in calls {
                if let Some(ref id) = tc.id {
                    tc_id_to_name.insert(id.clone(), tc.name.clone());
                }
            }
        }
    }

    let mut extracted_system: Option<String> = None;
    let mut converted = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for msg in messages {
        // Extract system messages whenever they are NOT meant to ride
        // inline as a message-role entry. Both `body_field` (Anthropic)
        // and `body_inject` (Gemini) need extraction so the adapter can
        // route the text to the right place. Only `message_role`
        // (OpenAI) keeps the system message inline.
        if msg.role == "system" && system_mode != "message_role" {
            if let Some(ref content) = msg.content {
                let text = match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                extracted_system = Some(text);
            }
            continue;
        }

        // Flush any pending tool-result blocks before processing a
        // non-tool message (content_blocks wrap_mode requirement).
        if msg.role != "tool" && msg.tool_call_id.is_none()
            && !pending_tool_results.is_empty()
        {
            let mut tr_msg = json!({"role": tr_role});
            tr_msg[content_key] = json!(std::mem::take(&mut pending_tool_results));
            converted.push(tr_msg);
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
        } else if let Some(ref tc_id) = msg.tool_call_id {
            // Tool-result message. Three behaviours by wrap_mode:
            //   "content_blocks" — render block_template, buffer; flush on next non-tool msg.
            //   "direct"         — render block_template, attach to {role: <tr_role>, ...rendered}.
            //   "parts"          — render block_template, set role.parts = [rendered].
            let result_content_value = match &msg.content {
                Some(content) => content.clone(),
                None => json!(null),
            };
            let result_content_str = match &result_content_value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };

            let rendered_block = if let Some(tmpl) = tr_block_template {
                use ryeos_runtime::template::apply_template;
                let tool_name = tc_id_to_name
                    .get(tc_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            tool_call_id = tc_id,
                            "tool-result message has no preceding assistant tool_call \
                             with this id; tool_name in template will be empty"
                        );
                        String::new()
                    });
                let mut ctx: HashMap<String, Value> = HashMap::new();
                ctx.insert("tool_call_id".to_string(), json!(tc_id));
                ctx.insert("tool_name".to_string(), json!(tool_name));
                ctx.insert("content".to_string(), json!(result_content_str));
                apply_template(tmpl, &ctx)
            } else if let Some(wk) = tool_result_wrap {
                // Legacy fallback (v0.3.0-first-cut shape).
                json!({ wk: result_content_value, "tool_call_id": tc_id })
            } else {
                json!({"tool_call_id": tc_id, "content": result_content_value})
            };

            match tr_wrap_mode {
                "content_blocks" => {
                    pending_tool_results.push(rendered_block);
                    continue; // Don't push obj this iteration.
                }
                "parts" => {
                    let mut tr_msg = json!({"role": tr_role});
                    tr_msg[content_key] = json!([rendered_block]);
                    converted.push(tr_msg);
                    continue;
                }
                _ => {
                    // "direct" — flatten block keys into the message itself.
                    let mut tr_msg = json!({"role": tr_role});
                    if let Value::Object(obj_map) = rendered_block {
                        for (k, v) in obj_map {
                            tr_msg[k] = v;
                        }
                    }
                    converted.push(tr_msg);
                    continue;
                }
            }
        } else {
            let nested_content = match &msg.content {
                Some(content) => content.clone(),
                None => Value::Null,
            };
            match text_placement {
                Some("parts_array") | Some("blocks_array") => {
                    let template = schemas.text_block_template.as_ref();
                    let wrapped = wrap_content(nested_content, content_key, template);
                    obj[content_key] = wrapped;
                }
                _ => {
                    // "string" (default) — content is the raw value.
                    obj[content_key] = nested_content;
                }
            }
        }

        if let Some(ref calls) = msg.tool_calls {
            // Render each tool_call with the profile's template (if any).
            let rendered: Vec<Value> = calls
                .iter()
                .map(|tc| {
                    if let Some(tmpl) = tc_block_template {
                        use ryeos_runtime::template::apply_template;
                        let mut ctx: HashMap<String, Value> = HashMap::new();
                        ctx.insert("id".to_string(), json!(tc.id.clone().unwrap_or_default()));
                        ctx.insert("name".to_string(), json!(tc.name));
                        ctx.insert("input".to_string(), tc.arguments.clone());
                        // Provide both the structured input AND a JSON-string
                        // form, so OpenAI templates can use {input_json}.
                        let input_json = serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string());
                        ctx.insert("input_json".to_string(), json!(input_json));
                        apply_template(tmpl, &ctx)
                    } else {
                        // Legacy fallback (v0.3.0-first-cut OpenAI shape).
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                            }
                        })
                    }
                })
                .collect();

            match tc_placement {
                "inline_blocks" => {
                    // Append rendered tool_call blocks into the message's
                    // existing content/parts array.
                    let existing = obj.get(content_key).cloned().unwrap_or(json!([]));
                    let mut merged = match existing {
                        Value::Array(arr) => arr,
                        Value::Null => Vec::new(),
                        other => vec![other],
                    };
                    merged.extend(rendered);
                    obj[content_key] = json!(merged);
                }
                _ => {
                    // "top_level_field" (default) — OpenAI-style: tool_calls
                    // is a top-level array field on the assistant message.
                    obj["tool_calls"] = json!(rendered);
                }
            }
        }

        converted.push(obj);
    }

    // Final flush for any remaining pending tool-result blocks.
    if !pending_tool_results.is_empty() {
        let mut tr_msg = json!({"role": tr_role});
        tr_msg[content_key] = json!(std::mem::take(&mut pending_tool_results));
        converted.push(tr_msg);
    }

    (converted, extracted_system)
}

/// Wrap text content into a parts/blocks array using a per-block
/// template. The template's `{text}` placeholder is replaced with
/// each text fragment.
///
/// - If `template` is `Some`, each string fragment is rendered as
///   `apply_template(template, {"text": fragment})`.
///   Gemini template `{text: "{text}"}` → `{text: "fragment"}`.
///   Anthropic-blocks template `{type: "text", text: "{text}"}` →
///   `{type: "text", text: "fragment"}`.
/// - If `template` is `None`, falls back to `[{<content_key>: <fragment>}]`
///   for backwards compat (this is the buggy v0.3.0-first-cut shape;
///   only kept so any pre-template profile still parses).
///
/// Non-string content (already-array, already-object) is passed
/// through wholesale (assumed already in provider-native shape).
fn wrap_content(content: Value, content_key: &str, template: Option<&Value>) -> Value {
    use ryeos_runtime::template::apply_template;
    use std::collections::HashMap;

    let render_text = |text: &str| -> Value {
        match template {
            Some(t) => {
                let mut ctx: HashMap<String, Value> = HashMap::new();
                ctx.insert("text".to_string(), Value::String(text.to_string()));
                apply_template(t, &ctx)
            }
            None => json!({ content_key: text }),
        }
    };

    match &content {
        Value::String(s) => json!([render_text(s)]),
        Value::Null => json!([]),
        Value::Array(arr) => {
            if arr.is_empty() {
                json!([])
            } else {
                let parts: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        if let Some(s) = v.as_str() {
                            render_text(s)
                        } else {
                            // Already a structured part (e.g. provider-native
                            // image part, tool_use block) — pass through.
                            v.clone()
                        }
                    })
                    .collect();
                json!(parts)
            }
        }
        other => {
            // Non-string scalar (number, bool) — coerce via to_string for
            // template substitution. Rare in practice.
            json!([render_text(&other.to_string())])
        }
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
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
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
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: Some(SystemMessageConfig {
                mode: Some("body_inject".to_string()),
                field: None,
                template: None,
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
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: Some(SystemMessageConfig {
                mode: Some("message_role".to_string()),
                field: None,
                template: None,
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
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: None,
            tool_result: Some(ToolResultConfig {
                wrap_key: Some("result".to_string()),
                role: None,
                wrap_mode: None,
                block_template: None,
            }),
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        // Legacy wrap_key path: block rendered as {wrap_key: content, tool_call_id: id},
        // flattened into the message in "direct" wrap_mode.
        assert_eq!(converted[0]["result"], "result data");
        assert_eq!(converted[0]["tool_call_id"], "call_123");
    }

    #[test]
    fn text_placement_parts() {
        let msgs = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("Hello world")),
            tool_calls: None,
            tool_call_id: None,
        }];
        // Without text_placement: plain string content.
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas.clone()));
        assert_eq!(converted[0]["content"], "Hello world");

        // With text_placement + text_block_template (Gemini-style):
        // content_key = "parts", wrap produces [{text: "..."}].
        let schemas_wrap = MessageSchemas {
            content_key: Some("parts".to_string()),
            text_placement: Some("parts_array".to_string()),
            text_block_template: Some(json!({"text": "{text}"})),
            ..schemas.clone()
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas_wrap));
        assert_eq!(converted[0]["parts"], json!([{"text": "Hello world"}]),
            "Gemini-style parts_array with text_block_template");
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
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted[0]["text"], "test");
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
        assert_eq!(field_val("count"), Some(msgs.len().to_string().as_str()));
    }

    #[test]
    fn system_message_extracted_for_body_field_mode() {
        use crate::directive::SystemMessageConfig;
        let msgs = vec![
            ProviderMessage {
                role: "system".to_string(),
                content: Some(Value::String("you are helpful".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(Value::String("hi".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: None,
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: Some(SystemMessageConfig {
                mode: Some("body_field".to_string()),
                field: Some("system".to_string()),
                template: None,
            }),
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, system) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(
            system,
            Some("you are helpful".to_string()),
            "body_field mode must extract system text out of messages"
        );
        assert_eq!(
            converted.len(),
            1,
            "system message should be removed from list"
        );
        assert_eq!(converted[0]["role"], "user");
    }

    #[test]
    fn wrap_content_with_gemini_template_produces_text_parts() {
        let template = json!({"text": "{text}"});
        let result = super::wrap_content(
            Value::String("hello world".to_string()),
            "parts",
            Some(&template),
        );
        assert_eq!(result, json!([{"text": "hello world"}]));
    }

    #[test]
    fn wrap_content_with_anthropic_blocks_template() {
        let template = json!({"type": "text", "text": "{text}"});
        let result = super::wrap_content(
            Value::String("hi".to_string()),
            "content",
            Some(&template),
        );
        assert_eq!(result, json!([{"type": "text", "text": "hi"}]));
    }

    #[test]
    fn wrap_content_no_template_falls_back_to_content_key() {
        let result = super::wrap_content(
            Value::String("hi".to_string()),
            "content",
            None,
        );
        assert_eq!(result, json!([{"content": "hi"}]));
    }

    #[test]
    fn wrap_content_preserves_structured_array_parts() {
        let template = json!({"text": "{text}"});
        let result = super::wrap_content(
            json!([{"inlineData": {"mimeType": "image/png", "data": "..."}}]),
            "parts",
            Some(&template),
        );
        // Structured parts pass through unchanged.
        assert_eq!(result, json!([{"inlineData": {"mimeType": "image/png", "data": "..."}}]));
    }

    #[test]
    fn convert_messages_gemini_user_message_uses_text_in_parts() {
        let msgs = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(Value::String("hello".to_string())),
            tool_calls: None,
            tool_call_id: None,
        }];
        let schemas = MessageSchemas {
            role_map: Some({
                let mut m = std::collections::HashMap::new();
                m.insert("user".to_string(), "user".to_string());
                m
            }),
            content_key: Some("parts".to_string()),
            text_placement: Some("parts_array".to_string()),
            assistant_tool_calls_placement: None,
            text_block_template: Some(json!({"text": "{text}"})),
            tool_call_block_template: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(
            converted[0]["parts"],
            json!([{"text": "hello"}]),
            "Gemini wants {{text}}, not {{parts}}"
        );
    }

    #[test]
    fn convert_messages_anthropic_tool_result_uses_content_blocks_wrap() {
        let msgs = vec![
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(Value::String("result-1".to_string())),
                tool_calls: None,
                tool_call_id: Some("tc_1".to_string()),
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(Value::String("result-2".to_string())),
                tool_calls: None,
                tool_call_id: Some("tc_2".to_string()),
            },
        ];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: None,
            tool_result: Some(ToolResultConfig {
                wrap_key: None,
                role: Some("user".to_string()),
                wrap_mode: Some("content_blocks".to_string()),
                block_template: Some(json!({
                    "type": "tool_result",
                    "tool_use_id": "{tool_call_id}",
                    "content": "{content}",
                })),
            }),
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        // Two consecutive tool messages → ONE user message with two blocks.
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "user");
        let blocks = converted[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "tc_1");
        assert_eq!(blocks[0]["content"], "result-1");
        assert_eq!(blocks[1]["tool_use_id"], "tc_2");
    }

    #[test]
    fn convert_messages_openai_tool_result_uses_direct_wrap() {
        let msgs = vec![ProviderMessage {
            role: "tool".to_string(),
            content: Some(Value::String("calc result".to_string())),
            tool_calls: None,
            tool_call_id: Some("tc_xyz".to_string()),
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None,
            assistant_tool_calls_placement: None,
            text_block_template: None,
            tool_call_block_template: None,
            system_message: None,
            tool_result: Some(ToolResultConfig {
                wrap_key: None,
                role: Some("tool".to_string()),
                wrap_mode: Some("direct".to_string()),
                block_template: Some(json!({
                    "tool_call_id": "{tool_call_id}",
                    "content": "{content}",
                })),
            }),
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "tool");
        assert_eq!(converted[0]["tool_call_id"], "tc_xyz");
        assert_eq!(converted[0]["content"], "calc result");
    }

    #[test]
    fn convert_messages_anthropic_assistant_tool_calls_go_inline_as_blocks() {
        // Anthropic: text_placement=string (default), but assistant
        // tool_calls must land as content blocks.
        let msgs = vec![ProviderMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("Let me check that.".to_string())),
            tool_calls: Some(vec![crate::directive::ToolCall {
                id: Some("toolu_01abc".to_string()),
                name: "search".to_string(),
                arguments: json!({"q": "rust"}),
            }]),
            tool_call_id: None,
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None, // default = string
            assistant_tool_calls_placement: Some("inline_blocks".to_string()),
            text_block_template: None,
            tool_call_block_template: Some(json!({
                "type": "tool_use",
                "id": "{id}",
                "name": "{name}",
                "input": "{input}",
            })),
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "assistant");
        // tool_calls MUST NOT be at top level
        assert!(converted[0].get("tool_calls").is_none(),
            "Anthropic must NOT have top-level tool_calls; got: {}", converted[0]);
        // The tool_use block MUST be inside content array (alongside the text content).
        let content = converted[0].get("content").unwrap();
        let arr = content.as_array().expect("content should be an array after tool_call merge");
        let has_tool_use = arr.iter().any(|v| v.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        assert!(has_tool_use, "tool_use block missing from content: {:?}", arr);
    }

    #[test]
    fn convert_messages_openai_assistant_tool_calls_go_top_level() {
        // OpenAI: defaults — text=string, tool_calls=top_level_field.
        let msgs = vec![ProviderMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![crate::directive::ToolCall {
                id: Some("call_xyz".to_string()),
                name: "search".to_string(),
                arguments: json!({"q": "rust"}),
            }]),
            tool_call_id: None,
        }];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None,
            assistant_tool_calls_placement: None, // default = top_level_field
            text_block_template: None,
            tool_call_block_template: Some(json!({
                "id": "{id}",
                "type": "function",
                "function": {"name": "{name}", "arguments": "{input_json}"},
            })),
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(converted.len(), 1);
        let tc_arr = converted[0]["tool_calls"].as_array()
            .expect("OpenAI must have top-level tool_calls array");
        assert_eq!(tc_arr.len(), 1);
        assert_eq!(tc_arr[0]["id"], "call_xyz");
        assert_eq!(tc_arr[0]["function"]["name"], "search");
    }

    #[test]
    fn convert_messages_gemini_tool_result_uses_function_name_from_lookup() {
        use crate::directive::ToolCall;
        // Gemini scenario: prior assistant tool_call followed by a
        // tool result. The tool_name MUST be threaded through to the
        // template (functionResponse.name).
        let msgs = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(Value::String("search rust".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: Some("gemini_tc_0".to_string()),
                    name: "search".to_string(),
                    arguments: json!({"q": "rust"}),
                }]),
                tool_call_id: None,
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(Value::String("results: ...".to_string())),
                tool_calls: None,
                tool_call_id: Some("gemini_tc_0".to_string()),
            },
        ];
        let schemas = MessageSchemas {
            role_map: Some({
                let mut m = std::collections::HashMap::new();
                m.insert("user".to_string(), "user".to_string());
                m.insert("assistant".to_string(), "model".to_string());
                m
            }),
            content_key: Some("parts".to_string()),
            text_placement: Some("parts_array".to_string()),
            assistant_tool_calls_placement: Some("inline_blocks".to_string()),
            text_block_template: Some(json!({"text": "{text}"})),
            tool_call_block_template: Some(json!({
                "functionCall": {"name": "{name}", "args": "{input}"},
            })),
            system_message: None,
            tool_result: Some(ToolResultConfig {
                wrap_key: None,
                role: Some("user".to_string()),
                wrap_mode: Some("content_blocks".to_string()),
                block_template: Some(json!({
                    "functionResponse": {
                        "name": "{tool_name}",
                        "response": {"content": "{content}"},
                    },
                })),
            }),
            tool_list_wrap: None,
        };
        let (converted, _) = convert_messages(&msgs, &Some(schemas));
        // Three messages → three converted messages (user, assistant, user-tool-result).
        assert_eq!(converted.len(), 3, "got: {:#?}", converted);
        // Find the tool-result message (last).
        let tool_result_msg = &converted[2];
        assert_eq!(tool_result_msg["role"], "user");
        let parts = tool_result_msg["parts"].as_array().expect("parts is array");
        let fr = parts[0].get("functionResponse").expect("functionResponse block");
        // The KEY assertion: name MUST come from the lookup, not "".
        assert_eq!(fr["name"], "search",
            "tool_name must be threaded from preceding assistant tool_call; got: {}", fr);
        assert_eq!(fr["response"]["content"], "results: ...");
    }

    #[test]
    fn convert_messages_anthropic_full_round_trip_system_extraction_into_body_field() {
        // End-to-end: system + user → extracted_system + 1 user message.
        let msgs = vec![
            ProviderMessage {
                role: "system".to_string(),
                content: Some(Value::String("be brief".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(Value::String("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("content".to_string()),
            text_placement: None, // string (default)
            assistant_tool_calls_placement: Some("inline_blocks".to_string()),
            text_block_template: None,
            tool_call_block_template: None,
            system_message: Some(crate::directive::SystemMessageConfig {
                mode: Some("body_field".to_string()),
                field: Some("system".to_string()),
                template: None,
            }),
            tool_result: None,
            tool_list_wrap: None,
        };
        let (converted, system) = convert_messages(&msgs, &Some(schemas));
        assert_eq!(system, Some("be brief".to_string()),
            "system must be extracted into the side-channel return");
        assert_eq!(converted.len(), 1, "system msg removed; only user remains");
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[0]["content"], "hello",
            "Anthropic content stays as plain string when text_placement is unset");
    }
}
