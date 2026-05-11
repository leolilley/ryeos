use std::time::Instant;

use serde_json::{json, Value};

use crate::budget::BudgetTracker;
use ryeos_runtime::callback_client::CallbackClient;
use crate::continuation::ContinuationCheck;
use crate::directive::{ExecutionConfig, FinishReason, OutputSpec, ProviderMessage, SamplingConfig, StreamEvent, ToolSchema};
use crate::dispatcher::{DispatchKind, Dispatcher};
use crate::harness::{HookAction, Harness};
use ryeos_runtime::envelope::RuntimeResult;
use crate::result_guard::ResultGuard;
use crate::resume::ResumeState;

#[derive(Debug)]
pub enum State {
    Init,
    CheckingLimits,
    CallingProvider,
    Streaming {
        // The full sequence of streamed events, kept for diagnostic
        // counts. Real per-delta `cognition_out` persistence already
        // happened inside `provider_adapter::call_provider_streaming`,
        // and the typed assistant message (text + tool_calls) was
        // pushed onto `self.messages` before this state runs.
        events: Vec<StreamEvent>,
    },
    ParsingResponse,
    DispatchingTools {
        pending: Vec<crate::directive::ToolCall>,
        index: usize,
    },
    ProcessingToolResult {
        call_id: String,
        tool_name: String,
        raw_args: String,
        pending: Vec<crate::directive::ToolCall>,
        index: usize,
    },
    /// directive_return lifecycle signal: the LLM invoked
    /// `directive_return` (a provider-API tool-call convention, not
    /// a real dispatchable tool). The runner intercepts by name in
    /// `DispatchingTools`, validates declared outputs, fires both
    /// `tool_call_start`/`tool_call_result` events for chain
    /// visibility, publishes the artifact, and finalizes the thread.
    ProcessingDirectiveReturn {
        call_id: String,
        raw_args: String,
    },
    FiringHooks {
        event: String,
        context: Value,
        resume_to: Box<State>,
    },
    CheckingContinuation,

    Finalizing {
        result: Value,
    },
    Continued,
    Errored {
        error: String,
    },
    Cancelled,
}

pub struct Runner {
    messages: Vec<ProviderMessage>,
    tools: Vec<ToolSchema>,
    dispatcher: Dispatcher,
    harness: Harness,
    budget: BudgetTracker,
    callback: CallbackClient,
    continuation: ContinuationCheck,
    result_guard: ResultGuard,
    provider_config: crate::directive::ProviderConfig,
    provider_id: String,
    /// Profile name that matched during daemon preflight.
    matched_profile: Option<String>,
    /// SHA-256 of the canonical-JSON provider config from the snapshot.
    config_hash: String,
    execution: ExecutionConfig,
    model_name: String,
    thread_id: String,
    initial_turn: u32,
    hooks: Vec<ryeos_runtime::HookDefinition>,
    /// Declared directive outputs — used to validate `directive_return`
    /// arguments before finalization. `None` = no outputs declared,
    /// any arguments accepted.
    directive_outputs: Option<Vec<OutputSpec>>,
    /// LLM sampling parameters from the directive's `model.sampling`.
    /// Passed to the provider adapter for inclusion in request body.
    /// `None` = use provider defaults.
    sampling: Option<SamplingConfig>,
    /// Shared HTTP client — created once and reused across all turns.
    /// Connection pooling keeps TCP/TLS handshakes to a minimum.
    http_client: reqwest::Client,
}

struct RunGuard {
    finalized: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.finalized {
            tracing::warn!("RunGuard dropped without finalization");
        }
    }
}

/// Synthesize a `directive_return` ToolSchema from declared outputs.
///
/// `directive_return` is a lifecycle signal recognized by name in the
/// runner — it's never dispatched to a real tool. Advertising it as a
/// provider tool gives the LLM a first-class way to emit structured
/// outputs instead of relying on prose conventions. All declared
/// outputs are marked `required` because the runner's
/// `ProcessingDirectiveReturn` validator rejects calls missing any of
/// them (see runner.rs:644-654).
fn build_directive_return_tool(outputs: &[OutputSpec]) -> ToolSchema {
    use serde_json::Map;
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::with_capacity(outputs.len());
    for o in outputs {
        let mut prop = Map::new();
        prop.insert(
            "type".to_string(),
            json!(o.r#type.clone().unwrap_or_else(|| "string".to_string())),
        );
        if let Some(desc) = &o.description {
            prop.insert("description".to_string(), json!(desc));
        }
        props.insert(o.name.clone(), Value::Object(prop));
        required.push(json!(o.name));
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(props));
    schema.insert("required".to_string(), Value::Array(required));
    ToolSchema {
        name: "directive_return".to_string(),
        // Synthetic item_id; the dispatcher rejects this name before
        // any cap/permission lookup so the value is never resolved.
        item_id: "lifecycle:directive_return".to_string(),
        description: Some(
            "Return final structured outputs and finish the directive. \
             Call this exactly once when you have a complete answer."
                .to_string(),
        ),
        input_schema: Some(Value::Object(schema)),
    }
}

/// Record a callback failure as a non-fatal warning. Replaces the
/// `let _ = self.callback.append_event(...)` pattern that silently
/// dropped event-store rejection (V5.4 P2.2 review finding).
fn record_callback_warning(
    warnings: &mut Vec<String>,
    event_label: &str,
    result: anyhow::Result<()>,
) {
    if let Err(e) = result {
        warnings.push(format!("callback append_event({event_label}) failed: {e}"));
    }
}

pub struct RunnerConfig {
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub harness: Harness,
    pub budget: BudgetTracker,
    pub callback: CallbackClient,
    pub context_window: u64,
    pub provider_config: crate::directive::ProviderConfig,
    pub provider_id: String,
    /// Profile name that matched during daemon preflight.
    pub matched_profile: Option<String>,
    /// SHA-256 of the canonical-JSON provider config from the snapshot.
    pub config_hash: String,
    pub execution: ExecutionConfig,
    pub model_name: String,
    pub thread_id: String,
    pub hooks: Vec<ryeos_runtime::HookDefinition>,
    pub outputs: Option<Vec<OutputSpec>>,
    pub sampling: Option<SamplingConfig>,
}

impl Runner {
    pub fn new(config: RunnerConfig) -> Self {
        let RunnerConfig {
            messages,
            tools,
            system_prompt,
            harness,
            budget,
            callback,
            context_window,
            provider_config,
            provider_id,
            execution,
            model_name,
            thread_id,
            hooks,
            outputs,
            sampling,
            matched_profile,
            config_hash,
        } = config;
        let mut initial_messages = Vec::new();

        if let Some(ref sys) = system_prompt {
            initial_messages.push(ProviderMessage {
                role: "system".to_string(),
                content: Some(json!(sys)),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        initial_messages.extend(messages);

        let effective_caps = harness.effective_caps().to_vec();
        let dispatcher = Dispatcher::new(tools.clone(), effective_caps);

        Self {
            messages: initial_messages,
            tools,
            dispatcher,
            harness,
            budget,
            callback,
            continuation: ContinuationCheck::new(context_window),
            result_guard: ResultGuard::new(),
            provider_config,
            provider_id,
            matched_profile,
            config_hash,
            execution,
            model_name,
            thread_id,
            initial_turn: 0,
            hooks,
            directive_outputs: outputs,
            sampling,
            http_client: reqwest::Client::builder()
                .pool_max_idle_per_host(8)
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .expect("reqwest client builder"),
        }
    }

    pub fn from_resume(
        resume: ResumeState,
        mut config: RunnerConfig,
    ) -> Self {
        if let Some(ref usage) = resume.thread_usage {
            config.harness.reseed(usage.completed_turns, usage.input_tokens + usage.output_tokens, usage.spend_usd, usage.spawns_used);
            config.budget.reseed(usage.input_tokens, usage.output_tokens, usage.spend_usd);
        }
        config.messages = resume.messages;
        let mut runner = Self::new(config);
        runner.initial_turn = resume.turns_completed;
        runner
    }

    pub fn messages(&self) -> &[ProviderMessage] {
        &self.messages
    }

    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    pub async fn run(&mut self) -> RuntimeResult {
        let mut guard = RunGuard { finalized: false };
        let mut state = State::Init;
        let mut turn = self.initial_turn;
        let max_turns = 100;
        // Collected non-fatal callback failures. P2.2 — runtime no
        // longer silently drops `append_event` errors; everything that
        // would have hit `let _ = ...` is now recorded here and
        // surfaced via `RuntimeResult.warnings` so the daemon /
        // operator can see contract drift (rejected event names,
        // transport hiccups, etc.).
        let mut warnings: Vec<String> = Vec::new();

        loop {
            state = match state {
                State::Init => {
                    if let Err(e) = self.callback.mark_running().await {
                        state = State::Errored { error: format!("resume-critical callback mark_running failed: {e}") };
                        continue;
                    }
                    State::CheckingLimits
                }

                State::CheckingLimits => {
                    if let Err(e) = self.harness.check_limits() {
                        if e == "cancelled" {
                            state = State::Cancelled;
                            continue;
                        }
                        state = State::Errored { error: e };
                        continue;
                    }
                    if turn >= max_turns {
                        state = State::Errored {
                            error: "max turns reached".to_string(),
                        };
                        continue;
                    }
                    State::CallingProvider
                }

                State::CallingProvider => {
                    let turn_start = Instant::now();
                    self.harness.record_turn();
                    turn += 1;

                    if let Err(e) = self.callback.emit_turn_start(turn).await {
                        state = State::Errored { error: format!("resume-critical callback emit_turn_start failed: {e}") };
                        continue;
                    }

                    if self.budget.is_exhausted() {
                        state = State::Errored { error: "budget_exceeded".to_string() };
                        continue;
                    }

                    let cancel_flag = self.harness.cancelled_flag();
                    // Filter tools by effective_caps so the LLM only sees
                    // tools it can actually call (saves context, avoids the
                    // "model names a tool the dispatcher would reject" path).
                    let visible_tools = crate::provider_adapter::tools::filter_tools_by_caps(
                        &self.tools,
                        self.harness.effective_caps(),
                    );
                    // Map borrowed refs back to owned slice for the adapter.
                    let mut visible_tools_owned: Vec<_> = visible_tools.into_iter().cloned().collect();
                    // If the directive declared `outputs:`, synthesize a
                    // `directive_return` tool from them so the LLM has a
                    // first-class function to call when it has the answer.
                    // Without this, the model can only mention "directive_return"
                    // in plain text — which leaves `result.outputs` empty and
                    // the directive never finalizes via the lifecycle path.
                    // The runner intercepts calls to `directive_return` by
                    // name in `DispatchingTools`; the dispatcher rejects it
                    // as a real tool, so this synthetic schema is purely a
                    // provider-API hint.
                    if let Some(ref outputs) = self.directive_outputs {
                        if !outputs.is_empty() {
                            visible_tools_owned.push(build_directive_return_tool(outputs));
                        }
                    }
                    match crate::provider_adapter::call_provider_streaming(
                        crate::provider_adapter::StreamingCallInput {
                            client: &self.http_client,
                            provider: &self.provider_config,
                            provider_id: &self.provider_id,
                            matched_profile: self.matched_profile.as_deref(),
                            config_hash: &self.config_hash,
                            execution: &self.execution,
                            model: &self.model_name,
                            messages: &self.messages,
                            tools: &visible_tools_owned,
                            callback: &self.callback,
                            turn,
                            sampling: self.sampling.as_ref(),
                            cancel_flag: Some(cancel_flag),
                        },
                    )
                    .await
                    {
                        Ok((resp, events)) => {
                            let input_tok = resp.usage.as_ref().map_or(0, |u| u.input_tokens);
                            let output_tok = resp.usage.as_ref().map_or(0, |u| u.output_tokens);
                            let usd = self.compute_cost(input_tok, output_tok);

                            let proposed_usage = ryeos_state::ThreadUsage {
                                completed_turns: self.harness.turns_used(),
                                input_tokens: self.budget.cost().input_tokens + input_tok,
                                output_tokens: self.budget.cost().output_tokens + output_tok,
                                spend_usd: self.budget.cost().total_usd + usd,
                                spawns_used: self.harness.spawns_used(),
                                started_at: lillux::time::iso8601_now(),
                                settled_at: lillux::time::iso8601_now(),
                                last_settled_turn_seq: turn as u64,
                                elapsed_ms: turn_start.elapsed().as_millis() as u64,
                            };

                            if let Err(e) = self.callback.emit_thread_usage(&proposed_usage).await {
                                state = State::Errored { error: format!("resume-critical callback emit_thread_usage failed: {e}") };
                                continue;
                            }

                            if let Some(ref usage) = resp.usage {
                                self.harness.record_tokens(usage.input_tokens, usage.output_tokens);
                                self.harness.record_spend(usd);
                                self.budget.report(usage.input_tokens, usage.output_tokens, usd);
                            }
                            self.messages.push(resp.message.clone());
                            if let Err(e) = self.callback
                                .emit_turn_complete(
                                    turn,
                                    resp.usage.as_ref().map(|u| (u.input_tokens, u.output_tokens)),
                                )
                                .await
                            {
                                state = State::Errored { error: format!("resume-critical callback emit_turn_complete failed: {e}") };
                                continue;
                            }
                            if let Some(ref reason) = resp.finish_reason {
                                tracing::debug!(finish_reason = %reason, "provider response");
                            }

                            // Real StreamEvents already persisted as
                            // cognition_out events during streaming.
                            // The runner's State::Streaming pass is now
                            // diagnostic-only — message.tool_calls and
                            // message.content are the source of truth.
                            State::Streaming { events }
                        }
                        Err(e) => State::Errored {
                            error: e.to_string(),
                        },
                    }
                }

                State::Streaming { events } => {
                    record_callback_warning(
                        &mut warnings,
                        "stream_opened",
                        self.callback
                            .append_event("stream_opened", json!({"turn": turn}))
                            .await,
                    );

                    let mut delta_count = 0u32;
                    let mut tool_use_count = 0u32;
                    let mut reasoning_count = 0u32;
                    let mut warning_count = 0u32;
                    let mut finish_reason: Option<FinishReason> = None;

                    for ev in &events {
                        match ev {
                            StreamEvent::Delta(_) => delta_count += 1,
                            StreamEvent::ToolUse { .. } => tool_use_count += 1,
                            StreamEvent::ToolUsePartial { .. } => {
                                // Diagnostic-only count; the cognition_out
                                // event was already appended by the
                                // streaming layer for the daemon to fan out.
                            }
                            StreamEvent::Finish { reason, raw } => {
                                finish_reason = Some(*reason);
                                if let Some(raw_str) = raw {
                                    tracing::debug!(
                                        finish_reason = ?reason,
                                        raw = %raw_str,
                                        "stream finished"
                                    );
                                }
                            }
                            StreamEvent::ReasoningDelta(text) => {
                                reasoning_count += 1;
                                tracing::trace!(
                                    len = text.len(),
                                    "reasoning delta received"
                                );
                            }
                            StreamEvent::Usage(update) => {
                                // Mid-stream usage is informational — the
                                // cumulative total arrives in
                                // AdapterResponse.usage and is recorded in
                                // CallingProvider. Logging here lets operators
                                // see token growth in the trace.
                                tracing::debug!(
                                    input = ?update.input_tokens,
                                    output = ?update.output_tokens,
                                    reasoning = ?update.reasoning_tokens,
                                    cache_read = ?update.cache_read_tokens,
                                    cache_write = ?update.cache_write_tokens,
                                    "mid-stream usage update"
                                );
                            }
                            StreamEvent::Warning { code, message } => {
                                warning_count += 1;
                                tracing::warn!(
                                    code = %code,
                                    message = %message,
                                    "provider warning during streaming"
                                );
                                warnings.push(format!(
                                    "provider warning: [{code}] {message}"
                                ));
                            }
                        }
                    }
                    tracing::debug!(
                        delta_count,
                        tool_use_count,
                        reasoning_count,
                        warning_count,
                        finish_reason = ?finish_reason,
                        "stream events processed"
                    );

                    State::ParsingResponse
                }

                State::ParsingResponse => {
                    let last = self.messages.last().cloned();
                    match last {
                        Some(msg) => {
                            let has_tool_calls = msg
                                .tool_calls
                                .as_ref()
                                .is_some_and(|tc| !tc.is_empty());
                            let has_content = msg
                                .content
                                .as_ref()
                                .is_some_and(|c| !c.is_null() && c.as_str().is_none_or(|s| !s.is_empty()));

                            if has_tool_calls {
                                if let Some(ref tool_calls) = msg.tool_calls {
                                    State::DispatchingTools {
                                        pending: tool_calls.clone(),
                                        index: 0,
                                    }
                                } else {
                                    State::CheckingContinuation
                                }
                            } else if has_content || msg.content.is_some() {
                                let content = msg
                                    .content
                                    .unwrap_or(Value::Null);
                                State::Finalizing { result: content }
                            } else {
                                State::CheckingContinuation
                            }
                        }
                        None => State::Errored {
                            error: "no response from provider".to_string(),
                        },
                    }
                }

                State::DispatchingTools { pending, index } => {
                    if self.harness.is_cancelled() {
                        state = State::Cancelled;
                        continue;
                    }
                    if index >= pending.len() {
                        State::CheckingContinuation
                    } else {
                        let tc = &pending[index];
                        if let Err(e) = self.callback.emit_tool_dispatch(&tc.name, tc.id.as_deref(), self.harness.effective_caps()).await {
                            state = State::Errored { error: format!("resume-critical callback emit_tool_dispatch failed: {e}") };
                            continue;
                        }

                        // directive_return: lifecycle signal, not a tool.
                        // Bypass permission check and dispatch entirely;
                        // the runner handles output validation,
                        // event emission, and finalization inline.
                        if tc.name == "directive_return" {
                            State::ProcessingDirectiveReturn {
                                call_id: tc.id.clone().unwrap_or_default(),
                                raw_args: tc.arguments.to_string(),
                            }
                        } else {
                            // Permission check deferred to the dispatcher,
                            // which uses the canonical ref (not the LLM-
                            // emitted tool name) for cap matching.
                            State::ProcessingToolResult {
                                call_id: tc.id.clone().unwrap_or_default(),
                                tool_name: tc.name.clone(),
                                raw_args: tc.arguments.to_string(),
                                pending,
                                index,
                            }
                        }
                    }
                }

                State::ProcessingToolResult { call_id, tool_name, raw_args, pending, index } => {
                    /// Tracks tool result metadata for SSE emission.
                    struct ToolResult {
                        tool: String,
                        content: String,
                        raw_size: u64,
                        result_guard_truncated: bool,
                        truncated_reason_override: Option<&'static str>,
                    }

                    let tool_result: ToolResult = match self.dispatcher.resolve(&tool_name, &raw_args, Some(call_id.clone())) {
                        Ok(dispatch_result) => {
                            // Record spawn for child executions (directive/graph)
                            match dispatch_result.dispatch_kind {
                                DispatchKind::DirectiveChild | DispatchKind::GraphChild => {
                                    self.harness.record_spawn();
                                }
                                DispatchKind::Tool => {}
                            }

                            // Risk assessment before dispatch
                            let required_cap = format!("ryeos.execute.tool.{}", dispatch_result.canonical_ref);
                            let risk = self.harness.assess(&required_cap);
                            if risk.blocked {
                                tracing::warn!(
                                    tool = %dispatch_result.canonical_ref,
                                    call_id = ?dispatch_result.call_id,
                                    risk_level = %risk.level,
                                    requires_ack = risk.requires_ack,
                                    "tool call blocked by risk policy"
                                );
                                let body_str = serde_json::to_string(&json!({"error": format!("blocked by risk policy: {}", dispatch_result.canonical_ref)}))
                                    .unwrap_or_else(|_| "{\"error\":\"blocked\"}".to_string());
                                // Risk-policy block surfaces as a
                                // `tool_call_result` with a `blocked`
                                // status payload so the daemon's
                                // event-store validator (which has no
                                // `risk_blocked` name) accepts it.
                                record_callback_warning(
                                    &mut warnings,
                                    "tool_call_result(blocked)",
                                    self.callback
                                        .append_event(
                                            "tool_call_result",
                                            json!({
                                                "tool": dispatch_result.canonical_ref,
                                                "call_id": dispatch_result.call_id,
                                                "blocked": true,
                                                "level": risk.level,
                                                "requires_ack": risk.requires_ack,
                                            }),
                                        )
                                        .await,
                                );
                                ToolResult {
                                    tool: dispatch_result.canonical_ref.clone(),
                                    raw_size: body_str.len() as u64,
                                    content: body_str,
                                    result_guard_truncated: false,
                                    truncated_reason_override: Some("error_envelope"),
                                }
                            } else {
                                match self.callback.dispatch_action(ryeos_runtime::callback::DispatchActionRequest {
                                    thread_id: self.thread_id.clone(),
                                    project_path: self.callback.project_path().to_string(),
                                    action: ryeos_runtime::callback::ActionPayload {
                                        item_id: dispatch_result.canonical_ref.clone(),
                                        params: dispatch_result.arguments.clone(),
                                        thread: "inline".to_string(),
                                    },
                                }).await {
                                    Ok(response) => {
                                        // Model-visible bytes are ONLY the leaf
                                        // dispatcher's `result` — never the
                                        // wrapping `thread` snapshot. Without
                                        // this, the LLM saw the whole
                                        // {thread, result} envelope and the
                                        // child-thread metadata leaked into
                                        // every tool-result message.
                                        let raw_bytes = serde_json::to_vec(&response.result)
                                            .unwrap_or_else(|e| {
                                                tracing::warn!("failed to serialize dispatch result: {e}");
                                                Vec::new()
                                            });
                                        let raw_size = raw_bytes.len() as u64;
                                        let processed_bytes = self.result_guard.process_bytes(&raw_bytes);
                                        let result_guard_truncated = processed_bytes.len() != raw_bytes.len();
                                        let content = String::from_utf8_lossy(&processed_bytes).to_string();
                                        ToolResult {
                                            tool: dispatch_result.canonical_ref.clone(),
                                            content,
                                            raw_size,
                                            result_guard_truncated,
                                            truncated_reason_override: None,
                                        }
                                    }
                                    Err(e) => {
                                        let body_str = serde_json::to_string(&json!({"error": e.to_string()})).unwrap_or_else(|_| "{\"error\":\"dispatch failed\"}".to_string());
                                        ToolResult {
                                            tool: dispatch_result.canonical_ref.clone(),
                                            raw_size: body_str.len() as u64,
                                            content: body_str,
                                            result_guard_truncated: false,
                                            truncated_reason_override: Some("error_envelope"),
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let body_str = serde_json::to_string(&json!({"error": e})).unwrap_or_else(|_| "{\"error\":\"resolve failed\"}".to_string());
                            ToolResult {
                                tool: tool_name.clone(),
                                raw_size: body_str.len() as u64,
                                content: body_str,
                                result_guard_truncated: false,
                                truncated_reason_override: Some("error_envelope"),
                            }
                        }
                    };

                    // Determine inline body and truncation flags
                    let inline_capped = tool_result.content.len() > ryeos_runtime::callback_client::TOOL_RESULT_INLINE_MAX_BYTES;
                    let body: Option<&str>;
                    let truncated: bool;
                    let truncated_reason: Option<&str>;
                    if inline_capped {
                        body = None;
                        truncated = true;
                        truncated_reason = Some("size_cap_exceeded");
                    } else if tool_result.result_guard_truncated {
                        body = Some(&tool_result.content);
                        truncated = true;
                        truncated_reason = Some("result_guard");
                    } else if let Some(reason) = tool_result.truncated_reason_override {
                        body = Some(&tool_result.content);
                        truncated = false;
                        truncated_reason = Some(reason);
                    } else {
                        body = Some(&tool_result.content);
                        truncated = false;
                        truncated_reason = None;
                    }
                    if let Err(e) = self.callback.emit_tool_result(&call_id, &tool_result.tool, body, truncated, truncated_reason, tool_result.raw_size).await {
                        state = State::Errored { error: format!("resume-critical callback emit_tool_result failed: {e}") };
                        continue;
                    }
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(tool_result.content)),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    });

                    let next_index = index + 1;
                    if next_index < pending.len() {
                        State::DispatchingTools { pending, index: next_index }
                    } else {
                        // All tools processed — fire after_step hook
                        State::FiringHooks {
                            event: "after_step".to_string(),
                            context: json!({"turn": turn}),
                            resume_to: Box::new(State::CheckingContinuation),
                        }
                    }
                }

                State::ProcessingDirectiveReturn { call_id, raw_args } => {
                    // directive_return is a lifecycle signal, not a
                    // dispatchable tool. The LLM calls it using the
                    // provider's tool-call convention; the runtime
                    // recognizes it by name and handles inline:
                    //   1. Parse arguments (typed-fail-loud)
                    //   2. Validate against declared outputs
                    //   3. Fire tool_call_result for chain visibility
                    //   4. Publish directive_outputs artifact
                    //   5. Finalize thread
                    let tool_result_content = match crate::adapter::parse_tool_arguments(&raw_args) {
                        Ok(args) => {
                            // Validate declared outputs
                            let mut validation_error = None;
                            if let Some(ref outputs) = self.directive_outputs {
                                for output in outputs {
                                    if args.get(&output.name).is_none_or(|v| v.is_null()) {
                                        validation_error = Some(format!(
                                            "directive_return: missing required output '{}'",
                                            output.name
                                        ));
                                        break;
                                    }
                                }
                            }

                            if let Some(err) = validation_error {
                                serde_json::to_string(&json!({"error": err}))
                                    .unwrap_or_else(|_| "{\"error\":\"output validation failed\"}".to_string())
                            } else {
                                // Publish outputs as artifact (non-fatal)
                                record_callback_warning(
                                    &mut warnings,
                                    "publish_artifact(directive_outputs)",
                                    self.callback.publish_artifact(json!({
                                        "artifact_type": "directive_outputs",
                                        "uri": format!("thread://{}/outputs", self.thread_id),
                                        "content": &args,
                                    })).await,
                                );

                                // Fire tool_call_result for chain visibility
                                let outputs_json = serde_json::to_string(&args).unwrap_or_default();
                                let outputs_size = outputs_json.len() as u64;
                                if let Err(e) = self.callback.emit_tool_result(&call_id, "directive_return", Some(&outputs_json), false, None, outputs_size).await {
                                    state = State::Errored { error: format!("resume-critical callback emit_tool_result failed: {e}") };
                                    continue;
                                }

                                // Finalize thread
                                if let Err(e) = self.callback.finalize_thread("completed").await {
                                    guard.finalized = true;
                                    return Self::attach_warnings(RuntimeResult {
                                        success: false,
                                        status: "errored".to_string(),
                                        thread_id: self.thread_id.clone(),
                                        result: Some(json!(format!("resume-critical callback finalize_thread failed: {e}"))),
                                        outputs: json!({}),
                                        cost: Some(self.budget.cost()),
                                        warnings: std::mem::take(&mut warnings),
                                    }, &mut warnings);
                                }
                                let mut result = self.finalize(json!("directive_return"));
                                result.outputs = args;
                                guard.finalized = true;
                                return Self::attach_warnings(result, &mut warnings);
                            }
                        }
                        Err(e) => serde_json::to_string(&json!({"error": e}))
                            .unwrap_or_else(|_| "{\"error\":\"malformed arguments\"}".to_string()),
                    };

                    // Validation or parse failure: fire tool_call_result,
                    // push error as tool message, and let the LLM retry.
                    // (Non-fatal — the LLM can correct its outputs.)
                    let failure_size = tool_result_content.len() as u64;
                    if let Err(e) = self.callback.emit_tool_result(&call_id, "directive_return", Some(&tool_result_content), false, Some("error_envelope"), failure_size).await {
                        state = State::Errored { error: format!("resume-critical callback emit_tool_result failed: {e}") };
                        continue;
                    }
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(tool_result_content)),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    });
                    State::CheckingContinuation
                }

                State::FiringHooks { event, context, resume_to } => {
                    let callback = self.callback.clone();
                    let thread_id = self.thread_id.clone();
                    let project_path = self.callback.project_path().to_string();

                    let dispatcher: ryeos_runtime::hooks_eval::HookDispatcher = Box::new(
                        move |action, proj| {
                            let cb = callback.clone();
                            let tid = thread_id.clone();
                            Box::pin(async move {
                                let payload: ryeos_runtime::callback::ActionPayload =
                                    serde_json::from_value(action)
                                    .map_err(|e| ryeos_runtime::callback::CallbackError::Transport(
                                        anyhow::anyhow!("invalid hook action: {}", e)
                                    ))?;
                                let response = cb.dispatch_action(
                                    ryeos_runtime::callback::DispatchActionRequest {
                                        thread_id: tid,
                                        project_path: proj,
                                        action: payload,
                                    },
                                )
                                .await
                                .map_err(|e| ryeos_runtime::callback::CallbackError::Transport(
                                    anyhow::anyhow!("{}", e),
                                ))?;
                                // Hooks run on the leaf result only —
                                // the parent-thread snapshot has no
                                // bearing on hook control flow.
                                Ok(response.result)
                            })
                        }
                    );

                    let hook_result = match ryeos_runtime::hooks_eval::run_hooks(
                        &event,
                        &context,
                        &self.hooks,
                        &project_path,
                        &dispatcher,
                    ).await {
                        Ok(result) => result,
                        Err(e) => {
                            tracing::warn!(hook_event = %event, "hook evaluation error, skipping: {e}");
                            None
                        }
                    };

                    // Hook events ("before_step", "after_step", …)
                    // are not in the daemon's event-vocabulary
                    // allow-list; map them to `cognition_reasoning`
                    // (journal_only) and stash the original hook name
                    // in the payload so consumers can still
                    // discriminate.
                    record_callback_warning(
                        &mut warnings,
                        &format!("cognition_reasoning(hook={event})"),
                        self.callback
                            .append_event(
                                "cognition_reasoning",
                                json!({
                                    "hook_event": event,
                                    "hook_result": hook_result,
                                }),
                            )
                            .await,
                    );

                    match hook_result {
                        Some(ref val) => {
                            let action = HookAction::from_value(val);
                            match action {
                                HookAction::Retry => State::CallingProvider,
                                HookAction::Abort | HookAction::Fail => State::Errored {
                                    error: format!("hook aborted: {}", event),
                                },
                                HookAction::Suspend | HookAction::Escalate => {
                                    tracing::warn!(action = ?action, "unsupported hook action, failing closed");
                                    State::Errored {
                                        error: format!("unsupported hook action: {:?}", action),
                                    }
                                }
                                HookAction::Continue => *resume_to,
                            }
                        }
                        None => *resume_to,
                    }
                }

                State::CheckingContinuation => {
                    let threshold = self.continuation.threshold();
                    let estimated = self.continuation.estimate_total_tokens(&self.messages, Some(&self.budget.cost()));
                    tracing::info!(estimated, threshold, "checking continuation");
                    if self
                        .continuation
                        .should_continue(&self.messages, Some(&self.budget.cost()))
                    {
                        // Context-window overflow → ask daemon to spawn
                        // a chained thread carrying the current
                        // transcript. This is the ONLY terminal path
                        // out of CheckingContinuation; otherwise we
                        // loop back to the agent loop for the next LLM
                        // turn (see comment below — the previous code
                        // finalized here, which short-circuited
                        // multi-turn tool-call dialogues after the
                        // very first tool dispatch).
                        State::Continued
                    } else {
                        // Loop back to the limits + provider call.
                        // Reaching CheckingContinuation means the prior
                        // turn either dispatched a tool (post-tool
                        // resume from `FiringHooks`) or returned an
                        // empty assistant message (edge case from
                        // `ParsingResponse`). In both cases the
                        // correct next step is another LLM turn —
                        // finalizing here would emit the last
                        // assistant content (typically `null` when
                        // the only assistant message is a tool_call
                        // envelope) and silently truncate the
                        // dialogue.
                        State::CheckingLimits
                    }
                }

                State::Finalizing { result } => {
                    if let Err(e) = self.callback.finalize_thread("completed").await {
                        let runtime_result = RuntimeResult {
                            success: false,
                            status: "errored".to_string(),
                            thread_id: self.thread_id.clone(),
                            result: Some(json!(format!("resume-critical callback finalize_thread failed: {e}"))),
                            outputs: json!({}),
                            cost: Some(self.budget.cost()),
                            warnings: Vec::new(),
                        };
                        guard.finalized = true;
                        return Self::attach_warnings(runtime_result, &mut warnings);
                    }
                    let runtime_result = self.finalize(result);
                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }

                State::Continued => {
                    // Request continuation chain from daemon
                    let reason = "context limit reached, continuation needed";
                    if let Err(e) = self.callback.request_continuation(reason).await {
                        warnings.push(format!("callback request_continuation failed: {e}"));
                    }
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "continued".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some(json!(reason)),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                        warnings: Vec::new(),
                    };
                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }

                State::Errored { error } => {
                    record_callback_warning(
                        &mut warnings,
                        "thread_failed(emit_error)",
                        self.callback.emit_error(&error).await,
                    );
                    if let Err(e) = self.callback.finalize_thread("failed").await {
                        // Finalize failed — surface in the error result
                        warnings.push(format!("resume-critical callback finalize_thread(failed) also failed: {e}"));
                    }
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "errored".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some(json!(error)),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                        warnings: Vec::new(),
                    };
                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }

                State::Cancelled => {
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "cancelled".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some(json!("cancelled by signal")),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                        warnings: Vec::new(),
                    };
                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }
            };
        }
    }

    fn compute_cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        let Some(ref pricing) = self.provider_config.pricing else {
            return 0.0;
        };
        let Some(rates) = pricing.for_model(&self.model_name) else {
            return 0.0;
        };
        let input_cost = (input_tokens as f64 / 1_000_000.0) * rates.input_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * rates.output_per_million;
        input_cost + output_cost
    }

    /// Drain the run-loop's accumulated warnings into a finished
    /// `RuntimeResult`. Caller MUST invoke this on every terminal
    /// branch so callback drift is surfaced; a missed call would
    /// silently drop everything `record_callback_warning` recorded.
    fn attach_warnings(
        mut result: RuntimeResult,
        warnings: &mut Vec<String>,
    ) -> RuntimeResult {
        result.warnings = std::mem::take(warnings);
        result
    }

    fn finalize(&self, result: Value) -> RuntimeResult {
        // D1: ship the structured terminal value through verbatim.
        // Previous behaviour stringified non-string Values, which lost
        // structure and forced HTTP callers to re-parse. RuntimeResult.result
        // is `Option<Value>` so callers see exactly what the directive
        // produced — assistant text as JSON string, tool outputs as the
        // tool's own structured result, etc.
        tracing::info!(
            thread_id = %self.thread_id,
            turns = self.harness.turns_used(),
            tokens = self.harness.tokens_used(),
            spend = format!("${:.4}", self.harness.spend_used()),
            depth = self.harness.depth(),
            "directive completed"
        );

        RuntimeResult {
            success: true,
            status: "completed".to_string(),
            thread_id: self.thread_id.clone(),
            result: Some(result),
            outputs: json!({}),
            cost: Some(self.budget.cost()),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::callback_client::CallbackClient;
    use crate::directive::PricingConfig;
    use ryeos_runtime::model_resolution::ModelPricing;
    use crate::harness::Harness;
    use ryeos_runtime::envelope::{EnvelopeCallback, EnvelopePolicy, HardLimits};
    use std::path::PathBuf;

    fn make_callback_env() -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
        }
    }

    fn make_callback() -> CallbackClient {
        CallbackClient::new(&make_callback_env(), "T-test", "/project", "tat-test")
    }

    fn make_policy() -> EnvelopePolicy {
        EnvelopePolicy {
            effective_caps: vec!["ryeos.execute.tool.*".to_string()],
            hard_limits: HardLimits::default(),
        }
    }

    #[test]
    fn compute_cost_with_pricing() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                input_per_million: Some(3.0),
                output_per_million: Some(15.0),
                models: Default::default(),
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: None,
        });

        let cost = runner.compute_cost(1_000_000, 500_000);
        assert!((cost - 10.5).abs() < f64::EPSILON);
    }

    #[test]
    fn finalize_extracts_string() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: None,
        });

        let result = runner.finalize(json!("Hello world"));
        assert!(result.success);
        assert_eq!(result.result.unwrap(), "Hello world");
        assert_eq!(result.status, "completed");
    }

    #[test]
    fn system_prompt_prepended() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: vec![],
            system_prompt: Some("You are helpful".to_string()),
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: None,
        });

        assert_eq!(runner.messages.len(), 2);
        assert_eq!(runner.messages[0].role, "system");
        assert_eq!(runner.messages[1].role, "user");
    }

    #[test]
    fn directive_outputs_stored_from_config() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };
        let outputs = Some(vec![OutputSpec {
            name: "answer".to_string(),
            description: None,
            r#type: None,
        }]);

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs,
            sampling: None,
        });

        assert!(runner.directive_outputs.is_some());
        assert_eq!(runner.directive_outputs.unwrap().len(), 1);
    }

    #[test]
    fn sampling_stored_from_config() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: Some(SamplingConfig {
                temperature: Some(0.3),
                seed: Some(42),
            }),
        });

        let s = runner.sampling.unwrap();
        assert!((s.temperature.unwrap() - 0.3).abs() < f64::EPSILON);
        assert_eq!(s.seed.unwrap(), 42);
    }

    #[test]
    fn compute_cost_uses_per_model_pricing_override() {
        let mut models = std::collections::HashMap::new();
        models.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing {
                input_per_million: 0.80,
                output_per_million: 4.00,
            },
        );
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                input_per_million: Some(0.0), // would yield $0 if used
                output_per_million: Some(0.0),
                models,
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(100.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "claude-haiku-4-5".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: None,
        });

        // 1M input + 1M output → 0.80 + 4.00 = 4.80
        let cost = runner.compute_cost(1_000_000, 1_000_000);
        assert!(
            (cost - 4.80).abs() < f64::EPSILON,
            "expected $4.80 for per-model pricing, got ${cost}"
        );
    }

    #[test]
    fn compute_cost_falls_back_to_provider_default_when_no_model_entry() {
        let provider = crate::directive::ProviderConfig {
            category: None,
            family: crate::directive::ProtocolFamily::ChatCompletions,
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                input_per_million: Some(1.0),
                output_per_million: Some(5.0),
                models: Default::default(),
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            budget: BudgetTracker::new(100.0),
            callback: make_callback(),
            context_window: 200_000,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "unknown-model".to_string(),
            thread_id: "T-test".to_string(),
            hooks: vec![],
            outputs: None,
            sampling: None,
        });

        // Falls back to provider defaults: 1M input + 1M output → 1.0 + 5.0 = 6.0
        let cost = runner.compute_cost(1_000_000, 1_000_000);
        assert!(
            (cost - 6.0).abs() < f64::EPSILON,
            "expected $6.00 for provider default pricing, got ${cost}"
        );
    }
}
