use serde_json::{json, Value};

use crate::adapter;
use crate::budget::BudgetTracker;
use crate::callback_client::CallbackClient;
use crate::continuation::ContinuationCheck;
use crate::directive::{ProviderMessage, ToolSchema};
use crate::dispatcher::Dispatcher;
use crate::harness::Harness;
use crate::launch_envelope::RuntimeResult;
use crate::result_guard::ResultGuard;
use crate::resume::ResumeState;

#[derive(Debug)]
pub enum State {
    Init,
    CheckingLimits,
    CallingProvider,
    Streaming,
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
        resume_to: Box<State>,
    },
    CheckingContinuation,
    ReportingBudget,
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
    #[allow(dead_code)]
    hooks: Vec<rye_runtime::HookDefinition>,
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
        allowed_primaries: Vec<String>,
        hooks: Vec<rye_runtime::HookDefinition>,
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
        let dispatcher = Dispatcher::new(tools.clone(), None, effective_caps, allowed_primaries);

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
        allowed_primaries: Vec<String>,
        hooks: Vec<rye_runtime::HookDefinition>,
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
            allowed_primaries,
            hooks,
        );
        runner.initial_turn = resume.turns_completed;
        runner
    }

    pub fn budget(&self) -> &BudgetTracker {
        &self.budget
    }

    pub async fn run(&mut self) -> RuntimeResult {
        let mut guard = RunGuard { finalized: false };
        let mut state = State::Init;
        let mut turn = self.initial_turn;
        let max_turns = 100;

        loop {
            state = match state {
                State::Init => {
                    let _ = self.callback.mark_running().await;
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

                    let _ = self.callback.append_event("turn_start", json!({"turn": turn})).await;

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
                                let _ = self.callback.report_budget(serde_json::json!({
                                    "input_tokens": usage.input_tokens,
                                    "output_tokens": usage.output_tokens,
                                    "total_usd": usd,
                                })).await;
                            }
                            self.messages.push(resp.message.clone());
                            let _ = self
                                .callback
                                .append_event(
                                    "turn_complete",
                                    {
                                        let mut data = json!({"turn": turn});
                                        if let Some((input, output)) = resp.usage.map(|u| (u.input_tokens, u.output_tokens)) {
                                            data["input_tokens"] = json!(input);
                                            data["output_tokens"] = json!(output);
                                        }
                                        data
                                    },
                                )
                                .await;
                            State::ParsingResponse
                        }
                        Err(e) => State::Errored {
                            error: e.to_string(),
                        },
                    }
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
                        let _ = self.callback.append_event("tool_dispatch", {
                            let mut data = json!({"tool": tc.name});
                            if let Some(ref id) = tc.id {
                                data["call_id"] = json!(id);
                            }
                            data
                        }).await;

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
                    let tool_result_content = match self.dispatcher.resolve(&tool_name, &raw_args) {
                        Ok(dispatch_result) => {
                            if dispatch_result.is_internal && dispatch_result.tool_name == "directive_return" {
                                let outputs = dispatch_result.arguments;
                                let mut result = self.finalize(json!("directive_return"));
                                result.outputs = outputs;
                                let _ = self.callback.finalize_thread("completed").await;
                                guard.finalized = true;
                                return result;
                            }

                            let primary = "execute";

                            match self.callback.dispatch_action(rye_runtime::callback::DispatchActionRequest {
                                thread_id: self.thread_id.clone(),
                                project_path: self.callback.project_path().to_string(),
                                action: rye_runtime::callback::ActionPayload {
                                    primary: primary.to_string(),
                                    item_id: dispatch_result.canonical_ref.clone(),
                                    kind: Some("tool".to_string()),
                                    params: dispatch_result.arguments.clone(),
                                    thread: "inline".to_string(),
                                },
                            }).await {
                                Ok(result_value) => {
                                    if let Some(s) = result_value.as_str() {
                                        s.to_string()
                                    } else {
                                        serde_json::to_string(&result_value).unwrap_or_else(|_| "{}".to_string())
                                    }
                                }
                                Err(e) => {
                                    serde_json::to_string(&json!({"error": e.to_string()})).unwrap_or_else(|_| "{\"error\":\"dispatch failed\"}".to_string())
                                }
                            }
                        }
                        Err(e) => serde_json::to_string(&json!({"error": e})).unwrap_or_else(|_| "{\"error\":\"resolve failed\"}".to_string()),
                    };

                    let processed = self.result_guard.process(&tool_result_content);
                    let truncated = processed.len() != tool_result_content.len();

                    let _ = self.callback.append_event("tool_result", json!({"call_id": call_id, "truncated": truncated})).await;
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(processed)),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    });

                    let next_index = index + 1;
                    if next_index < pending.len() {
                        State::DispatchingTools { pending, index: next_index }
                    } else {
                        State::CheckingLimits
                    }
                }

                State::FiringHooks { event, resume_to } => {
                    let _ = self.callback.append_event(&event, json!({})).await;
                    *resume_to
                }

                State::CheckingContinuation => {
                    let cost = self.budget.cost();
                    if self
                        .continuation
                        .should_continue(&self.messages, Some(&cost))
                    {
                        State::Continued
                    } else {
                        State::ReportingBudget
                    }
                }

                State::ReportingBudget => {
                    let content = self
                        .messages
                        .iter()
                        .rev()
                        .find_map(|m| {
                            if m.role == "assistant" && m.content.is_some() {
                                m.content.clone()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(Value::Null);

                    state = State::Finalizing { result: content };
                    continue;
                }

                State::Finalizing { result } => {
                    let _ = self.callback.finalize_thread("completed").await;
                    let runtime_result = self.finalize(result);
                    guard.finalized = true;
                    return runtime_result;
                }

                State::Continued => {
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "continued".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some("context limit reached, continuation needed".to_string()),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                    };
                    guard.finalized = true;
                    return runtime_result;
                }

                State::Errored { error } => {
                    let _ = self.callback.append_event("error", json!({"message": &error})).await;
                    let _ = self.callback.finalize_thread("failed").await;
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "errored".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some(error),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                    };
                    guard.finalized = true;
                    return runtime_result;
                }

                State::Cancelled => {
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: "cancelled".to_string(),
                        thread_id: self.thread_id.clone(),
                        result: Some("cancelled by signal".to_string()),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                    };
                    guard.finalized = true;
                    return runtime_result;
                }

                State::Streaming => {
                    let _ = self.callback.append_event("streaming", json!({"turn": turn})).await;
                    State::ParsingResponse
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

    fn finalize(&self, result: Value) -> RuntimeResult {
        let result_str = match result {
            Value::String(s) => s,
            other => other.to_string(),
        };

        RuntimeResult {
            success: true,
            status: "completed".to_string(),
            thread_id: self.thread_id.clone(),
            result: Some(result_str),
            outputs: json!({}),
            cost: Some(self.budget.cost()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback_client::CallbackClient;
    use crate::directive::PricingConfig;
    use crate::harness::Harness;
    use crate::launch_envelope::{EnvelopeCallback, EnvelopePolicy, HardLimits};
    use std::path::PathBuf;

    fn make_callback_env() -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
            allowed_primaries: vec!["execute".to_string()],
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

        let cb = make_callback_env();
        let runner = Runner::new(
            vec![],
            vec![],
            None,
            Harness::new(&make_policy(), 0),
            BudgetTracker::new(&cb, 1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec!["execute".to_string()],
            vec![],
        );

        let cost = runner.compute_cost(1_000_000, 500_000);
        assert!((cost - 10.5).abs() < f64::EPSILON);
    }

    #[test]
    fn finalize_extracts_string() {
        let cb = make_callback_env();
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
            Harness::new(&make_policy(), 0),
            BudgetTracker::new(&cb, 1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec!["execute".to_string()],
            vec![],
        );

        let result = runner.finalize(json!("Hello world"));
        assert!(result.success);
        assert_eq!(result.result.unwrap(), "Hello world");
        assert_eq!(result.status, "completed");
    }

    #[test]
    fn system_prompt_prepended() {
        let cb = make_callback_env();
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
            Harness::new(&make_policy(), 0),
            BudgetTracker::new(&cb, 1.0),
            make_callback(),
            200_000,
            provider,
            "test-model".to_string(),
            "T-test".to_string(),
            vec!["execute".to_string()],
            vec![],
        );

        assert_eq!(runner.messages.len(), 2);
        assert_eq!(runner.messages[0].role, "system");
        assert_eq!(runner.messages[1].role, "user");
    }
}
