use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::callback::{CallbackError, ReplayResponse, RuntimeCallbackAPI, TerminalCompletion};
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

fn storage_class_for_payload(event_type: &str, payload: &Value) -> &'static str {
    // Progressive streamed cognition_out is live-only (ephemeral): deltas, partial
    // tool args, AND complete `tool_use` blocks. The DURABLE record of a turn's
    // tool calls is `emit_turn_complete`'s `cognition_out{tool_calls}` — persisting
    // the mid-stream `tool_use` too would fold a spurious extra assistant turn on
    // resume (reconstruct_messages reads `tool_calls`, not `tool_use`). Must stay
    // in lock-step with the daemon's `is_ephemeral_allowed`. (The payload keys are
    // JSON field names, not event types, so they stay string-keyed.)
    if matches!(
        RuntimeEventType::parse(event_type),
        Ok(RuntimeEventType::CognitionOut)
    ) && (payload.get("delta").is_some()
        || payload.get("tool_use_partial").is_some()
        || payload.get("tool_use").is_some())
    {
        return StorageClass::Ephemeral.as_str();
    }

    storage_class_for(event_type)
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
    pub fn new(
        callback: &EnvelopeCallback,
        thread_id: &str,
        project_path: &str,
        thread_auth_token: &str,
    ) -> Self {
        let inner: Option<Arc<dyn RuntimeCallbackAPI>> = if callback.socket_path.exists() {
            Some(Arc::new(crate::callback_uds::UdsRuntimeClient::new(
                callback.socket_path.clone(),
                callback.token.clone(),
                thread_auth_token.to_string(),
            )))
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
    /// `crate::callback_contract`). We deserialize STRICTLY — an old
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
    ) -> std::result::Result<crate::callback_contract::CallbackDispatchResponse, CallbackError>
    {
        let client = self.inner.as_ref().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "callback dispatch_action called without an inner UDS client \
                 (socket missing); runtime cannot route to the daemon"
            ))
        })?;
        let raw: Value = client.dispatch_action(req).await?;
        serde_json::from_value::<crate::callback_contract::CallbackDispatchResponse>(raw).map_err(
            |e| {
                CallbackError::Transport(anyhow::anyhow!(
                    "invalid CallbackDispatchResponse from daemon: {e}"
                ))
            },
        )
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn append_event(&self, event_type: &str, payload: Value) -> Result<()> {
        let storage_class = storage_class_for_payload(event_type, &payload);
        let is_transcript =
            RuntimeEventType::parse(event_type).is_ok_and(RuntimeEventType::is_transcript);
        match &self.inner {
            Some(client) => {
                client
                    .append_event(&self.thread_id, event_type, payload, storage_class)
                    .await
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
        let storage_class = storage_class_for_payload(event_type.as_str(), &payload);
        match &self.inner {
            Some(client) => {
                client
                    .append_event(&self.thread_id, event_type.as_str(), payload, storage_class)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Report this process's pid so the daemon records the runtime's process
    /// group. Resume-critical: hard-fails when the callback channel is
    /// unavailable. A live runtime that cannot register its pgid must exit
    /// rather than keep doing untracked work — otherwise, after a daemon
    /// restart, reconcile cannot tell it from a crashed thread and would
    /// resume a duplicate alongside the still-running original.
    pub async fn attach_current_process(&self) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback attach_process called without an inner UDS client \
                 (socket missing); cannot register runtime process"
            )
        })?;
        client
            .attach_process(&self.thread_id, std::process::id())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Resume-critical: must hard-fail on disconnect.
    pub async fn mark_running(&self) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback mark_running called without an inner UDS client \
                 (socket missing); cannot mark thread as running"
            )
        })?;
        client
            .mark_running(&self.thread_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Drain pending thread commands (cancel/kill/…) for this thread. A missing
    /// UDS client is a hard error — cooperative cancellation must not silently
    /// no-op into "no commands".
    pub async fn claim_commands(&self) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback claim_commands called without an inner UDS client \
                 (socket missing); cannot drain thread commands"
            )
        })?;
        client
            .claim_commands(&self.thread_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Settle a claimed command as `completed` or `rejected`.
    pub async fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Value,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback complete_command called without an inner UDS client \
                 (socket missing); cannot settle command {command_id}"
            )
        })?;
        client
            .complete_command(&self.thread_id, command_id, status, result)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Resume-critical: must hard-fail on disconnect.
    pub async fn finalize_thread(&self, completion: TerminalCompletion) -> Result<()> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback finalize_thread called without an inner UDS client \
                 (socket missing); cannot finalize thread"
            )
        })?;
        client
            .finalize_thread(&self.thread_id, completion)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Resume-critical: a handoff MUST reach the daemon. NOT advisory — a missing
    /// UDS client (disconnected) is a hard error, never a silent `Ok(null)` that
    /// would settle the thread `continued` with no successor.
    pub async fn request_continuation(&self, log_reason: Option<&str>) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback request_continuation called without an inner UDS client \
                 (socket missing); the handoff cannot be recorded"
            )
        })?;
        client
            .request_continuation(&self.thread_id, log_reason)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Suspend-critical: ask the daemon to launch a detached follow child and
    /// suspend this thread. Like `request_continuation`, a missing UDS client is a
    /// hard error — a lost suspend would leave the graph believing it handed off.
    /// The caller's own thread + project identity are injected here; the daemon
    /// derives all trust-bearing state from the validated tokens.
    pub async fn spawn_follow_child(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step_count: i64,
        child_item_ref: &str,
        ref_bindings: std::collections::BTreeMap<String, String>,
        child_parameters: Value,
        frontier_id: Option<String>,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback spawn_follow_child called without an inner UDS client \
                 (socket missing); the follow suspend cannot be recorded"
            )
        })?;
        let request = crate::callback::SpawnFollowChildRequest {
            thread_id: self.thread_id.clone(),
            project_path: self.project_path.clone(),
            graph_run_id: graph_run_id.to_string(),
            follow_node: follow_node.to_string(),
            step_count,
            children: vec![crate::callback::FollowChildSpec {
                item_ref: child_item_ref.to_string(),
                ref_bindings,
                parameters: child_parameters,
                facets: None,
            }],
            launch_window_width: None,
            frontier_id,
        };
        client
            .spawn_follow_child(request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn spawn_follow_children(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step_count: i64,
        children: Vec<crate::callback::FollowChildSpec>,
        launch_window_width: Option<u32>,
        frontier_id: Option<String>,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| anyhow::anyhow!(
            "callback spawn_follow_children called without an inner UDS client (socket missing); the follow suspend cannot be recorded"
        ))?;
        client
            .spawn_follow_child(crate::callback::SpawnFollowChildRequest {
                thread_id: self.thread_id.clone(),
                project_path: self.project_path.clone(),
                graph_run_id: graph_run_id.to_string(),
                follow_node: follow_node.to_string(),
                step_count,
                children,
                launch_window_width,
                frontier_id,
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn publish_artifact(&self, artifact: Value) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client
                    .publish_artifact(&self.thread_id, artifact)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_thread(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client
                .get_thread(&self.thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn bundle_events_append(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback bundle_events_append called without an inner UDS client \
                 (socket missing); cannot append durable bundle event"
            )
        })?;
        client
            .bundle_events_append(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn bundle_events_read_chain(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback bundle_events_read_chain called without an inner UDS client \
                 (socket missing); cannot read durable bundle events"
            )
        })?;
        client
            .bundle_events_read_chain(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn bundle_events_scan(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback bundle_events_scan called without an inner UDS client \
                 (socket missing); cannot scan durable bundle events"
            )
        })?;
        client
            .bundle_events_scan(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn vault_put(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback vault_put called without an inner UDS client \
                 (socket missing); cannot store runtime vault secret"
            )
        })?;
        client
            .vault_put(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn vault_get(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback vault_get called without an inner UDS client \
                 (socket missing); cannot read runtime vault secret"
            )
        })?;
        client
            .vault_get(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn vault_delete(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback vault_delete called without an inner UDS client \
                 (socket missing); cannot delete runtime vault secret"
            )
        })?;
        client
            .vault_delete(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn vault_list(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback vault_list called without an inner UDS client \
                 (socket missing); cannot list runtime vault secrets"
            )
        })?;
        client
            .vault_list(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub async fn author_item(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback author_item called without an inner UDS client \
                 (socket missing); cannot author signed project item"
            )
        })?;
        client
            .author_item(&self.thread_id, request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_thread_by_id(&self, thread_id: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client
                .get_thread(thread_id)
                .await
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
        let raw: Value = client
            .replay_events(serde_json::json!({ "thread_id": thread_id }))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        serde_json::from_value::<ReplayResponse>(raw)
            .map_err(|e| anyhow::anyhow!("invalid ReplayResponse from daemon: {e}"))
    }

    /// Resume-critical: fold an entire chain (every thread sharing the
    /// `chain_root_id`) into one ordered event list. Hard-fails on disconnect.
    /// NB: a chain namespace can include non-continuation child threads
    /// (compose-context, sub-dispatch); prefer [`Self::replay_thread`] over the
    /// continuation path for rehydration so sibling-branch events don't pollute
    /// the transcript.
    pub async fn replay_chain(&self, chain_root_id: &str) -> Result<ReplayResponse> {
        self.replay_paged("chain_root_id", chain_root_id).await
    }

    /// Resume-critical: fold ONE thread's own events (thread-scoped), paginated.
    /// Used to fold the linear continuation path turn-by-turn — thread scoping
    /// structurally excludes child/sibling threads that share the chain root.
    pub async fn replay_thread(&self, thread_id: &str) -> Result<ReplayResponse> {
        self.replay_paged("thread_id", thread_id).await
    }

    /// Page through `after_chain_seq` cursors for a single replay scope
    /// (`chain_root_id` or `thread_id`) until exhausted, so long histories don't
    /// silently lose events. Hard-fails on disconnect.
    async fn replay_paged(&self, scope_key: &str, scope_value: &str) -> Result<ReplayResponse> {
        const REPLAY_PAGE_LIMIT: usize = 200;
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback replay_paged called without an inner UDS client \
                 (socket missing); runtime cannot replay for resume"
            )
        })?;

        let mut all_events = Vec::new();
        let mut after_chain_seq: Option<i64> = None;
        loop {
            let mut params = serde_json::Map::new();
            params.insert(
                scope_key.to_string(),
                Value::String(scope_value.to_string()),
            );
            params.insert("limit".to_string(), serde_json::json!(REPLAY_PAGE_LIMIT));
            if let Some(cursor) = after_chain_seq {
                params.insert("after_chain_seq".to_string(), serde_json::json!(cursor));
            }
            let raw: Value = client
                .replay_events(Value::Object(params))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let page: ReplayResponse = serde_json::from_value(raw)
                .map_err(|e| anyhow::anyhow!("invalid ReplayResponse from daemon: {e}"))?;
            all_events.extend(page.events);
            match page.next_cursor {
                Some(cursor) => after_chain_seq = Some(cursor),
                None => break,
            }
        }

        Ok(ReplayResponse {
            events: all_events,
            next_cursor: None,
        })
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn get_facets(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client
                .get_facets(&self.thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    /// Drain operator inputs staged for this running thread, in FIFO order.
    /// The daemon has ALREADY persisted any returned inputs as durable
    /// `cognition_in` (through the running-guarded path) before returning, so a
    /// non-empty result is in the braid — the runner only needs to fold them
    /// into its in-flight `messages`. Empty when disconnected (best-effort; the
    /// loop simply continues without new input).
    pub async fn poll_input(&self) -> Result<Vec<ryeos_state::objects::LiveInput>> {
        let Some(client) = &self.inner else {
            return Ok(Vec::new());
        };
        let raw = client
            .poll_input(&self.thread_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        match raw.get("inputs").cloned() {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(inputs) => serde_json::from_value(inputs)
                .map_err(|e| anyhow::anyhow!("invalid poll_input inputs from daemon: {e}")),
        }
    }

    // Typed event emission methods (merged from EventEmitter)

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to the validator-accepted `cognition_in` event.
    /// Resume-critical: transcript-bearing; hard-fails on disconnect.
    /// Emits the stimulus that opens a run as a `cognition_in` event — the
    /// input to cognition, not a "user" turn. A chained successor folds these
    /// from the chain to rebuild the prior context (the stimulus is rendered
    /// from the directive body + inputs at launch, so it is not otherwise
    /// recoverable from events).
    pub async fn emit_stimulus(&self, content: &str) -> Result<()> {
        self.append_event("cognition_in", serde_json::json!({ "content": content }))
            .await
    }

    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_event("cognition_in", serde_json::json!({"turn": turn}))
            .await
    }

    /// Resume-critical: seal a cognition cut short by a live interrupt. Emits a
    /// transcript-bearing `cognition_out` with the partial `content`/
    /// `reasoning_content` and `interrupted: true`, and deliberately NO
    /// `tool_calls` — an interrupted cognition didn't complete its tool call, so
    /// the folded wire history carries no unpaired tool call. Durable (indexed):
    /// resume folds it as an assistant message so the redirect has honest context.
    pub async fn emit_turn_interrupted(
        &self,
        turn: u32,
        partial_message: Option<Value>,
    ) -> Result<()> {
        let mut data = serde_json::json!({ "turn": turn, "interrupted": true });
        if let Some(Value::Object(message)) = partial_message {
            for key in ["content", "reasoning_content"] {
                if let Some(value) = message.get(key) {
                    data[key] = value.clone();
                }
            }
        }
        self.append_event("cognition_out", data).await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to the validator-accepted `cognition_out` event.
    pub async fn emit_turn_complete(
        &self,
        turn: u32,
        tokens: Option<(u64, u64)>,
        assistant_message: Option<Value>,
    ) -> Result<()> {
        let mut data = serde_json::json!({"turn": turn});
        if let Some(Value::Object(message)) = assistant_message {
            for key in ["content", "tool_calls", "reasoning_content"] {
                if let Some(value) = message.get(key) {
                    data[key] = value.clone();
                }
            }
        }
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
    pub async fn emit_tool_dispatch(
        &self,
        tool: &str,
        call_id: Option<&str>,
        effective_caps: &[String],
    ) -> Result<()> {
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
    /// `tool` is the canonical ref (e.g. `apps_tv_tracker_workspace_render_chart`)
    /// so SSE consumers can route results without cross-referencing tool_call_start.
    // Wire-shaped: each argument is one field of the emitted result
    // envelope; eight call sites pass them positionally today.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit_tool_result(
        &self,
        call_id: &str,
        tool: &str,
        body: Option<&str>,
        truncated: bool,
        truncated_reason: Option<&str>,
        result_size_bytes: u64,
        duplicate_of: Option<&str>,
    ) -> Result<()> {
        let mut data = serde_json::json!({
            "call_id": call_id,
            "tool": tool,
            "truncated": truncated,
            "result_size_bytes": result_size_bytes,
        });
        if let Some(body_str) = body {
            if let Some(hash) = duplicate_of {
                data["result_text"] = serde_json::json!(body_str);
                data["deduplicated"] = serde_json::json!(true);
                data["duplicate_of"] = serde_json::json!(hash);
            } else {
                match serde_json::from_str::<serde_json::Value>(body_str) {
                    Ok(parsed) => data["result"] = parsed,
                    Err(e) => {
                        tracing::warn!(
                            call_id,
                            tool,
                            error = %e,
                            "emit_tool_result received non-JSON body; preserving as result_text"
                        );
                        data["result_text"] = serde_json::json!(body_str);
                        data["result_parse_error"] = serde_json::json!(e.to_string());
                    }
                }
            }
        }
        if let Some(reason) = truncated_reason {
            data["truncated_reason"] = serde_json::json!(reason);
        }
        self.append_event("tool_call_result", data).await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    /// Maps to `thread_failed`.
    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_event("thread_failed", serde_json::json!({"message": error}))
            .await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_event(
            "thread_continued",
            serde_json::json!({"previous_thread_id": previous_id}),
        )
        .await
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
        client
            .append_event(&self.thread_id, "thread_usage", payload, storage_class)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn stream_opened(&self, turn: u32) -> Result<()> {
        self.append_event("stream_opened", serde_json::json!({"turn": turn}))
            .await
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

    #[test]
    fn progressive_cognition_out_is_ephemeral() {
        // delta / tool_use_partial / complete tool_use are all live-only so they
        // don't fold a spurious extra assistant turn on resume.
        for payload in [
            json!({"turn": 1, "delta": "hi"}),
            json!({"turn": 1, "tool_use_partial": {"id": "x"}}),
            json!({"turn": 1, "tool_use": {"id": "x", "name": "f", "arguments": {}}}),
        ] {
            assert_eq!(
                storage_class_for_payload("cognition_out", &payload),
                StorageClass::Ephemeral.as_str(),
                "payload {payload} should be ephemeral"
            );
        }
    }

    #[test]
    fn turn_complete_cognition_out_is_indexed() {
        // The durable record of a turn (with tool_calls array) is indexed.
        let payload = json!({"turn": 1, "content": "done", "tool_calls": []});
        assert_eq!(
            storage_class_for_payload("cognition_out", &payload),
            StorageClass::Indexed.as_str()
        );
        // An interrupted seal (no progressive keys) is also durable.
        let interrupted = json!({"turn": 1, "content": "par", "interrupted": true});
        assert_eq!(
            storage_class_for_payload("cognition_out", &interrupted),
            StorageClass::Indexed.as_str()
        );
    }

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
        async fn attach_process(
            &self,
            _thread_id: &str,
            _pid: u32,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _thread_id: &str,
            _completion: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
        async fn request_continuation(
            &self,
            _thread_id: &str,
            _log_reason: Option<&str>,
        ) -> Result<Value, CallbackError> {
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
        async fn replay_events(&self, _params: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_append(
            &self,
            _thread_id: &str,
            request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(request)
        }
        async fn bundle_events_read_chain(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(
            &self,
            _thread_id: &str,
            _request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(
            &self,
            _thread_id: &str,
            request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(request)
        }
        async fn vault_get(
            &self,
            _thread_id: &str,
            request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(request)
        }
        async fn vault_delete(
            &self,
            _thread_id: &str,
            request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(request)
        }
        async fn vault_list(
            &self,
            _thread_id: &str,
            request: Value,
        ) -> Result<Value, CallbackError> {
            Ok(request)
        }
        async fn claim_commands(&self, _thread_id: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _thread_id: &str,
            _command_id: i64,
            _status: &str,
            _result: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(
            &self,
            _thread_id: &str,
            _artifact: Value,
        ) -> Result<Value, CallbackError> {
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
            "test/render_chart",
            Some(r#"{"ok":true,"workspace_card":{"chart_kind":"callout"}}"#),
            false,
            None,
            58,
            None,
        )
        .await
        .unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["call_id"], "call_1");
        assert_eq!(evt["tool"], "test/render_chart");
        assert_eq!(evt["truncated"], false);
        assert_eq!(evt["result_size_bytes"], 58);
        assert_eq!(evt["result"]["ok"], true);
        assert_eq!(evt["result"]["workspace_card"]["chart_kind"], "callout");
        assert!(evt.get("truncated_reason").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_omits_body_when_size_capped() {
        let (cb, recorder) = make_recorder_client();
        cb.emit_tool_result(
            "call_2",
            "test/search",
            None,
            true,
            Some("size_cap_exceeded"),
            524_288,
            None,
        )
        .await
        .unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["truncated"], true);
        assert_eq!(evt["tool"], "test/search");
        assert_eq!(evt["truncated_reason"], "size_cap_exceeded");
        assert_eq!(evt["result_size_bytes"], 524_288);
        assert!(evt.get("result").is_none());
        assert!(evt.get("result_text").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_inlines_body_with_nested_json() {
        let (cb, recorder) = make_recorder_client();
        let body = r#"{"ok":true,"data":{"nested":[1,2,3]}}"#;
        cb.emit_tool_result(
            "call_4",
            "test/nested",
            Some(body),
            false,
            None,
            body.len() as u64,
            None,
        )
        .await
        .unwrap();
        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["result"]["data"]["nested"][2], 3);
        assert!(
            evt.get("result_text").is_none(),
            "result_text must never appear — all callers produce JSON"
        );
    }

    #[tokio::test]
    async fn emit_tool_result_preserves_invalid_json_body_without_panicking() {
        let (cb, recorder) = make_recorder_client();
        let body = "[truncated json";
        cb.emit_tool_result(
            "call_bad_json",
            "test/search",
            Some(body),
            true,
            Some("result_guard"),
            body.len() as u64,
            None,
        )
        .await
        .unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["call_id"], "call_bad_json");
        assert_eq!(evt["truncated"], true);
        assert_eq!(evt["truncated_reason"], "result_guard");
        assert_eq!(evt["result_text"], body);
        assert!(evt.get("result").is_none());
        assert!(!evt["result_parse_error"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emit_tool_result_marks_deduplicated_body_as_text_without_parse_error() {
        let (cb, recorder) = make_recorder_client();
        let body = "[duplicate result omitted — hash deadbeefdeadbeef]";
        cb.emit_tool_result(
            "call_duplicate",
            "test/search",
            Some(body),
            false,
            None,
            2048,
            Some("deadbeefdeadbeefdeadbeefdeadbeef"),
        )
        .await
        .unwrap();

        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["call_id"], "call_duplicate");
        assert_eq!(evt["result_text"], body);
        assert_eq!(evt["deduplicated"], true);
        assert_eq!(evt["duplicate_of"], "deadbeefdeadbeefdeadbeefdeadbeef");
        assert!(evt.get("result").is_none());
        assert!(evt.get("result_parse_error").is_none());
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
                ref_bindings: std::collections::BTreeMap::new(),
                params: json!({}),
                thread: "inline".to_string(),
                call: None,
                facets: None,
                launch_window: None,
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
        let err = client
            .finalize_thread(TerminalCompletion {
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result: None,
                error: None,
                cost: None,
                outputs: serde_json::Value::Null,
                warnings: Vec::new(),
            })
            .await
            .unwrap_err();
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
                crate::progress::ProgressEvent::new("download", "fetching").with_percent(10.0),
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
        const VALIDATOR_STORAGE: &[&str] = &["indexed", "journal_only", "ephemeral"];

        // Every event the runtime can emit, post-P2.2:
        let runtime_emits: &[&str] = &[
            // Typed emitters in CallbackClient
            "cognition_in",     // emit_turn_start
            "cognition_out",    // emit_turn_complete
            "tool_call_start",  // emit_tool_dispatch
            "tool_call_result", // emit_tool_result
            "thread_failed",    // emit_error
            "thread_continued", // emit_thread_continued
            "stream_snapshot",  // emit_progress / emit_status
            // Bare append_event calls in crates/runtimes/directive/runner.rs
            "stream_opened",       // State::Streaming
            "cognition_reasoning", // FiringHooks
            "thread_usage",        // emit_thread_usage
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

    #[test]
    fn cognition_out_progressive_payloads_are_ephemeral() {
        assert_eq!(
            storage_class_for_payload("cognition_out", &json!({"turn": 1, "delta": "hi"})),
            "ephemeral"
        );
        assert_eq!(
            storage_class_for_payload(
                "cognition_out",
                &json!({"turn": 1, "tool_use_partial": {"id": "c", "delta": "{}"}}),
            ),
            "ephemeral"
        );
        assert_eq!(
            storage_class_for_payload("cognition_out", &json!({"turn": 1})),
            "indexed"
        );
    }

    #[tokio::test]
    async fn emit_turn_complete_persists_final_assistant_summary() {
        let (cb, recorder) = make_recorder_client();
        cb.emit_turn_complete(
            1,
            Some((10, 5)),
            Some(json!({
                "content": "final answer",
                "tool_calls": [{"id": "c1", "name": "search", "arguments": {"q": "x"}}],
                "reasoning_content": "hidden",
                "delta": "must not be copied",
            })),
        )
        .await
        .unwrap();

        let evt = recorder.last("cognition_out").unwrap();
        assert_eq!(evt["turn"], 1);
        assert_eq!(evt["content"], "final answer");
        assert_eq!(evt["tool_calls"][0]["name"], "search");
        assert_eq!(evt["reasoning_content"], "hidden");
        assert_eq!(evt["input_tokens"], 10);
        assert_eq!(evt["output_tokens"], 5);
        assert!(evt.get("delta").is_none());
    }

    // ── replay_chain pagination ──────────────────────────────────────

    /// A daemon stand-in that serves the chain in two pages: the first call
    /// (no cursor) returns events `a,b` with `next_cursor=2`; the follow-up
    /// (cursor=2) returns `c` with no cursor. Exercises `replay_chain`'s paging
    /// loop and ordering.
    struct PagingReplay;

    #[async_trait::async_trait]
    impl crate::callback::RuntimeCallbackAPI for PagingReplay {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, params: Value) -> Result<Value, CallbackError> {
            use crate::callback::{ReplayResponse, ReplayedEventRecord};
            let ev = |t: &str| ReplayedEventRecord {
                event_type: t.to_string(),
                payload: json!({}),
            };
            let page = match params.get("after_chain_seq").and_then(|v| v.as_i64()) {
                None => ReplayResponse {
                    events: vec![ev("a"), ev("b")],
                    next_cursor: Some(2),
                },
                Some(2) => ReplayResponse {
                    events: vec![ev("c")],
                    next_cursor: None,
                },
                _ => ReplayResponse {
                    events: vec![],
                    next_cursor: None,
                },
            };
            Ok(serde_json::to_value(page).unwrap())
        }
        async fn bundle_events_append(&self, _: &str, r: Value) -> Result<Value, CallbackError> {
            Ok(r)
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, r: Value) -> Result<Value, CallbackError> {
            Ok(r)
        }
        async fn vault_get(&self, _: &str, r: Value) -> Result<Value, CallbackError> {
            Ok(r)
        }
        async fn vault_delete(&self, _: &str, r: Value) -> Result<Value, CallbackError> {
            Ok(r)
        }
        async fn vault_list(&self, _: &str, r: Value) -> Result<Value, CallbackError> {
            Ok(r)
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: i64,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(Value::Null)
        }
    }

    #[tokio::test]
    async fn replay_chain_folds_multiple_pages_in_order() {
        let client = CallbackClient::from_inner(
            Arc::new(PagingReplay) as Arc<dyn crate::callback::RuntimeCallbackAPI>,
            "T-test",
            "/project",
            "tat-test",
        );
        let resp = client.replay_chain("C-1").await.unwrap();
        let types: Vec<&str> = resp.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["a", "b", "c"],
            "all pages must fold in chain order"
        );
        assert!(resp.next_cursor.is_none());
    }
}
