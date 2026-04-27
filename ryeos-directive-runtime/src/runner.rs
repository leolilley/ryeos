use serde_json::{json, Value};

use crate::adapter;
use crate::budget::BudgetTracker;
use ryeos_runtime::callback_client::CallbackClient;
use crate::continuation::ContinuationCheck;
use crate::directive::{ProviderMessage, StreamEvent, ToolSchema};
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
        events: Vec<StreamEvent>,
        accumulated_text: String,
        tool_calls: Vec<crate::directive::ToolCall>,
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
    model_name: String,
    thread_id: String,
    initial_turn: u32,
    hooks: Vec<ryeos_runtime::HookDefinition>,
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

impl Runner {
    pub fn new(
        messages: Vec<ProviderMessage>,
        tools: Vec<ToolSchema>,
        system_prompt: Option<String>,
        harness: Harness,
        budget: BudgetTracker,
        callback: CallbackClient,
        context_window: u64,
        provider_config: crate::directive::ProviderConfig,
        model_name: String,
        thread_id: String,
        hooks: Vec<ryeos_runtime::HookDefinition>,
    ) -> Self {
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
        let dispatcher = Dispatcher::new(tools.clone(), None, effective_caps);

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
            model_name,
            thread_id,
            initial_turn: 0,
            hooks,
        }
    }

    pub fn from_resume(
        resume: ResumeState,
        tools: Vec<ToolSchema>,
        system_prompt: Option<String>,
        harness: Harness,
        budget: BudgetTracker,
        callback: CallbackClient,
        context_window: u64,
        provider_config: crate::directive::ProviderConfig,
        model_name: String,
        thread_id: String,
        hooks: Vec<ryeos_runtime::HookDefinition>,
    ) -> Self {
        let mut runner = Self::new(
            resume.messages,
            tools,
            system_prompt,
            harness,
            budget,
            callback,
            context_window,
            provider_config,
            model_name,
            thread_id,
            hooks,
        );
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
                    record_callback_warning(
                        &mut warnings,
                        "mark_running",
                        self.callback.mark_running().await,
                    );
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
                    self.harness.record_turn();
                    turn += 1;

                    record_callback_warning(
                        &mut warnings,
                        "emit_turn_start",
                        self.callback.emit_turn_start(turn).await,
                    );

                    if self.budget.is_exhausted() {
                        state = State::Errored { error: "budget_exceeded".to_string() };
                        continue;
                    }

                    let client = reqwest::Client::new();
                    match adapter::call_provider(
                        &client,
                        &self.provider_config,
                        &self.model_name,
                        &self.messages,
                        &self.tools,
                    )
                    .await
                    {
                        Ok(resp) => {
                            if let Some(ref usage) = resp.usage {
                                self.harness.record_tokens(usage.input_tokens, usage.output_tokens);
                                let usd = self.compute_cost(usage.input_tokens, usage.output_tokens);
                                self.harness.record_spend(usd);
                                self.budget.report(usage.input_tokens, usage.output_tokens, usd);
                            }
                            self.messages.push(resp.message.clone());
                            record_callback_warning(
                                &mut warnings,
                                "emit_turn_complete",
                                self.callback
                                    .emit_turn_complete(
                                        turn,
                                        resp.usage.as_ref().map(|u| (u.input_tokens, u.output_tokens)),
                                    )
                                    .await,
                            );
                            if let Some(ref reason) = resp.finish_reason {
                                tracing::debug!(finish_reason = %reason, "provider response");
                            }

                            // Convert response to StreamEvents for unified processing
                            let events = adapter::response_to_stream_events(&resp);
                            State::Streaming {
                                events,
                                accumulated_text: String::new(),
                                tool_calls: Vec::new(),
                            }
                        }
                        Err(e) => State::Errored {
                            error: e.to_string(),
                        },
                    }
                }

                State::Streaming { mut events, mut accumulated_text, mut tool_calls } => {
                    record_callback_warning(
                        &mut warnings,
                        "stream_opened",
                        self.callback
                            .append_event("stream_opened", json!({"turn": turn}))
                            .await,
                    );

                    // Process StreamEvents
                    while let Some(event) = events.pop() {
                        match event {
                            StreamEvent::Delta(text) => {
                                accumulated_text.push_str(&text);
                            }
                            StreamEvent::ToolUse { id, name, arguments } => {
                                let args = crate::adapter::parse_tool_arguments(&arguments);
                                tool_calls.push(crate::directive::ToolCall {
                                    id: Some(id),
                                    name,
                                    arguments: args,
                                });
                            }
                            StreamEvent::Done => {
                                // Terminal event — stop processing
                                break;
                            }
                        }
                    }

                    // StreamEvents have been processed into accumulated_text and tool_calls.
                    // The real message was already pushed in CallingProvider from the
                    // non-streaming adapter path, so no additional message push needed.
                    tracing::debug!(
                        text_len = accumulated_text.len(),
                        tool_call_count = tool_calls.len(),
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
                                .map_or(false, |tc| !tc.is_empty());
                            let has_content = msg
                                .content
                                .as_ref()
                                .map_or(false, |c| !c.is_null() && c.as_str().map_or(true, |s| !s.is_empty()));

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
                        record_callback_warning(
                            &mut warnings,
                            "emit_tool_dispatch",
                            self.callback.emit_tool_dispatch(&tc.name, tc.id.as_deref()).await,
                        );

                        let required_cap = format!("rye.execute.tool.{}", tc.name);
                        if !self.harness.check_permission(&required_cap) {
                            self.messages.push(ProviderMessage {
                                role: "tool".to_string(),
                                content: Some(json!({"error": format!("permission denied: {}", tc.name)})),
                                tool_calls: None,
                                tool_call_id: tc.id.clone(),
                            });
                            State::DispatchingTools { pending, index: index + 1 }
                        } else {
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
                    let tool_result_content = match self.dispatcher.resolve(&tool_name, &raw_args, Some(call_id.clone())) {
                        Ok(dispatch_result) => {
                            if dispatch_result.dispatch_kind == DispatchKind::Internal && self.dispatcher.is_directive_return(&dispatch_result.tool_name) {
                                let outputs = dispatch_result.arguments;
                                // Publish outputs as artifact
                                record_callback_warning(
                                    &mut warnings,
                                    "publish_artifact(directive_outputs)",
                                    self.callback.publish_artifact(json!({
                                        "artifact_type": "directive_outputs",
                                        "uri": format!("thread://{}/outputs", self.thread_id),
                                        "content": &outputs,
                                    })).await,
                                );
                                let mut result = self.finalize(json!("directive_return"));
                                result.outputs = outputs;
                                record_callback_warning(
                                    &mut warnings,
                                    "finalize_thread(completed)",
                                    self.callback.finalize_thread("completed").await,
                                );
                                guard.finalized = true;
                                return Self::attach_warnings(result, &mut warnings);
                            }

                            // Record spawn for child executions (directive/graph)
                            match dispatch_result.dispatch_kind {
                                DispatchKind::DirectiveChild | DispatchKind::GraphChild => {
                                    self.harness.record_spawn();
                                }
                                DispatchKind::Tool | DispatchKind::Internal => {}
                            }

                            // Risk assessment before dispatch
                            let required_cap = format!("rye.execute.tool.{}", dispatch_result.canonical_ref);
                            let risk = self.harness.assess(&required_cap);
                            if risk.blocked {
                                tracing::warn!(
                                    tool = %dispatch_result.canonical_ref,
                                    call_id = ?dispatch_result.call_id,
                                    risk_level = %risk.level,
                                    requires_ack = risk.requires_ack,
                                    "tool call blocked by risk policy"
                                );
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
                                serde_json::to_string(&json!({"error": format!("blocked by risk policy: {}", dispatch_result.canonical_ref)}))
                                    .unwrap_or_else(|_| "{\"error\":\"blocked\"}".to_string())
                            } else {
                                match self.callback.dispatch_action(ryeos_runtime::callback::DispatchActionRequest {
                                    thread_id: self.thread_id.clone(),
                                    project_path: self.callback.project_path().to_string(),
                                    action: ryeos_runtime::callback::ActionPayload {
                                        item_id: dispatch_result.canonical_ref.clone(),
                                        kind: Some("tool".to_string()),
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
                                            .unwrap_or_default();
                                        let processed_bytes = self.result_guard.process_bytes(&raw_bytes);
                                        String::from_utf8_lossy(&processed_bytes).to_string()
                                    }
                                    Err(e) => {
                                        serde_json::to_string(&json!({"error": e.to_string()})).unwrap_or_else(|_| "{\"error\":\"dispatch failed\"}".to_string())
                                    }
                                }
                            }
                        }
                        Err(e) => serde_json::to_string(&json!({"error": e})).unwrap_or_else(|_| "{\"error\":\"resolve failed\"}".to_string()),
                    };

                    let truncated = tool_result_content.len() != raw_args.len();
                    record_callback_warning(
                        &mut warnings,
                        "emit_tool_result",
                        self.callback.emit_tool_result(&call_id, truncated).await,
                    );
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(tool_result_content)),
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

                    let hook_result = ryeos_runtime::hooks_eval::run_hooks(
                        &event,
                        &context,
                        &self.hooks,
                        &project_path,
                        &dispatcher,
                    ).await;

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
                    record_callback_warning(
                        &mut warnings,
                        "finalize_thread(completed)",
                        self.callback.finalize_thread("completed").await,
                    );
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
                        result: Some(reason.to_string()),
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
                    record_callback_warning(
                        &mut warnings,
                        "finalize_thread(failed)",
                        self.callback.finalize_thread("failed").await,
                    );
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "errored".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some(error),
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
                        result: Some("cancelled by signal".to_string()),
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
        if let Some(ref pricing) = self.provider_config.pricing {
            let input_cost = (input_tokens as f64 / 1_000_000.0)
                * pricing.input_per_million.unwrap_or(0.0);
            let output_cost = (output_tokens as f64 / 1_000_000.0)
                * pricing.output_per_million.unwrap_or(0.0);
            input_cost + output_cost
        } else {
            0.0
        }
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
        let result_str = match result {
            Value::String(s) => s,
            other => other.to_string(),
        };

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
            result: Some(result_str),
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
        CallbackClient::new(&make_callback_env(), "T-test", "/project")
    }

    fn make_policy() -> EnvelopePolicy {
        EnvelopePolicy {
            effective_caps: vec!["rye.execute.tool.*".to_string()],
            hard_limits: HardLimits::default(),
        }
    }

    #[test]
    fn compute_cost_with_pricing() {
        let provider = crate::directive::ProviderConfig {
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                input_per_million: Some(3.0),
                output_per_million: Some(15.0),
            }),
            extra: Default::default(),
        };

        let runner = Runner::new(
            vec![],
            vec![],
            None,
            Harness::new(&make_policy(), 0, None),
            BudgetTracker::new(1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec![],
        );

        let cost = runner.compute_cost(1_000_000, 500_000);
        assert!((cost - 10.5).abs() < f64::EPSILON);
    }

    #[test]
    fn finalize_extracts_string() {
        let provider = crate::directive::ProviderConfig {
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
        };

        let runner = Runner::new(
            vec![],
            vec![],
            None,
            Harness::new(&make_policy(), 0, None),
            BudgetTracker::new(1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec![],
        );

        let result = runner.finalize(json!("Hello world"));
        assert!(result.success);
        assert_eq!(result.result.unwrap(), "Hello world");
        assert_eq!(result.status, "completed");
    }

    #[test]
    fn system_prompt_prepended() {
        let provider = crate::directive::ProviderConfig {
            base_url: "http://localhost".to_string(),
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
        };

        let runner = Runner::new(
            vec![ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
            }],
            vec![],
            Some("You are helpful".to_string()),
            Harness::new(&make_policy(), 0, None),
            BudgetTracker::new(1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec![],
        );

        assert_eq!(runner.messages.len(), 2);
        assert_eq!(runner.messages[0].role, "system");
        assert_eq!(runner.messages[1].role, "user");
    }
}
