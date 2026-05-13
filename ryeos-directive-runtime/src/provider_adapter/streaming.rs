use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::directive::{
    ExecutionConfig, FinishReason, ProtocolFamily, ProviderConfig, ProviderMessage,
    SamplingConfig, StreamEvent, SystemMessageMode, ToolCall, ToolSchema,
    UsageUpdate, normalize_finish_reason,
};
use crate::provider_adapter::http::{AdapterResponse, TokenUsage};
use ryeos_runtime::callback_client::CallbackClient;

/// Compute a sha256 hex digest of the input bytes.
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Redact provider error bodies by default. Returns a safe preview
/// (first 200 chars) plus a sha256 hash. Full body is logged only when
/// `RYEOS_LOG_PROVIDER_BODIES=1` is set (development/debugging).
fn safe_error_body(body: &str) -> String {
    let truncated: String = body.chars().take(512).collect();
    if truncated.len() < body.len() {
        format!("{}… [truncated, sha256={}]", truncated, sha256_hex(body.as_bytes()))
    } else {
        truncated
    }
}

/// Check if the cancellation flag is set.
fn cancelled(flag: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> bool {
    flag.as_ref()
        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

/// Async future that resolves when the cancellation flag is set.
/// Polls with a 50ms backoff — bounded cancellation latency.
async fn cancel_signal(flag: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
    if let Some(f) = flag {
        loop {
            if f.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    } else {
        std::future::pending::<()>().await;
    }
}

/// Parse a complete SSE buffer (no streaming state continuation).
/// Test-only helper retained because the existing parser unit tests
/// feed pre-framed buffers; production streaming uses the
/// state-preserving `parse_sse_events_with_state` directly.
#[tracing::instrument(level = "debug", name = "provider:parse_sse", skip(data), fields(byte_len = data.len()))]
#[cfg(test)]
fn parse_sse_events(data: &str, mode: Option<&str>) -> Vec<StreamEvent> {
    let mut tool_call_state: HashMap<String, String> = HashMap::new();
    parse_sse_events_with_state(data, mode, None, &mut tool_call_state)
}

/// State-preserving variant of `parse_sse_events`. The caller threads
/// in a `tool_call_state` map that survives across multiple calls so
/// streaming tool-use arguments fragmented across SSE event blocks
/// concatenate correctly. Used by `call_provider_streaming` to drain
/// one event block at a time without losing partial tool_use state.
pub fn parse_sse_events_with_state(
    data: &str,
    mode: Option<&str>,
    stream_paths: Option<&crate::directive::StreamPaths>,
    tool_call_state: &mut HashMap<String, String>,
) -> Vec<StreamEvent> {
    let raw_events = split_sse_events(data);
    let mut events = Vec::new();

    for (event_type, payload) in raw_events {
        if payload == "[DONE]" {
            events.push(StreamEvent::Finish { reason: FinishReason::Stop, raw: None });
            continue;
        }

        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    payload_preview = %payload.chars().take(80).collect::<String>(),
                    "SSE: skipping malformed JSON payload: {e:#}"
                );
                continue;
            }
        };

        match mode {
            Some("event_typed") => {
                parse_event_typed(&event_type, &parsed, &mut events, tool_call_state);
            }
            Some("delta_merge") => {
                parse_delta_merge(&parsed, &mut events, tool_call_state);
            }
            Some("complete_chunks") => {
                parse_complete_chunks(&parsed, &mut events, stream_paths, tool_call_state);
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
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            events.push(StreamEvent::Delta(text.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(|t| t.as_str()) {
                            events.push(StreamEvent::ReasoningDelta(text.to_string()));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(input) = delta.get("partial_json").and_then(|j| j.as_str()) {
                            if tool_call_state.contains_key("current_tool_id")
                                && tool_call_state.contains_key("current_tool_name")
                            {
                                let total_len = tool_call_state
                                    .entry("current_tool_args".to_string())
                                    .and_modify(|e| e.push_str(input))
                                    .or_insert_with(|| input.to_string())
                                    .len();
                                let id = tool_call_state
                                    .get("current_tool_id")
                                    .cloned()
                                    .filter(|s| !s.is_empty());
                                let name = tool_call_state
                                    .get("current_tool_name")
                                    .cloned()
                                    .unwrap_or_default();
                                events.push(StreamEvent::ToolUsePartial {
                                    id,
                                    name,
                                    delta: input.to_string(),
                                    total_len,
                                });
                            }
                        }
                    }
                    other => {
                        events.push(StreamEvent::Warning {
                            code: "unknown_delta_type".into(),
                            message: format!("anthropic content_block_delta type `{other}` not handled"),
                        });
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
                    if id.is_empty() || name.is_empty() {
                        tracing::warn!(
                            id = id,
                            name = name,
                            "SSE event_typed: content_block_start tool_use missing id or name, skipping"
                        );
                    } else {
                        tool_call_state.insert("current_tool_id".to_string(), id);
                        tool_call_state.insert("current_tool_name".to_string(), name);
                    }
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
                let arguments_value: Value = serde_json::from_str(&arguments)
                    .unwrap_or_else(|_| {
                        tracing::warn!(
                            tool_name = %name,
                            args_len = arguments.len(),
                            args_sha256 = %sha256_hex(arguments.as_bytes()),
                            "malformed tool arguments — defaulting to empty object \
                             (set RYEOS_LOG_TOOL_ARGS=1 to log raw)"
                        );
                        json!({})
                    });
                let id_opt = if id.is_empty() { None } else { Some(id) };
                events.push(StreamEvent::ToolUse { id: id_opt, name, arguments: arguments_value });
                tool_call_state.remove("current_tool_id");
                tool_call_state.remove("current_tool_name");
                tool_call_state.remove("current_tool_args");
            }
        "message_delta" => {
            // Capture stop_reason for use when message_stop arrives.
            if let Some(delta) = parsed.get("delta") {
                if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                    tool_call_state.insert(
                        "last_stop_reason".to_string(),
                        sr.to_string(),
                    );
                }
            }
            // Emit usage update from message_delta.usage (cumulative).
            if let Some(usage_obj) = parsed.get("usage") {
                let mut update = UsageUpdate::default();
                if let Some(v) = usage_obj.get("input_tokens").and_then(|v| v.as_u64()) {
                    update.input_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("output_tokens").and_then(|v| v.as_u64()) {
                    update.output_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
                    update.cache_read_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
                    update.cache_write_tokens = Some(v);
                }
                events.push(StreamEvent::Usage(update));
            }
        }
        "message_start" => {
            // Initial usage from message_start.message.usage.
            if let Some(usage_obj) = parsed
                .get("message")
                .and_then(|m| m.get("usage"))
            {
                let mut update = UsageUpdate::default();
                if let Some(v) = usage_obj.get("input_tokens").and_then(|v| v.as_u64()) {
                    update.input_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("output_tokens").and_then(|v| v.as_u64()) {
                    update.output_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
                    update.cache_read_tokens = Some(v);
                }
                if let Some(v) = usage_obj.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
                    update.cache_write_tokens = Some(v);
                }
                events.push(StreamEvent::Usage(update));
            }
        }
        "message_stop" => {
            let raw_reason = tool_call_state.remove("last_stop_reason");
            events.push(StreamEvent::Finish {
                reason: normalize_finish_reason(raw_reason.as_deref()),
                raw: raw_reason,
            });
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

            let finish_reason = choice
                .get("finish_reason")
                .and_then(|f| f.as_str());
            let is_terminal = finish_reason
                .map(|r| !r.is_empty())
                .unwrap_or(false);

            if is_terminal {
                // Flush accumulated tool calls BEFORE emitting Done.
                // OpenAI sends finish_reason="tool_calls" when the model
                // wants tools dispatched; the old code only flushed on
                // "stop", which meant streamed tool calls never fired.
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
                    let args_value: Value = serde_json::from_str(&arguments)
                        .unwrap_or_else(|_| {
                            tracing::warn!(
                                tool_name = %name,
                                args_len = arguments.len(),
                                args_sha256 = %sha256_hex(arguments.as_bytes()),
                                "malformed tool arguments — defaulting to empty object \
                                 (set RYEOS_LOG_TOOL_ARGS=1 to log raw)"
                            );
                            json!({})
                        });
                    let id_opt = if id.is_empty() { None } else { Some(id) };
                    events.push(StreamEvent::ToolUse { id: id_opt, name, arguments: args_value });
                }
                for idx in indices.iter() {
                    let idx_s = idx.to_string();
                    tool_call_state.remove(&format!("tool_id_{}", idx_s));
                    tool_call_state.remove(&format!("tool_name_{}", idx_s));
                    tool_call_state.remove(&format!("tool_args_{}", idx_s));
                }
                events.push(StreamEvent::Finish {
                    reason: normalize_finish_reason(finish_reason),
                    raw: finish_reason.map(|s| s.to_string()),
                });
            }
        }
    }

    // OpenAI usage: sent as a top-level `usage` object on the final chunk
    // (when stream_options.include_usage: true). The `choices` array is
    // empty on this chunk, so the loop above skips it.
    if let Some(usage_obj) = parsed.get("usage") {
        if usage_obj.is_object() && !usage_obj.is_null() {
            let mut update = UsageUpdate::default();
            let mut has_any = false;
            if let Some(v) = usage_obj.get("prompt_tokens").and_then(|v| v.as_u64()) {
                update.input_tokens = Some(v);
                has_any = true;
            }
            if let Some(v) = usage_obj.get("completion_tokens").and_then(|v| v.as_u64()) {
                update.output_tokens = Some(v);
                has_any = true;
            }
            if let Some(details) = usage_obj.get("completion_tokens_details") {
                if let Some(v) = details.get("reasoning_tokens").and_then(|v| v.as_u64()) {
                    update.reasoning_tokens = Some(v);
                    has_any = true;
                }
            }
            if has_any {
                events.push(StreamEvent::Usage(update));
            }
        }
    }
}

fn parse_complete_chunks(
    parsed: &Value,
    events: &mut Vec<StreamEvent>,
    paths: Option<&crate::directive::StreamPaths>,
    tool_call_state: &mut HashMap<String, String>,
) {
    use ryeos_runtime::template::resolve_path;

    // No paths config → can't drive a schema-aware parse.
    let Some(p) = paths else {
        tracing::warn!(
            "SSE complete_chunks: no streaming.paths config; cannot parse frame"
        );
        return;
    };

    // Walk the content_path to get the array of blocks.
    let blocks = match resolve_path(parsed, &p.content_path) {
        Some(Value::Array(arr)) => arr,
        Some(other) => {
            tracing::warn!(
                content_path = p.content_path,
                kind = ?other,
                "SSE complete_chunks: content_path did not resolve to an array"
            );
            return;
        }
        None => {
            // Frame may be a metadata/usage-only chunk with no
            // content. Not an error.
            return;
        }
    };

    let mut frame_text = String::new();

    for block in blocks {
        // Tool-call detection takes precedence over text.
        if let Some(ref tc_field) = p.tool_call_field {
            if block.get(tc_field).is_some() {
                if let (Some(name_path), Some(args_path)) =
                    (&p.tool_call_name_path, &p.tool_call_args_path)
                {
                    let name = resolve_path(block, name_path)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = resolve_path(block, args_path)
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    if name.is_empty() {
                        tracing::warn!(
                            "complete_chunks: tool_call block missing name; skipping"
                        );
                        continue;
                    }
                    // Deduplicate: Gemini sends cumulative frames, so the
                    // same tool call may appear in multiple chunks. Use
                    // (name + arguments) as a dedupe key.
                    let arguments = serde_json::to_string(&args)
                        .unwrap_or_else(|_| "{}".to_string());
                    let dedupe_key = format!("seen_tc_{}:{}", name, arguments);
                    if tool_call_state.contains_key(&dedupe_key) {
                        continue; // already emitted this tool call
                    }
                    tool_call_state.insert(dedupe_key, "1".to_string());

                    // Gemini does not assign tool_call ids; synthesize a
                    // stream-local sequential id. Counter lives in the
                    // tool_call_state map keyed `gemini_tc_seq`. IDs are
                    // stable within a stream (no clock dependency).
                    let seq: u64 = tool_call_state
                        .get("gemini_tc_seq")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let id = format!("gemini_tc_{}", seq);
                    tool_call_state.insert("gemini_tc_seq".to_string(), (seq + 1).to_string());
                    events.push(StreamEvent::ToolUse {
                        id: Some(id),
                        name,
                        arguments: args,
                    });
                    continue;
                }
            }
        }

        // Thinking blocks: emit as ReasoningDelta (not visible deltas).
        if let Some(ref thought_field) = p.thought_field {
            if block.get(thought_field).and_then(|v| v.as_bool()) == Some(true) {
                if let Some(text) = block.get(&p.text_field).and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        events.push(StreamEvent::ReasoningDelta(text.to_string()));
                    }
                }
                continue;
            }
        }

        // Collect visible text from this block for per-frame aggregation.
        if let Some(text) = block.get(&p.text_field).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                frame_text.push_str(text);
            }
        }
    }

    // Apply cursor once against the aggregated frame text, not per-part.
    // Per-part cursoring corrupts output when a single frame contains
    // multiple text parts (e.g. Gemini sends [{text:"Hello"},{text:" world"}]).
    if !frame_text.is_empty() {
        let seen_len: usize = tool_call_state
            .get("seen_text_len")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if frame_text.len() > seen_len {
            let new_text = &frame_text[seen_len..];
            if !new_text.is_empty() {
                events.push(StreamEvent::Delta(new_text.to_string()));
            }
        }
        tool_call_state.insert("seen_text_len".to_string(), frame_text.len().to_string());
    }

    // Usage from usageMetadata (Gemini path).
    if let Some(ref usage_path) = p.usage_path {
        if let Some(usage_md) = resolve_path(parsed, usage_path) {
            let mut update = UsageUpdate::default();
            let mut has_any = false;
            if let Some(v) = usage_md.get("promptTokenCount").and_then(|v| v.as_u64()) {
                update.input_tokens = Some(v);
                has_any = true;
            }
            if let Some(v) = usage_md.get("candidatesTokenCount").and_then(|v| v.as_u64()) {
                update.output_tokens = Some(v);
                has_any = true;
            }
            if let Some(v) = usage_md.get("thoughtsTokenCount").and_then(|v| v.as_u64()) {
                update.reasoning_tokens = Some(v);
                has_any = true;
            }
            if has_any {
                events.push(StreamEvent::Usage(update));
            }
        }
    }

    // Finish reason → emit Finish.
    if let Some(ref fr_path) = p.finish_reason_path {
        if let Some(reason) = resolve_path(parsed, fr_path).and_then(|v| v.as_str()) {
            if !reason.is_empty() {
                events.push(StreamEvent::Finish {
                    reason: normalize_finish_reason(Some(reason)),
                    raw: Some(reason.to_string()),
                });
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
/// Returns the accumulated `AdapterResponse` (final assistant message + usage +
/// finish_reason) plus the full sequence of `StreamEvent`s for the runner to
/// consume.
pub struct StreamingCallInput<'a> {
    pub client: &'a reqwest::Client,
    pub provider: &'a ProviderConfig,
    pub provider_id: &'a str,
    /// Profile name that matched during daemon preflight (e.g. `"claude-3.5-sonnet"`).
    /// Used for diagnostic error messages — never influences wire shape.
    pub matched_profile: Option<&'a str>,
    /// SHA-256 of the canonical-JSON provider config, computed by the daemon
    /// at snapshot time. Logged on errors so operators can correlate.
    pub config_hash: &'a str,
    pub execution: &'a ExecutionConfig,
    pub model: &'a str,
    pub messages: &'a [ProviderMessage],
    pub tools: &'a [ToolSchema],
    pub callback: &'a CallbackClient,
    pub turn: u32,
    pub sampling: Option<&'a SamplingConfig>,
    /// Optional cancellation flag. When `Some(atomic_bool)` and the
    /// bool becomes `true`, the stream loop breaks mid-flight and
    /// returns what it has accumulated so far.
    pub cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[tracing::instrument(
    name = "provider:request_streaming",
    skip(input),
    fields(adapter_type = "stream", model = %input.model, turn = input.turn)
)]
pub async fn call_provider_streaming(input: StreamingCallInput<'_>) -> Result<(AdapterResponse, Vec<StreamEvent>)> {
    let StreamingCallInput {
        client,
        provider,
        provider_id,
        matched_profile,
        config_hash,
        execution,
        model,
        messages,
        tools,
        callback,
        turn,
        sampling,
        cancel_flag: _,  // checked inside the stream loop
    } = input;
    let schemas = provider.schemas.as_ref().and_then(|s| s.messages.as_ref());

    let (converted_messages, system_prompt) =
        crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());

    let tool_schema = provider.schemas.as_ref().and_then(|s| s.tools.clone());
    let tools_val = crate::provider_adapter::tools::serialize_tools(
        tools,
        &tool_schema,
    );

    let stream_url = provider.extra.get("stream_url").and_then(|v| v.as_str());
    // Resolve {model} template in base_url (e.g. gemini profiles use
    // `{model}:streamGenerateContent`). Stream URL semantics:
    //   - None        → default to "/chat/completions"
    //   - Some("")    → base_url is the full endpoint; do not append
    //   - Some(other) → append (with leading slash if needed)
    let base_resolved = provider.base_url.replace("{model}", model);
    let url = match stream_url {
        Some("") => base_resolved,
        Some(su) => format!(
            "{}{}",
            base_resolved.trim_end_matches('/'),
            if su.starts_with('/') { su.to_string() } else { format!("/{}", su) }
        ),
        None => format!(
            "{}/chat/completions",
            base_resolved.trim_end_matches('/')
        ),
    };

    let mut body = build_request_body(
        provider,
        model,
        &converted_messages,
        system_prompt.as_deref(),
        &tools_val,
        !tools.is_empty(),
    );

    // Sampling parameters — gated by provider capabilities so we
    // never send a field the upstream API will reject with a 400.
    inject_sampling(&mut body, provider.family, sampling);

    // Pre-compute the request body hash for diagnostic error messages.
    // This runs before `body` is moved into the request builder.
    let request_body_str = serde_json::to_string(&body).unwrap_or_default();
    let request_body_sha256 = sha256_hex(request_body_str.as_bytes());

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
        let text = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!("failed to read error response body: {e}");
            String::new()
        });
        bail!(
            "provider returned {status} (streaming) \
             [provider={provider_id} profile={matched_profile:?} \
             config_hash={config_hash} request_body_sha256={request_body_sha256}]: \
             {safe_body}",
            status = status,
            provider_id = provider_id,
            matched_profile = matched_profile,
            config_hash = config_hash,
            request_body_sha256 = request_body_sha256,
            safe_body = safe_error_body(&text),
        );
    }

    let stream_mode = provider
        .schemas
        .as_ref()
        .and_then(|s| s.streaming.as_ref())
        .and_then(|st| st.mode.as_deref())
         .unwrap_or("delta_merge")
         .to_string();

    let stream_paths = provider
        .schemas
        .as_ref()
        .and_then(|s| s.streaming.as_ref())
        .and_then(|st| st.paths.as_ref());

    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    // Holds the trailing partial UTF-8 sequence carried over between
    // SSE chunks. A single multi-byte char (emoji, CJK, etc.) can
    // straddle chunk boundaries; keep the incomplete tail here so the
    // next chunk completes it instead of failing the whole stream
    // with `non-utf8 SSE chunk`.
    let mut utf8_carry: Vec<u8> = Vec::new();
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

    loop {
        // Preemptive cancellation: check BEFORE waiting for the next
        // chunk. Combined with the post-await check below, this
        // guarantees cancellation latency is bounded by the polling
        // interval (~50ms), not the next chunk arrival.
        if cancelled(&input.cancel_flag) {
            tracing::info!(turn = input.turn, "provider stream cancelled mid-flight (preemptive)");
            break;
        }

        let chunk_res = tokio::select! {
            biased;

            _ = cancel_signal(&input.cancel_flag) => {
                tracing::info!(turn = input.turn, "provider stream cancelled mid-flight (select)");
                break;
            }

            chunk = byte_stream.next() => {
                match chunk {
                    Some(c) => c,
                    None => break, // upstream closed
                }
            }
        };

        let chunk: Bytes = chunk_res.map_err(|e| anyhow!("stream chunk error: {e}"))?;
        // Prepend any partial UTF-8 sequence carried from the previous
        // chunk and decode the longest complete-UTF-8 prefix. The
        // incomplete tail (if any) is stashed back in `utf8_carry`
        // for the next iteration. This is the standard streaming
        // decoder pattern; without it, any SSE chunk boundary that
        // splits a multi-byte char (emoji, CJK punctuation, etc.)
        // aborts the whole stream.
        utf8_carry.extend_from_slice(&chunk);
        let (decoded, tail_start) = match std::str::from_utf8(&utf8_carry) {
            Ok(s) => (s.to_string(), utf8_carry.len()),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // Safe by construction: valid_up_to is the prefix the
                // validator already accepted as well-formed UTF-8.
                let prefix = unsafe {
                    std::str::from_utf8_unchecked(&utf8_carry[..valid_up_to])
                }
                .to_string();
                if let Some(invalid_len) = e.error_len() {
                    // A genuinely invalid byte sequence (not just a
                    // truncated multi-byte char) — fail loud.
                    return Err(anyhow!(
                        "non-utf8 SSE chunk: invalid byte sequence of length {invalid_len} at index {valid_up_to}"
                    ));
                }
                (prefix, valid_up_to)
            }
        };
        utf8_carry.drain(..tail_start);
        buffer.push_str(&decoded);

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
            harvest_chunk_meta(&block, &mut last_usage, &mut last_finish, stream_paths);

            // Re-frame the block as a complete SSE buffer for the
            // existing parser (which expects `\n\n` framing). Threads
            // the persistent tool_state so partial tool_use args
            // fragmented across blocks concatenate.
            let framed = format!("{}\n\n", block);
            let new_events = parse_sse_events_with_state(
                &framed,
                Some(&stream_mode),
                stream_paths,
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
                        accumulated_tools.push(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
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
                    StreamEvent::ToolUsePartial { id, name, delta, total_len } => {
                        callback
                            .append_event(
                                "cognition_out",
                                json!({
                                    "turn": turn,
                                    "tool_use_partial": {
                                        "id": id,
                                        "name": name,
                                        "delta": delta,
                                        "total_len": total_len,
                                    },
                                }),
                            )
                            .await
                            .map_err(|e| {
                                anyhow!(
                                    "persistence-first violation: cognition_out \
                                     tool_use_partial append failed mid-stream: {e}"
                                )
                            })?;
                    }
                    StreamEvent::Finish { .. } => {}
                    StreamEvent::Usage(_) => {}
                    StreamEvent::Warning { .. } => {}
                    StreamEvent::ReasoningDelta(_) => {}
                }
                all_events.push(ev);
            }

        }
    }

    // Final flush: any trailing block without terminal blank line.
    if !buffer.trim().is_empty() {
        let framed = format!("{}\n\n", buffer);
        harvest_chunk_meta(&buffer, &mut last_usage, &mut last_finish, stream_paths);
        let final_events = parse_sse_events_with_state(
            &framed,
            Some(&stream_mode),
            stream_paths,
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
                    accumulated_tools.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
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
                StreamEvent::ToolUsePartial { id, name, delta, total_len } => {
                    callback
                        .append_event(
                            "cognition_out",
                            json!({
                                "turn": turn,
                                "tool_use_partial": {
                                    "id": id,
                                    "name": name,
                                    "delta": delta,
                                    "total_len": total_len,
                                },
                            }),
                        )
                        .await
                        .map_err(|e| {
                            anyhow!(
                                "persistence-first violation: cognition_out \
                                 tool_use_partial append failed at stream end: {e}"
                            )
                        })?;
                }
                StreamEvent::Finish { .. } => {}
                StreamEvent::Usage(_) => {}
                StreamEvent::Warning { .. } => {}
                StreamEvent::ReasoningDelta(_) => {}
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
///
/// When `stream_paths` is provided, Gemini-style paths are used for
/// usage and finish_reason extraction. Otherwise OpenAI/Anthropic-style
/// paths are tried.
fn harvest_chunk_meta(
    block: &str,
    last_usage: &mut Option<TokenUsage>,
    last_finish: &mut Option<String>,
    stream_paths: Option<&crate::directive::StreamPaths>,
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
                tracing::trace!("skipping malformed SSE data JSON: {e}; raw={}", &rest[..rest.len().min(120)]);
                continue;
            }
        };

        // Usage extraction: try StreamPaths first (Gemini), then
        // OpenAI/Anthropic-style paths.
        if let Some(sp) = stream_paths {
            if let Some(ref usage_path) = sp.usage_path {
                if let Some(u) = resolve_path(&parsed, usage_path) {
                    let new_input = sp.input_tokens_field.as_ref()
                        .and_then(|f| u.get(f))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let new_output = sp.output_tokens_field.as_ref()
                        .and_then(|f| u.get(f))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if new_input > 0 || new_output > 0 {
                        let prev_input = last_usage.as_ref().map(|u| u.input_tokens).unwrap_or(0);
                        let prev_output = last_usage.as_ref().map(|u| u.output_tokens).unwrap_or(0);
                        *last_usage = Some(TokenUsage {
                            input_tokens: new_input.max(prev_input),
                            output_tokens: new_output.max(prev_output),
                        });
                    }
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
            // OpenAI / Anthropic style
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
                if new_input > 0 || new_output > 0 {
                    let prev_input = last_usage.as_ref().map(|u| u.input_tokens).unwrap_or(0);
                    let prev_output = last_usage.as_ref().map(|u| u.output_tokens).unwrap_or(0);
                    *last_usage = Some(TokenUsage {
                        input_tokens: new_input.max(prev_input),
                        output_tokens: new_output.max(prev_output),
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
}

/// Inject sampling fields into the request body, gated by provider
/// capabilities. Unknown providers get no fields (fail-closed).
fn inject_sampling(
    body: &mut Value,
    family: ProtocolFamily,
    sampling: Option<&SamplingConfig>,
) {
    if let Some(s) = sampling {
        let caps = family_capabilities(family);
        if caps.supports_temperature {
            if let Some(temp) = s.temperature {
                body["temperature"] = json!(temp);
            }
        }
        if caps.supports_seed {
            if let Some(seed) = s.seed {
                body["seed"] = json!(seed);
            }
        }
    }
}

// ── Provider capabilities ──────────────────────────────────────

/// Capability flags for a protocol family. Used to gate which
/// sampling fields are safe to inject into the request body.
#[derive(Debug, Clone, Default)]
struct ProviderCapabilities {
    supports_temperature: bool,
    supports_seed: bool,
}

/// Look up capabilities for a known protocol family. Falls back to
/// the conservative default for unrecognised families.
fn family_capabilities(family: ProtocolFamily) -> ProviderCapabilities {
    match family {
        ProtocolFamily::ChatCompletions => ProviderCapabilities {
            supports_temperature: true,
            supports_seed: true,
        },
        ProtocolFamily::AnthropicMessages => ProviderCapabilities {
            supports_temperature: true,
            supports_seed: false,
        },
        ProtocolFamily::GoogleGenerateContent => ProviderCapabilities {
            supports_temperature: true,
            supports_seed: false,
        },
    }
}

/// Build the request body from the resolved provider config.
///
/// Uses `body_template` with `apply_template` to render the body from
/// a context containing `model`, `messages`, `tools`, `stream`, and
/// `max_tokens`. Then deep-merges `body_extra`, injects the system
/// prompt per `schemas.messages.system_message` rules, and adds tools
/// if the template didn't already place them.
///
/// # Panics
///
/// Panics if `body_template` is not set. This should never happen in
/// production because `ProviderConfig::validate` catches it at load-time.
pub fn build_request_body(
    provider: &ProviderConfig,
    model: &str,
    converted_messages: &[Value],
    system_prompt: Option<&str>,
    tools_val: &Value,
    has_tools: bool,
) -> Value {
    use ryeos_runtime::template::{apply_template, deep_merge};

    let template = provider.body_template.as_ref().unwrap_or_else(|| {
        unreachable!(
             "ProviderConfig::validate() rejects providers without body_template \
              at config-load time (preflight_resolve → bootstrap, launch). \
             If this fires, either:\n  \
             1. validate() was not called on the path that produced this config, OR\n  \
             2. a code path constructed ProviderConfig directly without validation.\n  \
             The fix is to add a validate() call at the new construction site, \
             not to add a runtime fallback here. The adapter does not guess \
             body shapes — that is a config error and must surface loudly."
        )
    });

    let mut ctx: HashMap<String, Value> = HashMap::new();
    ctx.insert("model".to_string(), Value::String(model.to_string()));
    ctx.insert("messages".to_string(), Value::Array(converted_messages.to_vec()));
    ctx.insert("tools".to_string(), tools_val.clone());
    ctx.insert("stream".to_string(), Value::Bool(true));
    ctx.insert("max_tokens".to_string(), Value::Number(4096.into()));

    let mut body = apply_template(template, &ctx);

    // Apply body_extra (static fragment) before system prompt
    // injection so that body_extra can set defaults that
    // system-prompt logic might override.
    if let Some(extra) = &provider.body_extra {
        deep_merge(&mut body, extra);
    }

    // System prompt placement.
    if let Some(sys) = system_prompt {
        inject_system_prompt(&mut body, sys, provider);
    }

    // Tools — only set if the template didn't already place them
    // and we have tools to send. A template that includes
    // `"tools": "{tools}"` will already have them; we leave that
    // alone.
    if has_tools && body.get("tools").is_none_or(|v| v.is_null()) {
        body["tools"] = tools_val.clone();
    }

    body
}

/// Inject the system prompt into the body using the rules in
/// `provider.schemas.messages.system_message`.
fn inject_system_prompt(body: &mut Value, system: &str, provider: &ProviderConfig) {
    use ryeos_runtime::template::{apply_template, deep_merge};

    let sys_config = provider
        .schemas
        .as_ref()
        .and_then(|s| s.messages.as_ref())
        .and_then(|m| m.system_message.as_ref());

    // When system_message is configured, use its declared mode.
    // When absent, nothing to inject — system stays inline.
    let mode = sys_config
        .map(|s| s.mode)
        .unwrap_or(SystemMessageMode::MessageRole);

    match mode {
        SystemMessageMode::BodyField => {
            let field = sys_config
                .and_then(|s| s.field.as_deref())
                .unwrap_or("system");
            body[field] = json!(system);
        }
        SystemMessageMode::BodyInject => {
            if let Some(template) = sys_config.and_then(|s| s.template.as_ref()) {
                let mut ctx: HashMap<String, Value> = HashMap::new();
                ctx.insert("system".to_string(), Value::String(system.to_string()));
                let rendered = apply_template(template, &ctx);
                deep_merge(body, &rendered);
            } else {
                tracing::warn!(
                    "system_message.mode = body_inject but no template provided — \
                     system prompt dropped"
                );
            }
        }
        SystemMessageMode::MessageRole => {
            // Already handled in messages::convert_messages, which
            // prepends a system-role message. Nothing to inject in
            // the body itself.
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
        assert!(matches!(&events[2], StreamEvent::Finish { .. }));
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
            assert_eq!(id, &Some("toolu_1".to_string()));
            assert_eq!(name, "bash");
            assert_eq!(arguments, &json!({"cmd": "ls"}));
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
        let dones: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Finish { .. })).collect();
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
            assert_eq!(id, &Some("call_1".to_string()));
            assert_eq!(name, "bash");
            assert_eq!(arguments, &json!({"cmd": "ls"}));
        }
     }

    #[test]
    fn openai_streamed_tool_calls_flush_on_tool_calls_finish_reason() {
        // Regression test: OpenAI sends finish_reason="tool_calls" (not
        // "stop") when the model wants tools dispatched. The old code
        // only checked for "stop", so streamed tool calls never fired.
        let data = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\""}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"rust\"}"}}]}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1, "must emit exactly one ToolUse");
        if let StreamEvent::ToolUse { id, name, arguments } = &tool_uses[0] {
            assert_eq!(id, &Some("call_abc".to_string()));
            assert_eq!(name, "search");
            assert_eq!(arguments, &json!({"q": "rust"}));
        }
        let dones: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::Finish { .. })).collect();
        assert_eq!(dones.len(), 1, "must emit Finish after tool calls");
        // ToolUse must come before Finish.
        let tu_idx = events.iter().position(|e| matches!(e, StreamEvent::ToolUse { .. })).unwrap();
        let done_idx = events.iter().position(|e| matches!(e, StreamEvent::Finish { .. })).unwrap();
        assert!(tu_idx < done_idx, "ToolUse must precede Finish");
    }

    #[test]
    fn done_sentinel() {
        let data = "data: [DONE]\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(data, Some("delta_merge"));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Finish { .. }));
        assert!(matches!(&events[1], StreamEvent::Finish { .. }));
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

    #[test]
    fn family_capabilities_chat_completions_supports_seed() {
        let caps = family_capabilities(ProtocolFamily::ChatCompletions);
        assert!(caps.supports_temperature);
        assert!(caps.supports_seed);
    }

    #[test]
    fn family_capabilities_anthropic_omits_seed() {
        let caps = family_capabilities(ProtocolFamily::AnthropicMessages);
        assert!(caps.supports_temperature);
        assert!(!caps.supports_seed);
    }

    #[test]
    fn family_capabilities_google_omits_seed() {
        let caps = family_capabilities(ProtocolFamily::GoogleGenerateContent);
        assert!(caps.supports_temperature);
        assert!(!caps.supports_seed);
    }

    // ── Body-level sampling injection tests ─────────────────────

    #[test]
    fn sampling_injection_openai_includes_both_fields() {
        let mut body = json!({"model": "gpt-4", "messages": []});
        let sampling = SamplingConfig {
            temperature: Some(0.3),
            seed: Some(42),
        };
        inject_sampling(&mut body, ProtocolFamily::ChatCompletions, Some(&sampling));
        assert_eq!(body["temperature"].as_f64(), Some(0.3));
        assert_eq!(body["seed"].as_u64(), Some(42));
    }

    #[test]
    fn sampling_injection_anthropic_omits_seed() {
        let mut body = json!({"model": "claude-3", "messages": []});
        let sampling = SamplingConfig {
            temperature: Some(0.7),
            seed: Some(99),
        };
        inject_sampling(&mut body, ProtocolFamily::AnthropicMessages, Some(&sampling));
        assert_eq!(body["temperature"].as_f64(), Some(0.7));
        assert!(body.get("seed").is_none(), "anthropic body must not contain seed");
    }

    #[test]
    fn sampling_injection_google_omits_seed() {
        let mut body = json!({"model": "gemini-pro", "messages": []});
        let sampling = SamplingConfig {
            temperature: Some(0.5),
            seed: Some(1),
        };
        inject_sampling(&mut body, ProtocolFamily::GoogleGenerateContent, Some(&sampling));
        assert_eq!(body["temperature"].as_f64(), Some(0.5));
        assert!(body.get("seed").is_none(), "google body must not contain seed");
    }

    #[test]
    fn sampling_injection_with_no_sampling_config_is_noop() {
        let mut body = json!({"model": "test", "messages": []});
        inject_sampling(&mut body, ProtocolFamily::ChatCompletions, None);
        assert!(body.get("temperature").is_none());
        assert!(body.get("seed").is_none());
    }

    // ── Body template tests ────────────────────────────────────────

    #[test]
    fn build_request_body_renders_template() {
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: Some(json!({
                "contents": "{messages}",
                "model_pinned": "{model}",
            })),
            body_extra: None,
            profiles: vec![],
        };
        let msgs = vec![json!({"role": "user", "parts": [{"text": "hi"}]})];
        let body = super::build_request_body(&provider, "gemini-3-flash", &msgs, None, &json!([]), false);
        assert_eq!(body["model_pinned"], "gemini-3-flash");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        // No legacy keys when template is used.
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_request_body_deep_merges_body_extra() {
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: Some(json!({"contents": "{messages}"})),
            body_extra: Some(json!({"generationConfig": {"maxOutputTokens": 1024}})),
            profiles: vec![],
        };
        let body = super::build_request_body(&provider, "gemini-3-flash", &[], None, &json!([]), false);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
        assert!(body["contents"].is_array());
    }

    #[test]
    #[should_panic(expected = "entered unreachable code")]
    fn build_request_body_panics_when_no_template() {
        // Defense-in-depth: this should never trigger in production
        // because ProviderConfig::validate catches it at load-time.
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };
        let _ = super::build_request_body(&provider, "test-model", &[], None, &json!([]), false);
    }

    #[test]
    fn gemini_no_tools_request_omits_tools_field_entirely() {
        // When the Gemini body_template has no "tools" key and the tools
        // list is empty, the resulting request body must NOT contain a
        // "tools" key at all — not even tools: [].
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: Some(json!({
                "contents": "{messages}",
            })),
            body_extra: None,
            profiles: vec![],
        };
        let msgs = vec![json!({"role": "user", "parts": [{"text": "hi"}]})];
        let body = super::build_request_body(&provider, "gemini-3-flash", &msgs, None, &json!([]), false);
        assert!(body.get("tools").is_none(),
            "Gemini empty-tools must omit the field entirely, got: {:?}", body.get("tools"));
    }

    #[test]
    fn inject_system_prompt_body_field_with_explicit_field() {
        use crate::directive::{MessageSchemas, SchemasConfig, SystemMessageConfig};
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: Some(SchemasConfig {
                messages: Some(MessageSchemas {
                    system_message: Some(SystemMessageConfig {
                        mode: SystemMessageMode::BodyField,
                        field: Some("system".to_string()),
                        template: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };
        let mut body = json!({});
        super::inject_system_prompt(&mut body, "you are helpful", &provider);
        assert_eq!(body["system"], "you are helpful");
    }

    #[test]
    fn inject_system_prompt_body_inject_renders_gemini_shape() {
        use crate::directive::{MessageSchemas, SchemasConfig, SystemMessageConfig};
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: Some(SchemasConfig {
                messages: Some(MessageSchemas {
                    role_map: None,
                    content_key: None,
                    text_placement: None,
                    assistant_tool_calls_placement: None,
                    text_block_template: None,
                    tool_call_block_template: None,
                    system_message: Some(SystemMessageConfig {
                        mode: SystemMessageMode::BodyInject,
                        field: None,
                        template: Some(json!({
                            "systemInstruction": {
                                "parts": [{"text": "{system}"}]
                            }
                        })),
                    }),
                    tool_result: None,
                }),
                tools: None,
                streaming: None,
            }),
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };
        let mut body = json!({"contents": []});
        super::inject_system_prompt(&mut body, "be brief", &provider);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        // contents preserved
        assert!(body["contents"].is_array());
    }

    #[test]
    fn parse_complete_chunks_extracts_gemini_text_delta() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: Some("thought".to_string()),
            tool_call_field: Some("functionCall".to_string()),
            tool_call_name_path: Some("functionCall.name".to_string()),
            tool_call_args_path: Some("functionCall.args".to_string()),
            usage_path: Some("usageMetadata".to_string()),
            input_tokens_field: Some("promptTokenCount".to_string()),
            output_tokens_field: Some("candidatesTokenCount".to_string()),
            finish_reason_path: Some("candidates.0.finishReason".to_string()),
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hi there"}]}
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Delta(s) => assert_eq!(s, "Hi there"),
            other => panic!("expected Delta, got {:?}", other),
        }
    }

    #[test]
    fn parse_complete_chunks_filters_thinking_blocks() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: Some("thought".to_string()),
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"thought": true, "text": "let me think"},
                    {"text": "the answer is 42"}
                ]}
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        // Now emits ReasoningDelta for the thought + Delta for visible text.
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::ReasoningDelta(s) => assert_eq!(s, "let me think"),
            other => panic!("expected ReasoningDelta, got {:?}", other),
        }
        match &events[1] {
            StreamEvent::Delta(s) => assert_eq!(s, "the answer is 42"),
            other => panic!("expected Delta, got {:?}", other),
        }
    }

    #[test]
    fn parse_complete_chunks_emits_done_on_finish_reason() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: Some("candidates.0.finishReason".to_string()),
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "done."}]},
                "finishReason": "STOP"
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        // 1 Delta + 1 Done
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], StreamEvent::Finish { .. }));
    }

    #[test]
    fn parse_complete_chunks_no_paths_emits_warning_and_zero_events() {
        let frame = json!({"candidates": []});
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, None, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_complete_chunks_assigns_stable_sequential_tool_call_ids() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: Some("functionCall".to_string()),
            tool_call_name_path: Some("functionCall.name".to_string()),
            tool_call_args_path: Some("functionCall.args".to_string()),
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "search", "args": {"q": "rust"}}},
                    {"functionCall": {"name": "search", "args": {"q": "go"}}}
                ]}
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::ToolUse { id, name, .. } => {
                assert_eq!(id, &Some("gemini_tc_0".to_string()));
                assert_eq!(name, "search");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        match &events[1] {
            StreamEvent::ToolUse { id, .. } => assert_eq!(id, &Some("gemini_tc_1".to_string())),
            other => panic!("expected ToolUse, got {:?}", other),
        }
        // Counter persists in state.
        assert_eq!(state.get("gemini_tc_seq").map(|s| s.as_str()), Some("2"));
    }

    #[test]
    fn complete_chunks_cumulative_text_does_not_duplicate() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let mut events = vec![];
        let mut state = HashMap::new();

        // Frame 1: "Hello"
        let frame1 = json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello"}]}}]
        });
        super::parse_complete_chunks(&frame1, &mut events, Some(&paths), &mut state);
        // Frame 2: "Hello, world" (cumulative — full text so far)
        let frame2 = json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello, world"}]}}]
        });
        super::parse_complete_chunks(&frame2, &mut events, Some(&paths), &mut state);

        let deltas: Vec<&str> = events.iter().filter_map(|e| match e {
            StreamEvent::Delta(s) => Some(s.as_str()),
            _ => None,
        }).collect();
        // Must be ["Hello", ", world"], NOT ["Hello", "Hello, world"]
        assert_eq!(deltas, vec!["Hello", ", world"]);
    }

    #[test]
    fn gemini_complete_chunks_multi_part_text_in_one_frame_emits_full_text() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: Some("thought".to_string()),
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        // Single frame containing two visible text parts. The cursor must
        // operate on the concatenated frame text, not per-part.
        let frame = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "Hello"},
                        {"text": ", world"}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        let combined: String = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(combined, "Hello, world",
            "multi-part frame text must concatenate correctly; got: {combined:?}");
    }

    #[test]
    fn gemini_complete_chunks_cumulative_multi_part_does_not_truncate() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: Some("thought".to_string()),
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        // Two frames, each with two parts; second frame is cumulative.
        let f1 = json!({
            "candidates": [{
                "content": {"role":"model","parts":[{"text":"Hi"},{"text":" there"}]}
            }]
        });
        let f2 = json!({
            "candidates": [{
                "content": {"role":"model","parts":[
                    {"text":"Hi"}, {"text":" there"},
                    {"text":", how"}, {"text":" are you?"}
                ]},
                "finishReason": "STOP"
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&f1, &mut events, Some(&paths), &mut state);
        super::parse_complete_chunks(&f2, &mut events, Some(&paths), &mut state);
        let combined: String = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(combined, "Hi there, how are you?",
            "cumulative multi-part frames must produce the full text exactly once");
    }

    #[test]
    fn complete_chunks_repeated_tool_call_emits_once() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: Some("functionCall".to_string()),
            tool_call_name_path: Some("functionCall.name".to_string()),
            tool_call_args_path: Some("functionCall.args".to_string()),
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let mut events = vec![];
        let mut state = HashMap::new();

        // Frame 1: one tool call
        let frame1 = json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}}
            ]}}]
        });
        super::parse_complete_chunks(&frame1, &mut events, Some(&paths), &mut state);

        // Frame 2: same tool call again (cumulative frame)
        let frame2 = json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"name": "search", "args": {"q": "rust"}}}
            ]}}]
        });
        super::parse_complete_chunks(&frame2, &mut events, Some(&paths), &mut state);

        let tool_uses: Vec<_> = events.iter().filter(|e| matches!(e, StreamEvent::ToolUse { .. })).collect();
        assert_eq!(tool_uses.len(), 1, "cumulative frame must not duplicate tool calls");
    }

    // ── Real golden-wire tests ────────────────────────────────────────
    //
    // These tests load the ACTUAL signed bundled provider YAML through
    // VerifiedLoader, resolve the matching profile via preflight_resolve,
    // and build the request body. They catch:
    //   - Bundled YAML signature / parse failures
    //   - Profile-match resolution regressions
    //   - Trust-policy violations in shipped configs
    //   - Request-shape drift (placement enums, tool serialization,
    //     system-prompt placement)
    //
    // If a bundle YAML edit breaks one of these, CI catches it here.

    use ryeos_runtime::model_resolution::{
        DirectiveModelHeader, ModelSpec, preflight_resolve,
    };
    use ryeos_runtime::verified_loader::VerifiedLoader;

    /// Path to the bundled standard root in the workspace.
    fn bundle_root() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR is ryeos-directive-runtime/; parent is workspace.
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("ryeos-bundles/standard")
    }

    /// Build a VerifiedLoader rooted at the bundled standard, with an
    /// empty temp project root so project-overlay attacks can't interfere.
    fn loader_for_bundle() -> VerifiedLoader {
        let empty_project = std::env::temp_dir().join("ryeos-golden-wire-empty-project");
        let _ = std::fs::create_dir_all(&empty_project);
        VerifiedLoader::new(
            empty_project,
            None,
            vec![bundle_root()],
        )
    }

    /// Resolve a provider+model through the real loader + preflight chain.
    fn resolve(provider: &str, model: &str) -> ryeos_runtime::ResolvedProviderSnapshot {
        let loader = loader_for_bundle();
        let header = DirectiveModelHeader {
            model: Some(ModelSpec {
                tier: None,
                provider: Some(provider.to_string()),
                name: Some(model.to_string()),
                context_window: Some(128_000),
                sampling: None,
            }),
        };
        preflight_resolve(&header, &loader)
            .unwrap_or_else(|e| panic!("preflight_resolve failed for {provider}/{model}: {e:#}"))
    }

    /// Helper: build the full request body from a resolved snapshot.
    fn build_golden_body(
        snapshot: &ryeos_runtime::ResolvedProviderSnapshot,
        messages: &[ProviderMessage],
        tools: &[ToolSchema],
    ) -> Value {
        let schemas = snapshot.provider.schemas.as_ref().and_then(|s| s.messages.as_ref());
        let (converted, system_prompt) =
            crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());
        let tool_schema = snapshot.provider.schemas.as_ref().and_then(|s| s.tools.clone());
        let tools_val = crate::provider_adapter::tools::serialize_tools(tools, &tool_schema);
        super::build_request_body(
            &snapshot.provider,
            &snapshot.model_name,
            &converted,
            system_prompt.as_deref(),
            &tools_val,
            !tools.is_empty(),
        )
    }

    use crate::directive::ToolCall;

    /// Canonical transcript with a tool call.
    fn canonical_transcript_with_tool_call() -> Vec<ProviderMessage> {
        vec![
            ProviderMessage {
                role: "system".to_string(),
                content: Some(json!("You are helpful.")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("What is 2+2?")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("Let me calculate.")),
                tool_calls: Some(vec![ToolCall {
                    id: Some("call_1".to_string()),
                    name: "calculator".to_string(),
                    arguments: json!({"expr": "2+2"}),
                }]),
                tool_call_id: None,
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(json!("4")),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("Thanks.")),
                tool_calls: None,
                tool_call_id: None,
            },
        ]
    }

    fn canonical_transcript_no_tools() -> Vec<ProviderMessage> {
        vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hi")),
                tool_calls: None,
                tool_call_id: None,
            },
        ]
    }

    fn canonical_tool_schemas() -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "calculator".to_string(),
            item_id: "test/calculator".to_string(),
            description: Some("Evaluate arithmetic".to_string()),
            input_schema: Some(json!({
                "type": "object",
                "properties": { "expr": {"type":"string"} },
                "required": ["expr"],
            })),
        }]
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[test]
    fn bundled_provider_configs_all_pass_validation() {
        // Sanity: every provider in the bundle resolves and validates.
        for (provider, model) in [
            ("openai", "gpt-4o-mini"),
            ("anthropic", "claude-sonnet-4-5-20250929"),
            ("zen", "gpt-5.4-nano"),
        ] {
            let snapshot = resolve(provider, model);
            assert_eq!(snapshot.provider_id, provider);
            assert!(!snapshot.config_hash.is_empty(),
                "config_hash must be populated for {provider}");
        }
    }

    #[test]
    fn bundled_openai_profile_builds_top_level_tool_calls() {
        let snapshot = resolve("openai", "gpt-4o-mini");
        assert_eq!(snapshot.provider_id, "openai");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
        );
        assert_eq!(body["model"], "gpt-4o-mini");

        let messages = body["messages"].as_array().expect("messages must be array");
        let assistant = messages.iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have assistant message");

        // tool_calls must be a top-level array, not inline in content
        assert!(assistant["tool_calls"].is_array(),
            "OpenAI tool_calls must be top-level array on assistant message; got: {assistant}");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "calculator");

        // Content must be a plain string, not a block array
        assert!(assistant["content"].is_string(),
            "OpenAI assistant content must be plain string, got: {:?}", assistant["content"]);
    }

    #[test]
    fn bundled_anthropic_profile_builds_typed_blocks() {
        let snapshot = resolve("anthropic", "claude-sonnet-4-5-20250929");
        assert_eq!(snapshot.provider_id, "anthropic");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
        );

        // System must be a top-level body field, not in messages
        assert!(body["system"].is_string(),
            "Anthropic system must be top-level body field");

        let messages = body["messages"].as_array().expect("messages must be array");
        let assistant = messages.iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have assistant message");

        // Content MUST be an array of typed blocks — no raw strings
        let content = assistant["content"].as_array()
            .expect("Anthropic assistant content must be array, not raw string");
        assert!(content.iter().all(|b| b.is_object()),
            "Anthropic content blocks must all be objects, not raw strings");
        assert!(content.iter().any(|b| b["type"] == "text"),
            "must contain at least one text block");
        assert!(content.iter().any(|b| b["type"] == "tool_use"),
            "must contain at least one tool_use block");
    }

    #[test]
    fn bundled_gemini_profile_omits_tools_when_empty() {
        // Gemini provider comes through zen's gemini profile.
        // zen.yaml matches gemini-* to the gemini profile.
        let snapshot = resolve("zen", "gemini-3-flash");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_no_tools(),
            &[],
        );
        assert!(body.get("tools").is_none(),
            "Gemini empty-tools must omit the field; got: {:?}", body.get("tools"));
    }

    #[test]
    fn bundled_gemini_profile_tool_result_carries_real_function_name() {
        let snapshot = resolve("zen", "gemini-3-flash");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
        );
        let contents = body["contents"].as_array().expect("Gemini contents array");
        let function_response = contents.iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .find(|p| p.get("functionResponse").is_some())
            .expect("must have a functionResponse part");
        assert_eq!(
            function_response["functionResponse"]["name"], "calculator",
            "functionResponse.name must be the real tool name, not empty"
        );
    }

    #[tokio::test]
    async fn cancel_signal_resolves_within_250ms_of_flag_set() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        let start = tokio::time::Instant::now();

        // Spawn the cancel_signal future and set the flag after 10ms
        let handle = tokio::spawn(async move {
            let opt_flag: Option<Arc<AtomicBool>> = Some(flag_clone);
            cancel_signal(&opt_flag).await;
        });

        // Simulate external trigger after a short delay
        tokio::time::sleep(Duration::from_millis(10)).await;
        flag.store(true, Ordering::Relaxed);

        // cancel_signal must resolve within 250ms of the flag being set
        let result = tokio::time::timeout(Duration::from_millis(250), handle).await;
        assert!(
            result.is_ok(),
            "cancel_signal did not resolve within 250ms; elapsed: {}ms",
            start.elapsed().as_millis()
        );
    }

    #[test]
    fn cancelled_returns_false_when_no_flag() {
        assert!(!cancelled(&None));
    }

    #[test]
    fn cancelled_returns_true_when_flag_set() {
        use std::sync::atomic::{AtomicBool};
        use std::sync::Arc;
        let flag = Arc::new(AtomicBool::new(true));
        assert!(cancelled(&Some(flag)));
    }

    // ── Phase 5: Stream event emission tests ──────────────────────────

    #[test]
    fn anthropic_emits_finish_reason_from_stop_reason() {
        let data = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let finish = events.iter().find(|e| matches!(e, StreamEvent::Finish { .. }));
        assert!(finish.is_some(), "must have a Finish event");
        match finish.unwrap() {
            StreamEvent::Finish { reason, raw } => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert_eq!(raw.as_deref(), Some("tool_use"));
            }
            other => panic!("expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn anthropic_emits_usage_from_message_delta() {
        let data = r#"event: message_delta
data: {"type":"message_delta","delta":{},"usage":{"input_tokens":25,"output_tokens":10}}

"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let usage_events: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, Some(25));
        assert_eq!(usage_events[0].output_tokens, Some(10));
    }

    #[test]
    fn anthropic_emits_usage_from_message_start() {
        let data = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","role":"assistant","usage":{"input_tokens":100,"output_tokens":0}}}

"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let usage_events: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, Some(100));
    }

    #[test]
    fn anthropic_emits_reasoning_delta_from_thinking_delta() {
        let data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to calculate 2+2"}}

"#;
        let events = parse_sse_events(data, Some("event_typed"));
        let reasoning: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0], "I need to calculate 2+2");
    }

    #[test]
    fn gemini_emits_usage_from_usage_metadata() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: Some("usageMetadata".to_string()),
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hi"}]}
            }],
            "usageMetadata": {
                "promptTokenCount": 42,
                "candidatesTokenCount": 7,
                "thoughtsTokenCount": 3
            }
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        let usage_events: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, Some(42));
        assert_eq!(usage_events[0].output_tokens, Some(7));
        assert_eq!(usage_events[0].reasoning_tokens, Some(3));
    }

    #[test]
    fn gemini_emits_reasoning_delta_for_thought_parts() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: Some("thought".to_string()),
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: None,
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"thought": true, "text": "reasoning step 1"},
                    {"text": "visible answer"}
                ]}
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        let reasoning: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0], "reasoning step 1");
        // Also verify the visible text was emitted
        let deltas: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["visible answer"]);
    }

    #[test]
    fn gemini_uppercase_finish_reason_normalizes_correctly() {
        use crate::directive::StreamPaths;
        let paths = StreamPaths {
            content_path: "candidates.0.content.parts".to_string(),
            text_field: "text".to_string(),
            thought_field: None,
            tool_call_field: None,
            tool_call_name_path: None,
            tool_call_args_path: None,
            usage_path: None,
            input_tokens_field: None,
            output_tokens_field: None,
            finish_reason_path: Some("candidates.0.finishReason".to_string()),
        };
        let frame = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "done"}]},
                "finishReason": "STOP"
            }]
        });
        let mut events = vec![];
        let mut state = HashMap::new();
        super::parse_complete_chunks(&frame, &mut events, Some(&paths), &mut state);
        let finish = events.iter().find(|e| matches!(e, StreamEvent::Finish { .. }));
        assert!(finish.is_some());
        match finish.unwrap() {
            StreamEvent::Finish { reason, raw } => {
                assert_eq!(*reason, FinishReason::Stop,
                    "uppercase STOP must normalize to Stop");
                assert_eq!(raw.as_deref(), Some("STOP"));
            }
            other => panic!("expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn openai_emits_usage_from_stream_options_frame() {
        let data = r#"data: {"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":20,"completion_tokens_details":{"reasoning_tokens":5}}}

"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let usage_events: Vec<_> = events.iter()
            .filter_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, Some(50));
        assert_eq!(usage_events[0].output_tokens, Some(20));
        assert_eq!(usage_events[0].reasoning_tokens, Some(5));
    }

    #[test]
    fn normalize_finish_reason_case_insensitive() {
        use crate::directive::{normalize_finish_reason, FinishReason};
        assert_eq!(normalize_finish_reason(Some("stop")), FinishReason::Stop);
        assert_eq!(normalize_finish_reason(Some("STOP")), FinishReason::Stop);
        assert_eq!(normalize_finish_reason(Some("Stop")), FinishReason::Stop);
        assert_eq!(normalize_finish_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(normalize_finish_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(normalize_finish_reason(Some("SAFETY")), FinishReason::ContentFilter);
        assert_eq!(normalize_finish_reason(Some("MAX_TOKENS")), FinishReason::Length);
    }
}
