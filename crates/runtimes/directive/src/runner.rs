use std::time::Instant;

use serde_json::{json, Value};

use crate::budget::BudgetTracker;
use crate::continuation::ContinuationCheck;
use crate::directive::{
    ContinuationConfig, ExecutionConfig, FinishReason, OutputSpec, ProviderMessage, ReturnNudge,
    SamplingConfig, StreamEvent, ToolSchema,
};
use crate::dispatcher::{DispatchKind, Dispatcher};
use crate::harness::{Harness, HookAction};
use crate::result_guard::ResultGuard;
use crate::resume::ResumeState;
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{
    normalize_hook_dispatch_result, RuntimeCost, RuntimeResult, RuntimeResultStatus,
};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{TerminalCompletion, ThreadTerminalStatus};

mod request_context;

use request_context::{initial_messages, visible_provider_tools};

/// Free-form breadcrumb passed to `request_continuation` for logs only.
/// Continuation is autonomous by construction — this is NOT a typed reason the
/// substrate keys off; `State::Continued` has exactly one cause (the live
/// context window approaching the model limit), so it is a fixed string, not an
/// enum.
const CONTINUATION_LOG_REASON: &str = "context_window";

#[derive(Debug)]
pub enum State {
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
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence,
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
    definition_ref: String,
    definition_hash: String,
    initial_turn: u32,
    hooks: Vec<ryeos_runtime::CompiledHook>,
    /// Declared directive outputs — used to validate `directive_return`
    /// arguments before finalization. `None` = no outputs declared,
    /// any arguments accepted.
    directive_outputs: Option<Vec<OutputSpec>>,
    /// Opt-in (`return_nudge` in the header): grant one corrective turn
    /// when a run with declared outputs is about to settle without a
    /// successful `directive_return`. Carries the author-worded stimulus
    /// when the header sets a string.
    return_nudge: ReturnNudge,
    /// Whether the corrective turn has been granted in this segment —
    /// bounds the nudge to exactly one extra turn.
    return_nudge_sent: bool,
    /// What to do at the context-window continuation boundary: disabled
    /// (default) → stop with current state; enabled → self-continue.
    continuation_config: ContinuationConfig,
    /// LLM sampling parameters from the directive's `model.sampling`.
    /// Passed to the provider adapter for inclusion in request body.
    /// `None` = use provider defaults.
    sampling: Option<SamplingConfig>,
    /// Shared HTTP client — created once and reused across all turns.
    /// Connection pooling keeps TCP/TLS handshakes to a minimum.
    http_client: reqwest::Client,
    /// Persistence that must succeed before any callback can make the thread
    /// terminal. Keeping this inside the runner closes the authority gap where
    /// stdout could report a transcript failure after the callback had already
    /// committed `completed`.
    terminal_persistence: TerminalPersistence,
}

struct TerminalPersistence {
    state_root: std::path::PathBuf,
    source_path: String,
}

struct RunGuard {
    finalized: bool,
}

#[derive(Debug)]
struct ProviderAttemptAccountingPersistenceError(String);

impl std::fmt::Display for ProviderAttemptAccountingPersistenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderAttemptAccountingPersistenceError {}

#[derive(Debug, Default)]
struct AttemptAccountingLifecycle {
    last: Option<String>,
    active: Option<String>,
    closing_payload: Option<Value>,
}

impl AttemptAccountingLifecycle {
    fn admit_after_pending_ack(&mut self, attempt_id: String) -> Result<(), String> {
        if let Some(active) = self.active.as_deref() {
            return Err(format!(
                "cannot admit provider attempt `{attempt_id}` while `{active}` remains active"
            ));
        }
        self.last = Some(attempt_id.clone());
        self.active = Some(attempt_id);
        self.closing_payload = None;
        Ok(())
    }

    fn active_for_close(&self) -> Result<String, String> {
        self.active.clone().ok_or_else(|| {
            "provider attempt accounting lifecycle has no active attempt".to_string()
        })
    }

    fn ack_closed(&mut self, attempt_id: &str) -> Result<(), String> {
        match self.active.as_deref() {
            Some(active) if active == attempt_id => {
                self.active = None;
                self.closing_payload = None;
                Ok(())
            }
            Some(active) => Err(format!(
                "provider attempt closure ACK for `{attempt_id}` does not match active `{active}`"
            )),
            None => Err(format!(
                "provider attempt closure ACK for `{attempt_id}` has no active attempt"
            )),
        }
    }

    fn last(&self) -> Option<&str> {
        self.last.as_deref()
    }

    fn has_active(&self) -> bool {
        self.active.is_some()
    }

    fn bind_closing_payload(&mut self, payload: Value) -> Result<Value, String> {
        if self.active.is_none() {
            return Err("cannot bind a terminal payload without an active attempt".to_string());
        }
        if let Some(bound) = self.closing_payload.as_ref() {
            if bound != &payload {
                return Err(
                    "provider attempt already has a different terminal payload bound".to_string(),
                );
            }
            return Ok(bound.clone());
        }
        self.closing_payload = Some(payload.clone());
        Ok(payload)
    }

    #[cfg(test)]
    fn closing_payload(&self) -> Option<&Value> {
        self.closing_payload.as_ref()
    }
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

fn directive_outputs_artifact(thread_id: &str, outputs: &Value) -> Value {
    json!({
        "artifact_type": "directive_outputs",
        "uri": format!("thread://{thread_id}/outputs"),
        "metadata": outputs,
    })
}

/// Where a turn's computed cost came from — lets the run loop flag untracked
/// spend and one-time provider-default fallbacks (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PricingSource {
    /// The provider reported the request's actual charge.
    ProviderReported,
    /// A per-model pricing entry matched `model_name`.
    PerModel,
    /// No per-model entry; the provider-level default rates were used.
    ProviderDefault,
    /// Signed provider pricing explicitly declares this route free.
    ExplicitlyFree,
    /// Gateway reported zero while marking the request BYOK; upstream spend is
    /// outside RyeOS's observed charge and must not be called tracked/free.
    ByokUntracked,
    /// No pricing configured at all, or configured but with no rate for the
    /// model — cost could not be computed and is reported as `$0`.
    Unpriced,
}

impl PricingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::PerModel => "per_model",
            Self::ProviderDefault => "provider_default",
            Self::ExplicitlyFree => "explicitly_free",
            Self::ByokUntracked => "byok_untracked",
            Self::Unpriced => "unpriced",
        }
    }
}

/// A computed turn cost plus its pricing provenance.
struct CostBreakdown {
    usd: f64,
    source: PricingSource,
}

/// Decide whether a failed provider call should be retried, and if so, how long
/// to back off first (§1). Returns `None` when the error is not retryable or the
/// retry budget (`execution.retries`) is spent.
///
/// Only a [`ProviderStreamError`](crate::provider_adapter::ProviderStreamError)
/// is retryable — the adapter classifies exactly the transient transport/
/// provider classes (pre-stream send/status/timeout, and a stream that dies
/// mid-read). Everything else — invalid model, auth, context overflow, a
/// live callback publication failure — stays fail-fast. `never_retry` (status
/// codes as strings) is an absolute denylist that overrides
/// `retry_status_codes`. Backoff is exponential from `backoff_base_ms`
/// (`base * 2^attempt`).
fn retry_backoff(
    err: &anyhow::Error,
    attempt: u32,
    execution: &ExecutionConfig,
) -> Option<std::time::Duration> {
    use crate::provider_adapter::ProviderStreamError;

    if attempt >= execution.retries {
        return None;
    }
    let retryable = match err.downcast_ref::<ProviderStreamError>() {
        Some(ProviderStreamError::Status { code, .. }) => {
            !execution.never_retry.contains(&code.to_string())
                && execution.retry_status_codes.contains(code)
        }
        Some(ProviderStreamError::Timeout { .. }) => execution.retry_on_timeout,
        // A pre-stream `.send()` transport failure (DNS/connect/TLS/reset) is
        // always retry-safe — no cognition_out was emitted. Retry it under the
        // shared `execution.retries` budget so a burst-fanout connect blip backs
        // off and re-attempts instead of surfacing as a fatal generic error.
        Some(ProviderStreamError::Send { .. }) => true,
        // The stream died mid-read (chunk timeout/reset). The request is
        // idempotent; the durable `provider_retry` event records the count of
        // live/ephemeral deltas emitted before the cut.
        Some(ProviderStreamError::MidStream { .. }) => execution.retry_mid_stream,
        None => false,
    };
    if !retryable {
        return None;
    }
    let factor = 1u64 << attempt.min(16);
    let delay_ms = execution.backoff_base_ms.saturating_mul(factor);
    Some(std::time::Duration::from_millis(delay_ms))
}

pub struct RunnerConfig {
    pub messages: Vec<ProviderMessage>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub harness: Harness,
    pub budget: BudgetTracker,
    pub callback: CallbackClient,
    pub context_window: u64,
    /// Fraction of the context window at which the continuation boundary fires;
    /// from the directive runtime's `ryeos-runtime/continuation` config.
    pub context_threshold_ratio: f64,
    pub provider_config: crate::directive::ProviderConfig,
    pub provider_id: String,
    /// Profile name that matched during daemon preflight.
    pub matched_profile: Option<String>,
    /// SHA-256 of the canonical-JSON provider config from the snapshot.
    pub config_hash: String,
    pub execution: ExecutionConfig,
    pub model_name: String,
    pub thread_id: String,
    pub definition_ref: String,
    pub definition_hash: String,
    pub hooks: Vec<ryeos_runtime::CompiledHook>,
    pub outputs: Option<Vec<OutputSpec>>,
    pub return_nudge: ReturnNudge,
    pub continuation: ContinuationConfig,
    pub sampling: Option<SamplingConfig>,
    pub terminal_state_root: std::path::PathBuf,
    pub terminal_source_path: String,
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
            context_threshold_ratio,
            provider_config,
            provider_id,
            execution,
            model_name,
            thread_id,
            definition_ref,
            definition_hash,
            hooks,
            outputs,
            return_nudge,
            continuation,
            sampling,
            matched_profile,
            config_hash,
            terminal_state_root,
            terminal_source_path,
        } = config;
        let initial_messages = initial_messages(messages, system_prompt.as_deref());

        let effective_caps = harness.effective_caps().to_vec();
        let dispatcher = Dispatcher::new(tools.clone(), effective_caps);

        Self {
            messages: initial_messages,
            tools,
            dispatcher,
            harness,
            budget,
            callback,
            continuation: ContinuationCheck::new(context_window, context_threshold_ratio),
            result_guard: ResultGuard::new(),
            provider_config,
            provider_id,
            matched_profile,
            config_hash,
            execution,
            model_name,
            thread_id,
            definition_ref,
            definition_hash,
            initial_turn: 0,
            hooks,
            directive_outputs: outputs,
            return_nudge,
            return_nudge_sent: false,
            continuation_config: continuation,
            sampling,
            http_client: reqwest::Client::builder()
                .pool_max_idle_per_host(8)
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .expect("reqwest client builder"),
            terminal_persistence: TerminalPersistence {
                state_root: terminal_state_root,
                source_path: terminal_source_path,
            },
        }
    }

    fn persist_terminal_outputs(&self) -> anyhow::Result<()> {
        let persistence = &self.terminal_persistence;
        crate::knowledge::write_thread_transcript(
            &persistence.state_root,
            &self.thread_id,
            &persistence.source_path,
            &self.messages,
        )?;
        crate::knowledge::write_capabilities(
            &persistence.state_root,
            &self.thread_id,
            &self.tools,
            None,
        )?;
        Ok(())
    }

    pub fn from_resume(resume: ResumeState, mut config: RunnerConfig) -> anyhow::Result<Self> {
        if let Some(ref usage) = resume.thread_usage {
            let resumed_tokens = usage
                .input_tokens
                .checked_add(usage.output_tokens)
                .ok_or_else(|| anyhow::anyhow!("resume usage token count overflow"))?;
            let resumed_cost = RuntimeCost {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                total_usd: usage.spend_usd,
                basis: None,
            };
            resumed_cost
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid resume usage: {error}"))?;
            config.harness.reseed(
                usage.completed_turns,
                resumed_tokens,
                usage.spend_usd,
                usage.spawns_used,
            );
            config
                .budget
                .reseed(usage.input_tokens, usage.output_tokens, usage.spend_usd)
                .map_err(|error| anyhow::anyhow!("invalid resume budget: {error}"))?;
        }
        config.messages = resume.messages;
        let mut runner = Self::new(config);
        runner.initial_turn = resume.turns_completed;
        Ok(runner)
    }

    /// Drain operator inputs staged for this running thread and fold each as a
    /// `cognition_in` (user-role) message into the in-flight `messages`. The
    /// daemon has ALREADY persisted them as durable `cognition_in` (running-
    /// guarded) before returning, so this only updates the live wire-fold;
    /// `messages` and the braid stay consistent. Returns whether anything was
    /// folded.
    ///
    /// Resume-critical: a poll error is NOT swallowed. Because the daemon persists
    /// drained inputs before returning, a lost/failed response means we cannot
    /// know whether input was persisted-but-unfolded; continuing would answer with
    /// a transcript missing that input. Erroring stops the loop instead — a resume
    /// re-folds any persisted `cognition_in` from the braid.
    ///
    /// Drained ONLY at safe turn boundaries (never between an assistant tool-call
    /// message and its tool results), so the folded wire history is well-formed.
    async fn poll_pending_input(&mut self) -> Result<bool, String> {
        let inputs = self
            .callback
            .poll_input()
            .await
            .map_err(|e| format!("resume-critical callback poll_input failed: {e}"))?;
        if inputs.is_empty() {
            return Ok(false);
        }
        for s in &inputs {
            self.messages.push(ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(s.content)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        tracing::info!(
            folded = inputs.len(),
            "folded operator inputs as cognition_in"
        );
        Ok(true)
    }

    /// Whether another provider turn can actually start now (hard cap + budget +
    /// limits). Gates the pre-finalize steer drain: draining persists a durable
    /// `cognition_in`, so we must NOT drain when no turn remains — that would
    /// leave the input unanswered and turn a clean completion into a limit
    /// failure. A late steer stays queued and is cleared at finalize; the operator
    /// resubmits it as a settled continuation.
    fn can_start_another_turn(&self) -> bool {
        !self.budget.is_exhausted() && self.harness.check_limits().is_ok()
    }

    fn provider_usage_required(&self) -> bool {
        self.provider_config
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.accounting.as_ref())
            .is_some_and(|accounting| accounting.require_usage)
    }

    fn accounting_fail_closed(&self) -> bool {
        if self.provider_usage_required() {
            return true;
        }
        match self.execution.accounting.failure_policy {
            crate::directive::AccountingFailurePolicy::FailClosed => true,
            crate::directive::AccountingFailurePolicy::Warn => false,
            crate::directive::AccountingFailurePolicy::Auto => {
                self.harness.has_finite_accounting_budget()
            }
        }
    }

    pub async fn run(&mut self) -> RuntimeResult {
        let mut guard = RunGuard { finalized: false };
        // The entrypoint has already attached the process and durably moved
        // the thread to running before it constructs the runner.
        let mut state = State::CheckingLimits;
        let mut turn = self.initial_turn;
        // Admission happens only after the pending event is acknowledged.
        // Closure is two-phase: the ID remains active until the exact terminal
        // event is acknowledged or recovered by replay.
        let mut attempt_accounting = AttemptAccountingLifecycle::default();
        // Collected non-fatal callback failures. P2.2 — runtime no
        // longer silently drops `append_event` errors; everything that
        // would have hit `let _ = ...` is now recorded here and
        // surfaced via `RuntimeResult.warnings` so the daemon /
        // operator can see contract drift (rejected event names,
        // transport hiccups, etc.).
        let mut warnings: Vec<String> = Vec::new();
        // §2: the provider-default fallback and the untracked-cost condition are
        // steady-state per run (a run has one model + one pricing config), so
        // each contributes at most one entry to `warnings`. The per-turn
        // `tracing::warn!` still fires every turn for log visibility.
        let mut provider_default_pricing_logged = false;
        let mut cost_untracked_warned = false;

        loop {
            state = match state {
                State::CheckingLimits => {
                    if let Err(e) = self.harness.check_limits() {
                        if e == "cancelled" {
                            state = State::Cancelled;
                            continue;
                        }
                        state = State::Errored { error: e };
                        continue;
                    }
                    // Steer drain (between-turns boundary): fold any operator
                    // inputs staged since the last cognition before calling the
                    // provider. Also folds the input that accompanied a live
                    // interrupt (the interrupt arm routes back through here).
                    // Limits already passed above, so a turn WILL start — a fold
                    // here is always answered. Resume-critical: a poll error stops
                    // the loop rather than answering with input we may have dropped.
                    if let Err(e) = self.poll_pending_input().await {
                        state = State::Errored { error: e };
                        continue;
                    }
                    State::CallingProvider
                }

                State::CallingProvider => {
                    let turn_start = Instant::now();
                    self.harness.record_turn();
                    turn += 1;

                    if let Err(e) = self.callback.emit_turn_start(turn).await {
                        state = State::Errored {
                            error: format!("resume-critical callback emit_turn_start failed: {e}"),
                        };
                        continue;
                    }

                    let cancel_flag = self.harness.cancelled_flag();
                    // Clear any interrupt requested OUTSIDE active streaming (e.g.
                    // during tool dispatch or between turns): that input has
                    // already been folded as a steer, so a stale flag must not
                    // immediately cut — and seal an empty — the upcoming cognition.
                    // Only an interrupt arriving DURING this stream (after this
                    // reset) cuts it. An interrupt only makes sense against an
                    // in-progress generation.
                    self.harness.take_interrupt();
                    let interrupt_flag = self.harness.interrupted_flag();
                    // Filter tools by effective_caps so the LLM only sees
                    // tools it can actually call (saves context, avoids the
                    // "model names a tool the dispatcher would reject" path).
                    let visible_tools_owned = visible_provider_tools(
                        &self.tools,
                        self.harness.effective_caps(),
                        self.directive_outputs.as_deref(),
                    );
                    // §1 retry loop. A retryable provider failure — a configured
                    // HTTP status, a send timeout/transport error, or a stream
                    // that died mid-read — is re-attempted with exponential
                    // backoff. The request is idempotent (same message array).
                    // Pre-stream classes cannot duplicate an indexed completed
                    // cognition_out. Mid-stream deltas are live/ephemeral; the
                    // durable `provider_retry` event below records the abandoned
                    // attempt before re-issue. Anything not
                    // classified as a `ProviderStreamError` (invalid bytes, a
                    // live callback publication failure) routes to
                    // `State::Errored` unchanged.
                    //
                    // Budget is re-checked before every attempt so a retry never
                    // pushes spend past the wall. `record_turn`/`emit_turn_start`
                    // ran once above — a retry is a transparent re-attempt of the
                    // SAME turn, not a new one. Each retry is logged (tracing +
                    // a run warning surfaced on `RuntimeResult.warnings`) so the
                    // stall is visible instead of silent.
                    let mut attempt: u32 = 0;
                    let stream_result = loop {
                        if self.budget.is_exhausted() {
                            break Err(anyhow::anyhow!("budget_exceeded"));
                        }
                        if let Err(limit) = self.harness.check_retry_limits() {
                            break Err(anyhow::anyhow!(
                                "provider attempt refused by effective resource limits: {limit}"
                            ));
                        }
                        let attempt_number = attempt + 1;
                        let attempt_id = format!("{}:{turn}:{attempt_number}", self.thread_id);
                        let pending_state = match self.execution.accounting.budget_mode {
                            crate::directive::AccountingBudgetMode::Settled => "accounting_pending",
                            crate::directive::AccountingBudgetMode::Hard => {
                                break Err(anyhow::anyhow!(
                                    "provider_accounting_policy_invalid: hard budget mode requires the durable reservation/reconciliation backend, which is not enabled by this runtime build"
                                ));
                            }
                        };
                        if let Err(error) = self
                            .persist_provider_attempt_accounting(json!({
                                "attempt_id": &attempt_id,
                                "attempt_number": attempt_number,
                                "turn": turn,
                                "state": pending_state,
                                "budget_mode": "settled",
                                "settlement_semantics": "post_attempt_may_overshoot_by_one_attempt",
                            }))
                            .await
                        {
                            break Err(anyhow::anyhow!(
                                "resume-critical provider attempt accounting-pending persistence failed before request issue: {error}"
                            ));
                        }
                        if let Err(error) =
                            attempt_accounting.admit_after_pending_ack(attempt_id.clone())
                        {
                            break Err(anyhow::anyhow!(
                                "provider attempt accounting admission failed: {error}"
                            ));
                        }
                        let call = crate::provider_adapter::call_provider_streaming(
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
                                cancel_flag: Some(cancel_flag.clone()),
                                interrupt_flag: Some(interrupt_flag.clone()),
                            },
                        )
                        .await;
                        match call {
                            Err(e) => match retry_backoff(&e, attempt, &self.execution) {
                                Some(delay) => {
                                    let usage_required = self.accounting_fail_closed();
                                    let ambiguous_without_accounting = e
                                        .downcast_ref::<crate::provider_adapter::ProviderStreamError>()
                                        .is_some_and(|error| matches!(
                                            error,
                                            crate::provider_adapter::ProviderStreamError::Timeout { .. }
                                                | crate::provider_adapter::ProviderStreamError::Send {
                                                    connect: false,
                                                    ..
                                                }
                                        ));
                                    if usage_required && ambiguous_without_accounting {
                                        if let Err(close_error) = self
                                            .close_active_provider_attempt_accounting(
                                                &mut attempt_accounting,
                                                json!({
                                                    "attempt_number": attempt + 1,
                                                    "turn": turn,
                                                    "state": "accounting_unavailable_fail_closed",
                                                    "ambiguous_provider_acceptance": true,
                                                    "error": format!("{e:#}"),
                                                }),
                                            )
                                            .await
                                        {
                                            break Err(close_error);
                                        }
                                        break Err(anyhow::anyhow!(
                                            "provider_accounting_invalid: refusing an ambiguous retry because the provider may have accepted work but required attempt usage is unavailable; original error: {e:#}"
                                        ));
                                    }
                                    let mid_stream_attempt = e
                                        .downcast_ref::<crate::provider_adapter::ProviderStreamError>()
                                        .and_then(|error| match error {
                                            crate::provider_adapter::ProviderStreamError::MidStream {
                                                accepted_bytes,
                                                accepted_output_events,
                                                live_output_events_emitted,
                                                usage,
                                                generation_header_id,
                                                response_id,
                                                requested_output_tokens,
                                                ..
                                            } => Some((
                                                *accepted_bytes,
                                                *accepted_output_events,
                                                *live_output_events_emitted,
                                                usage.clone(),
                                                generation_header_id.clone(),
                                                response_id.clone(),
                                                *requested_output_tokens,
                                            )),
                                            _ => None,
                                        });
                                    let usage_valid = mid_stream_attempt
                                        .as_ref()
                                        .and_then(|(_, _, _, usage, _, _, _)| usage.as_ref())
                                        .is_some_and(|usage| usage.is_valid());
                                    if let Some((_, _, _, usage, _, _, _)) =
                                        mid_stream_attempt.as_ref()
                                    {
                                        if let Err(settlement_error) = self
                                            .settle_available_attempt_accounting(
                                                turn,
                                                usage.as_ref(),
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await
                                        {
                                            break Err(anyhow::anyhow!(
                                                "provider attempt accounting settlement failed before retry: {settlement_error:#}"
                                            ));
                                        }
                                    }
                                    if let Err(close_error) = self
                                        .close_active_provider_attempt_accounting(
                                            &mut attempt_accounting,
                                            json!({
                                                "attempt_number": attempt + 1,
                                                "turn": turn,
                                                "state": if usage_required && mid_stream_attempt.is_some() && !usage_valid {
                                                    if mid_stream_attempt
                                                        .as_ref()
                                                        .and_then(|(_, _, _, usage, _, _, _)| usage.as_ref())
                                                        .and_then(|usage| usage.reported_cost_usd)
                                                        .is_some()
                                                    {
                                                        "reported_spend_only_fail_closed"
                                                    } else {
                                                        "accounting_unavailable_fail_closed"
                                                    }
                                                } else {
                                                    mid_stream_attempt
                                                        .as_ref()
                                                        .and_then(|(_, _, _, usage, _, _, _)| usage.as_ref())
                                                        .map(|usage| if usage.is_valid() {
                                                            "reported"
                                                        } else if usage.reported_cost_usd.is_some() {
                                                            "reported_spend_only"
                                                        } else {
                                                            "accounting_unavailable_retry"
                                                        })
                                                        .unwrap_or("accounting_unavailable_retry")
                                                },
                                                "usage": mid_stream_attempt
                                                    .as_ref()
                                                    .and_then(|(_, _, _, usage, _, _, _)| usage.as_ref()),
                                                "retry_error": format!("{e:#}"),
                                            }),
                                        )
                                        .await
                                    {
                                        break Err(close_error);
                                    }
                                    if usage_required
                                        && mid_stream_attempt.is_some()
                                        && !usage_valid
                                    {
                                        break Err(anyhow::anyhow!(
                                            "provider_accounting_invalid: mid-stream attempt cannot be retried because required final usage is unavailable; original error: {e:#}"
                                        ));
                                    }
                                    if let Err(limit) = self.harness.check_retry_limits() {
                                        break Err(anyhow::anyhow!(
                                            "provider retry refused after settling the prior attempt: {limit}; original error: {e:#}"
                                        ));
                                    }
                                    attempt += 1;
                                    // Surface whether this was a connect-phase
                                    // transport failure (`Some(true)`) vs another
                                    // pre-stream send/reset (`Some(false)`) vs a
                                    // status/timeout retry (`None`) — the signal
                                    // for telling burst-fanout connect blips apart
                                    // from real provider throttling.
                                    let send_connect_phase = e
                                        .downcast_ref::<crate::provider_adapter::ProviderStreamError>(
                                        )
                                        .and_then(|pe| match pe {
                                            crate::provider_adapter::ProviderStreamError::Send {
                                                connect,
                                                ..
                                            } => Some(*connect),
                                            _ => None,
                                        });
                                    // A mid-stream cut abandons partial
                                    // live/ephemeral output; carry how much was
                                    // acknowledged so
                                    // the retry marker quantifies the attempt.
                                    let mid_stream_live_output_events_emitted = e
                                        .downcast_ref::<crate::provider_adapter::ProviderStreamError>(
                                        )
                                        .and_then(|pe| match pe {
                                            crate::provider_adapter::ProviderStreamError::MidStream {
                                                live_output_events_emitted,
                                                ..
                                            } => Some(*live_output_events_emitted),
                                            _ => None,
                                        });
                                    tracing::warn!(
                                        turn,
                                        attempt,
                                        max_retries = self.execution.retries,
                                        backoff_ms = delay.as_millis() as u64,
                                        send_connect_phase = ?send_connect_phase,
                                        error = %e,
                                        "provider call failed with a retryable error; \
                                         backing off before retry"
                                    );
                                    warnings.push(format!(
                                        "provider retry {attempt}/{max} on turn {turn} \
                                         after {ms}ms backoff: {e:#}",
                                        max = self.execution.retries,
                                        ms = delay.as_millis(),
                                    ));
                                    // Durable braid record so the stall shows in
                                    // the timeline, not just the terminal warning
                                    // summary. `provider_retry` is a canonical
                                    // RuntimeEventType (ryeos-runtime events.rs).
                                    record_callback_warning(
                                        &mut warnings,
                                        "provider_retry",
                                        self.callback
                                            .append_runtime_event(
                                                ryeos_runtime::RuntimeEventType::ProviderRetry,
                                                json!({
                                                    "turn": turn,
                                                    "attempt": attempt,
                                                    "max_retries": self.execution.retries,
                                                    "backoff_ms": delay.as_millis() as u64,
                                                    "send_connect_phase": send_connect_phase,
                                                    "mid_stream_live_output_events_emitted":
                                                        mid_stream_live_output_events_emitted,
                                                    "provider_attempt": mid_stream_attempt.as_ref().map(|(
                                                        accepted_bytes,
                                                        accepted_output_events,
                                                        live_output_events_emitted,
                                                        usage,
                                                        generation_header_id,
                                                        response_id,
                                                        requested_output_tokens,
                                                    )| json!({
                                                        "attempt_id": format!("{}:{turn}:{attempt}", self.thread_id),
                                                        "attempt_number": attempt,
                                                        "accepted_bytes": accepted_bytes,
                                                        "accepted_output_events": accepted_output_events,
                                                        "live_output_events_emitted": live_output_events_emitted,
                                                        "usage": usage,
                                                        "generation_header_id": generation_header_id,
                                                        "response_id": response_id,
                                                        "requested_output_tokens": requested_output_tokens,
                                                        "accounting_status": usage.as_ref().map(|usage| if usage.is_valid() {
                                                            "reported"
                                                        } else {
                                                            "malformed"
                                                        }).unwrap_or("unavailable"),
                                                    })),
                                                    "error": format!("{e:#}"),
                                                }),
                                            )
                                            .await,
                                    );
                                    tokio::select! {
                                        _ = tokio::time::sleep(delay) => {}
                                        _ = async {
                                            while !cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                            }
                                        } => {
                                            break Ok(crate::provider_adapter::StreamOutcome::Cancelled {
                                                attempt: crate::provider_adapter::streaming::CutAttemptState {
                                                    usage: None,
                                                    generation_header_id: None,
                                                    response_id: None,
                                                    requested_output_tokens: None,
                                                    observed_output: Default::default(),
                                                },
                                            });
                                        }
                                        _ = async {
                                            while !interrupt_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                            }
                                        } => {
                                            break Ok(crate::provider_adapter::StreamOutcome::Interrupted {
                                                partial_message: ProviderMessage {
                                                    role: "assistant".to_string(),
                                                    content: None,
                                                    tool_calls: None,
                                                    tool_call_id: None,
                                                    reasoning_content: None,
                                                },
                                                events: Vec::new(),
                                                attempt: crate::provider_adapter::streaming::CutAttemptState {
                                                    usage: None,
                                                    generation_header_id: None,
                                                    response_id: None,
                                                    requested_output_tokens: None,
                                                    observed_output: Default::default(),
                                                },
                                            });
                                        }
                                    }
                                    continue;
                                }
                                None => break Err(e),
                            },
                            ok => break ok,
                        }
                    };
                    match stream_result {
                        Ok(crate::provider_adapter::StreamOutcome::Completed {
                            response: resp,
                            events,
                        }) => {
                            let usage_required = self.accounting_fail_closed();
                            if let Some(usage) = resp.usage.as_ref() {
                                if !usage.anomalies.is_empty() {
                                    tracing::warn!(
                                        turn,
                                        anomalies = ?usage.anomalies,
                                        "provider usage metadata is structurally invalid"
                                    );
                                    warnings.push(format!(
                                        "provider_usage_malformed on turn {turn}: {}",
                                        usage.anomalies.join("; ")
                                    ));
                                }
                                if !usage.contract_anomalies.is_empty() {
                                    tracing::warn!(
                                        turn,
                                        anomalies = ?usage.contract_anomalies,
                                        "provider usage contradicts a declared request contract"
                                    );
                                    warnings.push(format!(
                                        "provider_contract_anomaly on turn {turn}: {}",
                                        usage.contract_anomalies.join("; ")
                                    ));
                                }
                                if !usage.metadata_anomalies.is_empty() {
                                    tracing::warn!(
                                        turn,
                                        anomalies = ?usage.metadata_anomalies,
                                        "provider optional billing metadata is malformed"
                                    );
                                    warnings.push(format!(
                                        "provider_metadata_malformed on turn {turn}: {}",
                                        usage.metadata_anomalies.join("; ")
                                    ));
                                }
                            }
                            let valid_usage = resp.usage.as_ref().filter(|usage| usage.is_valid());
                            // Token-schema validity and provider-reported spend
                            // validity are independent. Preserve a signed,
                            // non-negative reported charge even when token fields
                            // are missing/malformed; never synthesize zero tokens.
                            let settled_spend_only = if valid_usage.is_none() {
                                if let Some(reported_cost_usd) = resp
                                    .usage
                                    .as_ref()
                                    .and_then(|usage| usage.reported_cost_usd)
                                {
                                    if let Err(error) = self
                                        .settle_provider_spend_only(
                                            turn,
                                            reported_cost_usd,
                                            turn_start.elapsed().as_millis() as u64,
                                        )
                                        .await
                                    {
                                        let settlement_error = format!(
                                            "independent provider-reported spend settlement failed: {error:#}"
                                        );
                                        let close_result = self
                                            .close_active_provider_attempt_accounting(
                                                &mut attempt_accounting,
                                                json!({
                                                    "turn": turn,
                                                    "state": "settlement_failed",
                                                    "settlement_error": &settlement_error,
                                                }),
                                            )
                                            .await;
                                        state = State::Errored {
                                            error: match close_result {
                                                Ok(()) => settlement_error,
                                                Err(close_error) => format!(
                                                    "{settlement_error}; resume-critical provider attempt accounting closure failed: {close_error}"
                                                ),
                                            },
                                        };
                                        continue;
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            if usage_required && valid_usage.is_none() {
                                let usage_detail = resp
                                    .usage
                                    .as_ref()
                                    .map(|usage| {
                                        if usage.anomalies.is_empty() {
                                            "required input/output token counts are incomplete"
                                                .to_string()
                                        } else {
                                            usage.anomalies.join("; ")
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        "required usage snapshot is missing".to_string()
                                    });
                                state = State::Errored {
                                    error: format!(
                                        "provider_accounting_invalid: required usage snapshot is \
                                         missing or malformed on turn {turn}: {usage_detail} \
                                         (requested_output_tokens={:?}, generation_header_id={:?}, \
                                         response_id={:?}, provider_usage={:?})",
                                        resp.requested_output_tokens,
                                        resp.generation_header_id,
                                        resp.response_id,
                                        resp.usage,
                                    ),
                                };
                                let original_accounting_error = match &state {
                                    State::Errored { error } => error.clone(),
                                    _ => {
                                        unreachable!("required accounting failure set error state")
                                    }
                                };
                                if let Err(error) = self
                                    .close_active_provider_attempt_accounting(
                                        &mut attempt_accounting,
                                        json!({
                                            "turn": turn,
                                            "state": "accounting_unavailable_fail_closed",
                                            "usage": resp.usage,
                                            "requested_output_tokens": resp.requested_output_tokens,
                                            "generation_header_id": resp.generation_header_id,
                                            "response_id": resp.response_id,
                                            "observed_output": resp.observed_output,
                                        }),
                                    )
                                    .await
                                {
                                    state = State::Errored {
                                        error: format!(
                                            "{original_accounting_error}; resume-critical provider attempt accounting closure failed: {error}"
                                        ),
                                    };
                                }
                                continue;
                            }
                            if resp.usage.is_some() && valid_usage.is_none() {
                                warnings.push(format!(
                                    "provider accounting unavailable on turn {turn}; malformed \
                                     token counts were not settled as zero"
                                ));
                            } else if resp.usage.is_none() {
                                warnings.push(format!(
                                    "provider accounting unavailable on turn {turn}; no usage \
                                     snapshot was reported and no token counts were settled"
                                ));
                            }
                            let token_counts =
                                valid_usage.and_then(|usage| usage.complete_token_counts());
                            let (input_tok, output_tok) = token_counts.unwrap_or((0, 0));
                            let cost = if settled_spend_only {
                                CostBreakdown {
                                    usd: resp
                                        .usage
                                        .as_ref()
                                        .and_then(|usage| usage.reported_cost_usd)
                                        .unwrap_or(0.0),
                                    source: PricingSource::ProviderReported,
                                }
                            } else {
                                self.compute_cost_for_usage(valid_usage)
                            };
                            let usd = cost.usd;
                            // §2: an operator auditing spend must be able to tell
                            // "free" from "untracked", and know when a model's
                            // cost came from provider-default rates rather than a
                            // per-model entry. Default policy is warn (not
                            // fail-closed): missing pricing does not error the
                            // turn — it records a loud signal and keeps running.
                            match cost.source {
                                PricingSource::Unpriced | PricingSource::ByokUntracked => {
                                    if input_tok != 0 || output_tok != 0 {
                                        tracing::warn!(
                                            model = %self.model_name,
                                            provider_id = %self.provider_id,
                                            input_tokens = input_tok,
                                            output_tokens = output_tok,
                                            "turn consumed tokens but computed cost is $0 — no \
                                             pricing configured for this model; spend is untracked"
                                        );
                                        if !cost_untracked_warned {
                                            cost_untracked_warned = true;
                                            warnings.push(format!(
                                                "cost untracked: turns consumed tokens but no \
                                                 pricing is configured for model `{model}`; \
                                                 spend is recorded as $0 (first seen turn {turn})",
                                                model = self.model_name,
                                            ));
                                            // Braid record so an operator auditing
                                            // spend can see the untracked turn inline.
                                            // `cost_untracked` is a canonical
                                            // RuntimeEventType (ryeos-runtime events.rs).
                                            record_callback_warning(
                                                &mut warnings,
                                                "cost_untracked",
                                                self.callback
                                                    .append_runtime_event(
                                                        ryeos_runtime::RuntimeEventType::CostUntracked,
                                                        json!({
                                                            "turn": turn,
                                                            "model": self.model_name,
                                                            "input_tokens": input_tok,
                                                            "output_tokens": output_tok,
                                                            "reason": match cost.source {
                                                                PricingSource::ByokUntracked => "byok_upstream_charge_untracked",
                                                                _ => "pricing_missing",
                                                            },
                                                        }),
                                                    )
                                                    .await,
                                            );
                                        }
                                    }
                                }
                                PricingSource::ProviderDefault => {
                                    if !provider_default_pricing_logged {
                                        provider_default_pricing_logged = true;
                                        tracing::info!(
                                            model = %self.model_name,
                                            provider_id = %self.provider_id,
                                            "model has no per-model pricing entry; costing this \
                                             run at provider-default rates"
                                        );
                                    }
                                }
                                PricingSource::ProviderReported
                                | PricingSource::PerModel
                                | PricingSource::ExplicitlyFree => {}
                            }

                            if let Some(usage) = valid_usage {
                                if let Err(e) = self
                                    .settle_provider_usage(
                                        turn,
                                        usage,
                                        usd,
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await
                                {
                                    let settlement_error =
                                        format!("provider usage settlement failed: {e:#}");
                                    let close_result = self
                                        .close_active_provider_attempt_accounting(
                                            &mut attempt_accounting,
                                            json!({
                                                "turn": turn,
                                                "state": "settlement_failed",
                                                "settlement_error": &settlement_error,
                                            }),
                                        )
                                        .await;
                                    state = State::Errored {
                                        error: match close_result {
                                            Ok(()) => settlement_error,
                                            Err(close_error) => format!(
                                                "{settlement_error}; resume-critical provider attempt accounting closure failed: {close_error}"
                                            ),
                                        },
                                    };
                                    continue;
                                }
                            }
                            if let Err(error) = self
                                .close_active_provider_attempt_accounting(
                                    &mut attempt_accounting,
                                    json!({
                                        "turn": turn,
                                        "state": if valid_usage.is_some() {
                                            "reported"
                                        } else if settled_spend_only {
                                            "reported_spend_only"
                                        } else {
                                            "accounting_unavailable_warn"
                                        },
                                        "usage": resp.usage,
                                        "settled_cost_usd": usd,
                                        "pricing_source": cost.source.as_str(),
                                        "requested_output_tokens": resp.requested_output_tokens,
                                        "generation_header_id": resp.generation_header_id,
                                        "response_id": resp.response_id,
                                        "observed_output": resp.observed_output,
                                    }),
                                )
                                .await
                            {
                                state = State::Errored {
                                    error: format!(
                                        "resume-critical completed provider attempt accounting closure failed: {error}"
                                    ),
                                };
                                continue;
                            }
                            if crate::directive::normalize_finish_reason(
                                resp.finish_reason.as_deref(),
                            ) == FinishReason::Length
                            {
                                state = State::Errored {
                                    error: "provider_output_limit_reached: provider ended the \
                                            response at its declared/native output limit; this is \
                                            distinct from a RyeOS-local output byte limit"
                                        .to_string(),
                                };
                                continue;
                            }
                            let error_finish_reason =
                                self.provider_config
                                    .schemas
                                    .as_ref()
                                    .and_then(|schemas| schemas.streaming.as_ref())
                                    .and_then(|streaming| streaming.metadata.as_ref())
                                    .and_then(|metadata| metadata.error.as_ref())
                                    .is_some_and(|error| {
                                        resp.finish_reason.as_deref().is_some_and(|actual| {
                                            error.finish_reasons.iter().any(|declared| {
                                                declared.eq_ignore_ascii_case(actual)
                                            })
                                        })
                                    });
                            if error_finish_reason {
                                state = State::Errored {
                                    error: format!(
                                        "provider_protocol_error: response ended with declared \
                                         error finish reason {:?}, but no valid configured error \
                                         object was present (generation_header_id={:?}, \
                                         response_id={:?})",
                                        resp.finish_reason,
                                        resp.generation_header_id,
                                        resp.response_id,
                                    ),
                                };
                                continue;
                            }
                            self.messages.push(resp.message.clone());
                            let assistant_message = match serde_json::to_value(&resp.message) {
                                Ok(value) => value,
                                Err(e) => {
                                    state = State::Errored {
                                        error: format!(
                                            "serialize assistant message for turn completion: {e}"
                                        ),
                                    };
                                    continue;
                                }
                            };
                            let provider_accounting = Some(json!({
                                "attempt_id": format!("{}:{turn}:{}", self.thread_id, attempt + 1),
                                "attempt_number": attempt + 1,
                                "input_tokens": resp.usage.as_ref().and_then(|usage| usage.input_tokens),
                                "output_tokens": resp.usage.as_ref().and_then(|usage| usage.output_tokens),
                                "reasoning_tokens": resp.usage.as_ref().and_then(|usage| usage.reasoning_tokens),
                                "reported_cost_usd": resp.usage.as_ref().and_then(|usage| usage.reported_cost_usd),
                                "cost_details": resp.usage.as_ref().and_then(|usage| usage.cost_details.as_ref()),
                                "is_byok": resp.usage.as_ref().and_then(|usage| usage.is_byok),
                                "snapshots_seen": resp.usage.as_ref().map(|usage| usage.snapshots_seen),
                                "settled_cost_usd": usd,
                                "pricing_source": cost.source.as_str(),
                                "source": resp.usage.as_ref().map(|usage| usage.source),
                                "comparability": resp.usage.as_ref().map(|usage| usage.comparability),
                                "provider_limit_contract": resp.usage.as_ref().map(|usage| usage.provider_limit_contract),
                                "schema_errors": resp.usage.as_ref().map(|usage| &usage.anomalies),
                                "metadata_errors": resp.usage.as_ref().map(|usage| &usage.metadata_anomalies),
                                "contract_anomalies": resp.usage.as_ref().map(|usage| &usage.contract_anomalies),
                                "requested_output_tokens": resp.requested_output_tokens,
                                "observed_output": &resp.observed_output,
                                "generation_header_id": &resp.generation_header_id,
                                "response_id": &resp.response_id,
                            }));
                            if let Err(e) = self
                                .callback
                                .emit_turn_complete(
                                    turn,
                                    token_counts,
                                    Some(assistant_message),
                                    provider_accounting,
                                )
                                .await
                            {
                                state = State::Errored {
                                    error: format!(
                                        "resume-critical callback emit_turn_complete failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            if let Some(ref reason) = resp.finish_reason {
                                tracing::debug!(finish_reason = %reason, "provider response");
                            }
                            if resp.generation_header_id.is_some() || resp.response_id.is_some() {
                                tracing::debug!(
                                    generation_header_id = ?resp.generation_header_id,
                                    response_id = ?resp.response_id,
                                    "provider response identifiers"
                                );
                            }

                            // Progressive StreamEvents were already published as
                            // live/ephemeral cognition_out events. The indexed
                            // turn consequence was emitted above; this pass is
                            // diagnostic-only.
                            State::Streaming { events }
                        }
                        Ok(crate::provider_adapter::StreamOutcome::Cancelled { attempt: cut }) => {
                            if attempt_accounting.has_active() {
                                let settlement = match self
                                    .settle_available_attempt_accounting(
                                        turn,
                                        cut.usage.as_ref(),
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await
                                {
                                    Ok(true) => "reported",
                                    Ok(false) => "accounting_unavailable_cancelled",
                                    Err(error) => {
                                        let settlement_error = format!(
                                            "cancelled provider attempt accounting settlement failed: {error:#}"
                                        );
                                        let close_result = self
                                            .close_active_provider_attempt_accounting(
                                                &mut attempt_accounting,
                                                json!({
                                                    "turn": turn,
                                                    "state": "settlement_failed",
                                                    "settlement_error": &settlement_error,
                                                }),
                                            )
                                            .await;
                                        state = State::Errored {
                                            error: match close_result {
                                                Ok(()) => settlement_error,
                                                Err(close_error) => format!(
                                                    "{settlement_error}; resume-critical provider attempt accounting closure failed: {close_error}"
                                                ),
                                            },
                                        };
                                        continue;
                                    }
                                };
                                if let Err(error) = self
                                    .close_active_provider_attempt_accounting(
                                        &mut attempt_accounting,
                                        json!({
                                            "turn": turn,
                                            "state": settlement,
                                            "usage": cut.usage,
                                            "generation_header_id": cut.generation_header_id,
                                            "response_id": cut.response_id,
                                            "requested_output_tokens": cut.requested_output_tokens,
                                            "observed_output": cut.observed_output,
                                        }),
                                    )
                                    .await
                                {
                                    state = State::Errored {
                                        error: format!(
                                            "resume-critical cancelled provider attempt accounting closure failed: {error}"
                                        ),
                                    };
                                    continue;
                                }
                            }
                            // SIGTERM remains a cancellation, not a provider
                            // failure, after independently settling any usage
                            // received before the signal.
                            State::Cancelled
                        }
                        Ok(crate::provider_adapter::StreamOutcome::Interrupted {
                            partial_message,
                            events,
                            attempt: cut,
                        }) => {
                            if attempt_accounting.has_active() {
                                let settlement = match self
                                    .settle_available_attempt_accounting(
                                        turn,
                                        cut.usage.as_ref(),
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await
                                {
                                    Ok(true) => "reported",
                                    Ok(false) => "accounting_unavailable_interrupted",
                                    Err(error) => {
                                        let settlement_error = format!(
                                            "interrupted provider attempt accounting settlement failed: {error:#}"
                                        );
                                        let close_result = self
                                            .close_active_provider_attempt_accounting(
                                                &mut attempt_accounting,
                                                json!({
                                                    "turn": turn,
                                                    "state": "settlement_failed",
                                                    "settlement_error": &settlement_error,
                                                }),
                                            )
                                            .await;
                                        state = State::Errored {
                                            error: match close_result {
                                                Ok(()) => settlement_error,
                                                Err(close_error) => format!(
                                                    "{settlement_error}; resume-critical provider attempt accounting closure failed: {close_error}"
                                                ),
                                            },
                                        };
                                        continue;
                                    }
                                };
                                if let Err(error) = self
                                    .close_active_provider_attempt_accounting(
                                        &mut attempt_accounting,
                                        json!({
                                            "turn": turn,
                                            "state": settlement,
                                            "usage": cut.usage,
                                            "generation_header_id": cut.generation_header_id,
                                            "response_id": cut.response_id,
                                            "requested_output_tokens": cut.requested_output_tokens,
                                            "observed_output": cut.observed_output,
                                        }),
                                    )
                                    .await
                                {
                                    state = State::Errored {
                                        error: format!(
                                            "resume-critical interrupted provider attempt accounting closure failed: {error}"
                                        ),
                                    };
                                    continue;
                                }
                            }
                            // Live interrupt (SIGUSR1) cut the in-flight cognition.
                            // Surface any provider warnings from the partial stream
                            // (the diagnostic State::Streaming pass is skipped on the
                            // interrupt path, so scrape them here) before sealing.
                            for ev in &events {
                                if let StreamEvent::Warning { code, message } = ev {
                                    tracing::warn!(
                                        code = %code,
                                        message = %message,
                                        "provider warning during interrupted stream"
                                    );
                                    warnings.push(format!("provider warning: [{code}] {message}"));
                                }
                            }
                            // Observe-and-reset the flag so this SIGUSR1 cuts
                            // exactly one cognition.
                            self.harness.take_interrupt();
                            // DECISION 1: an interrupted attempt is not a completed
                            // turn — refund so the redirect's fresh cognition stays
                            // within `limits.turns` (record_turn ran at entry).
                            self.harness.refund_turn();
                            // Runaway backstop (separate from the refunded turn).
                            if !self.harness.record_interrupt() {
                                state = State::Errored {
                                    error: "live interrupt limit exceeded".to_string(),
                                };
                                continue;
                            }

                            // Seal the partial as a transcript-bearing
                            // cognition_out{interrupted:true} (content/reasoning
                            // only, no tool_calls) so the braid holds an honest,
                            // foldable consequence — then mirror it into the live
                            // wire-fold so the redirect has context.
                            let partial_value = match serde_json::to_value(&partial_message) {
                                Ok(v) => Some(v),
                                Err(e) => {
                                    state = State::Errored {
                                        error: format!(
                                            "serialize interrupted partial message: {e}"
                                        ),
                                    };
                                    continue;
                                }
                            };
                            if let Err(e) = self
                                .callback
                                .emit_turn_interrupted(turn, partial_value)
                                .await
                            {
                                state = State::Errored {
                                    error: format!(
                                        "resume-critical callback emit_turn_interrupted failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            self.messages.push(partial_message);

                            // Back to the between-turns boundary: CheckingLimits
                            // folds the queued input (DECISION 2: if the poll is
                            // empty, a fresh cognition still runs with no new input)
                            // and runs the redirect cognition. Does NOT finalize.
                            State::CheckingLimits
                        }
                        Err(e) => {
                            if e.downcast_ref::<ProviderAttemptAccountingPersistenceError>()
                                .is_some()
                            {
                                // The exact terminal payload may already be
                                // durable even though append and replay could
                                // not prove it. A different fallback transition
                                // would risk two terminal states for one attempt.
                                state = State::Errored {
                                    error: format!("{e:#}"),
                                };
                                continue;
                            }
                            if !attempt_accounting.has_active() {
                                // The issued attempt was already closed before a
                                // retry-admission/backoff failure. Do not attach
                                // another terminal transition to its diagnostic
                                // last-attempt locator.
                                state = State::Errored {
                                    error: format!("{e:#}"),
                                };
                                continue;
                            }
                            let error = if let Some(limit) = e.downcast_ref::<
                                crate::provider_adapter::LocalOutputByteLimitError,
                            >() {
                                let limit = limit.clone();
                                tracing::error!(
                                    turn,
                                    accepted_bytes = limit.accepted_bytes,
                                    prospective_bytes = limit.prospective_bytes,
                                    cap_bytes = limit.cap_bytes,
                                    accepted_output_events = limit.accepted_output_events,
                                    live_output_events_emitted =
                                        limit.live_output_events_emitted,
                                    generation_header_id = ?limit.generation_header_id,
                                    response_id = ?limit.response_id,
                                    "provider attempt crossed the RyeOS-local output byte limit"
                                );
                                let mut detail = format!("local_byte_limit_exceeded: {limit}");
                                if let Some(usage) = limit.usage.as_ref() {
                                    if !usage.is_valid() {
                                        detail.push_str(
                                            "; provider accounting was malformed and was not \
                                             settled as zero",
                                        );
                                        if let Some(reported_cost_usd) = usage.reported_cost_usd {
                                            if let Err(settlement_error) = self
                                                .settle_provider_spend_only(
                                                    turn,
                                                    reported_cost_usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await
                                            {
                                                detail.push_str(&format!(
                                                    "; reported-spend settlement also failed: {settlement_error:#}"
                                                ));
                                            }
                                        }
                                    } else {
                                        let cost = self.compute_cost_for_usage(Some(usage));
                                        if let Err(settlement_error) = self
                                            .settle_provider_usage(
                                                turn,
                                                usage,
                                                cost.usd,
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await
                                        {
                                            detail.push_str(&format!(
                                                "; accounting settlement also failed: \
                                                 {settlement_error:#}"
                                            ));
                                        }
                                    }
                                }
                                detail
                            } else if let Some(provider_error) = e.downcast_ref::<
                                crate::provider_adapter::ProviderReportedStreamError,
                            >() {
                                let provider_error = provider_error.clone();
                                tracing::error!(
                                    turn,
                                    provider_error_code = ?provider_error.code,
                                    provider_error_metadata_present = provider_error.metadata.is_some(),
                                    accepted_bytes = provider_error.accepted_bytes,
                                    accepted_output_events =
                                        provider_error.accepted_output_events,
                                    live_output_events_emitted =
                                        provider_error.live_output_events_emitted,
                                    generation_header_id =
                                        ?provider_error.generation_header_id,
                                    response_id = ?provider_error.response_id,
                                    "provider reported an error inside the response stream"
                                );
                                let mut detail =
                                    format!("provider_reported_error: {provider_error}");
                                if let Some(usage) = provider_error.usage.as_ref() {
                                    if !usage.is_valid() {
                                        detail.push_str(
                                            "; provider accounting was malformed and was not \
                                             settled as zero",
                                        );
                                        if let Some(reported_cost_usd) = usage.reported_cost_usd {
                                            if let Err(settlement_error) = self
                                                .settle_provider_spend_only(
                                                    turn,
                                                    reported_cost_usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await
                                            {
                                                detail.push_str(&format!(
                                                    "; reported-spend settlement also failed: {settlement_error:#}"
                                                ));
                                            }
                                        }
                                    } else {
                                        let (input_tokens, output_tokens) = usage
                                            .complete_token_counts()
                                            .expect("valid provider usage has complete token counts");
                                        let cost = self.compute_cost_for_usage(Some(usage));
                                        if matches!(
                                            cost.source,
                                            PricingSource::Unpriced
                                                | PricingSource::ByokUntracked
                                        )
                                            && (input_tokens != 0 || output_tokens != 0)
                                        {
                                            warnings.push(format!(
                                                "cost untracked for failed provider attempt on turn \
                                                 {turn}: model `{}` has no usable price",
                                                self.model_name
                                            ));
                                        }
                                        if let Err(settlement_error) = self
                                            .settle_provider_usage(
                                                turn,
                                                usage,
                                                cost.usd,
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await
                                        {
                                            detail.push_str(&format!(
                                                "; accounting settlement also failed: \
                                                 {settlement_error:#}"
                                            ));
                                        }
                                    }
                                }
                                detail
                            } else if let Some(mid_stream) = e.downcast_ref::<
                                crate::provider_adapter::ProviderStreamError,
                            >().and_then(|error| match error {
                                crate::provider_adapter::ProviderStreamError::MidStream {
                                    accepted_bytes,
                                    accepted_output_events,
                                    live_output_events_emitted,
                                    usage,
                                    generation_header_id,
                                    response_id,
                                    requested_output_tokens,
                                    detail,
                                } => Some((
                                    *accepted_bytes,
                                    *accepted_output_events,
                                    *live_output_events_emitted,
                                    usage.clone(),
                                    generation_header_id.clone(),
                                    response_id.clone(),
                                    *requested_output_tokens,
                                    detail.clone(),
                                )),
                                _ => None,
                            }) {
                                let (
                                    accepted_bytes,
                                    accepted_output_events,
                                    live_output_events_emitted,
                                    usage,
                                    generation_header_id,
                                    response_id,
                                    requested_output_tokens,
                                    transport_detail,
                                ) = mid_stream;
                                let mut detail = format!(
                                    "provider_stream_interrupted: {transport_detail} \
                                     (accepted_bytes={accepted_bytes}, \
                                     accepted_output_events={accepted_output_events}, \
                                     live_output_events_emitted={live_output_events_emitted}, \
                                     generation_header_id={generation_header_id:?}, \
                                     response_id={response_id:?}, \
                                     requested_output_tokens={requested_output_tokens:?})"
                                );
                                if let Err(settlement_error) = self
                                    .settle_available_attempt_accounting(
                                        turn,
                                        usage.as_ref(),
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await
                                {
                                    detail.push_str(&format!(
                                        "; accounting settlement also failed: {settlement_error:#}"
                                    ));
                                }
                                detail
                            } else if let Some(callback_error) = e.downcast_ref::<
                                crate::provider_adapter::streaming::RuntimeCallbackPublicationError,
                            >() {
                                let callback_error = callback_error.clone();
                                let mut detail = callback_error.to_string();
                                if let Err(settlement_error) = self
                                    .settle_available_attempt_accounting(
                                        turn,
                                        callback_error.usage.as_ref(),
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await
                                {
                                    detail.push_str(&format!(
                                        "; accounting settlement also failed: {settlement_error:#}"
                                    ));
                                }
                                detail
                            } else if let Some(protocol_error) = e.downcast_ref::<
                                crate::provider_adapter::ProviderProtocolStreamError,
                            >() {
                                let protocol_error = protocol_error.clone();
                                let mut detail = protocol_error.to_string();
                                if let Some(usage) = protocol_error.usage.as_ref() {
                                    if usage.is_valid() {
                                        let cost = self.compute_cost_for_usage(Some(usage));
                                        if let Err(settlement_error) = self
                                            .settle_provider_usage(
                                                turn,
                                                usage,
                                                cost.usd,
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await
                                        {
                                            detail.push_str(&format!(
                                                "; accounting settlement also failed: {settlement_error:#}"
                                            ));
                                        }
                                    } else if let Some(reported_cost_usd) =
                                        usage.reported_cost_usd
                                    {
                                        if let Err(settlement_error) = self
                                            .settle_provider_spend_only(
                                                turn,
                                                reported_cost_usd,
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await
                                        {
                                            detail.push_str(&format!(
                                                "; reported-spend settlement also failed: {settlement_error:#}"
                                            ));
                                        }
                                    }
                                }
                                detail
                            } else {
                                format!("{e:#}")
                            };
                            let carried_usage = e
                                .downcast_ref::<crate::provider_adapter::LocalOutputByteLimitError>()
                                .and_then(|error| error.usage.as_ref())
                                .or_else(|| {
                                    e.downcast_ref::<crate::provider_adapter::ProviderReportedStreamError>()
                                        .and_then(|error| error.usage.as_ref())
                                })
                                .or_else(|| {
                                    e.downcast_ref::<crate::provider_adapter::ProviderProtocolStreamError>()
                                        .and_then(|error| error.usage.as_ref())
                                })
                                .or_else(|| {
                                    e.downcast_ref::<crate::provider_adapter::streaming::RuntimeCallbackPublicationError>()
                                        .and_then(|error| error.usage.as_ref())
                                })
                                .or_else(|| {
                                    e.downcast_ref::<crate::provider_adapter::ProviderStreamError>()
                                        .and_then(|error| match error {
                                            crate::provider_adapter::ProviderStreamError::MidStream { usage, .. } => usage.as_ref(),
                                            _ => None,
                                        })
                                });
                            if let Err(close_error) = self
                                .close_active_provider_attempt_accounting(
                                    &mut attempt_accounting,
                                    json!({
                                            "turn": turn,
                                            "state": if carried_usage.is_some() {
                                                "reported_settlement_attempted"
                                            } else {
                                                "accounting_unavailable_terminal"
                                            },
                                            "usage": carried_usage,
                                            "provider_reported_error": e
                                                .downcast_ref::<crate::provider_adapter::ProviderReportedStreamError>()
                                                .map(|provider_error| json!({
                                                    "code": provider_error.code,
                                                    "message": provider_error.message,
                                                    "metadata": provider_error.metadata,
                                                    "generation_header_id": provider_error.generation_header_id,
                                                    "response_id": provider_error.response_id,
                                                    "requested_output_tokens": provider_error.requested_output_tokens,
                                                    "accepted_bytes": provider_error.accepted_bytes,
                                                    "accepted_output_events": provider_error.accepted_output_events,
                                                    "live_output_events_emitted": provider_error.live_output_events_emitted,
                                                })),
                                            "terminal_error": &error,
                                    }),
                                )
                                .await
                            {
                                State::Errored {
                                    error: format!(
                                        "{error}; resume-critical provider attempt accounting closure failed: {close_error}"
                                    ),
                                }
                            } else {
                                State::Errored { error }
                            }
                        }
                    }
                }

                State::Streaming { events } => {
                    record_callback_warning(
                        &mut warnings,
                        "stream_opened",
                        self.callback
                            .append_runtime_event(
                                RuntimeEventType::StreamOpened,
                                json!({"turn": turn}),
                            )
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
                                tracing::trace!(len = text.len(), "reasoning delta received");
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
                                warnings.push(format!("provider warning: [{code}] {message}"));
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
                            let has_tool_calls =
                                msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
                            let has_content = msg.content.as_ref().is_some_and(|c| {
                                !c.is_null() && c.as_str().is_none_or(|s| !s.is_empty())
                            });

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
                                let content = msg.content.unwrap_or(Value::Null);
                                // Steer pre-finalize drain (finding C): a no-tool
                                // content response is about to finalize. If an
                                // operator input is pending, fold it and take
                                // another turn instead — steering beats finalize.
                                // Content path only (never between a tool-call
                                // message and its tool results).
                                //
                                // Guard: only drain if another turn can actually
                                // start. Draining persists a durable cognition_in;
                                // draining with no turn left would strand it
                                // unanswered AND turn a clean completion into a
                                // limit failure. When no turn remains, finalize —
                                // the late input stays queued (cleared at finalize)
                                // and the operator resubmits as a settled
                                // continuation.
                                if self.can_start_another_turn() {
                                    match self.poll_pending_input().await {
                                        Ok(true) => State::CheckingLimits,
                                        Ok(false) => State::Finalizing { result: content },
                                        Err(e) => State::Errored { error: e },
                                    }
                                } else {
                                    State::Finalizing { result: content }
                                }
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
                        if let Err(e) = self
                            .callback
                            .emit_tool_dispatch(
                                &tc.name,
                                tc.id.as_deref(),
                                self.harness.effective_caps(),
                            )
                            .await
                        {
                            state = State::Errored {
                                error: format!(
                                    "resume-critical callback emit_tool_dispatch failed: {e}"
                                ),
                            };
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

                State::ProcessingToolResult {
                    call_id,
                    tool_name,
                    raw_args,
                    pending,
                    index,
                } => {
                    /// Tracks tool result metadata for SSE emission.
                    struct ToolResult {
                        tool: String,
                        content: String,
                        raw_size: u64,
                        result_guard_truncated: bool,
                        duplicate_of: Option<String>,
                        truncated_reason_override: Option<&'static str>,
                    }

                    let tool_result: ToolResult = match self.dispatcher.resolve(
                        &tool_name,
                        &raw_args,
                        Some(call_id.clone()),
                    ) {
                        Ok(dispatch_result) => {
                            // Record spawn for child executions (directive/graph)
                            match dispatch_result.dispatch_kind {
                                DispatchKind::DirectiveChild | DispatchKind::GraphChild => {
                                    self.harness.record_spawn();
                                }
                                DispatchKind::Tool => {}
                            }

                            // Risk assessment before dispatch
                            let required_cap =
                                format!("ryeos.execute.tool.{}", dispatch_result.canonical_ref);
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
                                        .append_runtime_event(
                                            RuntimeEventType::ToolCallResult,
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
                                    tool: tool_name.clone(),
                                    raw_size: body_str.len() as u64,
                                    content: body_str,
                                    result_guard_truncated: false,
                                    duplicate_of: None,
                                    truncated_reason_override: Some("error_envelope"),
                                }
                            } else {
                                match self
                                    .callback
                                    .dispatch_action(
                                        ryeos_runtime::callback::DispatchActionRequest {
                                            thread_id: self.thread_id.clone(),
                                            project_path: self.callback.project_path().to_string(),
                                            action: ryeos_runtime::callback::ActionPayload {
                                                operation_id: None,
                                                item_id: dispatch_result.canonical_ref.clone(),
                                                ref_bindings: std::collections::BTreeMap::new(),
                                                params: dispatch_result.arguments.clone(),
                                                thread: "inline".to_string(),
                                                // Directive tool-calls dispatch
                                                // `tool:` refs at their default
                                                // method; no method selector.
                                                call: None,
                                                facets: None,
                                                launch_window: None,
                                            },
                                            hook_dispatch: None,
                                        },
                                    )
                                    .await
                                {
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
                                                tracing::warn!(
                                                    "failed to serialize dispatch result: {e}"
                                                );
                                                Vec::new()
                                            });
                                        let raw_size = raw_bytes.len() as u64;
                                        let guarded = self.result_guard.process_bytes(&raw_bytes);
                                        let content =
                                            String::from_utf8_lossy(&guarded.bytes).to_string();
                                        ToolResult {
                                            tool: tool_name.clone(),
                                            content,
                                            raw_size,
                                            result_guard_truncated: guarded.truncated,
                                            duplicate_of: guarded.duplicate_of,
                                            truncated_reason_override: None,
                                        }
                                    }
                                    Err(e) => {
                                        let body_str = serde_json::to_string(
                                            &json!({"error": format!("{e:#}")}),
                                        )
                                        .unwrap_or_else(|_| {
                                            "{\"error\":\"dispatch failed\"}".to_string()
                                        });
                                        ToolResult {
                                            tool: tool_name.clone(),
                                            raw_size: body_str.len() as u64,
                                            content: body_str,
                                            result_guard_truncated: false,
                                            duplicate_of: None,
                                            truncated_reason_override: Some("error_envelope"),
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let body_str = serde_json::to_string(&json!({"error": e}))
                                .unwrap_or_else(|_| "{\"error\":\"resolve failed\"}".to_string());
                            ToolResult {
                                tool: tool_name.clone(),
                                raw_size: body_str.len() as u64,
                                content: body_str,
                                result_guard_truncated: false,
                                duplicate_of: None,
                                truncated_reason_override: Some("error_envelope"),
                            }
                        }
                    };

                    // Determine inline body and truncation flags
                    let inline_capped = tool_result.content.len()
                        > ryeos_runtime::callback_client::TOOL_RESULT_INLINE_MAX_BYTES;
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
                    if let Err(e) = self
                        .callback
                        .emit_tool_result(
                            &call_id,
                            &tool_result.tool,
                            body,
                            truncated,
                            truncated_reason,
                            tool_result.raw_size,
                            tool_result.duplicate_of.as_deref(),
                        )
                        .await
                    {
                        state = State::Errored {
                            error: format!("resume-critical callback emit_tool_result failed: {e}"),
                        };
                        continue;
                    }
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(tool_result.content)),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                        reasoning_content: None,
                    });

                    let next_index = index + 1;
                    if next_index < pending.len() {
                        State::DispatchingTools {
                            pending,
                            index: next_index,
                        }
                    } else {
                        // All tools processed — fire after_step hook
                        State::FiringHooks {
                            occurrence: ryeos_runtime::callback::HookDispatchOccurrence::DirectiveAfterStep {
                                definition_ref: self.definition_ref.clone(),
                                definition_hash: self.definition_hash.clone(),
                                turn,
                            },
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
                    let tool_result_content = match crate::adapter::parse_tool_arguments(&raw_args)
                    {
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
                                serde_json::to_string(&json!({"error": err})).unwrap_or_else(|_| {
                                    "{\"error\":\"output validation failed\"}".to_string()
                                })
                            } else {
                                // Publish outputs as artifact (non-fatal)
                                record_callback_warning(
                                    &mut warnings,
                                    "publish_artifact(directive_outputs)",
                                    self.callback
                                        .publish_artifact(directive_outputs_artifact(
                                            &self.thread_id,
                                            &args,
                                        ))
                                        .await,
                                );

                                // Fire tool_call_result for chain visibility
                                let outputs_json = serde_json::to_string(&args).unwrap_or_default();
                                let outputs_size = outputs_json.len() as u64;
                                if let Err(e) = self
                                    .callback
                                    .emit_tool_result(
                                        &call_id,
                                        "directive_return",
                                        Some(&outputs_json),
                                        false,
                                        None,
                                        outputs_size,
                                        None,
                                    )
                                    .await
                                {
                                    state = State::Errored {
                                        error: format!(
                                            "resume-critical callback emit_tool_result failed: {e}"
                                        ),
                                    };
                                    continue;
                                }

                                if let Err(e) = self.persist_terminal_outputs() {
                                    state = State::Errored {
                                        error: format!(
                                            "directive terminal persistence failed: {e:#}"
                                        ),
                                    };
                                    continue;
                                }

                                // Finalize thread. The persisted result mirrors
                                // the live RuntimeResult.result here (the
                                // `directive_return` sentinel); the structured
                                // outputs travel in `outputs` + the published
                                // artifact, so /execute and threads.get agree.
                                let completion = TerminalCompletion {
                                    status: ThreadTerminalStatus::Completed,
                                    outcome_code: Some("success".to_string()),
                                    result: Some(json!("directive_return")),
                                    error: None,
                                    cost: Some(
                                        serde_json::to_value(self.budget.cost()).expect(
                                            "validated directive cost must serialize for terminal settlement",
                                        ),
                                    ),
                                    // The structured return lives in `outputs`, not
                                    // `result` — carry it so a follow parent can
                                    // consume `${result.outputs.*}` on resume.
                                    outputs: args.clone(),
                                    warnings: warnings.clone(),
                                };
                                if let Err(e) = self.callback.finalize_thread(completion).await {
                                    guard.finalized = true;
                                    return Self::attach_warnings(RuntimeResult {
                                        success: false,
                                        status: RuntimeResultStatus::Failed,
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
                    if let Err(e) = self
                        .callback
                        .emit_tool_result(
                            &call_id,
                            "directive_return",
                            Some(&tool_result_content),
                            false,
                            Some("error_envelope"),
                            failure_size,
                            None,
                        )
                        .await
                    {
                        state = State::Errored {
                            error: format!("resume-critical callback emit_tool_result failed: {e}"),
                        };
                        continue;
                    }
                    self.messages.push(ProviderMessage {
                        role: "tool".to_string(),
                        content: Some(json!(tool_result_content)),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                        reasoning_content: None,
                    });
                    State::CheckingContinuation
                }

                State::FiringHooks {
                    occurrence,
                    context,
                    resume_to,
                } => {
                    let event = occurrence.event().to_string();
                    let callback = self.callback.clone();
                    let thread_id = self.thread_id.clone();
                    let project_path = self.callback.project_path().to_string();

                    let dispatcher: ryeos_runtime::hooks_eval::HookDispatcher =
                        Box::new(move |action, proj, hook_dispatch| {
                            let cb = callback.clone();
                            let tid = thread_id.clone();
                            Box::pin(async move {
                                let payload = ryeos_runtime::callback::parse_hook_action(action)
                                    .map_err(|message| {
                                        ryeos_runtime::callback::CallbackError::ActionFailed {
                                            code: "invalid_hook_action".to_string(),
                                            message,
                                            retryable: false,
                                        }
                                    })?;
                                let response = cb
                                    .dispatch_action(
                                        ryeos_runtime::callback::DispatchActionRequest {
                                            thread_id: tid,
                                            project_path: proj,
                                            action: payload,
                                            hook_dispatch: Some(hook_dispatch),
                                        },
                                    )
                                    .await?;
                                // Hooks run on the leaf result only —
                                // the parent-thread snapshot has no
                                // bearing on hook control flow.
                                normalize_hook_dispatch_result(response.result).map_err(|message| {
                                    ryeos_runtime::callback::CallbackError::ActionFailed {
                                        code: ryeos_runtime::envelope::HOOK_INTEGRITY_FAILURE_CODE
                                            .to_string(),
                                        message: message.to_string(),
                                        retryable: false,
                                    }
                                })
                            })
                        });

                    let hook_run = ryeos_runtime::hooks_eval::run_hooks(
                        occurrence,
                        &context,
                        &self.hooks,
                        &project_path,
                        &dispatcher,
                    )
                    .await;
                    let hook_cost = match &hook_run {
                        Ok(result) => result.cost.as_ref(),
                        Err(error) => error.cost.as_ref(),
                    };
                    if let Some(cost) = hook_cost {
                        if let Err(error) = self.budget.accumulate(cost) {
                            state = State::Errored {
                                error: format!(
                                    "hook event `{event}` cost violates accounting bounds: {error}"
                                ),
                            };
                            continue;
                        }
                    }

                    match hook_run {
                        Ok(hook_run) => {
                            let hook_result = hook_run.control;
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
                                    .append_runtime_event(
                                        RuntimeEventType::CognitionReasoning,
                                        json!({
                                            "hook_event": event.clone(),
                                            "hook_result": hook_result.clone(),
                                        }),
                                    )
                                    .await,
                            );

                            match hook_result.as_ref().map(HookAction::from_value) {
                                Some(Ok(action)) => match action {
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
                                },
                                Some(Err(e)) => State::Errored {
                                    error: format!(
                                        "hook event `{event}` returned invalid control action: {e}"
                                    ),
                                },
                                None => *resume_to,
                            }
                        }
                        Err(e) => State::Errored {
                            error: format!("hook event `{event}` failed: {e}"),
                        },
                    }
                }

                State::CheckingContinuation => {
                    let threshold = self.continuation.threshold();
                    // Measure the LIVE context window (post-trim messages), NOT
                    // cumulative chain spend — the threshold is a per-call
                    // quantity; comparing it to lifetime budget latches the
                    // check true and re-forks every successor.
                    let live_context = self
                        .continuation
                        .estimate_live_context_tokens(&self.messages);
                    tracing::info!(live_context, threshold, "checking continuation");
                    if self
                        .continuation
                        .should_continue_live_context(&self.messages)
                    {
                        // Context-window continuation boundary. Enabled: self-
                        // continue (fork a chained successor of the same
                        // directive). Disabled (default): STOP here with the
                        // current state — no nudge, no granted turn, no output
                        // enforcement. Emitting outputs before the boundary is the
                        // directive's job; the runtime does not do it for them.
                        // (Enabled is plain chain-fold; resume applies the
                        // resolved carry_turns policy when folding history.)
                        if self.continuation_config.enabled() {
                            State::FiringHooks {
                                occurrence: ryeos_runtime::callback::HookDispatchOccurrence::DirectiveContinuation {
                                    definition_ref: self.definition_ref.clone(),
                                    definition_hash: self.definition_hash.clone(),
                                    turn,
                                },
                                context: self.continuation_hook_context(live_context, threshold),
                                resume_to: Box::new(State::Continued),
                            }
                        } else {
                            let result = self
                                .messages
                                .last()
                                .and_then(|m| m.content.clone())
                                .unwrap_or(Value::Null);
                            State::Finalizing { result }
                        }
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
                    // Reaching Finalizing means no successful `directive_return`
                    // this segment (success finalizes inside
                    // ProcessingDirectiveReturn). When outputs are declared:
                    // with `return_nudge: true`, grant ONE corrective turn
                    // naming the missing call; otherwise (or after the nudge)
                    // settle with empty outputs and a recorded warning.
                    let declared_outputs: Vec<String> = self
                        .directive_outputs
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|o| o.name.clone())
                        .collect();
                    if !declared_outputs.is_empty() {
                        if self.return_nudge.enabled()
                            && !self.return_nudge_sent
                            && self.can_start_another_turn()
                        {
                            self.return_nudge_sent = true;
                            let nudge = self.return_nudge.message(&declared_outputs);
                            // Durable stimulus so the corrective turn is
                            // braid-visible; a failed append degrades to an
                            // unrecorded nudge rather than failing the run.
                            if let Err(e) = self.callback.emit_stimulus(&nudge).await {
                                tracing::warn!(
                                    error = %e,
                                    "return_nudge stimulus append failed; nudge turn proceeds unrecorded"
                                );
                            }
                            self.messages.push(ProviderMessage {
                                role: "user".to_string(),
                                content: Some(json!(nudge)),
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_content: None,
                            });
                            state = State::CheckingLimits;
                            continue;
                        }
                        warnings.push(format!(
                            "declared outputs ({}) were never emitted via directive_return; \
                             settling with empty outputs",
                            declared_outputs.join(", ")
                        ));
                    }
                    if let Err(e) = self.persist_terminal_outputs() {
                        state = State::Errored {
                            error: format!("directive terminal persistence failed: {e:#}"),
                        };
                        continue;
                    }
                    let completion = TerminalCompletion {
                        status: ThreadTerminalStatus::Completed,
                        outcome_code: Some("success".to_string()),
                        result: Some(result.clone()),
                        error: None,
                        cost: Some(serde_json::to_value(self.budget.cost()).expect(
                            "validated directive cost must serialize for terminal settlement",
                        )),
                        outputs: json!({}),
                        warnings: warnings.clone(),
                    };
                    if let Err(e) = self.callback.finalize_thread(completion).await {
                        let runtime_result = RuntimeResult {
                            success: false,
                            status: RuntimeResultStatus::Failed,
                            thread_id: self.thread_id.clone(),
                            result: Some(json!(format!(
                                "resume-critical callback finalize_thread failed: {e}"
                            ))),
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
                    // Cut off by a limit mid-task: hand off to a chain-fold
                    // successor. Continuation is autonomous by construction — no
                    // reason/gate/mode, only an optional free-form string for logs.
                    //
                    // Do NOT swallow: a failed handoff must not settle the thread
                    // `continued` with no recorded successor. Surface as terminal
                    // `failed`.
                    if let Err(e) = self.persist_terminal_outputs() {
                        state = State::Errored {
                            error: format!("directive terminal persistence failed: {e:#}"),
                        };
                        continue;
                    }
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: RuntimeResultStatus::Continued,
                        thread_id: self.thread_id.clone(),
                        result: None,
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                        warnings: Vec::new(),
                    };
                    let completion = TerminalCompletion {
                        status: ThreadTerminalStatus::Continued,
                        outcome_code: Some(ThreadTerminalStatus::Continued.as_str().to_string()),
                        result: runtime_result.result.clone(),
                        error: None,
                        cost: runtime_result.cost.as_ref().map(|cost| {
                            serde_json::to_value(cost).expect(
                                "typed directive cost must serialize for continuation settlement",
                            )
                        }),
                        outputs: runtime_result.outputs.clone(),
                        warnings: warnings.clone(),
                    };
                    if let Err(e) = self
                        .callback
                        .request_continuation(Some(CONTINUATION_LOG_REASON), completion)
                        .await
                    {
                        let runtime_result = RuntimeResult {
                            success: false,
                            status: RuntimeResultStatus::Failed,
                            thread_id: self.thread_id.clone(),
                            result: Some(json!(format!("continuation handoff failed: {e}"))),
                            outputs: json!({}),
                            cost: Some(self.budget.cost()),
                            warnings: Vec::new(),
                        };
                        guard.finalized = true;
                        return Self::attach_warnings(runtime_result, &mut warnings);
                    }

                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }

                State::Errored { error } => {
                    let error = match self.persist_terminal_outputs() {
                        Ok(()) => error,
                        Err(persistence_error) => format!(
                            "{error}; directive terminal persistence also failed: {persistence_error:#}"
                        ),
                    };
                    record_callback_warning(
                        &mut warnings,
                        "thread_failed(emit_error)",
                        self.callback.emit_error(&error).await,
                    );
                    let failure = runtime_failure_payload(
                        &error,
                        &self.thread_id,
                        turn,
                        attempt_accounting.last(),
                    );
                    let completion = TerminalCompletion {
                        status: ThreadTerminalStatus::Failed,
                        outcome_code: Some("failed".to_string()),
                        result: None,
                        error: Some(failure.clone()),
                        cost: Some(serde_json::to_value(self.budget.cost()).expect(
                            "validated directive cost must serialize for terminal settlement",
                        )),
                        outputs: json!({}),
                        warnings: warnings.clone(),
                    };
                    if let Err(e) = self.callback.finalize_thread(completion).await {
                        // Finalize failed — surface in the error result
                        warnings.push(format!(
                            "resume-critical callback finalize_thread(failed) also failed: {e}"
                        ));
                    }
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: RuntimeResultStatus::Failed,
                        thread_id: self.thread_id.clone(),
                        result: Some(failure),
                        outputs: json!({}),
                        cost: Some(self.budget.cost()),
                        warnings: Vec::new(),
                    };
                    guard.finalized = true;
                    return Self::attach_warnings(runtime_result, &mut warnings);
                }

                State::Cancelled => {
                    if let Err(e) = self.persist_terminal_outputs() {
                        state = State::Errored {
                            error: format!("directive terminal persistence failed: {e:#}"),
                        };
                        continue;
                    }
                    let runtime_result = RuntimeResult {
                        success: false,
                        status: RuntimeResultStatus::Cancelled,
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

    fn continuation_hook_context(&self, live_context_tokens: u64, threshold_tokens: u64) -> Value {
        let remaining_spend_usd = self.budget.remaining_spend_usd();
        json!({
            "event": {
                "name": "continuation",
                "reason": CONTINUATION_LOG_REASON,
                "live_context_tokens": live_context_tokens,
                "threshold_tokens": threshold_tokens,
                "messages": self.messages.clone(),
                "usage": self.budget.cost(),
                "budget_remaining": {
                    "spend_usd": remaining_spend_usd,
                    "spend_unlimited": remaining_spend_usd.is_none(),
                },
                "declared_outputs": self.directive_outputs.clone().unwrap_or_default(),
            }
        })
    }

    async fn settle_provider_usage(
        &mut self,
        turn: u32,
        usage: &crate::provider_adapter::http::TokenUsage,
        usd: f64,
        elapsed_ms: u64,
    ) -> anyhow::Result<()> {
        let (input_tokens, output_tokens) = usage
            .complete_token_counts()
            .ok_or_else(|| anyhow::anyhow!("provider token usage is incomplete"))?;
        let turn_cost = RuntimeCost {
            input_tokens,
            output_tokens,
            total_usd: usd,
            basis: None,
        };
        let mut proposed_cost = self.budget.cost();
        proposed_cost
            .checked_accumulate(&turn_cost)
            .map_err(|error| {
                anyhow::anyhow!("provider usage violates accounting bounds: {error}")
            })?;
        self.harness
            .tokens_used()
            .checked_add(input_tokens)
            .and_then(|tokens| tokens.checked_add(output_tokens))
            .ok_or_else(|| anyhow::anyhow!("provider usage exceeds the directive token counter"))?;

        let proposed_usage = ryeos_state::ThreadUsage {
            completed_turns: self.harness.turns_used(),
            input_tokens: proposed_cost.input_tokens,
            output_tokens: proposed_cost.output_tokens,
            spend_usd: proposed_cost.total_usd,
            spawns_used: self.harness.spawns_used(),
            started_at: lillux::time::iso8601_now(),
            settled_at: lillux::time::iso8601_now(),
            last_settled_turn_seq: turn as u64,
            elapsed_ms,
            provider_id: Some(self.provider_id.clone()),
            model: Some(self.model_name.clone()),
            profile: self.matched_profile.clone(),
        };

        self.emit_thread_usage_idempotent(&proposed_usage).await?;

        self.harness
            .record_tokens(input_tokens, output_tokens)
            .expect("provider token usage was prevalidated");
        self.harness
            .record_spend(usd)
            .expect("provider spend was prevalidated");
        self.budget
            .report(input_tokens, output_tokens, usd)
            .expect("provider budget usage was prevalidated");
        Ok(())
    }

    /// Persist one provider-attempt accounting transition. If the callback ACK
    /// is lost after the daemon commits the event, replay proves the exact
    /// payload before the runner changes lifecycle state or issues a request.
    async fn persist_provider_attempt_accounting(
        &self,
        payload: Value,
    ) -> Result<(), ProviderAttemptAccountingPersistenceError> {
        match self
            .callback
            .append_runtime_event(RuntimeEventType::ProviderAttemptAccounting, payload.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(append_error) => {
                let replay = self
                    .callback
                    .replay_thread(&self.thread_id)
                    .await
                    .map_err(|replay_error| {
                        ProviderAttemptAccountingPersistenceError(format!(
                            "provider attempt accounting append failed: {append_error}; ACK recovery replay also failed: {replay_error}"
                        ))
                    })?;
                if replay.events.iter().rev().any(|event| {
                    event.event_type == RuntimeEventType::ProviderAttemptAccounting.as_str()
                        && event.payload == payload
                }) {
                    tracing::warn!(
                        thread_id = %self.thread_id,
                        "provider attempt accounting ACK was lost; exact persisted payload recovered by replay"
                    );
                    Ok(())
                } else {
                    Err(ProviderAttemptAccountingPersistenceError(format!(
                        "provider attempt accounting append failed: {append_error}; replay did not contain the exact transition"
                    )))
                }
            }
        }
    }

    /// Persist and acknowledge the terminal state for the one active attempt.
    /// The lifecycle remains active if persistence/replay fails, preventing a
    /// subsequent attempt from being admitted against an unresolved predecessor.
    async fn close_active_provider_attempt_accounting(
        &self,
        lifecycle: &mut AttemptAccountingLifecycle,
        mut payload: Value,
    ) -> anyhow::Result<()> {
        let attempt_id = lifecycle.active_for_close().map_err(anyhow::Error::msg)?;
        let object = payload.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("provider attempt accounting terminal payload must be an object")
        })?;
        if let Some(reported_id) = object.get("attempt_id") {
            if reported_id.as_str() != Some(attempt_id.as_str()) {
                anyhow::bail!(
                    "provider attempt accounting terminal payload ID contradicts active attempt `{attempt_id}`"
                );
            }
        } else {
            object.insert("attempt_id".to_string(), Value::String(attempt_id.clone()));
        }
        let payload = lifecycle
            .bind_closing_payload(payload)
            .map_err(anyhow::Error::msg)?;
        self.persist_provider_attempt_accounting(payload)
            .await
            .map_err(anyhow::Error::new)?;
        lifecycle
            .ack_closed(&attempt_id)
            .map_err(anyhow::Error::msg)
    }

    /// Settle every independently trustworthy part of a provider attempt.
    /// Complete, structurally valid token usage is settled with its cost;
    /// otherwise a valid signed reported charge is settled without inventing a
    /// token tuple. `Ok(false)` means accounting was explicitly unavailable.
    async fn settle_available_attempt_accounting(
        &mut self,
        turn: u32,
        usage: Option<&crate::provider_adapter::http::TokenUsage>,
        elapsed_ms: u64,
    ) -> anyhow::Result<bool> {
        let Some(usage) = usage else {
            return Ok(false);
        };
        if usage.is_valid() {
            let cost = self.compute_cost_for_usage(Some(usage));
            self.settle_provider_usage(turn, usage, cost.usd, elapsed_ms)
                .await?;
            return Ok(true);
        }
        if let Some(reported_cost_usd) = usage.reported_cost_usd {
            self.settle_provider_spend_only(turn, reported_cost_usd, elapsed_ms)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Preserve a structurally valid provider-reported charge even when token
    /// fields are independently malformed. No synthetic zero token tuple is
    /// created; the existing token totals remain unchanged.
    async fn settle_provider_spend_only(
        &mut self,
        turn: u32,
        usd: f64,
        elapsed_ms: u64,
    ) -> anyhow::Result<()> {
        let mut proposed_cost = self.budget.cost();
        proposed_cost
            .checked_accumulate(&RuntimeCost {
                input_tokens: 0,
                output_tokens: 0,
                total_usd: usd,
                basis: Some("provider_reported_spend_only".to_string()),
            })
            .map_err(|error| {
                anyhow::anyhow!("provider spend violates accounting bounds: {error}")
            })?;
        let proposed_usage = ryeos_state::ThreadUsage {
            completed_turns: self.harness.turns_used(),
            input_tokens: proposed_cost.input_tokens,
            output_tokens: proposed_cost.output_tokens,
            spend_usd: proposed_cost.total_usd,
            spawns_used: self.harness.spawns_used(),
            started_at: lillux::time::iso8601_now(),
            settled_at: lillux::time::iso8601_now(),
            last_settled_turn_seq: turn as u64,
            elapsed_ms,
            provider_id: Some(self.provider_id.clone()),
            model: Some(self.model_name.clone()),
            profile: self.matched_profile.clone(),
        };
        self.emit_thread_usage_idempotent(&proposed_usage).await?;
        self.harness
            .record_spend(usd)
            .expect("provider spend was prevalidated");
        self.budget
            .report(0, 0, usd)
            .expect("provider spend was prevalidated");
        Ok(())
    }

    /// Resolve callback-ACK ambiguity without double-settling. If the append
    /// response is lost after the daemon persisted the exact cumulative usage,
    /// replay proves the event exists and local counters may safely commit.
    async fn emit_thread_usage_idempotent(
        &self,
        usage: &ryeos_state::ThreadUsage,
    ) -> anyhow::Result<()> {
        let expected = serde_json::to_value(usage)
            .map_err(|error| anyhow::anyhow!("serialize thread usage for ACK recovery: {error}"))?;
        match self.callback.emit_thread_usage(usage).await {
            Ok(()) => Ok(()),
            Err(append_error) => {
                let replay =
                    self.callback
                        .replay_thread(&self.thread_id)
                        .await
                        .map_err(|replay_error| {
                            anyhow::anyhow!(
                            "resume-critical callback emit_thread_usage failed: {append_error}; \
                             ACK recovery replay also failed: {replay_error}"
                        )
                        })?;
                if replay.events.iter().rev().any(|event| {
                    event.event_type == RuntimeEventType::ThreadUsage.as_str()
                        && event.payload == expected
                }) {
                    tracing::warn!(
                        thread_id = %self.thread_id,
                        "thread_usage append ACK was lost; exact persisted payload recovered by replay"
                    );
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "resume-critical callback emit_thread_usage failed: {append_error}; \
                         replay did not contain the exact proposed settlement"
                    ))
                }
            }
        }
    }

    fn compute_cost(&self, input_tokens: u64, output_tokens: u64) -> CostBreakdown {
        let Some(ref pricing) = self.provider_config.pricing else {
            return CostBreakdown {
                usd: 0.0,
                source: PricingSource::Unpriced,
            };
        };
        if pricing.explicitly_free {
            return CostBreakdown {
                usd: 0.0,
                source: PricingSource::ExplicitlyFree,
            };
        }
        // Distinguish a per-model entry from the provider-default fallback so
        // the caller can flag the (otherwise silent) fallback exactly once.
        let (rates, source) = if let Some(p) = pricing.models.get(&self.model_name) {
            (p.clone(), PricingSource::PerModel)
        } else {
            match (pricing.input_per_million, pricing.output_per_million) {
                (Some(0.0), Some(0.0)) => {
                    return CostBreakdown {
                        usd: 0.0,
                        source: PricingSource::Unpriced,
                    }
                }
                (Some(i), Some(o)) => (
                    ryeos_directive_core::ModelPricing {
                        input_per_million: i,
                        output_per_million: o,
                    },
                    PricingSource::ProviderDefault,
                ),
                _ => {
                    return CostBreakdown {
                        usd: 0.0,
                        source: PricingSource::Unpriced,
                    }
                }
            }
        };
        let input_cost = (input_tokens as f64 / 1_000_000.0) * rates.input_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * rates.output_per_million;
        CostBreakdown {
            usd: input_cost + output_cost,
            source,
        }
    }

    fn compute_cost_for_usage(
        &self,
        usage: Option<&crate::provider_adapter::http::TokenUsage>,
    ) -> CostBreakdown {
        let (input_tokens, output_tokens) = usage
            .and_then(|usage| usage.complete_token_counts())
            .unwrap_or((0, 0));
        if let Some(reported) = usage.and_then(|usage| usage.reported_cost_usd) {
            if reported.is_finite() && reported >= 0.0 {
                return CostBreakdown {
                    usd: reported,
                    source: if reported == 0.0
                        && usage.is_some_and(|usage| usage.is_byok == Some(true))
                    {
                        PricingSource::ByokUntracked
                    } else {
                        PricingSource::ProviderReported
                    },
                };
            }
        }
        self.compute_cost(input_tokens, output_tokens)
    }

    /// Drain the run-loop's accumulated warnings into a finished
    /// `RuntimeResult`. Caller MUST invoke this on every terminal
    /// branch so callback drift is surfaced; a missed call would
    /// silently drop everything `record_callback_warning` recorded.
    fn attach_warnings(mut result: RuntimeResult, warnings: &mut Vec<String>) -> RuntimeResult {
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
            status: RuntimeResultStatus::Completed,
            thread_id: self.thread_id.clone(),
            result: Some(result),
            outputs: json!({}),
            cost: Some(self.budget.cost()),
            warnings: Vec::new(),
        }
    }
}

fn runtime_failure_payload(
    error: &str,
    thread_id: &str,
    turn: u32,
    attempt_id: Option<&str>,
) -> Value {
    const SUMMARY_CHARS: usize = 4_096;
    const TRUNCATION_SUFFIX: &str = "… [truncated; see diagnostic locator]";
    let code = error
        .split_once(':')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit())
        })
        .unwrap_or("directive_failed")
        .to_string();
    let sanitized = error
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = if sanitized.is_empty() {
        "directive failed without an error message".to_string()
    } else {
        sanitized
    };
    let summary = if sanitized.chars().count() <= SUMMARY_CHARS {
        sanitized
    } else {
        let prefix_chars = SUMMARY_CHARS.saturating_sub(TRUNCATION_SUFFIX.chars().count());
        format!(
            "{}{}",
            sanitized.chars().take(prefix_chars).collect::<String>(),
            TRUNCATION_SUFFIX,
        )
    };
    let failure = ryeos_runtime::RuntimeFailure {
        kind: ryeos_runtime::RUNTIME_FAILURE_KIND.to_string(),
        version: 1,
        code,
        summary,
        diagnostic_locator: ryeos_runtime::RuntimeFailureDiagnosticLocator {
            thread_id: thread_id.to_string(),
            turn: Some(turn),
            attempt_id: attempt_id.map(str::to_string),
            event_type: "thread_failed".to_string(),
        },
        retryable: false,
    };
    failure
        .validate()
        .expect("runtime-created failure DTO must satisfy its shared contract");
    serde_json::to_value(failure).expect("runtime failure DTO must serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::PricingConfig;
    use crate::harness::Harness;
    use ryeos_directive_core::ModelPricing;
    use ryeos_runtime::callback_client::CallbackClient;
    use ryeos_runtime::envelope::{EnvelopeCallback, EnvelopePolicy, HardLimits};
    use std::path::PathBuf;

    #[test]
    fn attempt_accounting_lifecycle_requires_ack_before_reuse() {
        let mut lifecycle = AttemptAccountingLifecycle::default();
        lifecycle
            .admit_after_pending_ack("T-test:1:1".to_string())
            .unwrap();
        let first = lifecycle.active_for_close().unwrap();
        let first_payload = json!({"attempt_id": &first, "state": "reported"});
        lifecycle
            .bind_closing_payload(first_payload.clone())
            .unwrap();

        // Preparing or failing to persist a closure does not clear the active
        // attempt, and a second request cannot be admitted over it.
        assert_eq!(lifecycle.active_for_close().unwrap(), first);
        assert_eq!(
            lifecycle.bind_closing_payload(first_payload).unwrap(),
            lifecycle.closing_payload().unwrap().clone()
        );
        assert!(lifecycle
            .bind_closing_payload(json!({"attempt_id": &first, "state": "different"}))
            .is_err());
        assert!(lifecycle
            .admit_after_pending_ack("T-test:1:2".to_string())
            .is_err());

        lifecycle.ack_closed(&first).unwrap();
        assert!(lifecycle.ack_closed(&first).is_err());
        lifecycle
            .admit_after_pending_ack("T-test:1:2".to_string())
            .unwrap();
        let second = lifecycle.active_for_close().unwrap();
        assert_ne!(first, second);
        lifecycle.ack_closed(&second).unwrap();
        assert_eq!(lifecycle.last(), Some("T-test:1:2"));
    }

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
    fn runtime_failure_payload_is_versioned_and_locates_lossless_child_error() {
        let value = runtime_failure_payload(
            "provider_accounting_invalid: malformed usage",
            "T-child",
            15,
            Some("T-child:15:2"),
        );
        let failure: ryeos_runtime::RuntimeFailure = serde_json::from_value(value).unwrap();
        assert_eq!(failure.version, 1);
        assert_eq!(failure.code, "provider_accounting_invalid");
        assert_eq!(failure.diagnostic_locator.thread_id, "T-child");
        assert_eq!(failure.diagnostic_locator.turn, Some(15));
        assert_eq!(
            failure.diagnostic_locator.attempt_id.as_deref(),
            Some("T-child:15:2")
        );
        assert_eq!(failure.diagnostic_locator.event_type, "thread_failed");
        assert!(!failure.retryable);
    }

    #[test]
    fn runtime_failure_payload_normalizes_empty_and_oversized_codes() {
        let empty: ryeos_runtime::RuntimeFailure =
            serde_json::from_value(runtime_failure_payload("", "T-child", 1, None)).unwrap();
        assert_eq!(empty.code, "directive_failed");
        assert!(!empty.summary.is_empty());

        let oversized_prefix = "a".repeat(65);
        let oversized: ryeos_runtime::RuntimeFailure = serde_json::from_value(
            runtime_failure_payload(&format!("{oversized_prefix}: boom"), "T-child", 1, None),
        )
        .unwrap();
        assert_eq!(oversized.code, "directive_failed");
        assert!(oversized.validate().is_ok());
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
                explicitly_free: false,
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        // Model not in the (empty) per-model table → provider-default rates.
        let cost = runner.compute_cost(1_000_000, 500_000);
        assert!((cost.usd - 10.5).abs() < f64::EPSILON);
        assert_eq!(cost.source, PricingSource::ProviderDefault);

        let usage = crate::provider_adapter::http::TokenUsage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            reported_cost_usd: Some(7.25),
            ..Default::default()
        };
        let reported = runner.compute_cost_for_usage(Some(&usage));
        assert_eq!(reported.usd, 7.25);
        assert_eq!(reported.source, PricingSource::ProviderReported);
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        let result = runner.finalize(json!("Hello world"));
        assert!(result.success);
        assert_eq!(result.result.unwrap(), "Hello world");
        assert_eq!(result.status, RuntimeResultStatus::Completed);
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
                reasoning_content: None,
            }],
            tools: vec![],
            system_prompt: Some("You are helpful".to_string()),
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        assert!(runner.directive_outputs.is_some());
        assert_eq!(runner.directive_outputs.unwrap().len(), 1);
    }

    #[test]
    fn directive_outputs_artifact_uses_owned_artifact_schema() {
        let outputs = json!({"answer": 42});
        let artifact = directive_outputs_artifact("T-test", &outputs);

        assert_eq!(artifact["artifact_type"], "directive_outputs");
        assert_eq!(artifact["uri"], "thread://T-test/outputs");
        assert_eq!(artifact["metadata"], outputs);
        assert!(artifact.get("content").is_none());
    }

    #[test]
    fn continuation_hook_context_is_event_namespaced() {
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
        let mut budget = BudgetTracker::new(1.0);
        budget.report(10, 5, 0.25).unwrap();
        let runner = Runner::new(RunnerConfig {
            messages: vec![ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget,
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        let context = runner.continuation_hook_context(123, 456);

        assert!(context.get("messages").is_none());
        assert_eq!(context["event"]["name"], "continuation");
        assert_eq!(context["event"]["reason"], "context_window");
        assert_eq!(context["event"]["live_context_tokens"], 123);
        assert_eq!(context["event"]["threshold_tokens"], 456);
        assert!(context["event"]["messages"].is_array());
        assert_eq!(context["event"]["usage"]["input_tokens"], 10);
        assert_eq!(context["event"]["usage"]["output_tokens"], 5);
        assert_eq!(context["event"]["budget_remaining"]["spend_usd"], 0.75);
        assert_eq!(
            context["event"]["budget_remaining"]["spend_unlimited"],
            false
        );
        assert_eq!(context["event"]["declared_outputs"], json!([]));
    }

    #[test]
    fn hook_dispatch_result_unwraps_successful_runtime_envelope() {
        let result = normalize_hook_dispatch_result(json!({
            "success": true,
            "status": "completed",
            "result": {"action": "abort"},
            "outputs": {},
            "warnings": [],
            "cost": null
        }))
        .unwrap();

        assert_eq!(result.value, json!({"action": "abort"}));
    }

    #[test]
    fn hook_dispatch_result_rejects_failed_runtime_envelope() {
        let output = normalize_hook_dispatch_result(json!({
            "success": false,
            "status": "failed",
            "result": {"error": "boom"},
            "outputs": {},
            "warnings": [],
            "cost": null
        }))
        .unwrap();

        assert!(output.failure.unwrap().contains("hook_child_failed"));
    }

    #[test]
    fn hook_dispatch_result_rejects_legacy_or_contradictory_runtime_status() {
        for envelope in [
            json!({
                "success": false,
                "status": "error",
                "result": null,
                "outputs": {},
                "warnings": [],
                "cost": null
            }),
            json!({
                "success": true,
                "status": "failed",
                "result": null,
                "outputs": {},
                "warnings": [],
                "cost": null
            }),
        ] {
            match normalize_hook_dispatch_result(envelope) {
                Ok(output) => assert!(output
                    .failure
                    .is_some_and(|failure| failure.contains("hook_child_failed"))),
                Err(error) => assert!(error.contains("hook_child_failed")),
            }
        }
    }

    #[test]
    fn hook_dispatch_result_unwraps_successful_managed_envelope() {
        let result = normalize_hook_dispatch_result(json!({
            "outcome_code": "success",
            "result": {"action": "abort"},
            "error": null,
            "artifacts": []
        }))
        .unwrap();

        assert_eq!(result.value, json!({"action": "abort"}));
    }

    #[test]
    fn hook_dispatch_result_rejects_failed_managed_envelope() {
        let output = normalize_hook_dispatch_result(json!({
            "outcome_code": "failed",
            "result": null,
            "error": "boom",
            "artifacts": []
        }))
        .unwrap();

        assert!(output.failure.unwrap().contains("hook_child_failed"));
    }

    #[test]
    fn hook_dispatch_result_preserves_raw_tool_result() {
        let result = normalize_hook_dispatch_result(json!({"action": "abort"})).unwrap();

        assert_eq!(result.value, json!({"action": "abort"}));

        let error =
            normalize_hook_dispatch_result(json!({"success": true, "value": 42})).unwrap_err();
        assert!(error.contains("malformed native runtime envelope"));

        let error = normalize_hook_dispatch_result(json!({
            "success": true,
            "status": "completed",
            "action": "abort"
        }))
        .unwrap_err();
        assert!(error.contains("malformed native runtime envelope"));
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: Some(SamplingConfig {
                temperature: Some(0.3),
                seed: Some(42),
            }),
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
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
                explicitly_free: false,
                input_per_million: Some(0.0), // would yield $0 if used
                output_per_million: Some(0.0),
                models,
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            profiles: vec![],
        };

        let mut runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(100.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "claude-haiku-4-5".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        // 1M input + 1M output → 0.80 + 4.00 = 4.80
        let cost = runner.compute_cost(1_000_000, 1_000_000);
        assert!(
            (cost.usd - 4.80).abs() < f64::EPSILON,
            "expected $4.80 for per-model pricing, got ${}",
            cost.usd
        );
        assert_eq!(cost.source, PricingSource::PerModel);

        runner.model_name = "missing-paid-model".to_string();
        let missing = runner.compute_cost(1_000_000, 1_000_000);
        assert_eq!(missing.usd, 0.0);
        assert_eq!(
            missing.source,
            PricingSource::Unpriced,
            "zero provider defaults are an untracked sentinel, not free pricing"
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
                explicitly_free: false,
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(100.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "unknown-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        // Falls back to provider defaults: 1M input + 1M output → 1.0 + 5.0 = 6.0
        let cost = runner.compute_cost(1_000_000, 1_000_000);
        assert!(
            (cost.usd - 6.0).abs() < f64::EPSILON,
            "expected $6.00 for provider default pricing, got ${}",
            cost.usd
        );
        assert_eq!(cost.source, PricingSource::ProviderDefault);
    }

    #[test]
    fn compute_cost_unpriced_when_no_pricing_config() {
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
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(1.0),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            definition_hash: "definition-hash".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            terminal_state_root: std::env::temp_dir().join("ryeos-directive-runtime-tests"),
            terminal_source_path: "directive:test/fixture".to_string(),
        });

        // No pricing configured: nonzero tokens but $0 cost, flagged Unpriced so
        // the run loop can warn that spend is untracked (not free).
        let cost = runner.compute_cost(1_000_000, 1_000_000);
        assert_eq!(cost.usd, 0.0);
        assert_eq!(cost.source, PricingSource::Unpriced);
    }

    // ── §1 retry classification ──────────────────────────────────────

    fn retry_cfg() -> ExecutionConfig {
        ExecutionConfig {
            retries: 2,
            retry_status_codes: vec![429, 500, 502, 503],
            never_retry: vec!["401".into(), "403".into(), "404".into()],
            backoff_base_ms: 1000,
            retry_on_timeout: true,
            ..ExecutionConfig::default()
        }
    }

    #[test]
    fn provider_length_finish_is_distinct_and_fail_closed() {
        assert_eq!(
            crate::directive::normalize_finish_reason(Some("length")),
            FinishReason::Length
        );
    }

    fn status_err(code: u16) -> anyhow::Error {
        anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Status {
            code,
            detail: format!("provider returned {code}"),
        })
    }

    #[test]
    fn retry_backoff_retries_allowlisted_status_with_exponential_delay() {
        use std::time::Duration;
        let cfg = retry_cfg();
        let e = status_err(429);
        assert_eq!(
            retry_backoff(&e, 0, &cfg),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(
            retry_backoff(&e, 1, &cfg),
            Some(Duration::from_millis(2000))
        );
        // Retry budget spent once attempt reaches `retries`.
        assert_eq!(retry_backoff(&e, 2, &cfg), None);
    }

    #[test]
    fn retry_backoff_never_retry_overrides_allowlist() {
        // 403 is not in the allowlist anyway, but never_retry is the absolute
        // guard: even a code that WERE allowlisted would be denied.
        let cfg = retry_cfg();
        assert_eq!(retry_backoff(&status_err(403), 0, &cfg), None);
        let mut cfg2 = retry_cfg();
        cfg2.retry_status_codes.push(404);
        assert_eq!(retry_backoff(&status_err(404), 0, &cfg2), None);
    }

    #[test]
    fn retry_backoff_status_not_in_allowlist_is_not_retried() {
        let cfg = retry_cfg();
        assert_eq!(retry_backoff(&status_err(418), 0, &cfg), None);
    }

    #[test]
    fn retry_backoff_retries_pre_stream_send_failure() {
        use std::time::Duration;
        // A `.send()` transport failure (DNS/connect/TLS/reset) is pre-stream and
        // always retry-safe — retried under the shared budget, not gated by a
        // status allowlist or the timeout flag. This is the burst-fanout fix.
        let send_err = |connect: bool| {
            anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Send {
                connect,
                detail: "streaming request failed: error sending request".into(),
            })
        };
        let cfg = retry_cfg();
        assert_eq!(
            retry_backoff(&send_err(true), 0, &cfg),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(
            retry_backoff(&send_err(false), 1, &cfg),
            Some(Duration::from_millis(2000))
        );
        // Still bounded by the retry budget.
        assert_eq!(retry_backoff(&send_err(true), 2, &cfg), None);
    }

    #[test]
    fn retry_backoff_timeout_gated_by_retry_on_timeout() {
        use std::time::Duration;
        let timeout = || {
            anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Timeout {
                detail: "timed out".into(),
            })
        };
        let mut cfg = retry_cfg();
        assert_eq!(
            retry_backoff(&timeout(), 0, &cfg),
            Some(Duration::from_millis(1000))
        );
        cfg.retry_on_timeout = false;
        assert_eq!(retry_backoff(&timeout(), 0, &cfg), None);
    }

    #[test]
    fn retry_backoff_never_retries_unclassified_errors() {
        // Only errors the adapter classified as transient (a typed
        // `ProviderStreamError`) are retryable. Anything unclassified — invalid
        // bytes, a live callback publication failure, parse defects — fails the
        // turn immediately.
        let cfg = retry_cfg();
        let e = anyhow::anyhow!("non-utf8 SSE chunk: invalid byte sequence");
        assert_eq!(retry_backoff(&e, 0, &cfg), None);
    }

    #[test]
    fn retry_backoff_mid_stream_gated_by_retry_mid_stream() {
        use std::time::Duration;
        // A stream cut mid-read (chunk timeout/reset) is transient: retried by
        // default under the shared budget, and disabled by the knob.
        let mid_stream = || {
            anyhow::Error::new(crate::provider_adapter::ProviderStreamError::MidStream {
                live_output_events_emitted: 42,
                accepted_bytes: 128,
                accepted_output_events: 3,
                usage: None,
                generation_header_id: None,
                response_id: None,
                requested_output_tokens: Some(32_768),
                detail: "stream chunk error: operation timed out".into(),
            })
        };
        let mut cfg = retry_cfg();
        assert_eq!(
            retry_backoff(&mid_stream(), 0, &cfg),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(
            retry_backoff(&mid_stream(), 1, &cfg),
            Some(Duration::from_millis(2000))
        );
        // Still bounded by the retry budget.
        assert_eq!(retry_backoff(&mid_stream(), 2, &cfg), None);
        cfg.retry_mid_stream = false;
        assert_eq!(retry_backoff(&mid_stream(), 0, &cfg), None);
    }

    #[test]
    fn retry_backoff_disabled_when_retries_zero() {
        let cfg = ExecutionConfig {
            retries: 0,
            ..retry_cfg()
        };
        assert_eq!(retry_backoff(&status_err(429), 0, &cfg), None);
    }
}
