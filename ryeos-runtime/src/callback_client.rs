use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::callback::{ReplayResponse, RuntimeCallbackAPI};
use crate::envelope::EnvelopeCallback;
use crate::events::{RuntimeEventType, StorageClass};

/// Map an event type name to the storage class the daemon's
/// `EventStoreService::validate_storage_class` accepts.
///
/// V5.5 D11: this function delegates to `RuntimeEventType::storage_class`
/// — the typed enum is the single source of truth. Unknown event names
/// fall through to `"indexed"` so the daemon validator (which also
/// delegates to the enum's `parse`) can produce the canonical error
/// message at append time. Callers that have a `RuntimeEventType`
/// already should use `append_runtime_event` directly.
pub fn storage_class_for(event_type: &str) -> &'static str {
    match RuntimeEventType::parse(event_type) {
        Ok(t) => t.storage_class().as_str(),
        Err(_) => StorageClass::Indexed.as_str(),
    }
}

/// Inline cap for tool result bodies in `tool_call_result` SSE
/// payloads. Bodies up to this size are serialized into the event
/// directly; larger bodies are persisted in the transcript and the
/// event carries `truncated:true, truncated_reason:"size_cap_exceeded"`.
///
/// 256 KiB chosen so that a render-tool envelope (single-digit KB)
/// always inlines, and a search-tool result with several MB of rows
/// stays in the transcript instead of bloating every SSE consumer's
/// event log.
pub const TOOL_RESULT_INLINE_MAX_BYTES: usize = 256 * 1024;

pub struct CallbackClient {
    inner: Option<Arc<dyn RuntimeCallbackAPI>>,
    thread_id: String,
    project_path: String,
    thread_auth_token: String,
}

impl CallbackClient {
    /// Construct from a pre-built runtime API implementation (for tests).
    pub fn from_inner(
        inner: Arc<dyn RuntimeCallbackAPI>,
        thread_id: &str,
        project_path: &str,
        thread_auth_token: &str,
    ) -> Self {
        Self {
            inner: Some(inner),
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
            thread_auth_token: thread_auth_token.to_string(),
        }
    }
}

impl Clone for CallbackClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            thread_id: self.thread_id.clone(),
            project_path: self.project_path.clone(),
            thread_auth_token: self.thread_auth_token.clone(),
        }
    }
}

impl CallbackClient {
    pub fn new(callback: &EnvelopeCallback, thread_id: &str, project_path: &str, thread_auth_token: &str) -> Self {
        let inner: Option<Arc<dyn RuntimeCallbackAPI>> = if callback.socket_path.exists() {
            Some(Arc::new(
                crate::callback_uds::UdsRuntimeClient::new(
                    callback.socket_path.clone(),
                    callback.token.clone(),
                    thread_auth_token.to_string(),
                )
            ))
        } else {
            None
        };
        tracing::info!(
            socket = %callback.socket_path.display(),
            thread_id = %thread_id,
            has_uds = callback.socket_path.exists(),
            "callback client initialized"
        );
        Self {
            inner,
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
            thread_auth_token: thread_auth_token.to_string(),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn project_path(&self) -> &str {
        &self.project_path
    }

    /// Dispatch a sub-action through the daemon's `runtime.dispatch_action`
    /// endpoint and return the typed response.
    ///
    /// The daemon contract is `CallbackDispatchResponse` (see
    /// `crate::callback_contract`). We deserialize STRICTLY — a legacy
    /// envelope (`{thread, result, data, status}`) fails loudly here
    /// rather than silently dropping fields into the model's
    /// tool-result bytes.
    ///
    /// When the callback channel is disconnected (no UDS socket), we
    /// surface that explicitly rather than fabricating an empty
    /// response: a runtime that ignored the disconnect could feed
    /// "Null" to the model and the LLM would see a tool that returned
    /// `null` instead of failing visibly.
    pub async fn dispatch_action(
        &self,
        req: crate::callback::DispatchActionRequest,
    ) -> Result<crate::callback_contract::CallbackDispatchResponse> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback dispatch_action called without an inner UDS client \
                 (socket missing); runtime cannot route to the daemon"
            )
        })?;
        let raw: Value = client
            .dispatch_action(req)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        serde_json::from_value::<crate::callback_contract::CallbackDispatchResponse>(raw)
            .map_err(|e| anyhow::anyhow!("invalid CallbackDispatchResponse from daemon: {e}"))
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn append_event(&self, event_type: &str, payload: Value) -> Result<()> {
        let storage_class = storage_class_for(event_type);
        let is_transcript = matches!(
            event_type,
            "cognition_in" | "cognition_out" | "tool_call_start" | "tool_call_result"
        );
        match &self.inner {
            Some(client) => {
                client.append_event(&self.thread_id, event_type, payload, storage_class).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None if is_transcript => Err(anyhow::anyhow!(
                "callback append_event({event_type}) called without an inner UDS client \
                 (socket missing); transcript-bearing event must not be silently dropped"
            )),
            None => Ok(()),
        }
    }

    /// V5.5 D11: typed event emitter. Prefer this over `append_event`
    /// for new code — adding a new event variant to
    /// `RuntimeEventType` makes this method emit it without any
    /// further string-based dispatch. The daemon validator delegates
    /// to the same enum, so the producer/consumer surfaces stay in
    /// lock-step.
    pub async fn append_runtime_event(
        &self,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> Result<()> {
        let storage_class = event_type.storage_class().as_str();
        match &self.inner {
            Some(client) => {
                client
                    .append_event(
                        &self.thread_id,
                        event_type.as_str(),
                        payload,
                        storage_class,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Resume-critical: must hard-fail on disconnect.
    pub async fn mark_running(&self) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback mark_running called without an inner UDS client \
                 (socket missing); cannot mark thread as running"
            )
        })?;
        client.mark_running(&self.thread_id).await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Resume-critical: must hard-fail on disconnect.
    pub async fn finalize_thread(&self, status: &str) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback finalize_thread called without an inner UDS client \
                 (socket missing); cannot finalize thread"
            )
        })?;
        client.finalize_thread(&self.thread_id, status).await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn request_continuation(&self, prompt: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.request_continuation(&self.thread_id, prompt).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn publish_artifact(&self, artifact: Value) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.publish_artifact(&self.thread_id, artifact).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_thread(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(&self.thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_thread_by_id(&self, thread_id: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    /// Resume-critical: must hard-fail on disconnect.
    pub async fn replay_events_for(&self, thread_id: &str) -> Result<ReplayResponse> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback replay_events_for called without an inner UDS client \
                 (socket missing); runtime cannot replay events for resume"
            )
        })?;
        let raw: Value = client.replay_events(thread_id).await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        serde_json::from_value::<ReplayResponse>(raw)
            .map_err(|e| anyhow::anyhow!("invalid ReplayResponse from daemon: {e}"))
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_facets(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_facets(&self.thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    // Typed event emission methods (merged from EventEmitter)

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to the validator-accepted `cognition_in` event.
    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_event("cognition_in", serde_json::json!({"turn": turn})).await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to the validator-accepted `cognition_out` event.
    pub async fn emit_turn_complete(&self, turn: u32, tokens: Option<(u64, u64)>) -> Result<()> {
        let mut data = serde_json::json!({"turn": turn});
        if let Some((input, output)) = tokens {
            data["input_tokens"] = serde_json::json!(input);
            data["output_tokens"] = serde_json::json!(output);
        }
        self.append_event("cognition_out", data).await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to `tool_call_start`. Includes the thread's effective
    /// capabilities so event consumers can see what the thread was
    /// authorized to do at dispatch time.
    pub async fn emit_tool_dispatch(&self, tool: &str, call_id: Option<&str>, effective_caps: &[String]) -> Result<()> {
        let mut data = serde_json::json!({"tool": tool});
        if let Some(id) = call_id {
            data["call_id"] = serde_json::json!(id);
        }
        data["effective_caps"] = serde_json::json!(effective_caps);
        self.append_event("tool_call_start", data).await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to `tool_call_result`.
    ///
    /// `body` is the model-visible result string (the same content the
    /// runtime pushes into the LLM message stream). When the body is
    /// larger than the inline cap, callers pass `body=None` plus
    /// `truncated_reason=Some("size_cap_exceeded")` and `result_size_bytes`.
    ///
    /// Parsed JSON goes into `result`; opaque text goes into `result_text`.
    /// Consumers always check `result` first.
    pub async fn emit_tool_result(
        &self,
        call_id: &str,
        body: Option<&str>,
        truncated: bool,
        truncated_reason: Option<&str>,
        result_size_bytes: u64,
    ) -> Result<()> {
        let mut data = serde_json::json!({
            "call_id": call_id,
            "truncated": truncated,
            "result_size_bytes": result_size_bytes,
        });
        if let Some(body_str) = body {
            let parsed = serde_json::from_str::<serde_json::Value>(body_str)
                .unwrap_or_else(|e| panic!(
                    "emit_tool_result: body for call_id {call_id} is not valid JSON \
                     (this is a programmer error — all callers produce JSON): {e}"
                ));
            data["result"] = parsed;
        }
        if let Some(reason) = truncated_reason {
            data["truncated_reason"] = serde_json::json!(reason);
        }
        self.append_event("tool_call_result", data).await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    /// Maps to `thread_failed`.
    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_event("thread_failed", serde_json::json!({"message": error})).await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_event(
            "thread_continued",
            serde_json::json!({"previous_thread_id": previous_id}),
        ).await
    }

    /// Resume-critical: must hard-fail on disconnect.
    /// Emits a `thread_usage` event with the cumulative ThreadUsage
    /// payload. The daemon persists this so resumed threads can reseed
    /// BudgetTracker and Harness.
    pub async fn emit_thread_usage(&self, usage: &ryeos_state::ThreadUsage) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback emit_thread_usage called without an inner UDS client \
                 (socket missing); thread usage ACK is required for settlement"
            )
        })?;
        let storage_class = storage_class_for("thread_usage");
        let payload = serde_json::to_value(usage)
            .map_err(|e| anyhow::anyhow!("serialize ThreadUsage: {e}"))?;
        client.append_event(&self.thread_id, "thread_usage", payload, storage_class).await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn stream_opened(&self, turn: u32) -> Result<()> {
        self.append_event("stream_opened", serde_json::json!({"turn": turn})).await
    }

    // ── native_async streaming contract ─────────────────────────────
    //
    // The following helpers form the Phase 5.2 standard streaming
    // event contract. Tools that declare `runtime.handlers.native_async`
    // (bool shorthand or rich form) are expected to emit `progress`
    // / `status` events during long-running phases and may publish
    // intermediate artifacts via `publish_artifact`. The engine does
    // not enforce or interpret these — `native_async` signals intent
    // only. Tools without `native_async` may still call these (no-op
    // when no callback socket is present), but consumers should not
    // rely on receiving them.

    /// Advisory: warn-and-continue OK when disconnected.
    /// Emit a typed progress event.
    ///
    /// `phase` is a short identifier; `message` is human-readable;
    /// `percent` is 0.0–100.0 when meaningful or `None` for
    /// indeterminate progress. See [`crate::progress::ProgressEvent`].
    pub async fn emit_progress(&self, payload: crate::progress::ProgressEvent) -> Result<()> {
        let value = serde_json::to_value(&payload)
            .map_err(|e| anyhow::anyhow!("encode ProgressEvent: {e}"))?;
        // High-frequency progressive event — maps to `stream_snapshot`
        // (journal_only). The original "progress" name is preserved
        // inside the payload via a `kind` field for downstream
        // consumers that want to discriminate.
        let mut wrapped = serde_json::json!({"kind": "progress"});
        if let Some(map) = wrapped.as_object_mut() {
            map.insert("payload".into(), value);
        }
        self.append_event("stream_snapshot", wrapped).await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    /// Emit a typed status / lifecycle transition event.
    ///
    /// See [`crate::progress::StatusEvent`].
    pub async fn emit_status(&self, payload: crate::progress::StatusEvent) -> Result<()> {
        let value = serde_json::to_value(&payload)
            .map_err(|e| anyhow::anyhow!("encode StatusEvent: {e}"))?;
        // Lifecycle status update — maps to `stream_snapshot` (the
        // closest validator-accepted bucket; lifecycle transitions
        // proper go through `finalize_thread` which emits
        // thread_completed/failed/cancelled).
        let mut wrapped = serde_json::json!({"kind": "status"});
        if let Some(map) = wrapped.as_object_mut() {
            map.insert("payload".into(), value);
        }
        self.append_event("stream_snapshot", wrapped).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::{ActionPayload, CallbackError, DispatchActionRequest};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // ── Mock callback that records events in memory ──────────────────

    struct EventRecorder {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl EventRecorder {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn last(&self, event_type: &str) -> Option<Value> {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .rev()
                .find(|(t, _)| t == event_type)
                .map(|(_, v)| v.clone())
        }
    }

    #[async_trait::async_trait]
    impl crate::callback::RuntimeCallbackAPI for EventRecorder {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _thread_id: &str, _pid: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(&self, _thread_id: &str, _status: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
        async fn request_continuation(&self, _thread_id: &str, _prompt: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
        async fn append_event(
            &self,
            _thread_id: &str,
            event_type: &str,
            payload: Value,
            _storage_class: &str,
        ) -> Result<Value, CallbackError> {
            self.events
                .lock()
                .unwrap()
                .push((event_type.to_string(), payload));
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _thread_id: &str,
            _events: Vec<Value>,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn claim_commands(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _thread_id: &str,
            _command_id: &str,
            _result: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _thread_id: &str, _artifact: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
    }

    fn make_recorder_client() -> (CallbackClient, Arc<EventRecorder>) {
        let recorder = Arc::new(EventRecorder::new());
        let client = CallbackClient::from_inner(
            recorder.clone() as Arc<dyn crate::callback::RuntimeCallbackAPI>,
            "T-test",
            "/project",
            "tat-test",
        );
        (client, recorder)
    }

    // ── New emit_tool_result tests ───────────────────────────────────

    #[tokio::test]
    async fn emit_tool_result_inlines_small_body_as_json() {
        let (cb, recorder) = make_recorder_client();
        cb.emit_tool_result(
            "call_1",
            Some(r#"{"ok":true,"workspace_card":{"chart_kind":"callout"}}"#),
            false,
            None,
            58,
        ).await.unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["call_id"], "call_1");
        assert_eq!(evt["truncated"], false);
        assert_eq!(evt["result_size_bytes"], 58);
        assert_eq!(evt["result"]["ok"], true);
        assert_eq!(evt["result"]["workspace_card"]["chart_kind"], "callout");
        assert!(evt.get("truncated_reason").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_omits_body_when_size_capped() {
        let (cb, recorder) = make_recorder_client();
        cb.emit_tool_result("call_2", None, true, Some("size_cap_exceeded"), 524_288)
            .await.unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["truncated"], true);
        assert_eq!(evt["truncated_reason"], "size_cap_exceeded");
        assert_eq!(evt["result_size_bytes"], 524_288);
        assert!(evt.get("result").is_none());
        assert!(evt.get("result_text").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_inlines_body_with_nested_json() {
        let (cb, recorder) = make_recorder_client();
        let body = r#"{"ok":true,"data":{"nested":[1,2,3]}}"#;
        cb.emit_tool_result("call_4", Some(body), false, None, body.len() as u64)
            .await.unwrap();
        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["result"]["data"]["nested"][2], 3);
        assert!(evt.get("result_text").is_none(),
                "result_text must never appear — all callers produce JSON");
    }

    // ── Existing tests ───────────────────────────────────────────────

    fn make_callback() -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
        }
    }

    fn make_client() -> CallbackClient {
        CallbackClient::new(&make_callback(), "T-test", "/project", "tat-test")
    }

    #[tokio::test]
    async fn dispatch_action_errors_when_disconnected() {
        // Post-V5.4 callback contract: a disconnected callback MUST
        // surface as an error, not a fabricated empty response.
        // Otherwise the calling runtime would feed `null` to the model
        // as a tool result, hiding the daemon link being down.
        let client = make_client();
        let req = DispatchActionRequest {
            thread_id: "T-test".to_string(),
            project_path: "/project".to_string(),
            action: ActionPayload {
                item_id: "my/tool".to_string(),
                params: json!({}),
                thread: "inline".to_string(),
            },
        };
        let err = client.dispatch_action(req).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("socket missing") || msg.contains("inner UDS client"),
            "expected disconnect error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn append_event_noop_when_disconnected() {
        let client = make_client();
        client
            .append_event("turn_start", json!({"turn": 1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn append_event_transcript_type_errors_when_disconnected() {
        let client = make_client();
        let err = client
            .append_event("cognition_in", json!({"turn": 1}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("socket missing"), "got: {err}");
    }

    #[tokio::test]
    async fn mark_running_errors_when_disconnected() {
        let client = make_client();
        let err = client.mark_running().await.unwrap_err();
        assert!(err.to_string().contains("socket missing"), "got: {err}");
    }

    #[tokio::test]
    async fn finalize_thread_errors_when_disconnected() {
        let client = make_client();
        let err = client.finalize_thread("completed").await.unwrap_err();
        assert!(err.to_string().contains("socket missing"), "got: {err}");
    }

    #[tokio::test]
    async fn replay_events_for_errors_when_disconnected() {
        let client = make_client();
        let err = client.replay_events_for("T-other").await.unwrap_err();
        assert!(err.to_string().contains("socket missing"), "got: {err}");
    }

    #[test]
    fn thread_id_and_project_path_accessors() {
        let client = make_client();
        assert_eq!(client.thread_id(), "T-test");
        assert_eq!(client.project_path(), "/project");
    }

    #[test]
    fn clone_preserves_fields() {
        let client = make_client();
        let cloned = client.clone();
        assert_eq!(cloned.thread_id(), "T-test");
        assert_eq!(cloned.project_path(), "/project");
    }

    #[tokio::test]
    async fn emit_thread_continued_noop_when_disconnected() {
        let client = make_client();
        client.emit_thread_continued("T-prev").await.unwrap();
    }

    #[tokio::test]
    async fn get_facets_noop_when_disconnected() {
        let client = make_client();
        let result = client.get_facets().await.unwrap();
        assert_eq!(result, Value::Null);
    }

    #[tokio::test]
    async fn emit_progress_noop_when_disconnected() {
        let client = make_client();
        client
            .emit_progress(
                crate::progress::ProgressEvent::new("download", "fetching")
                    .with_percent(10.0),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn emit_status_noop_when_disconnected() {
        let client = make_client();
        client
            .emit_status(crate::progress::StatusEvent::new("ready"))
            .await
            .unwrap();
    }

    /// V5.4 P2.2 — every event name the runtime can emit MUST be in
    /// the daemon's `EventStoreService::validate_event_type` allow-list,
    /// and every storage class returned by `storage_class_for` MUST be
    /// in `validate_storage_class`. The list below is the canonical
    /// set of names produced by `CallbackClient`'s typed emitters
    /// AND the bare `append_event` calls in `ryeos-directive-runtime`'s
    /// `runner.rs`. If a new emitter is added without aligning the
    /// daemon validator (or vice versa), this test fails loudly here
    /// instead of being silently dropped at the daemon boundary.
    ///
    /// We mirror the validator's two allow-lists rather than depending
    /// on `ryeosd` (which would be a circular dep).
    #[test]
    fn every_emitted_event_passes_the_daemon_validator() {
        const VALIDATOR_EVENTS: &[&str] = &[
            "thread_created",
            "thread_started",
            "thread_completed",
            "thread_failed",
            "thread_cancelled",
            "thread_killed",
            "thread_timed_out",
            "thread_continued",
            "edge_recorded",
            "child_thread_spawned",
            "continuation_requested",
            "continuation_accepted",
            "command_submitted",
            "command_claimed",
            "command_completed",
            "stream_opened",
            "token_delta",
            "stream_snapshot",
            "stream_closed",
            "artifact_published",
            "thread_reconciled",
            "orphan_process_killed",
            "system_prompt",
            "context_injected",
            "cognition_in",
            "cognition_out",
            "cognition_reasoning",
            "tool_call_start",
            "tool_call_result",
            // Graph lifecycle events
            "graph_started",
            "graph_completed",
            "graph_step_started",
            "graph_step_completed",
            "graph_branch_taken",
            "graph_foreach_iteration",
            "thread_usage",
        ];
        const VALIDATOR_STORAGE: &[&str] = &["indexed", "journal_only"];

        // Every event the runtime can emit, post-P2.2:
        let runtime_emits: &[&str] = &[
            // Typed emitters in CallbackClient
            "cognition_in",          // emit_turn_start
            "cognition_out",         // emit_turn_complete
            "tool_call_start",       // emit_tool_dispatch
            "tool_call_result",      // emit_tool_result
            "thread_failed",         // emit_error
            "thread_continued",      // emit_thread_continued
            "stream_snapshot",       // emit_progress / emit_status
            // Bare append_event calls in ryeos-directive-runtime/runner.rs
            "stream_opened",         // State::Streaming
            "cognition_reasoning",   // FiringHooks
            "thread_usage",          // emit_thread_usage
            // tool_call_result(blocked) re-uses the validator name
            // already covered above.
        ];

        for ev in runtime_emits {
            assert!(
                VALIDATOR_EVENTS.contains(ev),
                "runtime emits {ev:?} but the daemon's validate_event_type \
                 does not accept it — runtime <> daemon vocabulary drift"
            );
            let sc = storage_class_for(ev);
            assert!(
                VALIDATOR_STORAGE.contains(&sc),
                "storage_class_for({ev:?}) returned {sc:?} which is not in \
                 the daemon's accepted set"
            );
        }
    }
}
