use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::callback::{
    CallbackError, ReplayResponse, ReplayedEventRecord, RuntimeCallbackAPI, TerminalCompletion,
};
use crate::envelope::EnvelopeCallback;
use crate::events::{RuntimeEventType, StorageClass};

/// Map a typed event to the storage class accepted by the daemon.
pub fn storage_class_for(event_type: RuntimeEventType) -> &'static str {
    event_type.storage_class().as_str()
}

fn storage_class_for_payload(event_type: RuntimeEventType, payload: &Value) -> &'static str {
    // Progressive streamed cognition_out is live-only (ephemeral): deltas, partial
    // tool args, AND complete `tool_use` blocks. The DURABLE record of a turn's
    // tool calls is `emit_turn_complete`'s `cognition_out{tool_calls}` — persisting
    // the mid-stream `tool_use` too would fold a spurious extra assistant turn on
    // resume (reconstruct_messages reads `tool_calls`, not `tool_use`). Must stay
    // in lock-step with the daemon's `is_ephemeral_allowed`. (The payload keys are
    // JSON field names, not event types, so they stay string-keyed.)
    if event_type == RuntimeEventType::CognitionOut
        && (payload.get("delta").is_some()
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

/// Maximum event count in one runtime replay page. The daemon enforces this
/// same wire limit; clients paginate until `next_cursor` is absent.
pub const MAX_RUNTIME_REPLAY_PAGE_LIMIT: usize = 32;

/// Process-local safety ceilings for one complete native-runtime recovery
/// fold. Authored graph/directive limits remain the semantic execution
/// authority; these bounds prevent an unexpectedly long or malformed daemon
/// replay from growing one runtime process without limit.
const MAX_RUNTIME_REPLAY_TOTAL_EVENTS: usize = 16 * 1024;
const MAX_RUNTIME_REPLAY_TOTAL_SERIALIZED_BYTES: usize = 64 * 1024 * 1024;

/// Shared process-local replay budget for one complete recovery fold.
///
/// Callers that fold more than one replay scope, such as a directive's linear
/// continuation path, must reuse one value across every scope so the safety
/// ceiling cannot multiply by the number of threads.
#[derive(Debug, Clone)]
pub struct RuntimeReplayBudget {
    total_events: usize,
    // Exact serialized size of the combined JSON event array, including its
    // opening and closing brackets. Each event after the first adds one comma.
    total_serialized_bytes: usize,
}

impl Default for RuntimeReplayBudget {
    fn default() -> Self {
        Self {
            total_events: 0,
            total_serialized_bytes: 2,
        }
    }
}

fn append_replay_page(
    all_events: &mut Vec<ReplayedEventRecord>,
    budget: &mut RuntimeReplayBudget,
    previous_cursor: Option<i64>,
    page: ReplayResponse,
    max_events: usize,
    max_serialized_bytes: usize,
) -> Result<Option<i64>> {
    if page.events.len() > MAX_RUNTIME_REPLAY_PAGE_LIMIT {
        anyhow::bail!(
            "runtime replay page contains {} events; maximum is {}",
            page.events.len(),
            MAX_RUNTIME_REPLAY_PAGE_LIMIT
        );
    }
    if let Some(cursor) = page.next_cursor {
        if page.events.is_empty() {
            anyhow::bail!("runtime replay returned an empty page with continuation cursor");
        }
        if cursor <= previous_cursor.unwrap_or(0) {
            anyhow::bail!(
                "runtime replay cursor did not advance monotonically: previous={previous_cursor:?}, next={cursor}"
            );
        }
    }

    let next_event_count = budget
        .total_events
        .checked_add(page.events.len())
        .ok_or_else(|| anyhow::anyhow!("runtime replay event count overflow"))?;
    if next_event_count > max_events {
        anyhow::bail!("runtime replay contains {next_event_count} events; maximum is {max_events}");
    }

    let mut next_serialized_bytes = budget.total_serialized_bytes;
    for (page_index, event) in page.events.iter().enumerate() {
        if budget.total_events > 0 || page_index > 0 {
            next_serialized_bytes = next_serialized_bytes
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("runtime replay byte count overflow"))?;
        }
        let event_bytes = serde_json::to_vec(event)
            .context("serialize runtime replay event for aggregate bound")?
            .len();
        next_serialized_bytes = next_serialized_bytes
            .checked_add(event_bytes)
            .ok_or_else(|| anyhow::anyhow!("runtime replay byte count overflow"))?;
    }
    if next_serialized_bytes > max_serialized_bytes {
        anyhow::bail!(
            "runtime replay serializes to {next_serialized_bytes} bytes; maximum is {max_serialized_bytes}"
        );
    }

    let next_cursor = page.next_cursor;
    all_events.extend(page.events);
    budget.total_events = next_event_count;
    budget.total_serialized_bytes = next_serialized_bytes;
    Ok(next_cursor)
}

pub struct CallbackClient {
    inner: Option<Arc<dyn RuntimeCallbackAPI>>,
    thread_id: String,
    thread_auth_token: String,
}

impl CallbackClient {
    /// Construct from a pre-built runtime API implementation (for tests).
    pub fn from_inner(
        inner: Arc<dyn RuntimeCallbackAPI>,
        thread_id: &str,
        _test_display_path: &str,
        thread_auth_token: &str,
    ) -> Self {
        Self {
            inner: Some(inner),
            thread_id: thread_id.to_string(),
            thread_auth_token: thread_auth_token.to_string(),
        }
    }
}

impl Clone for CallbackClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            thread_id: self.thread_id.clone(),
            thread_auth_token: self.thread_auth_token.clone(),
        }
    }
}

impl CallbackClient {
    pub fn new(callback: &EnvelopeCallback, thread_id: &str, thread_auth_token: &str) -> Self {
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
            thread_auth_token: thread_auth_token.to_string(),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Reach one opaque, test-selected runtime phase and wait for the
    /// qualification parent to crash the daemon.
    ///
    /// Normal launches never call this. The daemon endpoint is absent unless
    /// its explicit crash-qualification feature is enabled.
    #[doc(hidden)]
    pub async fn reach_test_phase_cut(
        &self,
        phase: &str,
    ) -> std::result::Result<(), CallbackError> {
        if phase.is_empty()
            || phase.len() > 128
            || !phase
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(CallbackError::Transport(anyhow::anyhow!(
                "runtime test phase is not bounded lower-snake ASCII"
            )));
        }
        let client = self.inner.as_ref().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "runtime test phase cut called without an inner UDS client"
            ))
        })?;
        client
            .reach_test_phase_cut(&self.thread_id, phase)
            .await
            .map(|_| ())
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

    /// Typed event emitter. The daemon validator delegates to the same enum, so
    /// producer and consumer vocabulary remain in lock-step. Transcript-bearing
    /// events fail closed when the callback channel is absent; advisory events
    /// remain no-ops when disconnected.
    pub async fn append_runtime_event(
        &self,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> Result<()> {
        let storage_class = storage_class_for_payload(event_type, &payload);
        let is_transcript = event_type.is_transcript();
        match &self.inner {
            Some(client) => {
                client
                    .append_event(&self.thread_id, event_type.as_str(), payload, storage_class)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None if is_transcript => Err(anyhow::anyhow!(
                "callback append_runtime_event({}) called without an inner UDS client \
                 (socket missing); transcript-bearing event must not be silently dropped",
                event_type.as_str()
            )),
            None => Ok(()),
        }
    }

    /// Append multiple typed runtime events atomically and publish them in the
    /// supplied order. This is primarily useful for high-frequency ephemeral
    /// progress where one acknowledged daemon RPC per event would otherwise
    /// backpressure the producer.
    ///
    /// The storage class remains a per-event decision derived from the same
    /// typed event and payload policy as [`Self::append_runtime_event`]. A batch
    /// containing any transcript-bearing event fails closed when the callback
    /// channel is absent.
    pub async fn append_runtime_events(
        &self,
        events: Vec<(RuntimeEventType, Value)>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let contains_transcript = events
            .iter()
            .any(|(event_type, _)| event_type.is_transcript());
        match &self.inner {
            Some(client) => {
                let events = events
                    .into_iter()
                    .map(|(event_type, payload)| {
                        let storage_class = storage_class_for_payload(event_type, &payload);
                        serde_json::json!({
                            "event_type": event_type.as_str(),
                            "payload": payload,
                            "storage_class": storage_class,
                        })
                    })
                    .collect();
                client
                    .append_events(&self.thread_id, events)
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                Ok(())
            }
            None if contains_transcript => Err(anyhow::anyhow!(
                "callback append_runtime_events called without an inner UDS client \
                 (socket missing); transcript-bearing event batch must not be silently dropped"
            )),
            None => Ok(()),
        }
    }

    /// Typed provider-attempt budget lifecycle wrappers. All five are
    /// financial authority operations: they hard-fail when the callback
    /// channel is absent — an attempt whose reservation state cannot be
    /// proven must never reach a provider.
    pub async fn provider_attempt_prepare(
        &self,
        params: &ryeos_accounting::ProviderAttemptPrepareParams,
    ) -> Result<ryeos_accounting::ProviderAttemptPrepareResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_prepare",
            params,
            |client, thread_id, value| async move {
                client.provider_attempt_prepare(&thread_id, value).await
            },
        )
        .await
    }

    pub async fn provider_attempt_mark_issued(
        &self,
        params: &ryeos_accounting::ProviderAttemptMarkIssuedParams,
    ) -> Result<ryeos_accounting::ProviderAttemptMarkIssuedResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_mark_issued",
            params,
            |client, thread_id, value| async move {
                client.provider_attempt_mark_issued(&thread_id, value).await
            },
        )
        .await
    }

    pub async fn provider_attempt_settle(
        &self,
        params: &ryeos_accounting::ProviderAttemptSettleParams,
    ) -> Result<ryeos_accounting::ProviderAttemptSettleResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_settle",
            params,
            |client, thread_id, value| async move {
                client.provider_attempt_settle(&thread_id, value).await
            },
        )
        .await
    }

    pub async fn provider_attempt_release_unissued(
        &self,
        params: &ryeos_accounting::ProviderAttemptReleaseUnissuedParams,
    ) -> Result<ryeos_accounting::ProviderAttemptReleaseUnissuedResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_release_unissued",
            params,
            |client, thread_id, value| async move {
                client
                    .provider_attempt_release_unissued(&thread_id, value)
                    .await
            },
        )
        .await
    }

    pub async fn provider_attempt_get(
        &self,
        params: &ryeos_accounting::ProviderAttemptGetParams,
    ) -> Result<Option<ryeos_accounting::ProviderAttemptBudgetRecord>, CallbackError> {
        let raw = self
            .provider_attempt_call_raw("provider_attempt_get", params)
            .await?;
        if raw.is_null() {
            return Ok(None);
        }
        serde_json::from_value(raw).map(Some).map_err(|e| {
            CallbackError::Transport(anyhow::anyhow!(
                "invalid provider_attempt_get response from daemon: {e}"
            ))
        })
    }

    pub async fn provider_attempt_local_stream_start(
        &self,
        params: &ryeos_accounting::ProviderAttemptLocalStreamStartParams,
    ) -> Result<ryeos_accounting::ProviderAttemptLocalStreamStartResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_local_stream_start",
            params,
            |client, thread_id, value| async move {
                client
                    .provider_attempt_local_stream_start(&thread_id, value)
                    .await
            },
        )
        .await
    }

    pub async fn provider_attempt_local_stream_next(
        &self,
        params: &ryeos_accounting::ProviderAttemptLocalStreamNextParams,
    ) -> Result<ryeos_accounting::ProviderAttemptLocalStreamNextResponse, CallbackError> {
        self.provider_attempt_call(
            "provider_attempt_local_stream_next",
            params,
            |client, thread_id, value| async move {
                client
                    .provider_attempt_local_stream_next(&thread_id, value)
                    .await
            },
        )
        .await
    }

    pub async fn provider_attempt_local_stream_control(
        &self,
        params: &ryeos_accounting::ProviderAttemptLocalStreamControlParams,
    ) -> Result<(), CallbackError> {
        let value = self
            .provider_attempt_call_raw_named(
                "provider_attempt_local_stream_control",
                params,
                |client, thread_id, value| async move {
                    client
                        .provider_attempt_local_stream_control(&thread_id, value)
                        .await
                },
            )
            .await?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(CallbackError::Transport(anyhow::anyhow!(
                "invalid provider_attempt_local_stream_control response"
            )));
        }
        Ok(())
    }

    async fn provider_attempt_call<P, R, F, Fut>(
        &self,
        label: &str,
        params: &P,
        call: F,
    ) -> Result<R, CallbackError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
        F: FnOnce(Arc<dyn RuntimeCallbackAPI>, String, Value) -> Fut,
        Fut: std::future::Future<Output = Result<Value, CallbackError>>,
    {
        let client = self.inner.as_ref().cloned().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "callback {label} called without an inner UDS client (socket missing); \
                 provider attempts cannot proceed without daemon ledger authority"
            ))
        })?;
        let value = serde_json::to_value(params).map_err(|e| {
            CallbackError::Transport(anyhow::anyhow!("serialize {label} params: {e}"))
        })?;
        let raw = call(client, self.thread_id.clone(), value).await?;
        serde_json::from_value(raw).map_err(|e| {
            CallbackError::Transport(anyhow::anyhow!("invalid {label} response from daemon: {e}"))
        })
    }

    async fn provider_attempt_call_raw<P: serde::Serialize>(
        &self,
        label: &str,
        params: &P,
    ) -> Result<Value, CallbackError> {
        let client = self.inner.as_ref().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "callback {label} called without an inner UDS client (socket missing); \
                 provider attempts cannot proceed without daemon ledger authority"
            ))
        })?;
        let value = serde_json::to_value(params).map_err(|e| {
            CallbackError::Transport(anyhow::anyhow!("serialize {label} params: {e}"))
        })?;
        client.provider_attempt_get(&self.thread_id, value).await
    }

    async fn provider_attempt_call_raw_named<P, F, Fut>(
        &self,
        label: &str,
        params: &P,
        call: F,
    ) -> Result<Value, CallbackError>
    where
        P: serde::Serialize,
        F: FnOnce(Arc<dyn RuntimeCallbackAPI>, String, Value) -> Fut,
        Fut: std::future::Future<Output = Result<Value, CallbackError>>,
    {
        let client = self.inner.as_ref().cloned().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "callback {label} called without an inner UDS client (socket missing)"
            ))
        })?;
        let value = serde_json::to_value(params).map_err(|error| {
            CallbackError::Transport(anyhow::anyhow!("serialize {label} params: {error}"))
        })?;
        call(client, self.thread_id.clone(), value).await
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
    pub async fn request_continuation(
        &self,
        log_reason: Option<&str>,
        completion: TerminalCompletion,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback request_continuation called without an inner UDS client \
                 (socket missing); the handoff cannot be recorded"
            )
        })?;
        client
            .request_continuation(&self.thread_id, log_reason, completion)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Suspend-critical: ask the daemon to launch a detached follow child and
    /// suspend this thread. Like `request_continuation`, a missing UDS client is a
    /// hard error — a lost suspend would leave the graph believing it handed off.
    /// The caller's own thread + project identity are injected here; the daemon
    /// derives all trust-bearing state from the validated tokens.
    // Keep graph position, child payload, frontier, and terminal handoff
    // explicit: each field participates independently in resume correctness.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_follow_child(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step_count: i64,
        child_item_ref: &str,
        ref_bindings: std::collections::BTreeMap<String, String>,
        child_parameters: Value,
        frontier_id: Option<String>,
        completion: TerminalCompletion,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback spawn_follow_child called without an inner UDS client \
                 (socket missing); the follow suspend cannot be recorded"
            )
        })?;
        let request = crate::callback::SpawnFollowChildRequest {
            thread_id: self.thread_id.clone(),
            graph_run_id: graph_run_id.to_string(),
            follow_node: follow_node.to_string(),
            step_count,
            result_shape: crate::callback::FollowResultShape::Single,
            children: vec![crate::callback::FollowChildSpec {
                item_ref: child_item_ref.to_string(),
                ref_bindings,
                parameters: child_parameters,
                facets: None,
            }],
            launch_window_width: Some(1),
            frontier_id,
            completion,
        };
        client
            .spawn_follow_child(request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    // Keep graph position, launch-window policy, frontier, and terminal handoff
    // explicit: each field participates independently in resume correctness.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_follow_children(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step_count: i64,
        children: Vec<crate::callback::FollowChildSpec>,
        launch_window_width: Option<u32>,
        frontier_id: Option<String>,
        completion: TerminalCompletion,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| anyhow::anyhow!(
            "callback spawn_follow_children called without an inner UDS client (socket missing); the follow suspend cannot be recorded"
        ))?;
        client
            .spawn_follow_child(crate::callback::SpawnFollowChildRequest {
                thread_id: self.thread_id.clone(),
                graph_run_id: graph_run_id.to_string(),
                follow_node: follow_node.to_string(),
                step_count,
                result_shape: crate::callback::FollowResultShape::Cohort,
                children,
                launch_window_width,
                frontier_id,
                completion,
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

    /// Authoritative: a missing callback is an error because a partial state
    /// anchor must never be presented as durable restore evidence.
    pub async fn publish_state_anchor(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback publish_state_anchor called without an inner UDS client (socket missing)"
            )
        })?;
        client
            .publish_state_anchor(&self.thread_id, request)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// Authoritative: exact retry must return the original durable event and
    /// divergent stable-ID reuse must fail the graph commit.
    pub async fn publish_project_observation(
        &self,
        graph_run_id: &str,
        node: &str,
        step: u32,
        observation: crate::ProjectObservationRequest,
    ) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback publish_project_observation called without an inner UDS client (socket missing)"
            )
        })?;
        client
            .publish_project_observation(crate::ProjectObservationPublishParams {
                thread_id: self.thread_id.clone(),
                graph_run_id: graph_run_id.to_string(),
                node: node.to_string(),
                step,
                observation,
            })
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
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

    pub async fn bundle_events_materialize_attachment(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback bundle_events_materialize_attachment called without an inner UDS client \
                 (socket missing); cannot materialize durable bundle event attachment"
            )
        })?;
        client
            .bundle_events_materialize_attachment(&self.thread_id, request)
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

    pub async fn project_snapshot(&self, request: Value) -> Result<Value> {
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback project_snapshot called without an inner UDS client (socket missing)"
            )
        })?;
        client
            .project_snapshot(&self.thread_id, request)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
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

    /// Resume-critical: fold an entire chain (every thread sharing the
    /// `chain_root_id`) into one ordered event list. Hard-fails on disconnect.
    /// NB: a chain namespace can include non-continuation child threads
    /// (compose-context, sub-dispatch); prefer [`Self::replay_thread`] over the
    /// continuation path for rehydration so sibling-branch events don't pollute
    /// the transcript.
    pub async fn replay_chain(&self, chain_root_id: &str) -> Result<ReplayResponse> {
        self.replay_paged(
            "chain_root_id",
            chain_root_id,
            &mut RuntimeReplayBudget::default(),
        )
        .await
    }

    /// Resume-critical: fold ONE thread's own events (thread-scoped), paginated.
    /// Used to fold the linear continuation path turn-by-turn — thread scoping
    /// structurally excludes child/sibling threads that share the chain root.
    pub async fn replay_thread(&self, thread_id: &str) -> Result<ReplayResponse> {
        self.replay_thread_with_budget(thread_id, &mut RuntimeReplayBudget::default())
            .await
    }

    /// Replay one thread while charging a budget shared by the caller's whole
    /// recovery fold. Reuse the same budget when concatenating continuation
    /// threads; creating one budget per thread defeats the aggregate ceiling.
    pub async fn replay_thread_with_budget(
        &self,
        thread_id: &str,
        budget: &mut RuntimeReplayBudget,
    ) -> Result<ReplayResponse> {
        self.replay_paged("thread_id", thread_id, budget).await
    }

    /// Page through `after_chain_seq` cursors for a single replay scope
    /// (`chain_root_id` or `thread_id`) until exhausted, so long histories don't
    /// silently lose events. Hard-fails on disconnect.
    async fn replay_paged(
        &self,
        scope_key: &str,
        scope_value: &str,
        budget: &mut RuntimeReplayBudget,
    ) -> Result<ReplayResponse> {
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
            params.insert(
                "limit".to_string(),
                serde_json::json!(MAX_RUNTIME_REPLAY_PAGE_LIMIT),
            );
            if let Some(cursor) = after_chain_seq {
                params.insert("after_chain_seq".to_string(), serde_json::json!(cursor));
            }
            let raw: Value = client
                .replay_events(Value::Object(params))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let page: ReplayResponse = serde_json::from_value(raw)
                .map_err(|e| anyhow::anyhow!("invalid ReplayResponse from daemon: {e}"))?;
            match append_replay_page(
                &mut all_events,
                budget,
                after_chain_seq,
                page,
                MAX_RUNTIME_REPLAY_TOTAL_EVENTS,
                MAX_RUNTIME_REPLAY_TOTAL_SERIALIZED_BYTES,
            )? {
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
        let payloads = crate::events::encode_cognition_in_payloads(content)?;
        if payloads.len() == 1 {
            return self
                .append_runtime_event(RuntimeEventType::CognitionIn, payloads[0].clone())
                .await;
        }

        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback emit_stimulus called without an inner UDS client \
                 (socket missing); transcript-bearing event batch must not be silently dropped"
            )
        })?;
        let event_type = RuntimeEventType::CognitionIn;
        let storage_class = storage_class_for(event_type);
        let events = payloads
            .into_iter()
            .map(|payload| {
                serde_json::json!({
                    "event_type": event_type.as_str(),
                    "payload": payload,
                    "storage_class": storage_class,
                })
            })
            .collect();
        client
            .append_events(&self.thread_id, events)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    }

    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_runtime_event(
            RuntimeEventType::CognitionIn,
            serde_json::json!({"turn": turn}),
        )
        .await
    }

    /// Resume-critical: seal a cognition cut short by a live interrupt. Emits a
    /// transcript-bearing `cognition_out` with the partial `content`/
    /// `reasoning_content` and `interrupted: true`, and deliberately NO
    /// `tool_calls` — an interrupted cognition didn't complete its tool call, so
    /// the folded wire history carries no unpaired tool call. Durable (indexed):
    /// resume renders it into the provider transcript so the redirect has honest context.
    pub async fn emit_turn_interrupted(
        &self,
        turn: u32,
        content: Option<Value>,
        reasoning_content: Option<String>,
    ) -> Result<()> {
        let mut data = serde_json::json!({ "turn": turn, "interrupted": true });
        if let Some(content) = content {
            data["content"] = content;
        }
        if let Some(reasoning_content) = reasoning_content {
            data["reasoning_content"] = Value::String(reasoning_content);
        }
        self.append_runtime_event(RuntimeEventType::CognitionOut, data)
            .await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to the validator-accepted `cognition_out` event.
    pub async fn emit_turn_complete(
        &self,
        turn: u32,
        tokens: Option<(u64, u64)>,
        content: Option<Value>,
        tool_calls: Option<Value>,
        reasoning_content: Option<String>,
        provider_accounting: Option<Value>,
    ) -> Result<()> {
        let mut data = serde_json::json!({"turn": turn});
        if let Some(content) = content {
            data["content"] = content;
        }
        if let Some(tool_calls) = tool_calls {
            data["tool_calls"] = tool_calls;
        }
        if let Some(reasoning_content) = reasoning_content {
            data["reasoning_content"] = Value::String(reasoning_content);
        }
        if let Some((input, output)) = tokens {
            data["input_tokens"] = serde_json::json!(input);
            data["output_tokens"] = serde_json::json!(output);
        }
        if let Some(provider_accounting) = provider_accounting {
            data["provider_accounting"] = provider_accounting;
        }
        self.append_runtime_event(RuntimeEventType::CognitionOut, data)
            .await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to `tool_call_start`. Includes the thread's effective
    /// capabilities so event consumers can see what the thread was
    /// authorized to do at dispatch time.
    pub async fn emit_tool_dispatch(
        &self,
        operation_id: &str,
        tool: &str,
        call_id: Option<&str>,
        effective_caps: &[String],
    ) -> Result<()> {
        if !crate::callback::valid_action_operation_id(operation_id) {
            anyhow::bail!("tool dispatch operation_id is not a canonical lowercase SHA-256 digest");
        }
        let mut data = serde_json::json!({
            "operation_id": operation_id,
            "tool": tool,
        });
        if let Some(id) = call_id {
            data["call_id"] = serde_json::json!(id);
        }
        data["effective_caps"] = serde_json::json!(effective_caps);
        self.append_runtime_event(RuntimeEventType::ToolCallStart, data)
            .await
    }

    /// Resume-critical: transcript-bearing event; hard-fails on disconnect.
    /// Maps to `tool_call_result`.
    ///
    /// `body` is the exact bounded model-visible result string (the same JSON
    /// string value the runtime pushes into the provider message stream). It is
    /// retained as `result_text` without reparsing so crash recovery reproduces
    /// the identical value and provider wire shape.
    ///
    /// `tool` is the canonical ref (e.g. `apps_tv_tracker_workspace_render_chart`)
    /// so SSE consumers can route results without cross-referencing tool_call_start.
    // Wire-shaped: each argument is one field of the emitted result
    // envelope; eight call sites pass them positionally today.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit_tool_result(
        &self,
        operation_id: &str,
        call_id: &str,
        tool: &str,
        body: &str,
        truncated: bool,
        truncated_reason: Option<&str>,
        result_size_bytes: u64,
        duplicate_of: Option<&str>,
    ) -> Result<()> {
        if !crate::callback::valid_action_operation_id(operation_id) {
            anyhow::bail!("tool result operation_id is not a canonical lowercase SHA-256 digest");
        }
        if body.len() > TOOL_RESULT_INLINE_MAX_BYTES {
            anyhow::bail!(
                "tool result model content is {} bytes; maximum is {}",
                body.len(),
                TOOL_RESULT_INLINE_MAX_BYTES
            );
        }
        let mut data = serde_json::json!({
            "operation_id": operation_id,
            "call_id": call_id,
            "tool": tool,
            "truncated": truncated,
            "result_size_bytes": result_size_bytes,
        });
        data["result_text"] = serde_json::json!(body);
        if let Some(hash) = duplicate_of {
            data["deduplicated"] = serde_json::json!(true);
            data["duplicate_of"] = serde_json::json!(hash);
        }
        if let Some(reason) = truncated_reason {
            data["truncated_reason"] = serde_json::json!(reason);
        }
        self.append_runtime_event(RuntimeEventType::ToolCallResult, data)
            .await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    /// Maps to `thread_failed`.
    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_runtime_event(
            RuntimeEventType::ThreadFailed,
            serde_json::json!({"message": error}),
        )
        .await
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_runtime_event(
            RuntimeEventType::ThreadContinued,
            serde_json::json!({"previous_thread_id": previous_id}),
        )
        .await
    }

    /// Resume-critical: must hard-fail on disconnect.
    /// Emits a `thread_usage` event with the cumulative ThreadUsage
    /// payload. The daemon persists this so resumed threads can reseed
    /// BudgetTracker and Harness.
    pub async fn emit_thread_usage(&self, usage: &ryeos_state::ThreadUsage) -> Result<()> {
        usage.validate().context("invalid thread usage")?;
        let client = self.inner.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "callback emit_thread_usage called without an inner UDS client \
                 (socket missing); thread usage ACK is required for settlement"
            )
        })?;
        let event_type = RuntimeEventType::ThreadUsage;
        let storage_class = storage_class_for(event_type);
        let payload = serde_json::to_value(usage)
            .map_err(|e| anyhow::anyhow!("serialize ThreadUsage: {e}"))?;
        client
            .append_event(&self.thread_id, event_type.as_str(), payload, storage_class)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Advisory: warn-and-continue OK when disconnected.
    pub async fn stream_opened(&self, turn: u32) -> Result<()> {
        self.append_runtime_event(
            RuntimeEventType::StreamOpened,
            serde_json::json!({"turn": turn}),
        )
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
        self.append_runtime_event(RuntimeEventType::StreamSnapshot, wrapped)
            .await
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
        self.append_runtime_event(RuntimeEventType::StreamSnapshot, wrapped)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::{ActionPayload, CallbackError, DispatchActionRequest};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    const TEST_OPERATION_ID: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

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
                storage_class_for_payload(RuntimeEventType::CognitionOut, &payload),
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
            storage_class_for_payload(RuntimeEventType::CognitionOut, &payload),
            StorageClass::Indexed.as_str()
        );
        // An interrupted seal (no progressive keys) is also durable.
        let interrupted = json!({"turn": 1, "content": "par", "interrupted": true});
        assert_eq!(
            storage_class_for_payload(RuntimeEventType::CognitionOut, &interrupted),
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
            _completion: TerminalCompletion,
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
            events: Vec<Value>,
        ) -> Result<Value, CallbackError> {
            let mut recorded = self.events.lock().unwrap();
            for event in events {
                let event_type =
                    event
                        .get("event_type")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            CallbackError::Transport(anyhow::anyhow!(
                                "recorded batch event has no event_type"
                            ))
                        })?;
                let payload = event.get("payload").cloned().ok_or_else(|| {
                    CallbackError::Transport(anyhow::anyhow!("recorded batch event has no payload"))
                })?;
                recorded.push((event_type.to_string(), payload));
            }
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

    #[tokio::test]
    async fn emit_stimulus_atomically_chunks_large_transcript_input() {
        let (callback, recorder) = make_recorder_client();
        let content = "ARC evidence \"quoted\"\n".repeat(16_000);

        callback.emit_stimulus(&content).await.unwrap();

        let events = recorder.events.lock().unwrap();
        assert!(events.len() > 1);
        assert!(
            events
                .iter()
                .all(|(event_type, _)| event_type == "cognition_in")
        );
        let mut assembler = crate::events::CognitionInAssembler::default();
        let mut recovered = None;
        for (_, payload) in events.iter() {
            if let crate::events::CognitionInAssembly::Complete(content) =
                assembler.push(payload).unwrap()
            {
                recovered = Some(content);
            }
        }
        assembler.finish().unwrap();
        assert_eq!(recovered.as_deref(), Some(content.as_str()));
    }

    // ── New emit_tool_result tests ───────────────────────────────────

    #[tokio::test]
    async fn emit_tool_result_retains_exact_model_visible_text() {
        let (cb, recorder) = make_recorder_client();
        let body = r#"{"ok":true,"workspace_card":{"chart_kind":"callout"}}"#;
        cb.emit_tool_result(
            TEST_OPERATION_ID,
            "call_1",
            "test/render_chart",
            body,
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
        assert_eq!(evt["result_text"], body);
        assert!(evt.get("result").is_none());
        assert!(evt.get("truncated_reason").is_none());
    }

    #[tokio::test]
    async fn tool_start_and_result_retain_one_exact_operation_id() {
        let (callback, recorder) = make_recorder_client();
        callback
            .emit_tool_dispatch(TEST_OPERATION_ID, "test/tool", Some("call-1"), &[])
            .await
            .unwrap();
        callback
            .emit_tool_result(
                TEST_OPERATION_ID,
                "call-1",
                "test/tool",
                r#"{"ok":true}"#,
                false,
                None,
                11,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            recorder.last("tool_call_start").unwrap()["operation_id"],
            TEST_OPERATION_ID
        );
        assert_eq!(
            recorder.last("tool_call_result").unwrap()["operation_id"],
            TEST_OPERATION_ID
        );
    }

    #[tokio::test]
    async fn tool_start_and_result_reject_uppercase_operation_ids_before_append() {
        let (callback, recorder) = make_recorder_client();
        let uppercase = "A".repeat(64);
        let start = callback
            .emit_tool_dispatch(&uppercase, "test/tool", Some("call-1"), &[])
            .await
            .unwrap_err();
        let result = callback
            .emit_tool_result(
                &uppercase,
                "call-1",
                "test/tool",
                r#"{"ok":true}"#,
                false,
                None,
                11,
                None,
            )
            .await
            .unwrap_err();
        assert!(start.to_string().contains("canonical lowercase"));
        assert!(result.to_string().contains("canonical lowercase"));
        assert!(recorder.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emit_tool_result_rejects_unbounded_model_content_before_append() {
        let (cb, recorder) = make_recorder_client();
        let body = "x".repeat(TOOL_RESULT_INLINE_MAX_BYTES + 1);
        let error = cb
            .emit_tool_result(
                TEST_OPERATION_ID,
                "call_2",
                "test/search",
                &body,
                true,
                Some("result_guard"),
                body.len() as u64,
                None,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("maximum"));
        assert!(recorder.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emit_tool_result_inlines_body_with_nested_json() {
        let (cb, recorder) = make_recorder_client();
        let body = r#"{"ok":true,"data":{"nested":[1,2,3]}}"#;
        cb.emit_tool_result(
            TEST_OPERATION_ID,
            "call_4",
            "test/nested",
            body,
            false,
            None,
            body.len() as u64,
            None,
        )
        .await
        .unwrap();
        let evt = recorder.last("tool_call_result").unwrap();
        assert_eq!(evt["result_text"], body);
        assert!(evt.get("result").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_preserves_invalid_json_body_without_panicking() {
        let (cb, recorder) = make_recorder_client();
        let body = "[truncated json";
        cb.emit_tool_result(
            TEST_OPERATION_ID,
            "call_bad_json",
            "test/search",
            body,
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
        assert!(evt.get("result_parse_error").is_none());
    }

    #[tokio::test]
    async fn emit_tool_result_marks_deduplicated_body_as_text_without_parse_error() {
        let (cb, recorder) = make_recorder_client();
        let body = "[duplicate result omitted — hash deadbeefdeadbeef]";
        cb.emit_tool_result(
            TEST_OPERATION_ID,
            "call_duplicate",
            "test/search",
            body,
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
        CallbackClient::new(&make_callback(), "T-test", "tat-test")
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
            action: ActionPayload {
                operation_id: Some(TEST_OPERATION_ID.to_string()),
                item_id: "my/tool".to_string(),
                ref_bindings: std::collections::BTreeMap::new(),
                params: json!({}),
                thread: "inline".to_string(),
                call: None,
                facets: None,
                launch_window: None,
            },
            hook_dispatch: None,
            effect_dispatch: None,
        };
        let err = client.dispatch_action(req).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("socket missing") || msg.contains("inner UDS client"),
            "expected disconnect error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn append_runtime_event_transcript_types_error_when_disconnected() {
        let client = make_client();
        for event_type in [
            RuntimeEventType::CognitionIn,
            RuntimeEventType::CognitionOut,
            RuntimeEventType::ToolCallStart,
            RuntimeEventType::ToolCallResult,
        ] {
            let err = client
                .append_runtime_event(event_type, json!({"turn": 1}))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("socket missing"),
                "{event_type:?}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn append_runtime_events_preserves_batch_order() {
        let (client, recorder) = make_recorder_client();
        client
            .append_runtime_events(vec![
                (
                    RuntimeEventType::CognitionOut,
                    json!({"turn": 1, "delta": "first"}),
                ),
                (
                    RuntimeEventType::CognitionOut,
                    json!({"turn": 1, "delta": " second"}),
                ),
                (RuntimeEventType::StreamSnapshot, json!({"kind": "after"})),
            ])
            .await
            .unwrap();

        let events = recorder.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            (
                "cognition_out".to_string(),
                json!({"turn": 1, "delta": "first"})
            )
        );
        assert_eq!(
            events[1],
            (
                "cognition_out".to_string(),
                json!({"turn": 1, "delta": " second"})
            )
        );
        assert_eq!(
            events[2],
            ("stream_snapshot".to_string(), json!({"kind": "after"}))
        );
    }

    #[tokio::test]
    async fn append_runtime_events_transcript_batch_errors_when_disconnected() {
        let client = make_client();
        let error = client
            .append_runtime_events(vec![(
                RuntimeEventType::CognitionOut,
                json!({"turn": 1, "delta": "text"}),
            )])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("socket missing"), "{error}");
    }

    #[tokio::test]
    async fn append_runtime_event_non_transcript_type_noops_when_disconnected() {
        let client = make_client();
        client
            .append_runtime_event(RuntimeEventType::StreamOpened, json!({"turn": 1}))
            .await
            .unwrap();
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
                status: crate::ThreadTerminalStatus::Completed,
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

    #[test]
    fn thread_id_accessor() {
        let client = make_client();
        assert_eq!(client.thread_id(), "T-test");
    }

    #[test]
    fn clone_preserves_fields() {
        let client = make_client();
        let cloned = client.clone();
        assert_eq!(cloned.thread_id(), "T-test");
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

    /// Every typed event the runtime emits must serialize to a name and storage
    /// class accepted by the daemon.
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
        let runtime_emits = [
            RuntimeEventType::CognitionIn,
            RuntimeEventType::CognitionOut,
            RuntimeEventType::ToolCallStart,
            RuntimeEventType::ToolCallResult,
            RuntimeEventType::ThreadFailed,
            RuntimeEventType::ThreadContinued,
            RuntimeEventType::StreamSnapshot,
            RuntimeEventType::StreamOpened,
            RuntimeEventType::CognitionReasoning,
            RuntimeEventType::ThreadUsage,
        ];

        for ev in runtime_emits {
            let wire_name = ev.as_str();
            assert!(
                VALIDATOR_EVENTS.contains(&wire_name),
                "runtime emits {wire_name:?} but the daemon's validate_event_type \
                 does not accept it — runtime <> daemon vocabulary drift"
            );
            let sc = storage_class_for(ev);
            assert!(
                VALIDATOR_STORAGE.contains(&sc),
                "storage_class_for({wire_name:?}) returned {sc:?} which is not in \
                 the daemon's accepted set"
            );
        }
    }

    #[test]
    fn cognition_out_progressive_payloads_are_ephemeral() {
        assert_eq!(
            storage_class_for_payload(
                RuntimeEventType::CognitionOut,
                &json!({"turn": 1, "delta": "hi"}),
            ),
            "ephemeral"
        );
        assert_eq!(
            storage_class_for_payload(
                RuntimeEventType::CognitionOut,
                &json!({"turn": 1, "tool_use_partial": {"id": "c", "delta": "{}"}}),
            ),
            "ephemeral"
        );
        assert_eq!(
            storage_class_for_payload(RuntimeEventType::CognitionOut, &json!({"turn": 1})),
            "indexed"
        );
    }

    #[tokio::test]
    async fn emit_turn_complete_persists_final_cognition() {
        let (cb, recorder) = make_recorder_client();
        cb.emit_turn_complete(
            1,
            Some((10, 5)),
            Some(json!("final answer")),
            Some(json!([{"id": "c1", "name": "search", "arguments": {"q": "x"}}])),
            Some("hidden".to_owned()),
            Some(json!({
                "requested_output_tokens": 32768,
                "generation_header_id": "generation-1",
                "contract_anomalies": ["reported output exceeds request limit"],
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
        assert_eq!(evt["provider_accounting"]["requested_output_tokens"], 32768);
        assert_eq!(
            evt["provider_accounting"]["generation_header_id"],
            "generation-1"
        );
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
            _: TerminalCompletion,
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

    fn replay_event(event_type: &str, payload: Value) -> ReplayedEventRecord {
        ReplayedEventRecord {
            event_type: event_type.to_string(),
            payload,
        }
    }

    #[test]
    fn replay_page_requires_a_nonempty_monotonically_advancing_cursor() {
        let mut events = Vec::new();
        let mut budget = RuntimeReplayBudget::default();
        let cursor = append_replay_page(
            &mut events,
            &mut budget,
            None,
            ReplayResponse {
                events: vec![replay_event("a", json!({}))],
                next_cursor: Some(2),
            },
            8,
            1024,
        )
        .unwrap();
        assert_eq!(cursor, Some(2));

        let repeated = append_replay_page(
            &mut events,
            &mut budget,
            cursor,
            ReplayResponse {
                events: vec![replay_event("b", json!({}))],
                next_cursor: Some(2),
            },
            8,
            1024,
        )
        .unwrap_err();
        assert!(format!("{repeated:#}").contains("did not advance monotonically"));

        let zero = append_replay_page(
            &mut Vec::new(),
            &mut RuntimeReplayBudget::default(),
            None,
            ReplayResponse {
                events: vec![replay_event("zero", json!({}))],
                next_cursor: Some(0),
            },
            8,
            1024,
        )
        .unwrap_err();
        assert!(format!("{zero:#}").contains("did not advance monotonically"));

        let empty = append_replay_page(
            &mut Vec::new(),
            &mut RuntimeReplayBudget::default(),
            None,
            ReplayResponse {
                events: vec![],
                next_cursor: Some(1),
            },
            8,
            1024,
        )
        .unwrap_err();
        assert!(format!("{empty:#}").contains("empty page with continuation cursor"));
    }

    #[test]
    fn replay_page_enforces_page_count_total_count_and_exact_byte_bounds() {
        let oversized_page = ReplayResponse {
            events: (0..=MAX_RUNTIME_REPLAY_PAGE_LIMIT)
                .map(|index| replay_event("event", json!({"index": index})))
                .collect(),
            next_cursor: None,
        };
        let page_error = append_replay_page(
            &mut Vec::new(),
            &mut RuntimeReplayBudget::default(),
            None,
            oversized_page,
            usize::MAX,
            usize::MAX,
        )
        .unwrap_err();
        assert!(format!("{page_error:#}").contains("runtime replay page contains"));

        let two_events = vec![
            replay_event("a", json!({"value": 1})),
            replay_event("b", json!({"value": 2})),
        ];
        let count_error = append_replay_page(
            &mut Vec::new(),
            &mut RuntimeReplayBudget::default(),
            None,
            ReplayResponse {
                events: two_events.clone(),
                next_cursor: None,
            },
            1,
            usize::MAX,
        )
        .unwrap_err();
        assert!(format!("{count_error:#}").contains("2 events; maximum is 1"));

        let exact_bytes = serde_json::to_vec(&two_events).unwrap().len();
        let byte_error = append_replay_page(
            &mut Vec::new(),
            &mut RuntimeReplayBudget::default(),
            None,
            ReplayResponse {
                events: two_events.clone(),
                next_cursor: None,
            },
            2,
            exact_bytes - 1,
        )
        .unwrap_err();
        assert!(format!("{byte_error:#}").contains("runtime replay serializes to"));

        let mut accepted = Vec::new();
        let mut accepted_budget = RuntimeReplayBudget::default();
        assert_eq!(
            append_replay_page(
                &mut accepted,
                &mut accepted_budget,
                None,
                ReplayResponse {
                    events: two_events,
                    next_cursor: None,
                },
                2,
                exact_bytes,
            )
            .unwrap(),
            None
        );
        assert_eq!(accepted_budget.total_serialized_bytes, exact_bytes);
    }

    #[test]
    fn replay_budget_rejects_aggregate_overflow_across_thread_scopes() {
        let mut budget = RuntimeReplayBudget::default();
        let mut first_thread_events = Vec::new();
        append_replay_page(
            &mut first_thread_events,
            &mut budget,
            None,
            ReplayResponse {
                events: vec![replay_event("first", json!({}))],
                next_cursor: None,
            },
            1,
            usize::MAX,
        )
        .unwrap();

        let mut second_thread_events = Vec::new();
        let error = append_replay_page(
            &mut second_thread_events,
            &mut budget,
            None,
            ReplayResponse {
                events: vec![replay_event("second", json!({}))],
                next_cursor: None,
            },
            1,
            usize::MAX,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("2 events; maximum is 1"));
        assert!(second_thread_events.is_empty());
    }
}
