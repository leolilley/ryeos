use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::directive::{
    ExecutionConfig, ProviderConfig, ProviderMessage, StreamEvent, ToolCall, ToolSchema,
};
use crate::provider_adapter::http::{AdapterResponse, TokenUsage};
use ryeos_runtime::callback_client::CallbackClient;

/// Parse a complete SSE buffer (no streaming state continuation).
/// Test-only helper retained because the existing parser unit tests
/// feed pre-framed buffers; production streaming uses the
/// state-preserving `parse_sse_events_with_state` directly.
#[tracing::instrument(level = "debug", name = "provider:parse_sse", skip(data), fields(byte_len = data.len()))]
#[cfg(test)]
fn parse_sse_events(data: &str, mode: Option<&str>) -> Vec<StreamEvent> {
    let mut tool_call_state: HashMap<String, String> = HashMap::new();
    parse_sse_events_with_state(data, mode, &mut tool_call_state)
}

/// State-preserving variant of `parse_sse_events`. The caller threads
/// in a `tool_call_state` map that survives across multiple calls so
/// streaming tool-use arguments fragmented across SSE event blocks
/// concatenate correctly. Used by `call_provider_streaming` to drain
/// one event block at a time without losing partial tool_use state.
pub fn parse_sse_events_with_state(
    data: &str,
    mode: Option<&str>,
    tool_call_state: &mut HashMap<String, String>,
) -> Vec<StreamEvent> {
    let raw_events = split_sse_events(data);
    let mut events = Vec::new();

    for (event_type, payload) in raw_events {
        if payload == "[DONE]" {
            events.push(StreamEvent::Done);
            continue;
        }

        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match mode {
            Some("event_typed") => {
                parse_event_typed(&event_type, &parsed, &mut events, tool_call_state);
            }
            Some("delta_merge") => {
                parse_delta_merge(&parsed, &mut events, tool_call_state);
            }
            Some("complete_chunks") => {
                parse_complete_chunks(&parsed, &mut events);
            }
            _ => {}
        }
    }

    events
}

fn split_sse_events(data: &str) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let mut current_event_type = String::new();
    let mut current_data = String::new();

    for line in data.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if !current_data.is_empty() {
                events.push((current_event_type.clone(), current_data.trim().to_string()));
                current_event_type.clear();
                current_data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            current_event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !current_data.is_empty() {
                current_data.push('\n');
            }
            current_data.push_str(rest.trim());
        }
        if line.strip_prefix("id:").is_some() || line.strip_prefix("retry:").is_some() {
        }
    }
    if !current_data.is_empty() {
        events.push((current_event_type, current_data.trim().to_string()));
    }

    events
}

fn parse_event_typed(
    event_type: &str,
    parsed: &Value,
    events: &mut Vec<StreamEvent>,
    tool_call_state: &mut HashMap<String, String>,
) {
    match event_type {
        "content_block_delta" => {
            if let Some(delta) = parsed.get("delta") {
                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                    events.push(StreamEvent::Delta(text.to_string()));
                } else if let Some(input) = delta.get("partial_json").and_then(|j| j.as_str()) {
                    // Only accumulate when we know which tool the
                    // partial_json belongs to (set by a preceding
                    // content_block_start). Anthropic guarantees
                    // ordering; this guard is defensive.
                    if tool_call_state.contains_key("current_tool_id")
                        && tool_call_state.contains_key("current_tool_name")
                    {
                        tool_call_state
                            .entry("current_tool_args".to_string())
                            .and_modify(|e| e.push_str(input))
                            .or_insert_with(|| input.to_string());
                    }
                }
            }
        }
        "content_block_start" => {
            if let Some(content_block) = parsed.get("content_block") {
                if let Some("tool_use") = content_block.get("type").and_then(|t| t.as_str()) {
                    let id = content_block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = content_block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    tool_call_state.insert("current_tool_id".to_string(), id);
                    tool_call_state.insert("current_tool_name".to_string(), name);
                    tool_call_state.remove("current_tool_args");
                }
            }
        }
        "content_block_stop"
            if tool_call_state.contains_key("current_tool_id") => {
                let id = tool_call_state
                    .get("current_tool_id")
                    .cloned()
                    .unwrap_or_default();
                let name = tool_call_state
                    .get("current_tool_name")
                    .cloned()
                    .unwrap_or_default();
                let arguments = tool_call_state
                    .get("current_tool_args")
                    .cloned()
                    .unwrap_or_else(|| "{}".to_string());
                events.push(StreamEvent::ToolUse { id, name, arguments });
                tool_call_state.remove("current_tool_id");
                tool_call_state.remove("current_tool_name");
                tool_call_state.remove("current_tool_args");
            }
        "message_stop" => {
            events.push(StreamEvent::Done);
        }
        _ => {}
    }
}

fn parse_delta_merge(
    parsed: &Value,
    events: &mut Vec<StreamEvent>,
    tool_call_state: &mut HashMap<String, String>,
) {
    if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(content) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
            {
                events.push(StreamEvent::Delta(content.to_string()));
            }

            if let Some(tool_calls) = choice
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .and_then(|tc| tc.as_array())
            {
                for tc in tool_calls {
                    let index = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .to_string();

                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        tool_call_state.insert(format!("tool_id_{}", index), id.to_string());
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            tool_call_state.insert(format!("tool_name_{}", index), name.to_string());
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_call_state
                                .entry(format!("tool_args_{}", index))
                                .and_modify(|e| e.push_str(args))
                                .or_insert_with(|| args.to_string());
                        }
                    }
                }
            }

            if let Some("stop") = choice.get("finish_reason").and_then(|f| f.as_str()) {
                let mut indices: Vec<u64> = Vec::new();
                for (key, _) in tool_call_state.iter() {
                    if let Some(idx) = key.strip_prefix("tool_id_") {
                        if let Ok(n) = idx.parse::<u64>() {
                            indices.push(n);
                        }
                    }
                }
                indices.sort();
                for idx in indices.iter() {
                    let id = tool_call_state
                        .get(&format!("tool_id_{}", idx))
                        .cloned()
                        .unwrap_or_default();
                    let name = tool_call_state
                        .get(&format!("tool_name_{}", idx))
                        .cloned()
                        .unwrap_or_default();
                    let arguments = tool_call_state
                        .get(&format!("tool_args_{}", idx))
                        .cloned()
                        .unwrap_or_else(|| "{}".to_string());
                    events.push(StreamEvent::ToolUse { id, name, arguments });
                }
                for idx in indices.iter() {
                    let idx_s = idx.to_string();
                    tool_call_state.remove(&format!("tool_id_{}", idx_s));
                    tool_call_state.remove(&format!("tool_name_{}", idx_s));
                    tool_call_state.remove(&format!("tool_args_{}", idx_s));
                }
                events.push(StreamEvent::Done);
            }
        }
    }
}

fn parse_complete_chunks(parsed: &Value, events: &mut Vec<StreamEvent>) {
    if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(message) = choice.get("message") {
                if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                    events.push(StreamEvent::Delta(content.to_string()));
                }
                if let Some(tool_calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name").and_then(|v| v.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let arguments = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        events.push(StreamEvent::ToolUse { id, name, arguments });
                    }
                }
            }
            if let Some("stop") = choice.get("finish_reason").and_then(|f| f.as_str()) {
                events.push(StreamEvent::Done);
            }
        }
    }
}

/// Streaming provider call. Issues a streaming POST (`stream: true` +
/// `stream_options.include_usage: true` for OpenAI-compatible
/// providers) and parses Server-Sent Events incrementally as they
/// arrive. For each `StreamEvent::Delta` and `StreamEvent::ToolUse` the
/// callback's `cognition_out` event is appended to the durable event
/// log BEFORE moving to the next chunk — this is the persistence-first
/// streaming contract: live broadcasts may never deliver a delta the
/// store doesn't already hold.
///
/// Returns the accumulated `AdapterResponse` (final assistant message
/// + usage + finish_reason) plus the full sequence of `StreamEvent`s
/// for the runner to consume.
#[tracing::instrument(
    name = "provider:request_streaming",
    skip(client, messages, tools, callback),
    fields(adapter_type = "stream", model = %model, turn = turn)
)]
pub async fn call_provider_streaming(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    execution: &ExecutionConfig,
    model: &str,
    messages: &[ProviderMessage],
    tools: &[ToolSchema],
    callback: &CallbackClient,
    turn: u32,
) -> Result<(AdapterResponse, Vec<StreamEvent>)> {
    let schemas = provider.schemas.as_ref().and_then(|s| s.messages.as_ref());

    let (converted_messages, system_prompt) =
        crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());

    let tools_val =
        crate::provider_adapter::tools::serialize_tools(tools, &schemas.cloned());

    let stream_url = provider.extra.get("stream_url").and_then(|v| v.as_str());
    let url = match stream_url {
        Some(su) => format!(
            "{}{}",
            provider.base_url.trim_end_matches('/'),
            if su.starts_with('/') { su.to_string() } else { format!("/{}", su) }
        ),
        None => format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        ),
    };

    let mut body = json!({
        "model": model,
        "messages": converted_messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(sys) = &system_prompt {
        body["system"] = json!(sys);
    }

    if !tools.is_empty() {
        body["tools"] = tools_val;
    }

    let mut req = client.post(&url);

    let auth_header = provider
        .auth
        .header_name
        .as_deref()
        .unwrap_or("Authorization");
    let auth_prefix = provider.auth.prefix.as_deref().unwrap_or("Bearer ");
    if let Some(ref env_var) = provider.auth.env_var {
        let key = std::env::var(env_var).map_err(|_| {
            anyhow!(
                "provider auth env var {env_var} is not set; refusing to call provider \
                 with no credentials (typed-fail-loud)"
            )
        })?;
        req = req.header(auth_header, format!("{}{}", auth_prefix, key));
    }

    for (k, v) in &provider.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req = req.header("Accept", "text/event-stream");

    let resp = req
        .json(&body)
        .timeout(Duration::from_secs(execution.timeout_seconds))
        .send()
        .await
        .map_err(|e| anyhow!("streaming request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "provider returned {} (streaming): {}",
            status,
            text.chars().take(500).collect::<String>()
        );
    }

    let stream_mode = provider
        .schemas
        .as_ref()
        .and_then(|s| s.streaming.as_ref())
        .and_then(|st| st.mode.as_deref())
        .unwrap_or("delta_merge")
        .to_string();

    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut all_events: Vec<StreamEvent> = Vec::new();
    let mut accumulated_text = String::new();
    let mut accumulated_tools: Vec<ToolCall> = Vec::new();
    let mut last_usage: Option<TokenUsage> = None;
    let mut last_finish: Option<String> = None;
    // Per-stream tool-call accumulator keyed by index; survives
    // across chunks so partial JSON arg fragments concatenate
    // correctly under both Anthropic (`partial_json`) and OpenAI
    // (`tool_calls[].function.arguments`) wire shapes.
    let mut tool_state: HashMap<String, String> = HashMap::new();

    while let Some(chunk_res) = byte_stream.next().await {
        let chunk: Bytes = chunk_res.map_err(|e| anyhow!("stream chunk error: {e}"))?;
        let text = std::str::from_utf8(&chunk)
            .map_err(|e| anyhow!("non-utf8 SSE chunk: {e}"))?;
        buffer.push_str(text);

        // Drain complete SSE event blocks (separated by blank line).
        loop {
            let cut = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n"));
            let Some(idx) = cut else { break };
            let sep_len = if buffer[idx..].starts_with("\r\n\r\n") { 4 } else { 2 };
            let block: String = buffer[..idx].to_string();
            buffer.drain(..idx + sep_len);

            // Pull provider-side usage + finish_reason from the raw
            // chunk JSON regardless of stream_mode (the existing
            // parse_* helpers don't surface these typed pieces).
            harvest_chunk_meta(&block, &mut last_usage, &mut last_finish);

            // Re-frame the block as a complete SSE buffer for the
            // existing parser (which expects `\n\n` framing). Threads
            // the persistent tool_state so partial tool_use args
            // fragmented across blocks concatenate.
            let framed = format!("{}\n\n", block);
            let new_events = parse_sse_events_with_state(
                &framed,
                Some(&stream_mode),
                &mut tool_state,
            );

            for ev in new_events {
                match &ev {
                    StreamEvent::Delta(text) => {
                        accumulated_text.push_str(text);
                        callback
                            .append_event(
                                "cognition_out",
                                json!({
                                    "turn": turn,
                                    "delta": text,
                                }),
                            )
                            .await
                            .map_err(|e| {
                                anyhow!(
                                    "persistence-first violation: cognition_out delta \
                                     append failed mid-stream: {e}"
                                )
                            })?;
                    }
                    StreamEvent::ToolUse {
                        id,
                        name,
                        arguments,
                    } => {
                        let args_value: Value = serde_json::from_str(arguments).map_err(|e| {
                            anyhow!(
                                "malformed streaming tool_use arguments for {name} \
                                 (id={id}): {e}; raw={}",
                                arguments
                            )
                        })?;
                        accumulated_tools.push(ToolCall {
                            id: Some(id.clone()),
                            name: name.clone(),
                            arguments: args_value,
                        });
                        callback
                            .append_event(
                                "cognition_out",
                                json!({
                                    "turn": turn,
                                    "tool_use": {
                                        "id": id,
                                        "name": name,
                                        "arguments": arguments,
                                    },
                                }),
                            )
                            .await
                            .map_err(|e| {
                                anyhow!(
                                    "persistence-first violation: cognition_out \
                                     tool_use append failed mid-stream: {e}"
                                )
                            })?;
                    }
                    StreamEvent::Done => {}
                }
                all_events.push(ev);
            }

        }
    }

    // Final flush: any trailing block without terminal blank line.
    if !buffer.trim().is_empty() {
        let framed = format!("{}\n\n", buffer);
        harvest_chunk_meta(&buffer, &mut last_usage, &mut last_finish);
        let final_events = parse_sse_events_with_state(
            &framed,
            Some(&stream_mode),
            &mut tool_state,
        );
        for ev in final_events {
            match &ev {
                StreamEvent::Delta(text) => {
                    accumulated_text.push_str(text);
                    callback
                        .append_event(
                            "cognition_out",
                            json!({"turn": turn, "delta": text}),
                        )
                        .await
                        .map_err(|e| {
                            anyhow!(
                                "persistence-first violation: cognition_out delta \
                                 append failed at stream end: {e}"
                            )
                        })?;
                }
                StreamEvent::ToolUse { id, name, arguments } => {
                    let args_value: Value = serde_json::from_str(arguments).map_err(|e| {
                        anyhow!(
                            "malformed streaming tool_use arguments for {name} \
                             (id={id}): {e}; raw={}",
                            arguments
                        )
                    })?;
                    accumulated_tools.push(ToolCall {
                        id: Some(id.clone()),
                        name: name.clone(),
                        arguments: args_value,
                    });
                    callback
                        .append_event(
                            "cognition_out",
                            json!({
                                "turn": turn,
                                "tool_use": {
                                    "id": id,
                                    "name": name,
                                    "arguments": arguments,
                                },
                            }),
                        )
                        .await
                        .map_err(|e| {
                            anyhow!(
                                "persistence-first violation: cognition_out tool_use \
                                 append failed at stream end: {e}"
                            )
                        })?;
                }
                StreamEvent::Done => {}
            }
            all_events.push(ev);
        }
    }

    let message = ProviderMessage {
        role: "assistant".to_string(),
        content: if accumulated_text.is_empty() {
            None
        } else {
            Some(json!(accumulated_text))
        },
        tool_calls: if accumulated_tools.is_empty() {
            None
        } else {
            Some(accumulated_tools)
        },
        tool_call_id: None,
    };

    Ok((
        AdapterResponse {
            message,
            usage: last_usage,
            finish_reason: last_finish,
        },
        all_events,
    ))
}

/// Pull provider-side metadata (usage totals, finish_reason) out of a
/// single SSE event block. The existing `parse_event_typed` /
/// `parse_delta_merge` helpers only emit `StreamEvent`s; we still need
/// to surface `TokenUsage` + `finish_reason` for cost accounting and
/// the runner's State::ParsingResponse routing.
fn harvest_chunk_meta(
    block: &str,
    last_usage: &mut Option<TokenUsage>,
    last_finish: &mut Option<String>,
) {
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
            Err(_) => continue,
        };

        if let Some(u) = parsed.get("usage") {
            let input = u
                .get("prompt_tokens")
                .or_else(|| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = u
                .get("completion_tokens")
                .or_else(|| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if input > 0 || output > 0 {
                *last_usage = Some(TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                });
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tracing::test as trace_test;

    #[test]
    fn sse_event_typed_anthropic() {
        let data = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","role":"assistant"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_stop
data: {"type":"message_stop"}
"#;
        let events = parse_sse_events(data, Some("event_typed"));
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::Delta(s) if s == "Hello"));
        assert!(matches!(&events[1], StreamEvent::Delta(s) if s == " world"));
        assert!(matches!(&events[2], StreamEvent::Done));
    }

    #[test]
    fn sse_event_typed_tool_use() {
        let data = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_stop
data: {"type":"message_stop"}
"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse { id, name, arguments } = &tool_uses[0] {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "bash");
            assert_eq!(arguments, r#"{"cmd":"ls"}"#);
        }
    }

    #[test]
    fn sse_delta_merge_openai() {
        let data = r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let deltas: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Delta(_))).collect();
        let dones: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Done)).collect();
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], StreamEvent::Delta(s) if s == "Hello"));
        assert!(matches!(&deltas[1], StreamEvent::Delta(s) if s == " world"));
        assert!(!dones.is_empty());
    }

    #[test]
    fn sse_delta_merge_tool_calls() {
        let data = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse { id, name, arguments } = &tool_uses[0] {
            assert_eq!(id, "call_1");
            assert_eq!(name, "bash");
            assert_eq!(arguments, r#"{"cmd":"ls"}"#);
        }
    }

    #[test]
    fn sse_complete_chunks() {
        let data = r#"data: {"choices":[{"message":{"role":"assistant","content":"Full response"},"finish_reason":"stop"}]}
"#;
        let events = parse_sse_events(data, Some("complete_chunks"));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Delta(s) if s == "Full response"));
        assert!(matches!(&events[1], StreamEvent::Done));
    }

    #[test]
    fn done_sentinel() {
        let data = "data: [DONE]\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(data, Some("delta_merge"));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Done));
        assert!(matches!(&events[1], StreamEvent::Done));
    }

    #[test]
    fn blank_line_splitting() {
        let data = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, r#"{"a":1}"#);
        assert_eq!(events[1].1, r#"{"b":2}"#);
    }

    #[test]
    fn data_line_concatenation() {
        let data = "data: part1\ndata: part2\n\ndata: second\n\n";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "part1\npart2");
        assert_eq!(events[1].1, "second");
    }

    #[test]
    fn no_mode_returns_empty() {
        let data = "data: {\"a\":1}\n\n";
        let events = parse_sse_events(data, None);
        assert!(events.is_empty());
    }

    #[test]
    fn event_type_tracking() {
        let data = "event: my_event\ndata: {\"x\":1}\n\n";
        let events = split_sse_events(data);
        assert_eq!(events[0].0, "my_event");
    }

    #[test]
    fn incomplete_data_no_final_event() {
        let data = "data: {\"a\":1}";
        let events = split_sse_events(data);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, r#"{"a":1}"#);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn parse_sse_events_emits_span() {
        let data = r#"data: {"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}
"#;
        let (_, spans) = trace_test::capture_traces(|| {
            parse_sse_events(data, Some("complete_chunks"));
        });

        let span = trace_test::find_span(&spans, "provider:parse_sse");
        assert!(span.is_some(), "expected provider:parse_sse span, got: {:?}", spans.iter().map(|s: &ryeos_tracing::test::RecordedSpan| &s.name).collect::<Vec<_>>());
    }
}
