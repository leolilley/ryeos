use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::directive::FinishReason;
use crate::directive::{
    normalize_finish_reason, ExecutionConfig, MalformedArgs, ProtocolFamily, ProviderConfig,
    ProviderMessage, SamplingConfig, StreamEvent, SystemMessageMode, ToolCall, ToolSchema,
    UsageUpdate,
};
use crate::provider_adapter::http::{
    AdapterResponse, ObservedOutput, ProviderLimitContractStatus, ProviderUsageSource, TokenUsage,
};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::events::RuntimeEventType;

mod metadata;

use metadata::harvest_chunk_meta;

/// Compute a sha256 hex digest of the input bytes.
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Parse a streamed tool-call's accumulated argument JSON, recovering
/// unparseable input as an empty object. On corruption, returns the recovery
/// object plus a [`MalformedArgs`] fact (flag + SHA-256 of the raw bytes) so the
/// runner can surface "args corrupted upstream" on the `tool_use` braid event
/// instead of a silent empty invocation. A clean parse returns `None`.
fn parse_tool_arguments(name: &str, arguments: &str) -> (Value, Option<MalformedArgs>) {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => (value, None),
        Err(_) => {
            let sha256 = sha256_hex(arguments.as_bytes());
            tracing::warn!(
                tool_name = %name,
                args_len = arguments.len(),
                args_sha256 = %sha256,
                "malformed tool arguments — defaulting to empty object \
                 (set RYEOS_LOG_TOOL_ARGS=1 to log raw)"
            );
            (
                json!({}),
                Some(MalformedArgs {
                    sha256,
                    raw_len: arguments.len(),
                }),
            )
        }
    }
}

/// Build the `tool_use` object embedded in a `cognition_out` braid event.
/// When the streamed arguments were corrupted upstream, the recovered `{}` is
/// carried alongside a `malformed_args` fact (flag + SHA-256 + raw length) so
/// the braid entry reads as an upstream fault rather than a mysterious empty
/// call.
fn tool_use_payload(
    id: &Option<String>,
    name: &str,
    arguments: &Value,
    malformed_args: &Option<MalformedArgs>,
) -> Value {
    let mut payload = json!({
        "id": id,
        "name": name,
        "arguments": arguments,
    });
    if let Some(m) = malformed_args {
        payload["malformed_args"] = json!({
            "corrupted": true,
            "sha256": m.sha256,
            "raw_len": m.raw_len,
        });
    }
    payload
}

/// Redact provider error bodies by default. Returns a safe preview
/// (first 200 chars) plus a sha256 hash. Full body is logged only when
/// `RYEOS_LOG_PROVIDER_BODIES=1` is set (development/debugging).
fn safe_error_body(body: &str) -> String {
    let truncated: String = body.chars().take(512).collect();
    if truncated.len() < body.len() {
        format!(
            "{}… [truncated, sha256={}]",
            truncated,
            sha256_hex(body.as_bytes())
        )
    } else {
        truncated
    }
}

/// A provider-call failure classified for the runner's retry loop. Every
/// variant describes a transient provider/transport failure the runner's retry
/// loop may re-issue. `Status`, `Timeout`, and `Send` occur STRICTLY BEFORE any
/// SSE byte is consumed, so retrying them cannot duplicate already-emitted live
/// `cognition_out`. `MidStream` is the one post-open class: the byte stream
/// died under us (read timeout, reset, dropped chunk) — the request itself is
/// idempotent (same message array), and the ephemeral deltas emitted before the
/// cut quantify the abandoned partial; the durable `provider_retry` event
/// delimits attempts. Any other post-open failure (invalid bytes or a runtime
/// callback publication error) is
/// returned as a plain `anyhow` error and is never retried.
///
/// Returned wrapped in `anyhow::Error` so diagnostic surfaces can preserve the
/// typed detail while the runner `downcast_ref`s to read the retry
/// classification.
#[derive(Debug, Clone)]
pub enum ProviderStreamError {
    /// The provider returned a non-success HTTP status before streaming began.
    Status { code: u16, detail: String },
    /// The request timed out before a response arrived. The provider may still
    /// have accepted work, so this is not proof of zero usage.
    Timeout { detail: String },
    /// A pre-stream transport failure at `.send().await` — DNS resolution, TCP
    /// connect, TLS handshake, or a connection reset before any response byte.
    /// No SSE byte has arrived, but a non-connect failure can still be
    /// ambiguous about upstream acceptance. Common under burst fanout
    /// (many concurrent directive streams opening connections at once), where a
    /// transient connect-phase failure would otherwise be fatal. `connect` marks
    /// a reqwest connect-phase error (vs a body/read transport error).
    Send { connect: bool, detail: String },
    /// The open stream failed mid-read — a chunk-level timeout, reset, or
    /// protocol error after content started arriving.
    /// `live_output_events_emitted` counts acknowledged live/ephemeral
    /// `cognition_out` publications for this attempt.
    MidStream {
        live_output_events_emitted: usize,
        accepted_bytes: u64,
        accepted_output_events: usize,
        usage: Option<TokenUsage>,
        generation_header_id: Option<String>,
        response_id: Option<String>,
        requested_output_tokens: Option<u64>,
        detail: String,
    },
}

impl std::fmt::Display for ProviderStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderStreamError::Status { detail, .. }
            | ProviderStreamError::Timeout { detail }
            | ProviderStreamError::Send { detail, .. }
            | ProviderStreamError::MidStream { detail, .. } => f.write_str(detail),
        }
    }
}

impl std::error::Error for ProviderStreamError {}

/// RyeOS-local byte backstop failure. Kept separate from provider usage and
/// provider-native token limits so retry and diagnostic surfaces preserve the
/// measurement's provenance.
#[derive(Debug, Clone)]
pub struct LocalOutputByteLimitError {
    pub accepted_bytes: u64,
    pub prospective_bytes: u64,
    pub cap_bytes: u64,
    pub accepted_output_events: usize,
    pub live_output_events_emitted: usize,
    pub usage: Option<TokenUsage>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
}

impl std::fmt::Display for LocalOutputByteLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local per-turn output byte cap exceeded: prospective {} bytes > cap {} bytes \
             (accepted_bytes={}, accepted_output_events={}, live_output_events_emitted={}, \
             provider_usage_output_tokens={:?}, generation_header_id={:?}, response_id={:?}); \
             provider usage metadata is not used for stream-limit enforcement",
            self.prospective_bytes,
            self.cap_bytes,
            self.accepted_bytes,
            self.accepted_output_events,
            self.live_output_events_emitted,
            self.usage.as_ref().and_then(|usage| usage.output_tokens),
            self.generation_header_id,
            self.response_id,
        )
    }
}

impl std::error::Error for LocalOutputByteLimitError {}

/// Error object reported inside an otherwise successful SSE response. This is
/// distinct from transport/protocol failures and local output-limit failures.
#[derive(Debug, Clone)]
pub struct ProviderReportedStreamError {
    pub code: Option<String>,
    pub message: String,
    pub metadata: Option<Value>,
    pub usage: Option<TokenUsage>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
    pub requested_output_tokens: Option<u64>,
    pub accepted_bytes: u64,
    pub accepted_output_events: usize,
    pub live_output_events_emitted: usize,
}

impl std::fmt::Display for ProviderReportedStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider reported a streaming error{}: {} \
             (accepted_bytes={}, accepted_output_events={}, \
             live_output_events_emitted={}, generation_header_id={:?}, response_id={:?}, \
             requested_output_tokens={:?})",
            self.code
                .as_deref()
                .map(|code| format!(" [{code}]"))
                .unwrap_or_default(),
            self.message,
            self.accepted_bytes,
            self.accepted_output_events,
            self.live_output_events_emitted,
            self.generation_header_id,
            self.response_id,
            self.requested_output_tokens,
        )
    }
}

impl std::error::Error for ProviderReportedStreamError {}

/// A provider stream violated its declared framing/termination contract. This
/// is non-retryable after any output was accepted and is never reported as a
/// local output-limit failure.
#[derive(Debug, Clone)]
pub struct ProviderProtocolStreamError {
    pub detail: String,
    pub accepted_bytes: u64,
    pub accepted_output_events: usize,
    pub live_output_events_emitted: usize,
    pub usage: Option<TokenUsage>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
    pub requested_output_tokens: Option<u64>,
}

impl std::fmt::Display for ProviderProtocolStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider_protocol_error: {} (accepted_bytes={}, \
             accepted_output_events={}, live_output_events_emitted={}, \
             provider_usage_output_tokens={:?}, generation_header_id={:?}, \
             response_id={:?}, requested_output_tokens={:?})",
            self.detail,
            self.accepted_bytes,
            self.accepted_output_events,
            self.live_output_events_emitted,
            self.usage.as_ref().and_then(|usage| usage.output_tokens),
            self.generation_header_id,
            self.response_id,
            self.requested_output_tokens,
        )
    }
}

impl std::error::Error for ProviderProtocolStreamError {}

/// The provider event was accepted by RyeOS's local meter, but publication to
/// the runtime callback failed. This is an infrastructure failure, not a
/// provider/protocol failure, and it is deliberately non-retryable. Retaining
/// the complete attempt state lets the runner settle any accounting already
/// received without claiming the accepted event was durably published.
#[derive(Debug, Clone)]
pub struct RuntimeCallbackPublicationError {
    pub detail: String,
    pub accepted_bytes: u64,
    pub accepted_output_events: usize,
    pub live_output_events_emitted: usize,
    pub usage: Option<TokenUsage>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
    pub requested_output_tokens: Option<u64>,
}

impl std::fmt::Display for RuntimeCallbackPublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "runtime_callback_publication_failed: {} (accepted_bytes={}, \
             accepted_output_events={}, live_output_events_emitted={}, \
             provider_usage_output_tokens={:?}, generation_header_id={:?}, \
             response_id={:?}, requested_output_tokens={:?})",
            self.detail,
            self.accepted_bytes,
            self.accepted_output_events,
            self.live_output_events_emitted,
            self.usage.as_ref().and_then(|usage| usage.output_tokens),
            self.generation_header_id,
            self.response_id,
            self.requested_output_tokens,
        )
    }
}

impl std::error::Error for RuntimeCallbackPublicationError {}

/// Accounting and locally observed output retained when an out-of-band signal
/// cuts an in-flight provider attempt. Cancellation/interruption are not
/// provider failures, but any usage received before the signal must not vanish.
#[derive(Debug, Clone)]
pub struct CutAttemptState {
    pub usage: Option<TokenUsage>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
    pub requested_output_tokens: Option<u64>,
    pub observed_output: ObservedOutput,
}

/// How the stream loop terminated. Replaces overloading `Ok(AdapterResponse)`
/// for partial termination: a cancelled or interrupted stream did NOT complete a
/// cognition, and the runner must treat each distinctly (finalize cancelled /
/// seal+redirect on interrupt / proceed on complete).
pub enum StreamOutcome {
    /// The provider finished the response normally.
    Completed {
        response: AdapterResponse,
        events: Vec<StreamEvent>,
    },
    /// A live interrupt (SIGUSR1) cut the in-flight cognition. The partial
    /// assistant message carries accumulated content/reasoning but NO tool_calls
    /// — an interrupted cognition didn't complete its tool call, so the folded
    /// wire history stays well-formed. The runner seals this as
    /// `cognition_out{interrupted:true}` then folds the queued input. `events`
    /// is the partial stream (already persisted live during streaming) — carried
    /// so the runner can still surface any provider `Warning`s from the cut turn.
    Interrupted {
        partial_message: ProviderMessage,
        events: Vec<StreamEvent>,
        attempt: CutAttemptState,
    },
    /// Cancellation (SIGTERM) cut the stream. The runner finalizes the thread as
    /// cancelled — there is no consequence to record.
    Cancelled { attempt: CutAttemptState },
}

/// Check if a flag (cancel or interrupt) is set.
fn flag_set(flag: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> bool {
    flag.as_ref()
        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

/// Async future that resolves when the given flag is set. Polls with a 50ms
/// backoff — bounded reaction latency. Pending forever when the flag is `None`.
async fn flag_signal(flag: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
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
            // Transport terminator only. Semantic completion comes from the
            // provider's declared/raw finish-reason field and must not be
            // overwritten by a synthetic stop.
            continue;
        }

        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(e) => {
                events.push(StreamEvent::Warning {
                    code: "malformed_sse_json".to_string(),
                    message: format!(
                        "logical SSE data payload is not valid JSON: {e}; preview={:?}",
                        payload.chars().take(80).collect::<String>()
                    ),
                });
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
        if line.strip_prefix("id:").is_some() || line.strip_prefix("retry:").is_some() {}
    }
    if !current_data.is_empty() {
        events.push((current_event_type, current_data.trim().to_string()));
    }

    events
}

fn provider_reported_error(
    block: &str,
    config: Option<&crate::directive::StreamErrorConfig>,
    usage: Option<TokenUsage>,
    generation_header_id: Option<String>,
    response_id: Option<String>,
    output_meter: &StreamOutputMeter,
    live_output_events_emitted: usize,
    requested_output_tokens: Option<u64>,
) -> Result<Option<ProviderReportedStreamError>> {
    let Some(config) = config else {
        return Ok(None);
    };
    use ryeos_runtime::template::resolve_path;
    for (_, payload) in split_sse_events(block) {
        if payload == "[DONE]" {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        let Some(error) = resolve_path(&parsed, &config.path).filter(|value| !value.is_null())
        else {
            continue;
        };
        if !error.is_object() {
            return Err(anyhow::Error::new(ProviderProtocolStreamError {
                detail: format!(
                    "declared provider error path `{}` resolved to a non-object value",
                    config.path
                ),
                accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
                accepted_output_events: output_meter.accepted_output_events,
                live_output_events_emitted,
                usage,
                generation_header_id,
                response_id,
                requested_output_tokens,
            }));
        }
        let Some(message) = resolve_path(error, &config.message_path)
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
        else {
            return Err(anyhow::Error::new(ProviderProtocolStreamError {
                detail: format!(
                    "declared provider error object has no non-empty string at message path `{}`",
                    config.message_path
                ),
                accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
                accepted_output_events: output_meter.accepted_output_events,
                live_output_events_emitted,
                usage,
                generation_header_id,
                response_id,
                requested_output_tokens,
            }));
        };
        let code = config.code_path.as_deref().and_then(|path| {
            resolve_path(error, path).map(|value| match value {
                Value::String(code) => code.clone(),
                other => other.to_string(),
            })
        });
        let metadata = config
            .metadata_path
            .as_deref()
            .and_then(|path| resolve_path(error, path))
            .cloned();
        let accepted_bytes = u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX);
        return Ok(Some(ProviderReportedStreamError {
            code,
            message: message.to_string(),
            metadata,
            usage,
            generation_header_id,
            response_id,
            requested_output_tokens,
            accepted_bytes,
            accepted_output_events: output_meter.accepted_output_events,
            live_output_events_emitted,
        }));
    }
    Ok(None)
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
                                let stream_key = tool_call_state
                                    .get("current_tool_stream_key")
                                    .cloned()
                                    .unwrap_or_else(|| "event_typed:unknown".to_string());
                                events.push(StreamEvent::ToolUsePartial {
                                    id,
                                    name,
                                    stream_key,
                                    delta: input.to_string(),
                                    total_len,
                                });
                            }
                        }
                    }
                    other => {
                        events.push(StreamEvent::Warning {
                            code: "unknown_delta_type".into(),
                            message: format!(
                                "anthropic content_block_delta type `{other}` not handled"
                            ),
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
                        let sequence =
                            parsed
                                .get("index")
                                .and_then(Value::as_u64)
                                .unwrap_or_else(|| {
                                    let next = tool_call_state
                                        .get("event_typed_next_sequence")
                                        .and_then(|value| value.parse::<u64>().ok())
                                        .unwrap_or(0);
                                    tool_call_state.insert(
                                        "event_typed_next_sequence".to_string(),
                                        next.saturating_add(1).to_string(),
                                    );
                                    next
                                });
                        tool_call_state.insert("current_tool_id".to_string(), id);
                        tool_call_state.insert("current_tool_name".to_string(), name);
                        tool_call_state.insert(
                            "current_tool_stream_key".to_string(),
                            format!("event_typed:{sequence}"),
                        );
                    }
                    tool_call_state.remove("current_tool_args");
                }
            }
        }
        "content_block_stop" if tool_call_state.contains_key("current_tool_id") => {
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
            let stream_key = tool_call_state
                .get("current_tool_stream_key")
                .cloned()
                .unwrap_or_else(|| "event_typed:unknown".to_string());
            let (arguments_value, malformed_args) = parse_tool_arguments(&name, &arguments);
            let id_opt = if id.is_empty() { None } else { Some(id) };
            events.push(StreamEvent::ToolUse {
                id: id_opt,
                name,
                stream_key,
                argument_bytes: arguments.len(),
                arguments: arguments_value,
                malformed_args,
            });
            tool_call_state.remove("current_tool_id");
            tool_call_state.remove("current_tool_name");
            tool_call_state.remove("current_tool_stream_key");
            tool_call_state.remove("current_tool_args");
        }
        "message_delta" => {
            // Capture stop_reason for use when message_stop arrives.
            if let Some(delta) = parsed.get("delta") {
                if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                    tool_call_state.insert("last_stop_reason".to_string(), sr.to_string());
                }
            }
            // Emit usage update from message_delta.usage (cumulative).
            if let Some(usage_obj) = parsed.get("usage") {
                let mut update = UsageUpdate::default();
                if !usage_obj.is_object() {
                    update.anomalies.push("usage is not an object".to_string());
                }
                update.input_tokens =
                    protocol_usage_u64(usage_obj, "input_tokens", &mut update.anomalies);
                update.output_tokens =
                    protocol_usage_u64(usage_obj, "output_tokens", &mut update.anomalies);
                update.cache_read_tokens =
                    protocol_usage_u64(usage_obj, "cache_read_input_tokens", &mut update.anomalies);
                update.cache_write_tokens = protocol_usage_u64(
                    usage_obj,
                    "cache_creation_input_tokens",
                    &mut update.anomalies,
                );
                events.push(StreamEvent::Usage(update));
            }
        }
        "message_start" => {
            // Initial usage from message_start.message.usage.
            if let Some(usage_obj) = parsed.get("message").and_then(|m| m.get("usage")) {
                let mut update = UsageUpdate::default();
                if !usage_obj.is_object() {
                    update
                        .anomalies
                        .push("message.usage is not an object".to_string());
                }
                update.input_tokens =
                    protocol_usage_u64(usage_obj, "input_tokens", &mut update.anomalies);
                update.output_tokens =
                    protocol_usage_u64(usage_obj, "output_tokens", &mut update.anomalies);
                update.cache_read_tokens =
                    protocol_usage_u64(usage_obj, "cache_read_input_tokens", &mut update.anomalies);
                update.cache_write_tokens = protocol_usage_u64(
                    usage_obj,
                    "cache_creation_input_tokens",
                    &mut update.anomalies,
                );
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

            if let Some(reasoning) = choice
                .get("delta")
                .and_then(|d| {
                    d.get("reasoning_content")
                        .or_else(|| d.get("reasoning"))
                        .or_else(|| d.get("thinking"))
                })
                .and_then(|r| r.as_str())
            {
                events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
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
                            tool_call_state
                                .insert(format!("tool_name_{}", index), name.to_string());
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            let total_len = tool_call_state
                                .entry(format!("tool_args_{}", index))
                                .and_modify(|e| e.push_str(args))
                                .or_insert_with(|| args.to_string())
                                .len();
                            let id = tool_call_state
                                .get(&format!("tool_id_{}", index))
                                .cloned()
                                .filter(|value| !value.is_empty());
                            let name = tool_call_state
                                .get(&format!("tool_name_{}", index))
                                .cloned()
                                .unwrap_or_default();
                            events.push(StreamEvent::ToolUsePartial {
                                id,
                                name,
                                stream_key: format!("delta_merge:{index}"),
                                delta: args.to_string(),
                                total_len,
                            });
                        }
                    }
                }
            }

            let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str());
            let is_terminal = finish_reason.map(|r| !r.is_empty()).unwrap_or(false);

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
                    let (args_value, malformed_args) = parse_tool_arguments(&name, &arguments);
                    let id_opt = if id.is_empty() { None } else { Some(id) };
                    events.push(StreamEvent::ToolUse {
                        id: id_opt,
                        name,
                        stream_key: format!("delta_merge:{idx}"),
                        argument_bytes: arguments.len(),
                        arguments: args_value,
                        malformed_args,
                    });
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
    if let Some(usage_obj) = parsed.get("usage").filter(|usage| !usage.is_null()) {
        if usage_obj.is_object() {
            let mut update = UsageUpdate::default();
            update.input_tokens =
                protocol_usage_u64(usage_obj, "prompt_tokens", &mut update.anomalies);
            update.output_tokens =
                protocol_usage_u64(usage_obj, "completion_tokens", &mut update.anomalies);
            if let Some(details) = usage_obj.get("completion_tokens_details") {
                update.reasoning_tokens =
                    protocol_usage_u64(details, "reasoning_tokens", &mut update.anomalies);
            }
            if update.input_tokens.is_some()
                || update.output_tokens.is_some()
                || update.reasoning_tokens.is_some()
                || !update.anomalies.is_empty()
            {
                events.push(StreamEvent::Usage(update));
            }
        } else {
            events.push(StreamEvent::Usage(UsageUpdate {
                anomalies: vec!["usage is not an object".to_string()],
                ..UsageUpdate::default()
            }));
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
        tracing::warn!("SSE complete_chunks: no streaming.paths config; cannot parse frame");
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
    let mut frame_reasoning = String::new();

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
                        tracing::warn!("complete_chunks: tool_call block missing name; skipping");
                        continue;
                    }
                    // Deduplicate: Gemini sends cumulative frames, so the
                    // same tool call may appear in multiple chunks. Use
                    // (name + arguments) as a dedupe key.
                    let arguments =
                        serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
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
                        id: Some(id.clone()),
                        name,
                        stream_key: format!("complete_chunks:{id}"),
                        argument_bytes: arguments.len(),
                        // Gemini arguments arrive as a parsed JSON value, not a
                        // streamed string, so there is no upstream-corruption
                        // recovery path to flag here.
                        arguments: args,
                        malformed_args: None,
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
                        frame_reasoning.push_str(text);
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

    // Apply a content cursor once against each aggregated channel, not
    // per-part. Complete-chunk protocols send cumulative snapshots; retaining
    // the prior canonical string both avoids double-counting and makes suffix
    // slicing UTF-8 safe. A non-prefix snapshot is a protocol violation.
    // Per-part cursoring corrupts output when a single frame contains
    // multiple text parts (e.g. Gemini sends [{text:"Hello"},{text:" world"}]).
    emit_cumulative_channel(
        events,
        tool_call_state,
        "complete_chunks_seen_reasoning",
        "reasoning",
        &frame_reasoning,
        StreamEvent::ReasoningDelta,
    );
    emit_cumulative_channel(
        events,
        tool_call_state,
        "complete_chunks_seen_text",
        "visible text",
        &frame_text,
        StreamEvent::Delta,
    );

    // Usage from usageMetadata (Gemini path).
    if let Some(ref usage_path) = p.usage_path {
        if let Some(usage_md) = resolve_path(parsed, usage_path) {
            let mut update = UsageUpdate::default();
            if !usage_md.is_object() {
                update
                    .anomalies
                    .push(format!("{usage_path} is not an object"));
            }
            update.input_tokens =
                protocol_usage_u64(usage_md, "promptTokenCount", &mut update.anomalies);
            update.output_tokens =
                protocol_usage_u64(usage_md, "candidatesTokenCount", &mut update.anomalies);
            update.reasoning_tokens =
                protocol_usage_u64(usage_md, "thoughtsTokenCount", &mut update.anomalies);
            if update.input_tokens.is_some()
                || update.output_tokens.is_some()
                || update.reasoning_tokens.is_some()
                || !update.anomalies.is_empty()
            {
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

fn emit_cumulative_channel(
    events: &mut Vec<StreamEvent>,
    state: &mut HashMap<String, String>,
    state_key: &str,
    label: &str,
    snapshot: &str,
    make_event: impl FnOnce(String) -> StreamEvent,
) {
    if snapshot.is_empty() {
        return;
    }
    let previous = state.get(state_key).map(String::as_str).unwrap_or("");
    let Some(suffix) = snapshot.strip_prefix(previous) else {
        events.push(StreamEvent::Warning {
            code: "cumulative_content_regression".to_string(),
            message: format!(
                "complete-chunk {label} snapshot is not prefixed by the previously accepted snapshot"
            ),
        });
        return;
    };
    if !suffix.is_empty() {
        events.push(make_event(suffix.to_string()));
    }
    state.insert(state_key.to_string(), snapshot.to_string());
}

/// Streaming provider call. Issues the request rendered by the provider's
/// signed body template and parses Server-Sent Events incrementally as they
/// arrive. For each `StreamEvent::Delta` and `StreamEvent::ToolUse` the
/// callback's progressive `cognition_out` event is published before moving to
/// the next chunk. These payloads are live/ephemeral; the final turn-complete
/// cognition is the indexed transcript record.
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
    /// Optional cancellation flag (SIGTERM). When set mid-stream, the loop
    /// breaks and the call returns [`StreamOutcome::Cancelled`].
    pub cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Optional live-interrupt flag (SIGUSR1). When set mid-stream, the loop
    /// breaks and the call returns [`StreamOutcome::Interrupted`] with the
    /// partial assistant message accumulated so far.
    pub interrupt_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[tracing::instrument(
    name = "provider:request_streaming",
    skip(input),
    fields(adapter_type = "stream", model = %input.model, turn = input.turn)
)]
pub async fn call_provider_streaming(input: StreamingCallInput<'_>) -> Result<StreamOutcome> {
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
        cancel_flag: _,    // checked inside the stream loop
        interrupt_flag: _, // checked inside the stream loop
    } = input;
    let schemas = provider.schemas.as_ref().and_then(|s| s.messages.as_ref());

    let (converted_messages, system_prompt) =
        crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());

    let tool_schema = provider.schemas.as_ref().and_then(|s| s.tools.clone());
    let tools_val = crate::provider_adapter::tools::serialize_tools(tools, &tool_schema);

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
            if su.starts_with('/') {
                su.to_string()
            } else {
                format!("/{}", su)
            }
        ),
        None => format!("{}/chat/completions", base_resolved.trim_end_matches('/')),
    };

    let mut body = build_request_body(
        provider,
        model,
        &converted_messages,
        system_prompt.as_deref(),
        &tools_val,
        !tools.is_empty(),
        (execution.max_provider_output_tokens_per_turn != 0)
            .then_some(execution.max_provider_output_tokens_per_turn),
    )?;

    // Sampling parameters — gated by provider capabilities so we
    // never send a field the upstream API will reject with a 400.
    inject_sampling(&mut body, provider.family, sampling);
    apply_declared_output_limit(
        &mut body,
        provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.output_limit.as_ref()),
        (execution.max_provider_output_tokens_per_turn != 0)
            .then_some(execution.max_provider_output_tokens_per_turn),
    )?;
    let requested_output_tokens = declared_output_limit_from_body(provider, &body)?;

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

    let empty_cut_attempt = || CutAttemptState {
        usage: None,
        generation_header_id: None,
        response_id: None,
        requested_output_tokens,
        observed_output: ObservedOutput::default(),
    };
    if flag_set(&input.cancel_flag) {
        return Ok(StreamOutcome::Cancelled {
            attempt: empty_cut_attempt(),
        });
    }
    if flag_set(&input.interrupt_flag) {
        return Ok(StreamOutcome::Interrupted {
            partial_message: ProviderMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            events: Vec::new(),
            attempt: empty_cut_attempt(),
        });
    }

    let send = req
        .json(&body)
        .timeout(Duration::from_secs(execution.timeout_seconds))
        .send();
    tokio::pin!(send);
    let send_result = tokio::select! {
        biased;
        _ = flag_signal(&input.cancel_flag) => {
            return Ok(StreamOutcome::Cancelled { attempt: empty_cut_attempt() });
        }
        _ = flag_signal(&input.interrupt_flag) => {
            return Ok(StreamOutcome::Interrupted {
                partial_message: ProviderMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                events: Vec::new(),
                attempt: empty_cut_attempt(),
            });
        }
        response = &mut send => response,
    };
    let resp = send_result.map_err(|e| {
        // Every `.send().await` failure is pre-stream from RyeOS's point of
        // view (no SSE byte arrived), but timeout/non-connect failures do
        // not prove that the provider performed no work. Capture the
        // reqwest classifiers before wrapping, then let anyhow's alternate
        // Display walk the source chain; reqwest 0.12's Display alone drops
        // the underlying DNS / TCP / TLS / reset cause.
        let is_timeout = e.is_timeout();
        let connect = e.is_connect();
        let chain = format!("{:#}", anyhow::Error::new(e));
        if is_timeout {
            anyhow::Error::new(ProviderStreamError::Timeout {
                detail: format!(
                    "streaming request timed out after {}s: {chain}",
                    execution.timeout_seconds
                ),
            })
        } else {
            anyhow::Error::new(ProviderStreamError::Send {
                connect,
                detail: format!("streaming request failed: {chain}"),
            })
        }
    })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!(
                "failed to read error response body: {:#}",
                anyhow::Error::new(e)
            );
            String::new()
        });
        // Lead with the provider's own error body — it's the actionable part
        // (e.g. "Insufficient balance"). The diagnostic context trails it so a
        // truncated surface (the TUI feed line) keeps the message, not the IDs.
        // Carry the status code as a typed `ProviderStreamError` so the runner's
        // retry loop can key off it; Display still renders this full message.
        let detail = format!(
            "provider returned {status} (streaming): {safe_body} \
             [provider={provider_id} profile={matched_profile:?} \
             config_hash={config_hash} request_body_sha256={request_body_sha256}]",
            status = status,
            safe_body = safe_error_body(&text),
            provider_id = provider_id,
            matched_profile = matched_profile,
            config_hash = config_hash,
            request_body_sha256 = request_body_sha256,
        );
        return Err(anyhow::Error::new(ProviderStreamError::Status {
            code: status,
            detail,
        }));
    }

    let stream_mode = provider
        .schemas
        .as_ref()
        .and_then(|s| s.streaming.as_ref())
        .and_then(|st| st.mode)
        .ok_or_else(|| anyhow!("validated provider config is missing schemas.streaming.mode"))?
        .as_str()
        .to_string();

    let streaming_config = provider.schemas.as_ref().and_then(|s| s.streaming.as_ref());
    let stream_paths = streaming_config.and_then(|streaming| streaming.paths.as_ref());
    // A declared metadata schema is authoritative for response accounting and
    // termination. Protocol-family parsers may also emit Usage/Finish events,
    // but those events must not override signed paths or aggregation semantics.
    let metadata_usage_declared = streaming_config
        .and_then(|streaming| streaming.metadata.as_ref())
        .and_then(|metadata| metadata.usage.as_ref())
        .is_some();
    let metadata_finish_declared = streaming_config
        .and_then(|streaming| streaming.metadata.as_ref())
        .and_then(|metadata| metadata.finish_reason_path.as_ref())
        .is_some();

    let generation_header_id = streaming_config
        .and_then(|streaming| streaming.metadata.as_ref())
        .and_then(|metadata| metadata.generation_id_header.as_deref())
        .and_then(|header| resp.headers().get(header))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
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
    let mut accumulated_reasoning = String::new();
    let mut accumulated_tools: Vec<ToolCall> = Vec::new();
    let mut output_meter = StreamOutputMeter::default();
    let max_stream_output_bytes = execution.max_stream_output_bytes_per_turn;
    let mut last_usage: Option<TokenUsage> = None;
    let mut last_finish: Option<String> = None;
    let mut last_response_id: Option<String> = None;
    // Per-stream tool-call accumulator keyed by index; survives
    // across chunks so partial JSON arg fragments concatenate
    // correctly under both Anthropic (`partial_json`) and OpenAI
    // (`tool_calls[].function.arguments`) wire shapes.
    let mut tool_state: HashMap<String, String> = HashMap::new();
    // Acknowledged live/ephemeral `cognition_out` publications for this
    // attempt. The count is carried on failures separately from locally
    // accepted semantic events.
    let mut live_output_events_emitted: usize = 0;

    // How the loop terminated. `Closed` = upstream finished normally; the others
    // are out-of-band cuts. Cancel takes priority over interrupt (SIGTERM ends
    // the run; SIGUSR1 only redirects it).
    #[derive(PartialEq)]
    enum LoopExit {
        Closed,
        Cancelled,
        Interrupted,
    }
    let mut exit = LoopExit::Closed;

    loop {
        // Preemptive checks BEFORE waiting for the next chunk. Combined with the
        // post-await select below, reaction latency is bounded by the polling
        // interval (~50ms), not the next chunk arrival.
        if flag_set(&input.cancel_flag) {
            tracing::info!(
                turn = input.turn,
                "provider stream cancelled mid-flight (preemptive)"
            );
            exit = LoopExit::Cancelled;
            break;
        }
        if flag_set(&input.interrupt_flag) {
            tracing::info!(
                turn = input.turn,
                "provider stream interrupted mid-flight (preemptive)"
            );
            exit = LoopExit::Interrupted;
            break;
        }

        let chunk_res = tokio::select! {
            biased;

            _ = flag_signal(&input.cancel_flag) => {
                tracing::info!(turn = input.turn, "provider stream cancelled mid-flight (select)");
                exit = LoopExit::Cancelled;
                break;
            }

            _ = flag_signal(&input.interrupt_flag) => {
                tracing::info!(turn = input.turn, "provider stream interrupted mid-flight (select)");
                exit = LoopExit::Interrupted;
                break;
            }

            chunk = byte_stream.next() => {
                match chunk {
                    Some(c) => c,
                    None => break, // upstream closed
                }
            }
        };

        // A chunk-level read failure (timeout, reset, dropped stream) is a
        // TRANSIENT transport error on an idempotent request: classify it as a
        // typed `MidStream` so the runner's retry loop can re-issue instead of
        // failing the whole directive thread over one dropped HTTP stream.
        let chunk: Bytes = chunk_res.map_err(|e| {
            let chain = format!("{:#}", anyhow::Error::new(e));
            anyhow::Error::new(ProviderStreamError::MidStream {
                live_output_events_emitted,
                accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
                accepted_output_events: output_meter.accepted_output_events,
                usage: last_usage.clone(),
                generation_header_id: generation_header_id.clone(),
                response_id: last_response_id.clone(),
                requested_output_tokens,
                detail: format!("stream chunk error: {chain}"),
            })
        })?;
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
                let prefix = unsafe { std::str::from_utf8_unchecked(&utf8_carry[..valid_up_to]) }
                    .to_string();
                if let Some(invalid_len) = e.error_len() {
                    // A genuinely invalid byte sequence (not just a
                    // truncated multi-byte char) — fail loud.
                    return Err(anyhow::Error::new(ProviderProtocolStreamError {
                        detail: format!(
                            "non-utf8 SSE chunk: invalid byte sequence of length {invalid_len} at index {valid_up_to}"
                        ),
                        accepted_bytes: u64::try_from(output_meter.total_bytes())
                            .unwrap_or(u64::MAX),
                        accepted_output_events: output_meter.accepted_output_events,
                        live_output_events_emitted,
                        usage: last_usage.clone(),
                        generation_header_id: generation_header_id.clone(),
                        response_id: last_response_id.clone(),
                        requested_output_tokens,
                    }));
                }
                (prefix, valid_up_to)
            }
        };
        utf8_carry.drain(..tail_start);
        buffer.push_str(&decoded);

        // Drain complete SSE event blocks (separated by blank line).
        loop {
            // A stream may legally switch line endings between logical events.
            // Pick the earliest delimiter of either form; `Option::or_else`
            // would incorrectly prefer a later LF delimiter whenever one
            // existed, merging an earlier CRLF-framed event into it.
            let Some((idx, sep_len)) = next_sse_delimiter(&buffer) else {
                break;
            };
            if stream_frame_exceeds(execution.max_provider_stream_frame_bytes, idx) {
                return Err(anyhow::Error::new(ProviderProtocolStreamError {
                    detail: format!(
                        "logical SSE event exceeded the configured framing limit: {idx} bytes > {} bytes",
                        execution.max_provider_stream_frame_bytes
                    ),
                    accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
                    accepted_output_events: output_meter.accepted_output_events,
                    live_output_events_emitted,
                    usage: last_usage.clone(),
                    generation_header_id: generation_header_id.clone(),
                    response_id: last_response_id.clone(),
                    requested_output_tokens,
                }));
            }
            let block: String = buffer[..idx].to_string();
            buffer.drain(..idx + sep_len);

            // Pull provider-side usage + finish_reason from the raw
            // chunk JSON regardless of stream_mode (the existing
            // parse_* helpers don't surface these typed pieces).
            harvest_chunk_meta(
                &block,
                &mut last_usage,
                &mut last_finish,
                &mut last_response_id,
                streaming_config,
            );
            validate_usage_against_requested_limit(last_usage.as_mut(), requested_output_tokens);
            if let Some(error) = provider_reported_error(
                &format!("{block}\n\n"),
                streaming_config
                    .and_then(|streaming| streaming.metadata.as_ref())
                    .and_then(|metadata| metadata.error.as_ref()),
                last_usage.clone(),
                generation_header_id.clone(),
                last_response_id.clone(),
                &output_meter,
                live_output_events_emitted,
                requested_output_tokens,
            )? {
                return Err(anyhow::Error::new(error));
            }

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
            reject_malformed_sse_events(
                &new_events,
                &output_meter,
                live_output_events_emitted,
                last_usage.clone(),
                generation_header_id.clone(),
                last_response_id.clone(),
                requested_output_tokens,
            )?;

            for ev in new_events {
                if !metadata_usage_declared {
                    if let StreamEvent::Usage(update) = &ev {
                        merge_stream_usage_update(&mut last_usage, update);
                        validate_usage_against_requested_limit(
                            last_usage.as_mut(),
                            requested_output_tokens,
                        );
                    }
                }
                if !metadata_finish_declared {
                    if let StreamEvent::Finish { raw: Some(raw), .. } = &ev {
                        last_finish = Some(raw.clone());
                    }
                }
                output_meter
                    .accept(&ev, max_stream_output_bytes, live_output_events_emitted)
                    .map_err(|error| {
                        enrich_local_output_limit(
                            error,
                            last_usage.as_ref(),
                            generation_header_id.as_deref(),
                            last_response_id.as_deref(),
                        )
                    })?;
                match &ev {
                    StreamEvent::Delta(text) => {
                        accumulated_text.push_str(text);
                        callback
                            .append_runtime_event(
                                RuntimeEventType::CognitionOut,
                                json!({
                                    "turn": turn,
                                    "delta": text,
                                }),
                            )
                            .await
                            .map_err(|e| {
                                callback_publication_error(
                                    format!(
                                        "live cognition_out delta publication failed mid-stream: {e}"
                                    ),
                                    &output_meter,
                                    live_output_events_emitted,
                                    last_usage.clone(),
                                    generation_header_id.clone(),
                                    last_response_id.clone(),
                                    requested_output_tokens,
                                )
                            })?;
                        live_output_events_emitted += 1;
                    }
                    StreamEvent::ToolUse {
                        id,
                        name,
                        arguments,
                        malformed_args,
                        ..
                    } => {
                        accumulated_tools.push(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                        callback
                            .append_runtime_event(
                                RuntimeEventType::CognitionOut,
                                json!({
                                    "turn": turn,
                                    "tool_use": tool_use_payload(id, name, arguments, malformed_args),
                                }),
                            )
                            .await
                            .map_err(|e| {
                                callback_publication_error(
                                    format!(
                                        "live cognition_out tool_use publication failed mid-stream: {e}"
                                    ),
                                    &output_meter,
                                    live_output_events_emitted,
                                    last_usage.clone(),
                                    generation_header_id.clone(),
                                    last_response_id.clone(),
                                    requested_output_tokens,
                                )
                            })?;
                        live_output_events_emitted += 1;
                    }
                    StreamEvent::ToolUsePartial {
                        id,
                        name,
                        delta,
                        total_len,
                        ..
                    } => {
                        callback
                            .append_runtime_event(
                                RuntimeEventType::CognitionOut,
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
                                callback_publication_error(
                                    format!(
                                        "live cognition_out tool_use_partial publication failed \
                                         mid-stream: {e}"
                                    ),
                                    &output_meter,
                                    live_output_events_emitted,
                                    last_usage.clone(),
                                    generation_header_id.clone(),
                                    last_response_id.clone(),
                                    requested_output_tokens,
                                )
                            })?;
                        live_output_events_emitted += 1;
                    }
                    StreamEvent::Finish { .. } => {}
                    StreamEvent::Usage(_) => {}
                    StreamEvent::Warning { .. } => {}
                    StreamEvent::ReasoningDelta(text) => {
                        accumulated_reasoning.push_str(text);
                    }
                }
                all_events.push(ev);
            }
        }
        if stream_frame_exceeds(execution.max_provider_stream_frame_bytes, buffer.len()) {
            return Err(anyhow::Error::new(ProviderProtocolStreamError {
                detail: format!(
                    "unterminated logical SSE event exceeded the configured framing limit: {} bytes > {} bytes",
                    buffer.len(), execution.max_provider_stream_frame_bytes
                ),
                accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
                accepted_output_events: output_meter.accepted_output_events,
                live_output_events_emitted,
                usage: last_usage.clone(),
                generation_header_id: generation_header_id.clone(),
                response_id: last_response_id.clone(),
                requested_output_tokens,
            }));
        }
    }

    // Cancellation (SIGTERM) ends the run — no consequence to record. Bail out
    // before building any partial response.
    if exit == LoopExit::Cancelled {
        return Ok(StreamOutcome::Cancelled {
            attempt: CutAttemptState {
                usage: last_usage,
                generation_header_id,
                response_id: last_response_id,
                requested_output_tokens,
                observed_output: output_meter.observed_output(live_output_events_emitted),
            },
        });
    }

    if exit == LoopExit::Closed && !utf8_carry.is_empty() {
        return Err(anyhow::Error::new(ProviderProtocolStreamError {
            detail: format!(
                "provider stream closed with {} bytes of an incomplete UTF-8 sequence",
                utf8_carry.len()
            ),
            accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
            accepted_output_events: output_meter.accepted_output_events,
            live_output_events_emitted,
            usage: last_usage.clone(),
            generation_header_id: generation_header_id.clone(),
            response_id: last_response_id.clone(),
            requested_output_tokens,
        }));
    }

    // Final flush: any trailing block without terminal blank line. Only on a
    // normal close — an interrupt cut leaves an incomplete trailing SSE block we
    // intentionally drop (the partial is whatever fully-parsed blocks produced).
    if exit == LoopExit::Closed && !buffer.trim().is_empty() {
        let framed = format!("{}\n\n", buffer);
        harvest_chunk_meta(
            &buffer,
            &mut last_usage,
            &mut last_finish,
            &mut last_response_id,
            streaming_config,
        );
        validate_usage_against_requested_limit(last_usage.as_mut(), requested_output_tokens);
        if let Some(error) = provider_reported_error(
            &framed,
            streaming_config
                .and_then(|streaming| streaming.metadata.as_ref())
                .and_then(|metadata| metadata.error.as_ref()),
            last_usage.clone(),
            generation_header_id.clone(),
            last_response_id.clone(),
            &output_meter,
            live_output_events_emitted,
            requested_output_tokens,
        )? {
            return Err(anyhow::Error::new(error));
        }
        let final_events =
            parse_sse_events_with_state(&framed, Some(&stream_mode), stream_paths, &mut tool_state);
        reject_malformed_sse_events(
            &final_events,
            &output_meter,
            live_output_events_emitted,
            last_usage.clone(),
            generation_header_id.clone(),
            last_response_id.clone(),
            requested_output_tokens,
        )?;
        for ev in final_events {
            if !metadata_usage_declared {
                if let StreamEvent::Usage(update) = &ev {
                    merge_stream_usage_update(&mut last_usage, update);
                    validate_usage_against_requested_limit(
                        last_usage.as_mut(),
                        requested_output_tokens,
                    );
                }
            }
            if !metadata_finish_declared {
                if let StreamEvent::Finish { raw: Some(raw), .. } = &ev {
                    last_finish = Some(raw.clone());
                }
            }
            output_meter
                .accept(&ev, max_stream_output_bytes, live_output_events_emitted)
                .map_err(|error| {
                    enrich_local_output_limit(
                        error,
                        last_usage.as_ref(),
                        generation_header_id.as_deref(),
                        last_response_id.as_deref(),
                    )
                })?;
            match &ev {
                StreamEvent::Delta(text) => {
                    accumulated_text.push_str(text);
                    callback
                        .append_runtime_event(
                            RuntimeEventType::CognitionOut,
                            json!({"turn": turn, "delta": text}),
                        )
                        .await
                        .map_err(|e| {
                            callback_publication_error(
                                format!(
                                    "live cognition_out delta publication failed at stream end: {e}"
                                ),
                                &output_meter,
                                live_output_events_emitted,
                                last_usage.clone(),
                                generation_header_id.clone(),
                                last_response_id.clone(),
                                requested_output_tokens,
                            )
                        })?;
                    live_output_events_emitted += 1;
                }
                StreamEvent::ToolUse {
                    id,
                    name,
                    arguments,
                    malformed_args,
                    ..
                } => {
                    accumulated_tools.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    callback
                        .append_runtime_event(
                            RuntimeEventType::CognitionOut,
                            json!({
                                "turn": turn,
                                "tool_use": tool_use_payload(id, name, arguments, malformed_args),
                            }),
                        )
                        .await
                        .map_err(|e| {
                            callback_publication_error(
                                format!(
                                    "live cognition_out tool_use publication failed at stream end: {e}"
                                ),
                                &output_meter,
                                live_output_events_emitted,
                                last_usage.clone(),
                                generation_header_id.clone(),
                                last_response_id.clone(),
                                requested_output_tokens,
                            )
                        })?;
                    live_output_events_emitted += 1;
                }
                StreamEvent::ToolUsePartial {
                    id,
                    name,
                    delta,
                    total_len,
                    ..
                } => {
                    callback
                        .append_runtime_event(
                            RuntimeEventType::CognitionOut,
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
                            callback_publication_error(
                                format!(
                                    "live cognition_out tool_use_partial publication failed at \
                                     stream end: {e}"
                                ),
                                &output_meter,
                                live_output_events_emitted,
                                last_usage.clone(),
                                generation_header_id.clone(),
                                last_response_id.clone(),
                                requested_output_tokens,
                            )
                        })?;
                    live_output_events_emitted += 1;
                }
                StreamEvent::Finish { .. } => {}
                StreamEvent::Usage(_) => {}
                StreamEvent::Warning { .. } => {}
                StreamEvent::ReasoningDelta(text) => {
                    accumulated_reasoning.push_str(text);
                }
            }
            all_events.push(ev);
        }
    }

    if exit == LoopExit::Closed && metadata_finish_declared && last_finish.is_none() {
        return Err(anyhow::Error::new(ProviderProtocolStreamError {
            detail:
                "stream closed without the finish reason required by the signed metadata schema"
                    .to_string(),
            accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
            accepted_output_events: output_meter.accepted_output_events,
            live_output_events_emitted,
            usage: last_usage.clone(),
            generation_header_id: generation_header_id.clone(),
            response_id: last_response_id.clone(),
            requested_output_tokens,
        }));
    }

    let content = if accumulated_text.is_empty() {
        None
    } else {
        Some(json!(accumulated_text))
    };
    let reasoning_content = if accumulated_reasoning.is_empty() {
        None
    } else {
        Some(accumulated_reasoning)
    };

    // Interrupt cut: seal the partial as an assistant message with content +
    // reasoning only. NO tool_calls — an interrupted cognition didn't complete
    // its tool call, so the folded wire history has no unpaired tool_call → the
    // runner records `cognition_out{interrupted:true}` from this.
    if exit == LoopExit::Interrupted {
        return Ok(StreamOutcome::Interrupted {
            partial_message: ProviderMessage {
                role: "assistant".to_string(),
                content,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content,
            },
            events: all_events,
            attempt: CutAttemptState {
                usage: last_usage,
                generation_header_id,
                response_id: last_response_id,
                requested_output_tokens,
                observed_output: output_meter.observed_output(live_output_events_emitted),
            },
        });
    }

    let message = ProviderMessage {
        role: "assistant".to_string(),
        content,
        tool_calls: if accumulated_tools.is_empty() {
            None
        } else {
            Some(accumulated_tools)
        },
        tool_call_id: None,
        reasoning_content,
    };

    Ok(StreamOutcome::Completed {
        response: AdapterResponse {
            message,
            usage: last_usage,
            finish_reason: last_finish,
            generation_header_id,
            response_id: last_response_id,
            requested_output_tokens,
            observed_output: output_meter.observed_output(live_output_events_emitted),
        },
        events: all_events,
    })
}

fn reject_malformed_sse_events(
    events: &[StreamEvent],
    output_meter: &StreamOutputMeter,
    live_output_events_emitted: usize,
    usage: Option<TokenUsage>,
    generation_header_id: Option<String>,
    response_id: Option<String>,
    requested_output_tokens: Option<u64>,
) -> Result<()> {
    let Some((code, message)) = events.iter().find_map(|event| match event {
        StreamEvent::Warning { code, message }
            if code == "malformed_sse_json" || code == "cumulative_content_regression" =>
        {
            Some((code, message))
        }
        _ => None,
    }) else {
        return Ok(());
    };
    Err(anyhow::Error::new(ProviderProtocolStreamError {
        detail: format!("{code}: {message}"),
        accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
        accepted_output_events: output_meter.accepted_output_events,
        live_output_events_emitted,
        usage,
        generation_header_id,
        response_id,
        requested_output_tokens,
    }))
}

fn callback_publication_error(
    detail: String,
    output_meter: &StreamOutputMeter,
    live_output_events_emitted: usize,
    usage: Option<TokenUsage>,
    generation_header_id: Option<String>,
    response_id: Option<String>,
    requested_output_tokens: Option<u64>,
) -> anyhow::Error {
    anyhow::Error::new(RuntimeCallbackPublicationError {
        detail,
        accepted_bytes: u64::try_from(output_meter.total_bytes()).unwrap_or(u64::MAX),
        accepted_output_events: output_meter.accepted_output_events,
        live_output_events_emitted,
        usage,
        generation_header_id,
        response_id,
        requested_output_tokens,
    })
}

fn stream_frame_exceeds(limit: u64, observed: usize) -> bool {
    limit != 0 && u64::try_from(observed).unwrap_or(u64::MAX) > limit
}

fn next_sse_delimiter(buffer: &str) -> Option<(usize, usize)> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
struct StreamOutputMeter {
    text_bytes: usize,
    reasoning_bytes: usize,
    tool_argument_bytes: HashMap<String, usize>,
    accepted_output_events: usize,
}

fn enrich_local_output_limit(
    error: anyhow::Error,
    usage: Option<&TokenUsage>,
    generation_header_id: Option<&str>,
    response_id: Option<&str>,
) -> anyhow::Error {
    let Some(limit) = error.downcast_ref::<LocalOutputByteLimitError>() else {
        return error;
    };
    anyhow::Error::new(LocalOutputByteLimitError {
        usage: usage.cloned(),
        generation_header_id: generation_header_id.map(str::to_string),
        response_id: response_id.map(str::to_string),
        ..limit.clone()
    })
}

impl StreamOutputMeter {
    fn observed_output(&self, live_output_events_emitted: usize) -> ObservedOutput {
        ObservedOutput {
            text_bytes: u64::try_from(self.text_bytes).unwrap_or(u64::MAX),
            reasoning_bytes: u64::try_from(self.reasoning_bytes).unwrap_or(u64::MAX),
            tool_argument_bytes: u64::try_from(
                self.tool_argument_bytes.values().copied().sum::<usize>(),
            )
            .unwrap_or(u64::MAX),
            accepted_output_events: u64::try_from(self.accepted_output_events).unwrap_or(u64::MAX),
            live_output_events_emitted: u64::try_from(live_output_events_emitted)
                .unwrap_or(u64::MAX),
        }
    }

    fn total_bytes(&self) -> usize {
        self.text_bytes
            .saturating_add(self.reasoning_bytes)
            .saturating_add(self.tool_argument_bytes.values().copied().sum())
    }

    fn prospective_total_bytes(&self, event: &StreamEvent) -> usize {
        match event {
            StreamEvent::Delta(text) => self.total_bytes().saturating_add(text.len()),
            StreamEvent::ReasoningDelta(text) => self.total_bytes().saturating_add(text.len()),
            StreamEvent::ToolUsePartial {
                stream_key,
                total_len,
                ..
            } => self
                .total_bytes()
                .saturating_sub(
                    self.tool_argument_bytes
                        .get(stream_key)
                        .copied()
                        .unwrap_or(0),
                )
                .saturating_add(*total_len),
            StreamEvent::ToolUse {
                stream_key,
                argument_bytes,
                ..
            } => {
                if self.tool_argument_bytes.contains_key(stream_key) {
                    self.total_bytes()
                } else {
                    self.total_bytes().saturating_add(*argument_bytes)
                }
            }
            StreamEvent::Usage(_) | StreamEvent::Warning { .. } | StreamEvent::Finish { .. } => {
                self.total_bytes()
            }
        }
    }

    fn accept(
        &mut self,
        event: &StreamEvent,
        cap_bytes: u64,
        live_output_events_emitted: usize,
    ) -> Result<()> {
        let accepted_bytes = self.total_bytes();
        let prospective_bytes = self.prospective_total_bytes(event);
        enforce_per_turn_output_byte_cap(
            cap_bytes,
            accepted_bytes,
            prospective_bytes,
            self.accepted_output_events,
            live_output_events_emitted,
        )?;

        match event {
            StreamEvent::Delta(text) => {
                self.text_bytes = self.text_bytes.saturating_add(text.len());
                self.accepted_output_events = self.accepted_output_events.saturating_add(1);
            }
            StreamEvent::ReasoningDelta(text) => {
                self.reasoning_bytes = self.reasoning_bytes.saturating_add(text.len());
                self.accepted_output_events = self.accepted_output_events.saturating_add(1);
            }
            StreamEvent::ToolUsePartial {
                stream_key,
                total_len,
                ..
            } => {
                self.tool_argument_bytes
                    .insert(stream_key.clone(), *total_len);
                self.accepted_output_events = self.accepted_output_events.saturating_add(1);
            }
            StreamEvent::ToolUse {
                stream_key,
                argument_bytes,
                ..
            } => {
                self.tool_argument_bytes
                    .entry(stream_key.clone())
                    .or_insert(*argument_bytes);
                self.accepted_output_events = self.accepted_output_events.saturating_add(1);
            }
            StreamEvent::Usage(_) | StreamEvent::Warning { .. } | StreamEvent::Finish { .. } => {}
        }
        Ok(())
    }
}

fn enforce_per_turn_output_byte_cap(
    cap_bytes: u64,
    accepted_bytes: usize,
    prospective_bytes: usize,
    accepted_output_events: usize,
    live_output_events_emitted: usize,
) -> Result<()> {
    if cap_bytes == 0 {
        return Ok(());
    }

    let accepted_bytes = u64::try_from(accepted_bytes).unwrap_or(u64::MAX);
    let prospective_bytes = u64::try_from(prospective_bytes).unwrap_or(u64::MAX);

    if prospective_bytes > cap_bytes {
        return Err(anyhow::Error::new(LocalOutputByteLimitError {
            accepted_bytes,
            prospective_bytes,
            cap_bytes,
            accepted_output_events,
            live_output_events_emitted,
            usage: None,
            generation_header_id: None,
            response_id: None,
        }));
    }

    Ok(())
}

/// Inject sampling fields into the request body, gated by provider
/// capabilities. Unknown providers get no fields (fail-closed).
fn inject_sampling(body: &mut Value, family: ProtocolFamily, sampling: Option<&SamplingConfig>) {
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
/// a context containing `model`, `messages`, `tools`, and `stream`. Then
/// deep-merges `body_extra`, applies a schema-declared provider-native output
/// limit, injects the system
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
    max_output_tokens: Option<u64>,
) -> Result<Value> {
    use ryeos_runtime::template::{apply_template, deep_merge};

    let template = provider.body_template.as_ref().unwrap_or_else(|| {
        unreachable!(
            "ProviderConfig::validate() rejects providers without body_template \
              during launch preparation before bootstrap and provider launch. \
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
    ctx.insert(
        "messages".to_string(),
        Value::Array(converted_messages.to_vec()),
    );
    ctx.insert("tools".to_string(), tools_val.clone());
    ctx.insert("stream".to_string(), Value::Bool(true));
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

    // Apply the execution ceiling after every authored request-body mutation.
    // Only the signed descriptor selects the path; provider/model identity is
    // never consulted. Nothing may overwrite the effective value afterward.
    apply_declared_output_limit(
        &mut body,
        provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.output_limit.as_ref()),
        max_output_tokens,
    )?;

    Ok(body)
}

fn apply_declared_output_limit(
    body: &mut Value,
    config: Option<&crate::directive::OutputLimitConfig>,
    hard_limit: Option<u64>,
) -> Result<()> {
    use ryeos_runtime::template::resolve_path;

    let Some(config) = config else {
        return Ok(());
    };
    // No runtime ceiling means "do not mutate". Some protocols require an
    // authored request limit, so removing a template/body default here would
    // turn a disabled RyeOS ceiling into a malformed provider request.
    let Some(hard_limit) = hard_limit else {
        return Ok(());
    };
    let effective = match resolve_path(body, &config.path) {
        None | Some(Value::Null) => hard_limit,
        Some(existing) => existing
            .as_u64()
            .ok_or_else(|| {
                anyhow!(
                    "provider output-limit field `{}` is not an unsigned integer after template rendering",
                    config.path
                )
            })?
            .min(hard_limit),
    };
    let segments = config.path.split('.').collect::<Vec<_>>();
    write_optional_object_path(body, &segments, Some(Value::Number(effective.into())))
}

fn declared_output_limit_from_body(provider: &ProviderConfig, body: &Value) -> Result<Option<u64>> {
    use ryeos_runtime::template::resolve_path;

    let Some(config) = provider
        .schemas
        .as_ref()
        .and_then(|schemas| schemas.output_limit.as_ref())
    else {
        return Ok(None);
    };
    match config.semantics {
        crate::directive::OutputLimitSemantics::ProviderNativeOutputTokens => resolve_path(
            body,
            &config.path,
        )
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                anyhow!(
                    "provider output-limit field `{}` is not an unsigned integer in the rendered request",
                    config.path
                )
            })
        })
        .transpose(),
    }
}

fn validate_usage_against_requested_limit(
    usage: Option<&mut TokenUsage>,
    requested_output_tokens: Option<u64>,
) {
    let (Some(usage), Some(requested_output_tokens)) = (usage, requested_output_tokens) else {
        return;
    };
    let Some(output_tokens) = usage.output_tokens else {
        return;
    };
    if output_tokens > requested_output_tokens {
        usage.provider_limit_contract = ProviderLimitContractStatus::ReportedAboveRequestedLimit;
        let anomaly = format!(
            "reported output_tokens {output_tokens} exceed the effective provider-native request \
             limit {requested_output_tokens}"
        );
        if !usage.contract_anomalies.contains(&anomaly) {
            usage.contract_anomalies.push(anomaly);
        }
    } else {
        usage.provider_limit_contract = ProviderLimitContractStatus::WithinRequestedLimit;
    }
}

fn merge_stream_usage_update(last_usage: &mut Option<TokenUsage>, update: &UsageUpdate) {
    let usage = last_usage.get_or_insert_with(TokenUsage::default);
    if usage.source == ProviderUsageSource::Unknown {
        usage.source = ProviderUsageSource::ProtocolEvent;
    }
    usage.snapshots_seen = usage.snapshots_seen.saturating_add(1);
    usage.anomalies.extend(update.anomalies.iter().cloned());
    merge_stream_usage_counter(
        "input_tokens",
        &mut usage.input_tokens,
        update.input_tokens,
        &mut usage.anomalies,
    );
    merge_stream_usage_counter(
        "output_tokens",
        &mut usage.output_tokens,
        update.output_tokens,
        &mut usage.anomalies,
    );
    merge_stream_usage_counter(
        "reasoning_tokens",
        &mut usage.reasoning_tokens,
        update.reasoning_tokens,
        &mut usage.anomalies,
    );
}

fn protocol_usage_u64(usage: &Value, field: &str, anomalies: &mut Vec<String>) -> Option<u64> {
    let value = usage.get(field)?;
    match value.as_u64() {
        Some(value) => Some(value),
        None => {
            anomalies.push(format!("{field} is not a u64"));
            None
        }
    }
}

fn merge_stream_usage_counter(
    label: &str,
    current: &mut Option<u64>,
    update: Option<u64>,
    anomalies: &mut Vec<String>,
) {
    let Some(update) = update else {
        return;
    };
    if let Some(previous) = *current {
        if update < previous {
            anomalies.push(format!(
                "{label} regressed from {previous} to {update} in streamed usage metadata"
            ));
        }
    }
    *current = Some(update);
}

fn write_optional_object_path(
    root: &mut Value,
    segments: &[&str],
    value: Option<Value>,
) -> Result<()> {
    let Some((last, parents)) = segments.split_last() else {
        return Err(anyhow!("provider output-limit path is empty"));
    };
    let mut cursor = root;
    for segment in parents {
        if cursor.get(*segment).is_none() {
            cursor[*segment] = json!({});
        }
        cursor = cursor.get_mut(*segment).ok_or_else(|| {
            anyhow!("provider output-limit path segment `{segment}` is unavailable")
        })?;
        if !cursor.is_object() {
            return Err(anyhow!(
                "provider output-limit path segment `{segment}` is not an object"
            ));
        }
    }
    let object = cursor.as_object_mut().ok_or_else(|| {
        anyhow!("provider request body is not an object at the output-limit path")
    })?;
    match value {
        Some(value) => {
            object.insert((*last).to_string(), value);
        }
        None => {
            object.remove(*last);
        }
    }
    Ok(())
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
    use std::result::Result;

    #[test]
    fn per_turn_output_byte_cap_allows_disabled_under_cap_and_exact_cap() {
        enforce_per_turn_output_byte_cap(0, 0, 1_000_000, 0, 0).unwrap();
        enforce_per_turn_output_byte_cap(37, 0, 36, 0, 3).unwrap();
        enforce_per_turn_output_byte_cap(36, 0, 36, 0, 3).unwrap();
    }

    #[test]
    fn per_turn_output_byte_cap_trips_only_above_boundary() {
        let err = enforce_per_turn_output_byte_cap(39, 0, 40, 0, 2).unwrap_err();
        assert!(err
            .to_string()
            .contains("local per-turn output byte cap exceeded"));
        assert!(err.to_string().contains("40 bytes > cap 39 bytes"));
        assert!(err.downcast_ref::<ProviderStreamError>().is_none());
        let limit = err
            .downcast_ref::<LocalOutputByteLimitError>()
            .expect("typed local limit error");
        assert_eq!(limit.accepted_bytes, 0);
        assert_eq!(limit.prospective_bytes, 40);
        assert_eq!(limit.cap_bytes, 39);
        assert_eq!(limit.accepted_output_events, 0);
        assert_eq!(limit.live_output_events_emitted, 2);
        assert!(limit.usage.is_none());
    }

    #[test]
    fn stream_frame_limit_allows_disabled_and_equality_but_rejects_excess() {
        assert!(!stream_frame_exceeds(0, usize::MAX));
        assert!(!stream_frame_exceeds(32, 32));
        assert!(stream_frame_exceeds(32, 33));
    }

    #[test]
    fn mixed_sse_line_endings_choose_the_earliest_logical_delimiter() {
        let buffer = concat!(
            "data: {\"first\":true}\r\n\r\n",
            "data: {\"second\":true}\n\n",
        );
        let (index, width) = next_sse_delimiter(buffer).expect("first delimiter");
        assert_eq!(&buffer[..index], "data: {\"first\":true}");
        assert_eq!(width, 4);

        let remaining = &buffer[index + width..];
        let (index, width) = next_sse_delimiter(remaining).expect("second delimiter");
        assert_eq!(&remaining[..index], "data: {\"second\":true}");
        assert_eq!(width, 2);
    }

    #[test]
    fn cumulative_channels_emit_utf8_safe_suffixes_and_reject_regression() {
        let mut events = Vec::new();
        let mut state = HashMap::new();
        emit_cumulative_channel(
            &mut events,
            &mut state,
            "reasoning",
            "reasoning",
            "思考",
            StreamEvent::ReasoningDelta,
        );
        emit_cumulative_channel(
            &mut events,
            &mut state,
            "reasoning",
            "reasoning",
            "思考中",
            StreamEvent::ReasoningDelta,
        );
        emit_cumulative_channel(
            &mut events,
            &mut state,
            "reasoning",
            "reasoning",
            "別物",
            StreamEvent::ReasoningDelta,
        );

        assert!(matches!(
            &events[0],
            StreamEvent::ReasoningDelta(value) if value == "思考"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ReasoningDelta(value) if value == "中"
        ));
        assert!(matches!(
            &events[2],
            StreamEvent::Warning { code, .. } if code == "cumulative_content_regression"
        ));
    }

    #[test]
    fn provider_usage_is_not_an_input_to_local_output_enforcement() {
        let mut meter = StreamOutputMeter::default();
        meter
            .accept(&StreamEvent::Delta("x".repeat(3_910)), 32_768 * 4, 0)
            .unwrap();
        meter
            .accept(
                &StreamEvent::Usage(UsageUpdate {
                    output_tokens: Some(65_536),
                    ..UsageUpdate::default()
                }),
                32_768 * 4,
                1,
            )
            .unwrap();
        assert_eq!(meter.total_bytes(), 3_910);
    }

    #[test]
    fn usage_above_schema_declared_request_limit_is_accounting_error_not_local_cap() {
        let mut usage = TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(65_536),
            ..TokenUsage::default()
        };

        validate_usage_against_requested_limit(Some(&mut usage), Some(32_768));

        assert!(usage.is_valid());
        assert!(usage.contract_anomalies.iter().any(|anomaly| {
            anomaly.contains("reported output_tokens 65536")
                && anomaly.contains("request limit 32768")
        }));
    }

    #[test]
    fn incident_sse_usage_is_settleable_and_never_drives_local_cap() {
        let text = "x".repeat(3_910);
        let content_frame = format!(
            "data: {}\n\n",
            json!({
                "id": "response-1",
                "usage": null,
                "error": null,
                "choices": [{
                    "delta": {"content": text},
                    "finish_reason": "stop"
                }]
            })
        );
        let usage_frame = format!(
            "data: {}\n\n",
            json!({
                "id": "response-1",
                "choices": [],
                "usage": {"prompt_tokens": 100, "completion_tokens": 65_536}
            })
        );
        let streaming = crate::directive::StreamingConfig {
            mode: Some(crate::directive::StreamingMode::DeltaMerge),
            paths: None,
            metadata: Some(crate::directive::StreamMetadataConfig {
                usage: Some(crate::directive::StreamUsageConfig {
                    path: "usage".into(),
                    input_tokens_path: Some("prompt_tokens".into()),
                    output_tokens_path: Some("completion_tokens".into()),
                    reasoning_tokens_path: None,
                    reported_cost_path: None,
                    reported_cost_unit: None,
                    cost_details_path: None,
                    is_byok_path: None,
                    reasoning_included_in_output: false,
                    aggregation: crate::directive::UsageAggregation::LatestSnapshot,
                    single_snapshot: true,
                }),
                finish_reason_path: Some("choices.0.finish_reason".into()),
                error: Some(crate::directive::StreamErrorConfig {
                    path: "error".into(),
                    message_path: "message".into(),
                    finish_reasons: vec!["error".into()],
                    code_path: Some("code".into()),
                    metadata_path: Some("metadata".into()),
                }),
                response_id_path: Some("id".into()),
                generation_id_header: None,
            }),
        };
        let mut usage = None;
        let mut finish = None;
        let mut response_id = None;
        let mut parser_state = HashMap::new();
        let mut meter = StreamOutputMeter::default();

        for frame in [&content_frame, &usage_frame] {
            harvest_chunk_meta(
                frame,
                &mut usage,
                &mut finish,
                &mut response_id,
                Some(&streaming),
            );
            assert!(provider_reported_error(
                frame,
                streaming
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.error.as_ref()),
                usage.clone(),
                None,
                response_id.clone(),
                &meter,
                0,
                Some(32_768),
            )
            .unwrap()
            .is_none());
            validate_usage_against_requested_limit(usage.as_mut(), Some(32_768));
            for event in
                parse_sse_events_with_state(frame, Some("delta_merge"), None, &mut parser_state)
            {
                meter.accept(&event, 131_072, 0).unwrap();
            }
        }

        assert_eq!(meter.total_bytes(), 3_910);
        let usage = usage.expect("provider usage retained");
        assert_eq!(usage.complete_token_counts(), Some((100, 65_536)));
        assert!(usage.is_valid(), "contract anomaly remains settleable");
        assert_eq!(usage.source, ProviderUsageSource::SignedMetadata);
        assert_eq!(
            usage.comparability,
            crate::provider_adapter::http::UsageComparability::NotComparable
        );
        assert_eq!(
            usage.provider_limit_contract,
            ProviderLimitContractStatus::ReportedAboveRequestedLimit
        );
        assert_eq!(usage.contract_anomalies.len(), 1);
        assert_eq!(finish.as_deref(), Some("stop"));
        assert_eq!(response_id.as_deref(), Some("response-1"));
    }

    #[test]
    fn null_declared_error_is_absent() {
        let block = "data: {\"error\":null,\"choices\":[]}\n\n";
        let config = crate::directive::StreamErrorConfig {
            path: "error".into(),
            message_path: "message".into(),
            finish_reasons: vec!["error".into()],
            code_path: Some("code".into()),
            metadata_path: Some("metadata".into()),
        };
        assert!(provider_reported_error(
            block,
            Some(&config),
            None,
            None,
            None,
            &StreamOutputMeter::default(),
            0,
            None,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn anthropic_split_usage_events_form_complete_accounting() {
        let data = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":9}}\n\n",
        );
        let events = parse_sse_events(data, Some("event_typed"));
        let mut usage = None;
        for event in &events {
            if let StreamEvent::Usage(update) = event {
                merge_stream_usage_update(&mut usage, update);
            }
        }
        let usage = usage.expect("merged Anthropic usage");
        assert_eq!(usage.complete_token_counts(), Some((12, 9)));
        assert_eq!(usage.snapshots_seen, 2);
        assert!(usage.is_valid());
    }

    #[test]
    fn protocol_usage_preserves_malformed_present_fields_as_anomalies() {
        let events = parse_sse_events(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":\"bad\",\"completion_tokens\":5}}\n\n",
            Some("delta_merge"),
        );
        let mut usage = None;
        for event in &events {
            if let StreamEvent::Usage(update) = event {
                merge_stream_usage_update(&mut usage, update);
            }
        }
        let usage = usage.expect("malformed-present usage snapshot");
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.snapshots_seen, 1);
        assert!(usage
            .anomalies
            .iter()
            .any(|anomaly| anomaly.contains("prompt_tokens is not a u64")));
    }

    #[test]
    fn malformed_logical_sse_payload_is_a_protocol_error() {
        let events = parse_sse_events("data: {not-json}\n\n", Some("delta_merge"));
        let error = reject_malformed_sse_events(
            &events,
            &StreamOutputMeter::default(),
            0,
            None,
            None,
            None,
            None,
        )
        .expect_err("malformed logical payload must fail");
        assert!(error
            .downcast_ref::<ProviderProtocolStreamError>()
            .is_some());
        assert!(error.to_string().contains("provider_protocol_error"));
    }

    #[test]
    fn declared_top_level_error_is_typed_and_keeps_attempt_diagnostics() {
        let mut meter = StreamOutputMeter::default();
        meter
            .accept(&StreamEvent::Delta("partial".to_string()), 100, 0)
            .unwrap();
        let block = concat!(
            "data: {\"id\":\"gen-body-1\",\"error\":{",
            "\"code\":\"provider_error\",\"message\":\"upstream failed\",",
            "\"metadata\":{\"provider_name\":\"example\"}},",
            "\"choices\":[{\"finish_reason\":\"error\"}]}\n\n",
        );
        let config = crate::directive::StreamErrorConfig {
            path: "error".into(),
            message_path: "message".into(),
            finish_reasons: vec!["error".into()],
            code_path: Some("code".into()),
            metadata_path: Some("metadata".into()),
        };
        let error = provider_reported_error(
            block,
            Some(&config),
            Some(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..TokenUsage::default()
            }),
            Some("gen-header-1".to_string()),
            Some("gen-body-1".to_string()),
            &meter,
            1,
            Some(32_768),
        )
        .unwrap()
        .expect("top-level provider error must be recognized");

        assert_eq!(error.code.as_deref(), Some("provider_error"));
        assert_eq!(error.message, "upstream failed");
        assert_eq!(error.accepted_bytes, 7);
        assert_eq!(error.accepted_output_events, 1);
        assert_eq!(error.live_output_events_emitted, 1);
        assert_eq!(error.generation_header_id.as_deref(), Some("gen-header-1"));
        assert_eq!(error.response_id.as_deref(), Some("gen-body-1"));
        assert_eq!(error.requested_output_tokens, Some(32_768));
        assert_eq!(error.usage.unwrap().output_tokens, Some(2));
    }

    #[test]
    fn output_meter_counts_tool_arguments_once_across_partial_and_terminal_events() {
        let mut meter = StreamOutputMeter::default();
        let partial = StreamEvent::ToolUsePartial {
            id: Some("call-1".to_string()),
            name: "directive_return".to_string(),
            stream_key: "test:call-1".to_string(),
            delta: "{\"x\"".to_string(),
            total_len: 4,
        };
        meter.accept(&partial, 10, 0).unwrap();
        assert_eq!(meter.total_bytes(), 4);

        let terminal = StreamEvent::ToolUse {
            id: Some("call-1".to_string()),
            name: "directive_return".to_string(),
            stream_key: "test:call-1".to_string(),
            argument_bytes: 42,
            arguments: json!({"x": "longer normalized representation"}),
            malformed_args: None,
        };
        meter.accept(&terminal, 10, 1).unwrap();
        assert_eq!(
            meter.total_bytes(),
            4,
            "terminal assembly is not re-counted"
        );

        let over = StreamEvent::ToolUsePartial {
            id: Some("call-1".to_string()),
            name: "directive_return".to_string(),
            stream_key: "test:call-1".to_string(),
            delta: "too much".to_string(),
            total_len: 11,
        };
        assert!(meter.accept(&over, 10, 2).is_err());
        assert_eq!(meter.total_bytes(), 4, "rejected fragment is not committed");
    }

    #[test]
    fn delta_merge_tool_argument_fragments_trip_cap_before_publication() {
        let data = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"id\":\"call-1\",\"function\":{\"name\":\"search\",",
            "\"arguments\":\"{\\\"query\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"abcdef\\\"}\"}}]},",
            "\"finish_reason\":null}]}\n\n",
        );
        let mut state = HashMap::new();
        let events = parse_sse_events_with_state(data, Some("delta_merge"), None, &mut state);
        let partials = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ToolUsePartial { .. }))
            .collect::<Vec<_>>();
        assert_eq!(partials.len(), 2, "every raw argument fragment is metered");

        let (first_len, final_len) = match (partials[0], partials[1]) {
            (
                StreamEvent::ToolUsePartial {
                    total_len: first, ..
                },
                StreamEvent::ToolUsePartial {
                    total_len: final_len,
                    ..
                },
            ) => (*first, *final_len),
            _ => unreachable!(),
        };
        assert!(final_len > first_len);

        let mut meter = StreamOutputMeter::default();
        meter
            .accept(partials[0], (final_len - 1) as u64, 0)
            .unwrap();
        let error = meter
            .accept(partials[1], (final_len - 1) as u64, 1)
            .unwrap_err();
        let limit = error
            .downcast_ref::<LocalOutputByteLimitError>()
            .expect("typed local byte limit");
        assert_eq!(limit.accepted_bytes, first_len as u64);
        assert_eq!(limit.prospective_bytes, final_len as u64);
        assert_eq!(meter.total_bytes(), first_len);
    }

    #[test]
    fn local_limit_diagnostic_can_retain_usage_and_response_provenance() {
        let error = enforce_per_turn_output_byte_cap(3, 0, 4, 0, 0).unwrap_err();
        let usage = TokenUsage {
            input_tokens: Some(11),
            output_tokens: Some(2),
            ..TokenUsage::default()
        };
        let error = enrich_local_output_limit(
            error,
            Some(&usage),
            Some("generation-1"),
            Some("response-1"),
        );
        let limit = error
            .downcast_ref::<LocalOutputByteLimitError>()
            .expect("typed enriched local limit");
        assert_eq!(
            limit
                .usage
                .as_ref()
                .and_then(|usage| usage.complete_token_counts()),
            Some((11, 2))
        );
        assert_eq!(limit.generation_header_id.as_deref(), Some("generation-1"));
        assert_eq!(limit.response_id.as_deref(), Some("response-1"));
    }

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
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse {
            id,
            name,
            arguments,
            ..
        } = &tool_uses[0]
        {
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
        let deltas: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Delta(_)))
            .collect();
        let dones: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Finish { .. }))
            .collect();
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], StreamEvent::Delta(s) if s == "Hello"));
        assert!(matches!(&deltas[1], StreamEvent::Delta(s) if s == " world"));
        assert!(!dones.is_empty());
    }

    #[test]
    fn sse_delta_merge_local_openai_includes_usage() {
        // Conformance for the `local-openai` provider: a local
        // OpenAI-compatible server (vLLM/llama.cpp) with
        // `stream_options.include_usage: true` streams content deltas,
        // then a final usage-only chunk (empty `choices`), then [DONE].
        // The adapter must accumulate the text AND surface a Usage event
        // so graph cost accounting receives token counts even offline.
        let data = r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"2+2"}}]}

data: {"choices":[{"index":0,"delta":{"content":" = 4"}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}}

data: [DONE]
"#;
        let events = parse_sse_events(data, Some("delta_merge"));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "2+2 = 4");

        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u),
                _ => None,
            })
            .expect("a Usage event from the include_usage final chunk");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(8));
    }

    #[test]
    fn sse_delta_merge_tool_calls() {
        let data = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}
"#;
        let events = parse_sse_events(data, Some("delta_merge"));
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_uses.len(), 1);
        if let StreamEvent::ToolUse {
            id,
            name,
            arguments,
            ..
        } = &tool_uses[0]
        {
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
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_uses.len(), 1, "must emit exactly one ToolUse");
        if let StreamEvent::ToolUse {
            id,
            name,
            arguments,
            ..
        } = &tool_uses[0]
        {
            assert_eq!(id, &Some("call_abc".to_string()));
            assert_eq!(name, "search");
            assert_eq!(arguments, &json!({"q": "rust"}));
        }
        let dones: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Finish { .. }))
            .collect();
        assert_eq!(dones.len(), 1, "must emit Finish after tool calls");
        // ToolUse must come before Finish.
        let tu_idx = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .unwrap();
        let done_idx = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Finish { .. }))
            .unwrap();
        assert!(tu_idx < done_idx, "ToolUse must precede Finish");
    }

    #[test]
    fn done_sentinel() {
        let data = "data: [DONE]\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(data, Some("delta_merge"));
        assert!(events.is_empty(), "[DONE] is not a semantic finish reason");
    }

    #[test]
    fn done_sentinel_does_not_overwrite_length_finish_reason() {
        let data = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = parse_sse_events(data, Some("delta_merge"));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::Finish {
                reason: FinishReason::Length,
                ..
            }
        ));
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
        assert!(
            span.is_some(),
            "expected provider:parse_sse span, got: {:?}",
            spans
                .iter()
                .map(|s: &ryeos_tracing::test::RecordedSpan| &s.name)
                .collect::<Vec<_>>()
        );
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
        inject_sampling(
            &mut body,
            ProtocolFamily::AnthropicMessages,
            Some(&sampling),
        );
        assert_eq!(body["temperature"].as_f64(), Some(0.7));
        assert!(
            body.get("seed").is_none(),
            "anthropic body must not contain seed"
        );
    }

    #[test]
    fn sampling_injection_google_omits_seed() {
        let mut body = json!({"model": "gemini-pro", "messages": []});
        let sampling = SamplingConfig {
            temperature: Some(0.5),
            seed: Some(1),
        };
        inject_sampling(
            &mut body,
            ProtocolFamily::GoogleGenerateContent,
            Some(&sampling),
        );
        assert_eq!(body["temperature"].as_f64(), Some(0.5));
        assert!(
            body.get("seed").is_none(),
            "google body must not contain seed"
        );
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
        let body = super::build_request_body(
            &provider,
            "gemini-3-flash",
            &msgs,
            None,
            &json!([]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(body["model_pinned"], "gemini-3-flash");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        // No deprecated keys when template is used.
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
        let body = super::build_request_body(
            &provider,
            "gemini-3-flash",
            &[],
            None,
            &json!([]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
        assert!(body["contents"].is_array());
    }

    #[test]
    fn build_request_body_exposes_limit_to_authoritative_provider_template() {
        let provider = ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://example.com".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: Some(crate::directive::SchemasConfig {
                accounting: None,
                messages: None,
                tools: None,
                streaming: None,
                output_limit: Some(crate::directive::OutputLimitConfig {
                    path: "max_tokens".into(),
                    semantics: crate::directive::OutputLimitSemantics::ProviderNativeOutputTokens,
                }),
            }),
            pricing: None,
            extra: Default::default(),
            body_template: Some(json!({
                "messages": "{messages}",
            })),
            body_extra: None,
            profiles: vec![],
        };

        let rendered = super::build_request_body(
            &provider,
            "test-model",
            &[],
            None,
            &json!([]),
            false,
            Some(32_768),
        )
        .unwrap();
        assert_eq!(rendered["max_tokens"], 32_768);
        assert_eq!(
            declared_output_limit_from_body(&provider, &rendered).unwrap(),
            Some(32_768)
        );

        let mut lower_provider = provider.clone();
        lower_provider.body_extra = Some(json!({"max_tokens": 4_096}));
        let lower = super::build_request_body(
            &lower_provider,
            "test-model",
            &[],
            None,
            &json!([]),
            false,
            Some(32_768),
        )
        .unwrap();
        assert_eq!(lower["max_tokens"], 4_096);
        assert_eq!(
            declared_output_limit_from_body(&lower_provider, &lower).unwrap(),
            Some(4_096)
        );

        let mut authored_override = provider.clone();
        authored_override.body_extra = Some(json!({"max_tokens": 65_536}));
        let overridden = super::build_request_body(
            &authored_override,
            "test-model",
            &[],
            None,
            &json!([]),
            false,
            Some(32_768),
        )
        .unwrap();
        assert_eq!(overridden["max_tokens"], 32_768);

        let omitted =
            super::build_request_body(&provider, "test-model", &[], None, &json!([]), false, None)
                .unwrap();
        assert!(omitted.get("max_tokens").is_none());

        let mut authored_without_runtime_ceiling = provider.clone();
        authored_without_runtime_ceiling.body_extra = Some(json!({"max_tokens": 8_192}));
        let preserved = super::build_request_body(
            &authored_without_runtime_ceiling,
            "test-model",
            &[],
            None,
            &json!([]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(preserved["max_tokens"], 8_192);
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
        let _ =
            super::build_request_body(&provider, "test-model", &[], None, &json!([]), false, None);
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
        let body = super::build_request_body(
            &provider,
            "gemini-3-flash",
            &msgs,
            None,
            &json!([]),
            false,
            None,
        )
        .unwrap();
        assert!(
            body.get("tools").is_none(),
            "Gemini empty-tools must omit the field entirely, got: {:?}",
            body.get("tools")
        );
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
                accounting: None,
                output_limit: None,
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
                accounting: None,
                output_limit: None,
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

        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
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
        let combined: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            combined, "Hello, world",
            "multi-part frame text must concatenate correctly; got: {combined:?}"
        );
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
        let combined: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            combined, "Hi there, how are you?",
            "cumulative multi-part frames must produce the full text exactly once"
        );
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

        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .collect();
        assert_eq!(
            tool_uses.len(),
            1,
            "cumulative frame must not duplicate tool calls"
        );
    }

    // ── Real golden-wire tests ────────────────────────────────────────
    //
    // These tests load the ACTUAL signed bundled provider YAML through
    // VerifiedLoader, resolve the matching profile from the prepared provider snapshot,
    // and build the request body. They catch:
    //   - Bundled YAML signature / parse failures
    //   - Profile-match resolution regressions
    //   - Trust-policy violations in shipped configs
    //   - Request-shape drift (placement enums, tool serialization,
    //     system-prompt placement)
    //
    // If a bundle YAML edit breaks one of these, CI catches it here.

    use ryeos_directive_core::{
        ProviderConfigSource, ResolvedProviderSnapshot, SnapshotItemSpace, SnapshotTrustClass,
    };
    use ryeos_runtime::verified_loader::VerifiedLoader;

    /// Path to the bundled standard root in the workspace.
    fn bundle_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .join("bundles/standard")
    }

    /// Dev publisher public key trust entry — the standard bundle's configs
    /// are signed with this key, so tests that load real bundle configs need
    /// it in the trust store.  This is public key material (not a secret).
    const DEV_PUBLISHER_TRUST_TOML: &str = r#"
public_key = "ed25519:sDKyQ9rFxIduNjGtXq6aTrLlAg39177NzCT1+YYqpRk="
fingerprint = "741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea"
owner = "ryeos-dev"
"#;

    /// Build a VerifiedLoader rooted at the bundled standard, with a temp
    /// project root that holds only the dev-publisher trust entry (no
    /// project config), so project-overlay attacks can't interfere while
    /// the trust store still verifies the bundle's signed configs.
    fn loader_for_bundle() -> VerifiedLoader {
        let project_root = tempfile::tempdir().expect("create temp project root");
        let trust_dir = project_root.path().join(".ai/config/keys/trusted");
        let _ = std::fs::create_dir_all(&trust_dir);
        std::fs::write(
            trust_dir.join("dev-publisher.toml"),
            DEV_PUBLISHER_TRUST_TOML,
        )
        .expect("write trust entry");

        VerifiedLoader::new(
            project_root.path().to_path_buf(),
            vec![bundle_root()],
            std::path::Path::new("/nonexistent-operator-trust"),
        )
    }

    /// Resolve a provider+model through the real strict loader for adapter fixtures.
    fn resolve(provider: &str, model: &str) -> ResolvedProviderSnapshot {
        let loader = loader_for_bundle();
        let config_id = format!("ryeos-runtime/model-providers/{provider}");
        let base = loader
            .load_config_strict::<ProviderConfig>(&config_id)
            .unwrap_or_else(|e| panic!("provider load failed for {provider}/{model}: {e:#}"))
            .unwrap_or_else(|| panic!("provider config missing for {provider}"));
        let matched_profile = base
            .matched_profile(model)
            .map(|profile| profile.name.clone());
        let resolved = base.resolve_for_model(model);
        resolved
            .validate(&format!(" for test model {model}"))
            .unwrap_or_else(|e| panic!("provider validation failed for {provider}/{model}: {e:#}"));
        let config_hash = ResolvedProviderSnapshot::compute_hash(&resolved)
            .expect("resolved provider config is serializable");
        ResolvedProviderSnapshot {
            provider_id: provider.to_string(),
            model_name: model.to_string(),
            context_window: 128_000,
            sampling: None,
            matched_profile,
            config_value_digest: "0".repeat(64),
            config_sources: vec![ProviderConfigSource {
                space: SnapshotItemSpace::Bundle,
                root_label: "bundle:standard".to_string(),
                canonical_id: config_id,
                content_digest: "0".repeat(64),
                trust_class: SnapshotTrustClass::TrustedBundle,
            }],
            config_hash,
            provider: resolved,
        }
    }

    /// Helper: build the full request body from a resolved snapshot.
    fn build_golden_body(
        snapshot: &ResolvedProviderSnapshot,
        messages: &[ProviderMessage],
        tools: &[ToolSchema],
        max_output_tokens: Option<u64>,
    ) -> Value {
        let schemas = snapshot
            .provider
            .schemas
            .as_ref()
            .and_then(|s| s.messages.as_ref());
        let (converted, system_prompt) =
            crate::provider_adapter::messages::convert_messages(messages, &schemas.cloned());
        let tool_schema = snapshot
            .provider
            .schemas
            .as_ref()
            .and_then(|s| s.tools.clone());
        let tools_val = crate::provider_adapter::tools::serialize_tools(tools, &tool_schema);
        super::build_request_body(
            &snapshot.provider,
            &snapshot.model_name,
            &converted,
            system_prompt.as_deref(),
            &tools_val,
            !tools.is_empty(),
            max_output_tokens,
        )
        .unwrap()
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
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("What is 2+2?")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
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
                reasoning_content: None,
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(json!("4")),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("Thanks.")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ]
    }

    fn canonical_transcript_no_tools() -> Vec<ProviderMessage> {
        vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hi")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }]
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
            ("openrouter", "anthropic/claude-sonnet-4.6"),
            ("zen", "gpt-5.4-nano"),
            ("local-openai", "local-test-model"),
        ] {
            let snapshot = resolve(provider, model);
            assert_eq!(snapshot.provider_id, provider);
            assert!(
                !snapshot.config_hash.is_empty(),
                "config_hash must be populated for {provider}"
            );
        }
    }

    #[test]
    fn provider_validation_rejects_removed_template_and_implicit_protocol_contracts() {
        let provider = resolve("openai", "gpt-4o-mini").provider;

        let mut removed_template_placeholder = provider.clone();
        removed_template_placeholder.body_template = Some(json!({
            "model": "{model}",
            "messages": "{messages}",
            "stream": "{stream}",
            "max_tokens": "{max_tokens}",
        }));
        let error = removed_template_placeholder
            .validate(" for removed placeholder test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("placeholder `{max_tokens}` is not supported"));

        let mut missing_mode = provider.clone();
        missing_mode
            .schemas
            .as_mut()
            .expect("bundled provider schemas")
            .streaming
            .as_mut()
            .expect("bundled provider streaming")
            .mode = None;
        let error = missing_mode
            .validate(" for missing mode test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("schemas.streaming.mode is required"));

        let mut missing_output_limit = provider;
        missing_output_limit
            .schemas
            .as_mut()
            .expect("bundled provider schemas")
            .output_limit = None;
        let error = missing_output_limit
            .validate(" for missing output limit test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("schemas.output_limit is required"));
    }

    #[test]
    fn bundled_openrouter_sends_runtime_limit_and_prices_exact_model_id() {
        let model = "anthropic/claude-sonnet-4.6";
        let snapshot = resolve("openrouter", model);
        let messages = canonical_transcript_no_tools();
        let schemas = snapshot
            .provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.messages.as_ref());
        let (converted, system_prompt) =
            crate::provider_adapter::messages::convert_messages(&messages, &schemas.cloned());
        let body = super::build_request_body(
            &snapshot.provider,
            model,
            &converted,
            system_prompt.as_deref(),
            &json!([]),
            false,
            Some(32_768),
        )
        .unwrap();

        assert_eq!(body["max_tokens"], 32_768);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let pricing = snapshot
            .provider
            .pricing
            .as_ref()
            .and_then(|pricing| pricing.models.get(model))
            .expect("routed OpenRouter model must have exact pricing");
        assert!(pricing.input_per_million > 0.0);
        assert!(pricing.output_per_million > 0.0);
    }

    struct StreamingEventRecorder {
        events: std::sync::Mutex<Vec<(String, Value)>>,
    }

    impl StreamingEventRecorder {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn event_count(&self, event_type: &str) -> usize {
            self.events
                .lock()
                .expect("event recorder lock")
                .iter()
                .filter(|(actual, _)| actual == event_type)
                .count()
        }
    }

    #[async_trait::async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for StreamingEventRecorder {
        async fn dispatch_action(
            &self,
            _request: ryeos_runtime::callback::DispatchActionRequest,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(
            &self,
            _thread_id: &str,
            _pid: u32,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(
            &self,
            _thread_id: &str,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _thread_id: &str,
            _completion: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(
            &self,
            _thread_id: &str,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(Value::Null)
        }
        async fn request_continuation(
            &self,
            _thread_id: &str,
            _log_reason: Option<&str>,
            _completion: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(Value::Null)
        }
        async fn append_event(
            &self,
            _thread_id: &str,
            event_type: &str,
            payload: Value,
            _storage_class: &str,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            self.events
                .lock()
                .expect("event recorder lock")
                .push((event_type.to_string(), payload));
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _thread_id: &str,
            _events: Vec<Value>,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(
            &self,
            _params: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({"events": [], "next_cursor": null}))
        }
        async fn bundle_events_append(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_scan(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn vault_put(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(Value::Null)
        }
        async fn vault_delete(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!([]))
        }
        async fn claim_commands(
            &self,
            _thread_id: &str,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({"commands": []}))
        }
        async fn complete_command(
            &self,
            _thread_id: &str,
            _command_id: i64,
            _status: &str,
            _result: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(
            &self,
            _thread_id: &str,
            _artifact: Value,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(
            &self,
            _thread_id: &str,
        ) -> Result<Value, ryeos_runtime::callback::CallbackError> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn real_streaming_call_accepts_incident_shape_without_false_token_cap() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let address = listener.local_addr().expect("mock provider address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0u8; 16 * 1024];
            let _ = socket.read(&mut request).await.expect("read request");

            let mut body = String::new();
            body.push_str("data: {\"usage\":null,\"error\":null,\"choices\":[]}\n\n");
            for index in 0..56 {
                let width = if index == 55 { 115 } else { 69 };
                body.push_str(&format!(
                    "data: {{\"usage\":null,\"error\":null,\"choices\":[{{\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\n",
                    "x".repeat(width)
                ));
            }
            body.push_str(concat!(
                "data: {\"id\":\"response-incident\",\"error\":null,",
                "\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
                "\"usage\":{\"prompt_tokens\":394276,\"completion_tokens\":65536,",
                "\"completion_tokens_details\":{\"reasoning_tokens\":60000},",
                "\"cost\":0.25,\"is_byok\":false}}\n\n"
            ));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Generation-Id: generation-incident\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let mut snapshot = resolve("openrouter", "anthropic/claude-sonnet-4.6");
        snapshot.provider.base_url = format!("http://{address}");
        snapshot.provider.auth.env_var = None;
        snapshot
            .provider
            .extra
            .insert("stream_url".to_string(), json!(""));

        let recorder = Arc::new(StreamingEventRecorder::new());
        let callback =
            CallbackClient::from_inner(recorder.clone(), "T-incident", "/project", "tat-test");
        let execution = ExecutionConfig {
            timeout_seconds: 5,
            max_provider_output_tokens_per_turn: 32_768,
            max_stream_output_bytes_per_turn: 3_910,
            ..ExecutionConfig::default()
        };
        let messages = vec![ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hypothesize")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }];

        let outcome = call_provider_streaming(StreamingCallInput {
            client: &reqwest::Client::new(),
            provider: &snapshot.provider,
            provider_id: "opaque-test-route",
            matched_profile: snapshot.matched_profile.as_deref(),
            config_hash: &snapshot.config_hash,
            execution: &execution,
            model: &snapshot.model_name,
            messages: &messages,
            tools: &[],
            callback: &callback,
            turn: 15,
            sampling: None,
            cancel_flag: None,
            interrupt_flag: None,
        })
        .await
        .expect("incident-shaped stream must complete");
        server.await.expect("mock provider task");

        let StreamOutcome::Completed { response, .. } = outcome else {
            panic!("provider stream did not complete");
        };
        assert_eq!(response.observed_output.text_bytes, 3_910);
        assert_eq!(response.observed_output.accepted_output_events, 56);
        assert_eq!(response.requested_output_tokens, Some(32_768));
        let usage = response.usage.expect("provider-native usage retained");
        assert_eq!(usage.output_tokens, Some(65_536));
        assert_eq!(usage.reasoning_tokens, Some(60_000));
        assert_eq!(
            usage.provider_limit_contract,
            ProviderLimitContractStatus::ReportedAboveRequestedLimit
        );
        assert!(usage.is_valid());
        assert_eq!(recorder.event_count("cognition_out"), 56);
    }

    #[test]
    fn bundled_openai_profile_builds_top_level_tool_calls() {
        let snapshot = resolve("openai", "gpt-4o-mini");
        assert_eq!(snapshot.provider_id, "openai");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
            Some(32_768),
        );
        assert_eq!(body["max_completion_tokens"], 32_768);
        assert_eq!(body["model"], "gpt-4o-mini");

        let messages = body["messages"].as_array().expect("messages must be array");
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have assistant message");

        // tool_calls must be a top-level array, not inline in content
        assert!(
            assistant["tool_calls"].is_array(),
            "OpenAI tool_calls must be top-level array on assistant message; got: {assistant}"
        );
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "calculator");

        // Content must be a plain string, not a block array
        assert!(
            assistant["content"].is_string(),
            "OpenAI assistant content must be plain string, got: {:?}",
            assistant["content"]
        );
    }

    #[test]
    fn bundled_anthropic_profile_builds_typed_blocks() {
        let snapshot = resolve("anthropic", "claude-sonnet-4-5-20250929");
        assert_eq!(snapshot.provider_id, "anthropic");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
            None,
        );

        // System must be a top-level body field, not in messages
        assert!(
            body["system"].is_string(),
            "Anthropic system must be top-level body field"
        );

        let messages = body["messages"].as_array().expect("messages must be array");
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have assistant message");

        // Content MUST be an array of typed blocks — no raw strings
        let content = assistant["content"]
            .as_array()
            .expect("Anthropic assistant content must be array, not raw string");
        assert!(
            content.iter().all(|b| b.is_object()),
            "Anthropic content blocks must all be objects, not raw strings"
        );
        assert!(
            content.iter().any(|b| b["type"] == "text"),
            "must contain at least one text block"
        );
        assert!(
            content.iter().any(|b| b["type"] == "tool_use"),
            "must contain at least one tool_use block"
        );
    }

    #[test]
    fn bundled_gemini_profile_omits_tools_when_empty() {
        // Gemini provider comes through zen's gemini profile.
        // zen.yaml matches gemini-* to the gemini profile.
        let snapshot = resolve("zen", "gemini-3-flash");
        let body = build_golden_body(&snapshot, &canonical_transcript_no_tools(), &[], None);
        assert!(
            body.get("tools").is_none(),
            "Gemini empty-tools must omit the field; got: {:?}",
            body.get("tools")
        );
    }

    #[test]
    fn bundled_gemini_profile_tool_result_carries_real_function_name() {
        let snapshot = resolve("zen", "gemini-3-flash");
        let body = build_golden_body(
            &snapshot,
            &canonical_transcript_with_tool_call(),
            &canonical_tool_schemas(),
            None,
        );
        let contents = body["contents"].as_array().expect("Gemini contents array");
        let function_response = contents
            .iter()
            .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
            .find(|p| p.get("functionResponse").is_some())
            .expect("must have a functionResponse part");
        assert_eq!(
            function_response["functionResponse"]["name"], "calculator",
            "functionResponse.name must be the real tool name, not empty"
        );
    }

    #[tokio::test]
    async fn flag_signal_resolves_within_250ms_of_flag_set() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        let start = tokio::time::Instant::now();

        // Spawn the flag_signal future and set the flag after 10ms
        let handle = tokio::spawn(async move {
            let opt_flag: Option<Arc<AtomicBool>> = Some(flag_clone);
            flag_signal(&opt_flag).await;
        });

        // Simulate external trigger after a short delay
        tokio::time::sleep(Duration::from_millis(10)).await;
        flag.store(true, Ordering::Relaxed);

        // flag_signal must resolve within 250ms of the flag being set
        let result = tokio::time::timeout(Duration::from_millis(250), handle).await;
        assert!(
            result.is_ok(),
            "flag_signal did not resolve within 250ms; elapsed: {}ms",
            start.elapsed().as_millis()
        );
    }

    #[test]
    fn flag_set_returns_false_when_no_flag() {
        assert!(!flag_set(&None));
    }

    #[test]
    fn flag_set_returns_true_when_flag_set() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let flag = Arc::new(AtomicBool::new(true));
        assert!(flag_set(&Some(flag)));
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
        let finish = events
            .iter()
            .find(|e| matches!(e, StreamEvent::Finish { .. }));
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
        let usage_events: Vec<_> = events
            .iter()
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
        let usage_events: Vec<_> = events
            .iter()
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
        let reasoning: Vec<_> = events
            .iter()
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
        let usage_events: Vec<_> = events
            .iter()
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
        let reasoning: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0], "reasoning step 1");
        // Also verify the visible text was emitted
        let deltas: Vec<_> = events
            .iter()
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
        let finish = events
            .iter()
            .find(|e| matches!(e, StreamEvent::Finish { .. }));
        assert!(finish.is_some());
        match finish.unwrap() {
            StreamEvent::Finish { reason, raw } => {
                assert_eq!(
                    *reason,
                    FinishReason::Stop,
                    "uppercase STOP must normalize to Stop"
                );
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
        let usage_events: Vec<_> = events
            .iter()
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
        assert_eq!(
            normalize_finish_reason(Some("end_turn")),
            FinishReason::Stop
        );
        assert_eq!(
            normalize_finish_reason(Some("tool_use")),
            FinishReason::ToolCalls
        );
        assert_eq!(
            normalize_finish_reason(Some("SAFETY")),
            FinishReason::ContentFilter
        );
        assert_eq!(
            normalize_finish_reason(Some("MAX_TOKENS")),
            FinishReason::Length
        );
    }
}
