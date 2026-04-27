use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::callback::RuntimeCallbackAPI;
use crate::envelope::EnvelopeCallback;

/// Map an event type name to the storage class the daemon's
/// `EventStoreService::validate_storage_class` accepts. High-frequency
/// progressive events go to `journal_only`; everything else is a
/// milestone and goes to `indexed`. The set of accepted event names
/// (and the journal_only short-list) MUST stay in sync with
/// `ryeosd/src/services/event_store.rs::validate_event_type`.
pub fn storage_class_for(event_type: &str) -> &'static str {
    match event_type {
        // High-frequency progressive events.
        "token_delta"
        | "stream_snapshot"
        | "cognition_reasoning" => "journal_only",
        // Everything else is a milestone.
        _ => "indexed",
    }
}

pub struct CallbackClient {
    inner: Option<Arc<dyn RuntimeCallbackAPI>>,
    thread_id: String,
    project_path: String,
}

impl CallbackClient {
    /// Construct from a pre-built runtime API implementation (for tests).
    pub fn from_inner(
        inner: Arc<dyn RuntimeCallbackAPI>,
        thread_id: &str,
        project_path: &str,
    ) -> Self {
        Self {
            inner: Some(inner),
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
        }
    }
}

impl Clone for CallbackClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            thread_id: self.thread_id.clone(),
            project_path: self.project_path.clone(),
        }
    }
}

impl CallbackClient {
    pub fn new(callback: &EnvelopeCallback, thread_id: &str, project_path: &str) -> Self {
        let inner: Option<Arc<dyn RuntimeCallbackAPI>> = if callback.socket_path.exists() {
            Some(Arc::new(
                crate::callback_uds::UdsRuntimeClient::new(
                    callback.socket_path.clone(),
                    callback.token.clone(),
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

    pub async fn append_event(&self, event_type: &str, payload: Value) -> Result<()> {
        let storage_class = storage_class_for(event_type);
        match &self.inner {
            Some(client) => {
                client.append_event(&self.thread_id, event_type, payload, storage_class).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn mark_running(&self) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.mark_running(&self.thread_id).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn finalize_thread(&self, status: &str) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.finalize_thread(&self.thread_id, status).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn request_continuation(&self, prompt: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.request_continuation(&self.thread_id, prompt).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

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

    pub async fn get_thread(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(&self.thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn get_thread_by_id(&self, thread_id: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn replay_events_for(&self, thread_id: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.replay_events(thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn get_facets(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_facets(&self.thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    // Typed event emission methods (merged from EventEmitter)

    /// Begin-of-turn marker — model inputs going into cognition.
    /// Maps to the validator-accepted `cognition_in` event.
    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_event("cognition_in", serde_json::json!({"turn": turn})).await
    }

    /// End-of-turn marker — model output and token usage.
    /// Maps to the validator-accepted `cognition_out` event.
    pub async fn emit_turn_complete(&self, turn: u32, tokens: Option<(u64, u64)>) -> Result<()> {
        let mut data = serde_json::json!({"turn": turn});
        if let Some((input, output)) = tokens {
            data["input_tokens"] = serde_json::json!(input);
            data["output_tokens"] = serde_json::json!(output);
        }
        self.append_event("cognition_out", data).await
    }

    /// A tool call is being dispatched. Maps to `tool_call_start`.
    pub async fn emit_tool_dispatch(&self, tool: &str, call_id: Option<&str>) -> Result<()> {
        let mut data = serde_json::json!({"tool": tool});
        if let Some(id) = call_id {
            data["call_id"] = serde_json::json!(id);
        }
        self.append_event("tool_call_start", data).await
    }

    /// A tool call returned. Maps to `tool_call_result`.
    pub async fn emit_tool_result(&self, call_id: &str, truncated: bool) -> Result<()> {
        self.append_event(
            "tool_call_result",
            serde_json::json!({"call_id": call_id, "truncated": truncated}),
        ).await
    }

    /// Terminal failure — surfaced as `thread_failed`. Callers that
    /// also call `finalize_thread("failed")` should call this FIRST so
    /// the failure reason hits the audit trail before the lifecycle
    /// transition.
    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_event("thread_failed", serde_json::json!({"message": error})).await
    }

    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_event(
            "thread_continued",
            serde_json::json!({"previous_thread_id": previous_id}),
        ).await
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
    use crate::callback::{ActionPayload, DispatchActionRequest};
    use serde_json::json;
    use std::path::PathBuf;

    fn make_callback() -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
        }
    }

    fn make_client() -> CallbackClient {
        CallbackClient::new(&make_callback(), "T-test", "/project")
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
                kind: None,
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
    async fn mark_running_noop_when_disconnected() {
        let client = make_client();
        client.mark_running().await.unwrap();
    }

    #[tokio::test]
    async fn all_methods_noop_when_disconnected() {
        let client = make_client();

        client.finalize_thread("completed").await.unwrap();
        client
            .publish_artifact(json!({"type": "summary", "content": "done"}))
            .await
            .unwrap();
        let cont = client.request_continuation("continue?").await.unwrap();
        assert_eq!(cont, Value::Null);
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
    async fn emit_turn_start_noop_when_disconnected() {
        let client = make_client();
        client.emit_turn_start(1).await.unwrap();
    }

    #[tokio::test]
    async fn emit_tool_dispatch_with_call_id() {
        let client = make_client();
        client.emit_tool_dispatch("read_file", Some("call_123")).await.unwrap();
    }

    #[tokio::test]
    async fn emit_turn_complete_with_tokens() {
        let client = make_client();
        client.emit_turn_complete(1, Some((100, 50))).await.unwrap();
    }

    #[tokio::test]
    async fn emit_tool_result_noop_when_disconnected() {
        let client = make_client();
        client.emit_tool_result("call_1", false).await.unwrap();
    }

    #[tokio::test]
    async fn emit_error_noop_when_disconnected() {
        let client = make_client();
        client.emit_error("something broke").await.unwrap();
    }

    #[tokio::test]
    async fn emit_thread_continued_noop_when_disconnected() {
        let client = make_client();
        client.emit_thread_continued("T-prev").await.unwrap();
    }

    #[tokio::test]
    async fn replay_events_for_noop_when_disconnected() {
        let client = make_client();
        let result = client.replay_events_for("T-other").await.unwrap();
        assert_eq!(result, Value::Null);
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
