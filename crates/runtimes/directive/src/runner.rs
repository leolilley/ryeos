use std::time::Instant;

use serde_json::{Value, json};

use crate::budget::BudgetTracker;
use crate::continuation::ContinuationCheck;
use crate::directive::{
    CompactionConfig, CompactionOnPressure, DIRECTIVE_RETURN_TOOL, ExecutionConfig, FinishReason,
    ProviderMessage, ProviderMessageExt, StreamEvent, SummaryMode, TIME_LEFT_TOOL, ToolSchema,
};
use crate::dispatcher::{DispatchKind, Dispatcher};
use crate::harness::{Harness, HookAction};
use crate::result_guard::ResultGuard;
use crate::resume::{CompactionMemo, ResumeDisposition, ResumeState};
use crate::run_clock::{RunClock, RunPhase};
use ryeos_accounting::{
    AttemptBudgetState, BillableDimension, ChargeReconciliationAuthority, HexDigest,
    ProviderAccountingAuthority, ProviderAttemptGetParams, ProviderAttemptMarkIssuedParams,
    ProviderAttemptPrepareParams, ProviderAttemptPrepareResponse,
    ProviderAttemptReleaseUnissuedParams, ProviderAttemptSettleParams, ProviderRetryAdvance,
    ProviderRetryDecision, ProviderRetryReason, ReconciliationReason, SpendAccounting,
    SpendBoundAuthority, TokenAccounting, UnitCount, VerifiedPreparedSpendBound,
};
use ryeos_directive_definition::{
    ContinuationConfig, OutputSpec, ProviderConfig, ReasoningConfig, ReturnNudge, SamplingConfig,
    UnknownOutcomeClass,
};
#[cfg(test)]
use ryeos_directive_definition::{PricingConfig, ProtocolFamily, ProviderTransportConfig};
use ryeos_provider_contract::{Part, ToolResultStatus};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{
    EnvelopeAccountingScope, RuntimeCost, RuntimeResult, RuntimeResultStatus,
    normalize_hook_dispatch_result,
};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{TerminalCompletion, ThreadTerminalStatus};

mod request_context;

/// The tools a launch offers the model on its first turn: the tools its
/// capabilities admit, plus the lifecycle return tool when outputs are
/// declared and the in-process `time_left` tool when the run has a deadline.
/// This is the inventory a frame records.
pub(crate) fn launch_inventory(
    tools: &[ToolSchema],
    effective_caps: &[String],
    outputs: Option<&[OutputSpec]>,
    run_deadline: bool,
) -> Vec<ToolSchema> {
    visible_provider_tools(tools, effective_caps, outputs, true, run_deadline)
}

use request_context::{initial_messages, visible_provider_tools};

/// Free-form breadcrumb passed to `request_continuation` for logs only.
/// Continuation is autonomous by construction — this is NOT a typed reason the
/// substrate keys off; `State::Continued` has exactly one cause (the live
/// context window approaching the model limit), so it is a fixed string, not an
/// enum.
const CONTINUATION_LOG_REASON: &str = "context_window";

/// Restart seam used only when a feature-built daemon explicitly forwards the
/// reserved selection. Recovery authority remains the retained
/// event/effect/child state; this merely reaches the daemon-owned test gate
/// after a named durable boundary so the test can kill the daemon at that
/// exact cut.
const DIRECTIVE_TEST_BLOCK_AT_ENV: &str = "RYEOS_DIRECTIVE_TEST_BLOCK_AT";

async fn block_at_directive_recovery_test_cut(callback: &CallbackClient, stage: &str) {
    if !ryeos_runtime::CheckpointWriter::is_resume()
        && std::env::var(DIRECTIVE_TEST_BLOCK_AT_ENV).ok().as_deref() == Some(stage)
    {
        tracing::info!(stage, "directive recovery test cut reached");
        // The feature-only daemon endpoint reports the exact phase to its
        // parent and intentionally never answers. A production daemon rejects
        // the method; return in that case instead of letting an injected
        // environment value wedge a production runtime.
        if let Err(error) = callback.reach_test_phase_cut(stage).await {
            tracing::warn!(stage, error = %error, "directive recovery test cut rejected");
        }
    }
}

/// The recorded `finish` token for a normalised finish reason.
fn finish_label(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other => "other",
    }
}

/// Rendered model-visible tool result plus SSE emission metadata. Shared by
/// the serial and batch dispatch paths so their emitted shapes cannot diverge.
#[derive(Debug)]
struct RenderedToolResult {
    tool: String,
    content: String,
    raw_size: u64,
    truncated: bool,
    duplicate_of: Option<String>,
    truncated_reason: Option<&'static str>,
    status: ToolResultStatus,
    /// The images the result carries, after the checks of `tool_images`.
    images: Vec<RenderedImage>,
    /// The images dropped by those checks; the content ends with a note for
    /// each.
    dropped: Vec<ryeos_runtime::DroppedImage>,
}

/// One image of a rendered tool result.
#[derive(Debug)]
struct RenderedImage {
    part: ryeos_provider_contract::ImagePart,
    bytes: Vec<u8>,
    /// The upload, absent when the node already stores these bytes.
    upload: Option<ryeos_runtime::callback::EventAttachment>,
}

impl RenderedToolResult {
    /// This result with every image it carries dropped under `code`, each
    /// noted after the guarded text like any other dropped image. Positions
    /// stay the images' places among the tool's parts: the admitted images
    /// took the places the earlier drops left free.
    fn without_images(mut self, code: &'static str) -> Self {
        let total = self.images.len() + self.dropped.len();
        let prior: Vec<(usize, &str)> = self
            .dropped
            .iter()
            .map(|image| (image.position as usize, image.code.as_str()))
            .collect();
        let guarded = if prior.is_empty() {
            self.content.clone()
        } else {
            crate::tool_images::strip_drop_notes(&self.content, &prior)
                .unwrap_or_default()
                .to_string()
        };
        let mut free = (1..=total as u32).filter(|position| {
            !self
                .dropped
                .iter()
                .any(|dropped| dropped.position == *position)
        });
        let refused: Vec<ryeos_runtime::DroppedImage> = self
            .images
            .iter()
            .filter_map(|_| free.next())
            .map(|position| ryeos_runtime::DroppedImage {
                position,
                code: code.to_string(),
            })
            .collect();
        self.dropped.extend(refused);
        self.dropped.sort_by_key(|image| image.position);
        let all: Vec<(usize, &str)> = self
            .dropped
            .iter()
            .map(|image| (image.position as usize, image.code.as_str()))
            .collect();
        self.content = crate::tool_images::with_drop_notes(&guarded, &all);
        self.images.clear();
        self
    }

    /// The rendering of a result whose parts passed the image checks: the
    /// guard sees the tool's text alone, then a note for each dropped image
    /// follows it, so truncation and duplicate collapse never remove a note.
    fn from_settled(
        tool_name: &str,
        raw_size: u64,
        settled: crate::tool_images::Settled,
        result_guard: &mut ResultGuard,
    ) -> Self {
        let guarded = result_guard.process_bytes(settled.guarded_input().as_bytes());
        let content = crate::tool_images::with_drop_notes(
            &String::from_utf8_lossy(&guarded.bytes),
            &settled.dropped,
        );
        let dropped = settled
            .dropped
            .iter()
            .map(|(position, code)| ryeos_runtime::DroppedImage {
                position: *position as u32,
                code: (*code).to_string(),
            })
            .collect();
        Self {
            tool: tool_name.to_string(),
            content,
            raw_size,
            truncated: guarded.truncated,
            duplicate_of: guarded.duplicate_of,
            truncated_reason: guarded.truncated.then_some("result_guard"),
            status: ToolResultStatus::Ok,
            images: settled
                .images
                .into_iter()
                .map(|admitted| RenderedImage {
                    part: admitted.part,
                    bytes: admitted.bytes,
                    upload: admitted.upload,
                })
                .collect(),
            dropped,
        }
    }

    /// The recorded `tool_call_result` for this rendering: one leading text
    /// part, then the images in order.
    fn payload(&self, operation_id: &str, call_id: &str) -> ryeos_runtime::ToolResultPayload {
        let mut parts = vec![Part::text(self.content.clone())];
        parts.extend(
            self.images
                .iter()
                .map(|image| Part::Image(image.part.clone())),
        );
        ryeos_runtime::ToolResultPayload {
            operation_id: operation_id.to_string(),
            call_id: call_id.to_string(),
            tool: self.tool.clone(),
            status: self.status,
            attachment_refs: ryeos_runtime::ToolResultPayload::image_attachment_hashes(&parts),
            parts,
            guard: ryeos_runtime::ToolResultGuard {
                result_size_bytes: self.raw_size,
                truncated: self.truncated,
                truncated_reason: self.truncated_reason.map(str::to_string),
                duplicate_of: self.duplicate_of.clone(),
                dropped_images: self.dropped.clone(),
            },
        }
    }
}

/// One model-proposed call paired with its RyeOS logical occurrence.
/// Provider call IDs remain transcript data and may be missing or repeated;
/// `operation_id` is derived from the emitting thread, turn, and ordered call
/// index and is the callback reservation key.
#[derive(Debug, Clone)]
pub(crate) struct ToolOccurrence {
    pub call: crate::directive::ToolCall,
    pub operation_id: String,
    /// The turn number along the chain that proposed the call.
    pub turn: u32,
    /// The ordinal of the call among the tool calls of that turn.
    pub call_index: usize,
    /// The braid already contains this occurrence's `tool_call_start`.
    pub start_recorded: bool,
    /// The harness already charged this proposed non-lifecycle tool call.
    pub counted: bool,
    /// A recovered child-bearing start already charged the spawn-attempt
    /// counter reconstructed from the signed tool catalog.
    pub spawn_counted: bool,
    /// The call's streamed arguments were not valid JSON. It is never
    /// dispatched; the model gets an error result instead.
    pub malformed: Option<ryeos_runtime::MalformedCall>,
}

/// Failure text for a turn the provider ended at its output limit when the
/// signed policy does not recover it.
const OUTPUT_LIMIT_ERROR: &str = "provider_output_limit_reached: provider ended the \
     response at its declared/native output limit; this is \
     distinct from a RyeOS-local output byte limit";

/// The model-visible error for a call whose arguments did not parse.
fn malformed_call_error(malformed: &ryeos_runtime::MalformedCall) -> String {
    format!(
        "arguments were not valid JSON: {} bytes, sha256 {}; resend the call with complete arguments",
        malformed.raw_len, malformed.sha256
    )
}

/// Malformed-argument facts for one completed turn, by call ordinal. The
/// stream yields one `ToolUse` event per tool-call part in part order; a count
/// that disagrees with the assembled message is a defect, not a guess.
fn malformed_calls_of_turn(
    events: &[StreamEvent],
    message: &ProviderMessage,
) -> anyhow::Result<Vec<ryeos_runtime::MalformedCall>> {
    let mut malformed = Vec::new();
    let mut tool_use_count = 0usize;
    for event in events {
        let StreamEvent::ToolUse { malformed_args, .. } = event else {
            continue;
        };
        if let Some(args) = malformed_args {
            malformed.push(ryeos_runtime::MalformedCall {
                call_index: u32::try_from(tool_use_count)
                    .map_err(|_| anyhow::anyhow!("tool call ordinal exceeds u32"))?,
                raw_len: u64::try_from(args.raw_len).unwrap_or(u64::MAX),
                sha256: args.sha256.clone(),
            });
        }
        tool_use_count += 1;
    }
    if !malformed.is_empty() && tool_use_count != message.calls().len() {
        anyhow::bail!(
            "provider stream carried {tool_use_count} tool_use events but the assembled turn has {} tool calls; cannot attribute malformed arguments",
            message.calls().len()
        );
    }
    Ok(malformed)
}

/// Whether one assistant message's tool-call batch carries the
/// `directive_return` lifecycle signal. Lifecycle-bearing batches must run
/// through the strict serial dispatch path; only plain batches are eligible
/// for the bounded concurrent window.
fn batch_is_lifecycle_bearing(calls: &[ToolOccurrence]) -> bool {
    calls
        .iter()
        .any(|occurrence| occurrence.call.name == DIRECTIVE_RETURN_TOOL)
}

fn tool_call_limit_error() -> String {
    serde_json::to_string(&json!({
        "error": {
            "code": "tool_call_limit_exceeded",
            "message": "The signed tool-call limit is exhausted. No further dispatchable tools are available; answer using the results already gathered."
        }
    }))
    .unwrap_or_else(|_| {
        "{\"error\":{\"code\":\"tool_call_limit_exceeded\"}}".to_string()
    })
}

/// What the model sees for a call that sat after a failed `directive_return`
/// in the same batch: the call did not run, and may be proposed again.
fn tool_not_run_after_failed_return_error() -> String {
    serde_json::to_string(&crate::resume::failed_return_refusal_body())
        .expect("fixed failed-return refusal envelope serializes")
}

/// Error observations are model-visible transcript data. Keep them useful but
/// bounded independently of successful tool output, and never copy a daemon
/// error message (which can contain filesystem, credential, or policy detail)
/// into the provider transcript.
const MAX_ERROR_ENVELOPE_BYTES: usize = 32 * 1024;

/// Outcome code the daemon records for a tool it stopped at the run deadline.
const TOOL_INTERRUPTED_OUTCOME: &str =
    ryeos_runtime::callback_contract::TOOL_INTERRUPTED_AT_DEADLINE_OUTCOME;

/// What the model sees for a tool cut at the run deadline: an interrupted call
/// with an unknown outcome. The call is not retried, and its side effects may
/// have partially happened.
fn tool_interrupted_envelope() -> String {
    serde_json::to_string(&json!({
        "error": {
            "code": TOOL_INTERRUPTED_OUTCOME,
            "outcome": "unknown",
            "message": "The tool was stopped at the run deadline while it was running. \
                        Its outcome is unknown and its side effects may have partially \
                        happened. It was not retried.",
            "retryable": false,
        }
    }))
    .expect("fixed interrupted-tool envelope serializes")
}

/// Code of a tool dispatch refused because the run's hard stop was reached.
const TOOL_DEADLINE_ELAPSED: &str = "tool_deadline_elapsed";

/// What the model sees for a tool call that arrived at or after the run's hard
/// stop: the tool did not start, and the call is not retried. A tool started
/// inside the reserve window would run toward the tree end and leave the
/// runtime no time to settle itself.
fn tool_deadline_elapsed_error() -> String {
    serde_json::to_string(&json!({
        "error": {
            "code": TOOL_DEADLINE_ELAPSED,
            "message": "The run's hard stop is reached, so this tool did not start. It \
                        was not retried. Do not call further tools; finish with what you \
                        have.",
            "retryable": false,
        }
    }))
    .expect("fixed tool-deadline refusal envelope serializes")
}

/// Whether a tool dispatch at `now_ms` is refused because the hard stop is
/// reached. This is the comparison the turn-start boundary uses
/// (`RunClock::phase_at`). `time_left` is a local read and always runs.
fn refuse_tool_at_hard_stop(
    clock: Option<&crate::run_clock::RunClock>,
    tool_name: &str,
    now_ms: i64,
) -> bool {
    tool_name != TIME_LEFT_TOOL
        && clock.is_some_and(|clock| clock.phase_at(now_ms) == RunPhase::HardStop)
}

/// The runtime's result for its own deadline stop. The status is `TimedOut`
/// whether or not the callback finalize landed: when it did not (an older
/// daemon rejects the status, or the callback failed), the daemon settles the
/// thread from this stdout result, and a deadline stop must not turn into a
/// failure on that route. The finalize error rides as a warning.
fn deadline_stop_result(
    thread_id: &str,
    result: Value,
    cost: ryeos_runtime::envelope::RuntimeCost,
    finalized: Result<(), String>,
    warnings: &mut Vec<String>,
) -> RuntimeResult {
    if let Err(error) = finalized {
        warnings.push(format!(
            "callback finalize_thread for the deadline stop failed: {error}"
        ));
    }
    RuntimeResult {
        success: false,
        status: RuntimeResultStatus::TimedOut,
        thread_id: thread_id.to_string(),
        result: Some(result),
        outputs: json!({}),
        cost: Some(cost),
        warnings: Vec::new(),
    }
}

/// The error envelope the model reads when a tool result is refused for a
/// stable reason.
fn tool_error_body(code: &str, message: &str) -> String {
    json!({"error": {"code": code, "message": message}}).to_string()
}

fn callback_error_envelope(error: &ryeos_runtime::callback::CallbackError) -> String {
    let (code, retryable) = match error {
        ryeos_runtime::callback::CallbackError::ActionFailed {
            code, retryable, ..
        } => {
            let safe = code.len() <= 128
                && !code.is_empty()
                && code.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
                });
            (
                if safe { code.as_str() } else { "action_failed" },
                *retryable,
            )
        }
        ryeos_runtime::callback::CallbackError::Transport(_) => {
            ("runtime_action_outcome_unknown", false)
        }
    };
    serde_json::to_string(&json!({
        "error": {
            "code": code,
            "message": "The RyeOS execution boundary refused the action.",
            "retryable": retryable,
        }
    }))
    .expect("fixed callback error envelope serializes")
}

fn directive_output_validation_error(
    outputs: Option<&[OutputSpec]>,
    arguments: &Value,
) -> Option<String> {
    outputs.and_then(|outputs| {
        outputs.iter().find_map(|output| {
            arguments
                .get(&output.name)
                .is_none_or(Value::is_null)
                .then(|| {
                    format!(
                        "directive_return: missing required output '{}'",
                        output.name
                    )
                })
        })
    })
}

fn recovered_return_nudge_seen(
    messages: &[ProviderMessage],
    return_nudge: &ReturnNudge,
    outputs: Option<&[OutputSpec]>,
) -> bool {
    if !return_nudge.enabled() {
        return false;
    }
    let declared = outputs
        .unwrap_or_default()
        .iter()
        .map(|output| output.name.clone())
        .collect::<Vec<_>>();
    if declared.is_empty() {
        return false;
    }
    let expected = return_nudge.message(&declared);
    let Some(last_assistant) = messages
        .iter()
        .rposition(|message| message.role() == "assistant")
    else {
        return false;
    };
    messages[last_assistant + 1..].iter().any(|message| {
        message.role() == "user" && message.text().as_deref() == Some(expected.as_str())
    })
}

fn state_after_serial_tool_result(
    pending: Vec<ToolOccurrence>,
    index: usize,
    turn: u32,
    definition_ref: &str,
    root_raw_content_digest: &str,
    effective_definition_digest: &str,
) -> State {
    let next_index = index + 1;
    if next_index < pending.len() {
        State::DispatchingTools {
            pending,
            index: next_index,
        }
    } else {
        State::FiringHooks {
            occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                "directive",
                "after_step",
                definition_ref,
                root_raw_content_digest,
                effective_definition_digest,
            )
            .with_counter_coordinate("turn", turn),
            context: json!({"turn": turn}),
            resume_to: Box::new(State::CheckingContinuation),
        }
    }
}

#[derive(Debug)]
pub enum State {
    CheckingLimits,
    /// The run deadline's hard stop: the execution settles as a deadline stop
    /// without starting another turn or tool call.
    DeadlineStopped,
    CallingProvider,
    Streaming {
        // The full sequence of streamed events, kept for diagnostic
        // counts. Real per-delta `cognition_out` persistence already
        // happened inside the prepared provider streaming transport,
        // and the typed assistant message (text + tool_calls) was
        // pushed onto `self.messages` before this state runs.
        events: Vec<StreamEvent>,
    },
    ParsingResponse,
    DispatchingTools {
        pending: Vec<ToolOccurrence>,
        index: usize,
    },
    /// One assistant message's plain tool-call batch (no `directive_return`),
    /// dispatched concurrently through a bounded window and folded back in
    /// call order. Lifecycle-bearing batches use `DispatchingTools`.
    DispatchingToolBatch {
        pending: Vec<ToolOccurrence>,
    },
    ProcessingToolResult {
        call_id: String,
        tool_name: String,
        operation_id: String,
        raw_args: String,
        malformed: Option<ryeos_runtime::MalformedCall>,
        pending: Vec<ToolOccurrence>,
        index: usize,
        spawn_counted: bool,
    },
    /// directive_return lifecycle signal: the LLM invoked
    /// `directive_return` (a provider-API tool-call convention, not
    /// a real dispatchable tool). The runner intercepts by name in
    /// `DispatchingTools`, validates declared outputs, fires both
    /// `tool_call_start`/`tool_call_result` events for chain
    /// visibility, publishes the artifact, and finalizes the thread.
    ProcessingDirectiveReturn {
        call_id: String,
        operation_id: String,
        raw_args: String,
        malformed: Option<ryeos_runtime::MalformedCall>,
        /// The calls after this one in its batch. A successful return ends
        /// the run and abandons them; a failed one answers each with a
        /// refusal and never runs it.
        siblings: Vec<ToolOccurrence>,
    },
    /// Answers every call after a failed `directive_return` with the
    /// `tool_not_run_after_failed_return` refusal, in proposal order. A call
    /// whose start is already recorded gets only its result.
    SettlingOwedSiblings {
        siblings: Vec<ToolOccurrence>,
    },
    FiringHooks {
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence,
        context: Value,
        resume_to: Box<State>,
    },
    /// Decides, after every settled turn, whether the conversation has
    /// reached the compaction trigger.
    CheckingContinuation,
    /// The context-window continuation boundary, after any compaction.
    ContinuationBoundary,
    /// Replaces the older part of the conversation by a summary. `trigger` is
    /// `pressure` (live context reached the trigger) or `overflow` (the
    /// provider refused the request as too long).
    Compacting {
        trigger: String,
    },
    /// The compaction's summary call settled: write the record and rebuild the
    /// conversation from it.
    FinishingCompaction,

    Finalizing {
        result: Value,
    },
    FinalizingDirectiveReturn {
        outputs: Value,
    },
    Continued,
    Errored {
        error: String,
    },
    /// An acknowledged outcome is unavailable after a resume-critical daemon
    /// callback. The process exits non-terminally; native resume reconstructs
    /// the braid and re-drives the same logical operation.
    RecoveryRequired {
        error: String,
    },
    Cancelled,
}

pub struct Runner {
    messages: Vec<ProviderMessage>,
    tools: Vec<ToolSchema>,
    /// The names of the tools the launch offers the model: the tool
    /// inventory the frame records. A turn that offers a different set
    /// records its offered names on its turn marker.
    inventory_tool_names: Vec<String>,
    dispatcher: Dispatcher,
    harness: Harness,
    budget: BudgetTracker,
    callback: CallbackClient,
    continuation: ContinuationCheck,
    result_guard: ResultGuard,
    /// The bytes of the images requests carry, by attachment hash.
    image_store: crate::provider_adapter::images::ImageStore,
    /// What this run has stored of tool images, against the signed limits.
    image_run: crate::tool_images::ImageRunState,
    provider_config: ProviderConfig,
    provider_id: String,
    /// Profile name that matched during daemon preflight.
    matched_profile: Option<String>,
    /// SHA-256 of the canonical-JSON provider config from the snapshot.
    config_hash: String,
    /// Sealed-program opt-in for provider-call effect records.
    provider_effects_recorded: bool,
    /// Sealed financial authority when the launch carried one. `Paid` /
    /// `ExplicitlyFree` spend bounds make this a ledger route: the daemon
    /// owns the attempt budget lifecycle (reserve → issue → settle).
    financial_authority: Option<ProviderAccountingAuthority>,
    /// Daemon-minted budget scope identities; cross-checked against each
    /// reservation response (the daemon derives authoritative scope
    /// server-side, never from runtime fields).
    accounting_scope: Option<EnvelopeAccountingScope>,
    /// Declared model context window; the input-side unit bound for the
    /// spend verifier.
    context_window: u64,
    execution: ExecutionConfig,
    model_name: String,
    thread_id: String,
    definition_ref: String,
    root_raw_content_digest: String,
    effective_definition_digest: String,
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
    /// Whether the run-deadline wrap-up input is already in the transcript.
    /// Rebuilt from the replayed record on resume, so a recovered or
    /// continued segment never repeats it.
    wrap_up_delivered: bool,
    /// What to do at the context-window continuation boundary: disabled
    /// (default) → stop with current state; enabled → self-continue.
    continuation_config: ContinuationConfig,
    /// LLM sampling parameters from the directive's `model.sampling`.
    /// Passed to the provider adapter for inclusion in request body.
    /// `None` = use provider defaults.
    sampling: Option<SamplingConfig>,
    /// Signed provider-neutral reasoning policy from the bound model.
    /// The selected provider descriptor owns all wire-path/value mappings.
    reasoning: Option<ReasoningConfig>,
    /// Shared HTTP client — created once and reused across all turns.
    /// Connection pooling keeps TCP/TLS handshakes to a minimum.
    http_client: reqwest::Client,
    resume_pending_tools: Vec<ToolOccurrence>,
    /// Calls after a recovered failed `directive_return` that still owe a
    /// refusal result.
    resume_owed_siblings: Vec<ToolOccurrence>,
    /// Malformed-argument facts of the turn just recorded, read when its
    /// tool calls become occurrences.
    turn_malformed_calls: Vec<ryeos_runtime::MalformedCall>,
    /// Trailing turns in a row that ended at the provider's output limit.
    /// Derived from the record on resume; non-zero means the latest recorded
    /// turn ended at the limit.
    consecutive_length_turns: u32,
    resume_disposition: ResumeDisposition,
    resume_active_provider_turn: Option<u32>,
    provider_retry_testimony_digests: std::collections::BTreeSet<String>,
    /// The compaction policy, from the signed runtime config.
    compaction: CompactionConfig,
    /// The latest compaction on the path, as far as the run needs it.
    compaction_memo: Option<CompactionMemo>,
    /// A predecessor's request was refused: the run starts by compacting.
    start_with_overflow_compaction: bool,
    /// What a `context_pressure` hook's action chooses between.
    pressure_hook: Option<PressureHook>,
    /// The compaction's summary call in flight.
    summary_job: Option<SummaryJob>,
    /// No compaction is attempted before this many turns have been used: a
    /// conversation with nothing yet to replace is looked at again once it has
    /// grown by the kept turns.
    compaction_retry_at_turn: u32,
    /// Continuation predecessors of this thread, and the most a chain may have.
    chain_depth: u32,
    chain_depth_limit: u32,
    /// The compacted start of this thread is still over the continuation
    /// threshold: it compacts again before its first request.
    start_over_ceiling: bool,
}

/// The two states a `context_pressure` hook chooses between.
struct PressureHook {
    compact: State,
    skip: State,
}

/// A compaction's summary call: its request and, once it settles, its text.
struct SummaryJob {
    trigger: String,
    messages: Vec<ProviderMessage>,
    omitted_from_input: u32,
    result: Option<Vec<Part>>,
    /// The summary ended at the output limit: it is cut off and not carried.
    truncated: bool,
}

/// How the compaction policy answers a refusal.
enum OverflowRecovery {
    /// Continue into a successor that compacts first.
    Allowed,
    /// Not a refusal the policy answers (or already answered once).
    NotApplicable,
    /// The chain has spent its recoveries.
    Exhausted,
}

enum CompactionOutcome {
    Compacted,
    NothingToReplace,
}

enum CompactionFailure {
    /// The compaction could not be done; the run's state is unchanged.
    Failed(String),
    /// A record append or rebuild whose outcome is not known: a same-thread
    /// recovery decides.
    Unknown(String),
}

/// The name of the hook event that lets a directive act on context pressure.
const CONTEXT_PRESSURE_EVENT: &str = "context_pressure";

struct RunGuard {
    finalized: bool,
}

/// Bounded lost-reply recovery: how many exact idempotent retries a ledger
/// RPC gets before the runner attempts a recorded-state read (or fails
/// closed). Every retry carries the byte-identical params, so a replay can
/// only recover the recorded transition — never create a second one.
const LEDGER_RPC_RETRIES: usize = 2;

/// Paced spacing between ledger RPC retries. A vault or anchor blip that
/// outlives three back-to-back calls should not be thread-fatal; a few
/// hundred milliseconds of spacing lets transient faults clear without
/// meaningfully delaying the failure path.
async fn ledger_retry_backoff(retry: usize) {
    tokio::time::sleep(std::time::Duration::from_millis(250 * (retry as u64 + 1))).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderRetryWaitOutcome {
    DeadlineReached,
    Cancelled,
    Interrupted,
}

/// What the run deadline decides at the start of a turn.
enum RunDeadlineBoundary {
    Proceed,
    HardStop,
}

/// The text the model receives when the run enters its wrap-up phase. It is
/// the only push of time information; the `time_left` tool answers on demand.
fn wrap_up_message(remaining_ms: u64, outputs_declared: bool) -> String {
    let seconds = remaining_ms.div_ceil(1_000);
    let finish = if outputs_declared {
        "Finish with `directive_return` carrying every declared output."
    } else {
        "Finish with your best result."
    };
    format!(
        "Deadline notice: about {seconds} seconds remain before this run is stopped. \
         Do not start new lines of work. Record what you have, hand off anything \
         unfinished, and wrap up now. {finish}"
    )
}

/// Sets the live-interrupt flag when the run's hard stop arrives, and keeps
/// setting it until dropped, so a model call that begins at the boundary is
/// cut too. The run's own limits check decides the stop.
struct DeadlineInterruptTimer {
    task: tokio::task::JoinHandle<()>,
}

impl DeadlineInterruptTimer {
    fn start(
        clock: RunClock,
        interrupt_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let task = tokio::spawn(async move {
            // Re-read the wall clock on every wake so a clock step cannot cut a
            // call before the hard stop: the flag is set only once the clock
            // itself has reached it.
            loop {
                let wait_ms = clock
                    .hard_stop_at_ms()
                    .saturating_sub(lillux::time::timestamp_millis());
                if wait_ms <= 0 {
                    interrupt_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        u64::try_from(wait_ms).unwrap_or(0).min(1_000),
                    ))
                    .await;
                }
            }
        });
        Self { task }
    }
}

impl Drop for DeadlineInterruptTimer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Wait for a daemon-admitted durable retry deadline while retaining live
/// cancel/interrupt responsiveness. Lillux owns the wall-clock read and the
/// persisted `not_before_ms` owns retry admission; Tokio only schedules this
/// process-local wait and can never move the authoritative deadline.
async fn wait_for_provider_retry_deadline(
    not_before_ms: i64,
    cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    interrupt_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ProviderRetryWaitOutcome {
    use std::sync::atomic::Ordering;

    let remaining_ms = not_before_ms.saturating_sub(lillux::time::timestamp_millis());
    let remaining = std::time::Duration::from_millis(u64::try_from(remaining_ms).unwrap_or(0));
    tokio::select! {
        _ = tokio::time::sleep(remaining) => ProviderRetryWaitOutcome::DeadlineReached,
        _ = async {
            while !cancel_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => ProviderRetryWaitOutcome::Cancelled,
        _ = async {
            while !interrupt_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => ProviderRetryWaitOutcome::Interrupted,
    }
}

fn provider_retry_wait_terminal(
    outcome: ProviderRetryWaitOutcome,
) -> Option<crate::provider_adapter::StreamOutcome> {
    let empty_attempt = || crate::provider_adapter::streaming::CutAttemptState { usage: None };
    match outcome {
        ProviderRetryWaitOutcome::DeadlineReached => None,
        ProviderRetryWaitOutcome::Cancelled => {
            Some(crate::provider_adapter::StreamOutcome::Cancelled {
                attempt: empty_attempt(),
            })
        }
        ProviderRetryWaitOutcome::Interrupted => {
            Some(crate::provider_adapter::StreamOutcome::Interrupted {
                partial_cognition: crate::provider_adapter::InterruptedCognition::default(),
                events: Vec::new(),
                attempt: empty_attempt(),
            })
        }
    }
}

/// Live handle to one daemon-admitted provider attempt reservation. Mirrors
/// daemon authority but never calculates availability locally.
#[derive(Debug)]
struct LedgerAttempt {
    /// Daemon-minted attempt ID (keyed server-side by thread/generation/
    /// turn/attempt-number — the runner never invents this).
    attempt_id: String,
    /// Reservation intent hash (§9.3) frozen for every later transition.
    request_hash: String,
    /// Sealed authority digest echoed back by the reserve transition.
    authority_digest: HexDigest,
    coordinate: ryeos_provider_contract::RequestCoordinate,
}

/// Outcome of ledger admission for one physical attempt.
#[derive(Debug)]
enum LedgerAdmission {
    /// A verified immutable provider answer exists. No reservation or provider
    /// contact is permitted for this attempt.
    Replayed {
        record_hash: String,
        response: crate::provider_adapter::http::AdapterResponse,
    },
    /// The requested predecessor coordinate is already terminal and its
    /// settlement atomically authorized exactly one successor. No contact is
    /// permitted until the runtime validates and testifies this advancement.
    RetryAdvanced(ProviderRetryAdvance),
    /// The requested successor is exact, but the ledger-owned earliest
    /// admission deadline has not arrived yet.
    RetryNotBefore(ProviderRetryAdvance),
    /// Reserved and durably issued: the exact prepared bytes may be sent.
    Admitted(LedgerAttempt),
    /// The daemon durably denied the reservation (insufficient budget).
    Denied,
    /// A runner signal arrived between reserve and issue; the reservation
    /// was released (`ReleasedByRunner`) and no provider contact happened.
    CancelledBeforeIssue,
    InterruptedBeforeIssue,
    /// The daemon released the reservation before issue (certificate expiry
    /// or credential unavailability). Fail closed without provider contact.
    ReleasedByDaemon {
        detail: String,
    },
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

/// A computed turn cost plus its pricing provenance. Money is exact from the
/// moment it is derived: rate math runs in checked i128 nanos, and a
/// provider-reported float is quantized here, at its entry.
struct CostBreakdown {
    usd: ryeos_runtime::envelope::UsdNanos,
    source: PricingSource,
}

/// Decide whether a failed provider call should be retried, and if so, how long
/// to back off first. Returns `None` when the error is not retryable or the
/// retry budget (`execution.retries`) is spent.
///
/// Only a [`ProviderStreamError`](crate::provider_adapter::ProviderStreamError)
/// is retryable. A configured status response and a connection-establishment
/// failure are always admissible. A timeout, a post-connect send failure and a
/// stream that dies mid-read leave it unknown whether the provider acted; they
/// are admissible only when `execution.unknown_outcome_retry` (already reduced
/// to the launch-sealed ceiling) names the class and the failed attempt is
/// within its first `max_retries`. Invalid model, auth, context overflow, and a
/// live callback publication failure stay fail-fast. `never_retry` (status
/// codes as strings) is an absolute denylist that overrides
/// `retry_status_codes`. Backoff is exponential from `backoff_base_ms`
/// (`base * 2^attempt`), capped at `backoff_max_ms`.
#[cfg(test)]
fn retry_backoff(
    err: &anyhow::Error,
    attempt: u32,
    execution: &ExecutionConfig,
) -> Option<std::time::Duration> {
    retry_classification(err, attempt, execution).map(|(delay, _)| delay)
}

/// A request the provider refused that the entry's `request_rejections` rows
/// name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestRejection {
    code: String,
    status: u16,
}

/// Which declared rejection row, if any, a refused request matches. Only a
/// response that carried a status can match: such a refusal means the
/// provider did not act on the request, so this never touches the
/// uncertain-outcome path.
fn request_rejection(provider: &ProviderConfig, error: &anyhow::Error) -> Option<RequestRejection> {
    let Some(crate::provider_adapter::ProviderStreamError::Status { code, body, .. }) =
        error.downcast_ref::<crate::provider_adapter::ProviderStreamError>()
    else {
        return None;
    };
    let rows = provider.request_rejections.as_deref()?;
    ryeos_directive_definition::classify_request_rejection(rows, *code, body).map(|row_code| {
        RequestRejection {
            code: row_code.to_string(),
            status: *code,
        }
    })
}

fn retry_classification(
    err: &anyhow::Error,
    attempt: u32,
    execution: &ExecutionConfig,
) -> Option<(std::time::Duration, ProviderRetryReason)> {
    use crate::provider_adapter::ProviderStreamError;

    let reason = match err.downcast_ref::<ProviderStreamError>() {
        Some(ProviderStreamError::Status { code, .. }) => {
            ProviderRetryReason::Status { code: *code }
        }
        Some(ProviderStreamError::Timeout { .. }) => ProviderRetryReason::Timeout,
        // A reqwest connect-phase failure did not establish the provider
        // connection. Other `.send()` failures can occur after request bytes
        // crossed the boundary and therefore have an unknown outcome.
        Some(ProviderStreamError::Send { connect, .. }) => ProviderRetryReason::Send {
            connect_phase: *connect,
        },
        // Once a stream opened, the provider may have acted on the request;
        // the outcome is unknown.
        Some(ProviderStreamError::MidStream(mid_stream)) => ProviderRetryReason::MidStream {
            live_output_events_emitted: u64::try_from(mid_stream.live_output_events_emitted)
                .unwrap_or(u64::MAX),
        },
        None => return None,
    };
    retry_delay_for_reason(&reason, attempt.checked_add(1)?, execution).map(|delay| (delay, reason))
}

fn provider_outcome_is_uncertain(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<crate::provider_adapter::ProviderStreamError>(),
        Some(crate::provider_adapter::ProviderStreamError::Timeout { .. })
            | Some(crate::provider_adapter::ProviderStreamError::Send { connect: false, .. })
            | Some(crate::provider_adapter::ProviderStreamError::MidStream(_))
    )
}

fn retry_delay_for_reason(
    reason: &ProviderRetryReason,
    failed_attempt_number: u32,
    execution: &ExecutionConfig,
) -> Option<std::time::Duration> {
    let attempt = failed_attempt_number.checked_sub(1)?;
    if attempt >= execution.retries {
        return None;
    }
    let admitted = match reason {
        ProviderRetryReason::Status { code } => {
            !execution.never_retry.contains(&code.to_string())
                && execution.retry_status_codes.contains(code)
        }
        ProviderRetryReason::Send {
            connect_phase: true,
        } => true,
        ProviderRetryReason::Timeout => unknown_outcome_admitted(
            UnknownOutcomeClass::Timeout,
            failed_attempt_number,
            execution,
        ),
        ProviderRetryReason::Send {
            connect_phase: false,
        } => unknown_outcome_admitted(UnknownOutcomeClass::Send, failed_attempt_number, execution),
        ProviderRetryReason::MidStream { .. } => unknown_outcome_admitted(
            UnknownOutcomeClass::MidStream,
            failed_attempt_number,
            execution,
        ),
    };
    if !admitted {
        return None;
    }
    let factor = 1u64 << attempt.min(16);
    let delay_ms = execution
        .backoff_base_ms
        .saturating_mul(factor)
        .min(execution.backoff_max_ms);
    Some(lillux::time::Duration::from_millis(delay_ms))
}

/// Whether closing the failed attempt before a retry must see valid usage.
/// A failure that leaves the provider outcome unknown never has final usage to
/// show; when the signed policy and sealed ceiling already admit the retry
/// (classification only returns a reason in that case), the daemon ledger
/// bounds the cost by charging the attempt's reserved maximum, so the
/// fail-closed usage requirement does not apply. Known-outcome retries keep it.
fn retry_requires_attempt_usage(reason: &ProviderRetryReason, fail_closed: bool) -> bool {
    fail_closed && !reason.leaves_outcome_unknown()
}

/// Whether a retry after an unknown provider outcome is within policy: the
/// class is named and the failed attempt is among the first `max_retries`.
fn unknown_outcome_admitted(
    class: UnknownOutcomeClass,
    failed_attempt_number: u32,
    execution: &ExecutionConfig,
) -> bool {
    execution
        .unknown_outcome_retry
        .as_ref()
        .is_some_and(|policy| {
            failed_attempt_number <= policy.max_retries && policy.on.contains(&class)
        })
}

/// What a retry abandons: the failed attempt of a turn and the count of live
/// output fragments it had published. A reader voids every progressive
/// fragment of that turn published since its marker or the previous retry.
/// The value is derived from the decision so recovery can check it exactly.
pub(crate) fn retry_supersedes(decision: &ProviderRetryDecision) -> Value {
    let output_events = match decision.reason {
        ProviderRetryReason::MidStream {
            live_output_events_emitted,
        } => live_output_events_emitted,
        _ => 0,
    };
    json!({
        "turn": decision.turn,
        "attempt": decision.failed_attempt_number,
        "output_events": output_events,
    })
}

fn retry_policy_digest(execution: &ExecutionConfig) -> anyhow::Result<HexDigest> {
    let value = json!({
        "schema": "ryeos.directive.provider-retry-policy.v2",
        "retries": execution.retries,
        "retry_status_codes": execution.retry_status_codes,
        "never_retry": execution.never_retry,
        "backoff_base_ms": execution.backoff_base_ms,
        "backoff_max_ms": execution.backoff_max_ms,
        "unknown_outcome_retry": execution.unknown_outcome_retry,
        "accounting": execution.accounting,
    });
    let canonical = lillux::canonical_json(&value)?;
    HexDigest::new(lillux::sha256_hex(canonical.as_bytes()))
        .map_err(|error| anyhow::anyhow!("provider retry policy digest: {error}"))
}

fn provider_retry_decision(
    turn: u32,
    failed_attempt_number: u32,
    delay: std::time::Duration,
    reason: ProviderRetryReason,
    error: &anyhow::Error,
    execution: &ExecutionConfig,
) -> anyhow::Result<ProviderRetryDecision> {
    let decision = ProviderRetryDecision {
        turn,
        failed_attempt_number,
        next_attempt_number: failed_attempt_number
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("provider retry attempt coordinate overflow"))?,
        backoff_ms: u64::try_from(delay.as_millis())
            .map_err(|_| anyhow::anyhow!("provider retry backoff exceeds u64"))?,
        reason,
        failure_digest: HexDigest::new(lillux::sha256_hex(format!("{error:#}").as_bytes()))
            .map_err(|digest_error| anyhow::anyhow!("provider failure digest: {digest_error}"))?,
        retry_policy_digest: retry_policy_digest(execution)?,
    };
    decision
        .validate()
        .map_err(|validation| anyhow::anyhow!(validation))?;
    Ok(decision)
}

/// Truncate a settlement diagnostic to the shared RPC bound (char-safe).
fn bound_diagnostic(text: &str) -> String {
    if text.len() <= ryeos_accounting::MAX_DIAGNOSTIC_LEN {
        return text.to_string();
    }
    let mut end = ryeos_accounting::MAX_DIAGNOSTIC_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Render the adapter's parsed `f64` provider-reported charge as a canonical
/// decimal string for the daemon's raw-decimal settlement boundary.
///
/// Documented lossy step: the adapter parses the wire number to `f64` today,
/// so digits beyond f64 precision are already gone before this runs. We
/// render at nine fractional digits — exactly the `UsdNanos` resolution the
/// daemon settles at — then trim trailing zeros. Non-finite, negative, or
/// over-length renderings return `None` (settled as `Unavailable` rather
/// than inventing a charge).
/// Extract any provider usage carried on a typed adapter failure so the
/// daemon settlement (and the reporting path) can preserve it.
fn carried_usage_from_error(
    error: &anyhow::Error,
) -> Option<&crate::provider_adapter::http::TokenUsage> {
    error
        .downcast_ref::<crate::provider_adapter::LocalOutputByteLimitError>()
        .and_then(|e| e.usage.as_ref())
        .or_else(|| {
            error
                .downcast_ref::<crate::provider_adapter::ProviderReportedStreamError>()
                .and_then(|e| e.usage.as_ref())
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::provider_adapter::ProviderProtocolStreamError>()
                .and_then(|e| e.usage.as_ref())
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::provider_adapter::streaming::RuntimeCallbackPublicationError>()
                .and_then(|e| e.usage.as_ref())
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::provider_adapter::ProviderStreamError>()
                .and_then(|e| match e {
                    crate::provider_adapter::ProviderStreamError::MidStream(mid_stream) => {
                        mid_stream.usage.as_ref()
                    }
                    _ => None,
                })
        })
}

/// Build the typed daemon settlement observations for one issued attempt.
///
/// Token and spend validity are independent (§7.5): tokens settle from the
/// reported usage snapshot; spend settles per the sealed reconciliation
/// authority. Anything unusable becomes `Unavailable` — the daemon then
/// conservatively charges the reserved maximum, never a synthetic zero.
/// A stream that did not complete never claims a final reported charge or
/// tariff cost: finality contracts bind to a completed response.
fn build_ledger_settlement(
    authority: &ProviderAccountingAuthority,
    stream_completed: bool,
    usage: Option<&crate::provider_adapter::http::TokenUsage>,
    diagnostic: Option<&str>,
) -> (SpendAccounting, TokenAccounting) {
    let tokens = match usage {
        None => TokenAccounting::Unavailable,
        Some(usage) => match usage.complete_token_counts() {
            Some((input_tokens, output_tokens)) if usage.anomalies.is_empty() => {
                TokenAccounting::Reported {
                    input_tokens,
                    output_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                }
            }
            _ => TokenAccounting::Invalid {
                diagnostic: bound_diagnostic(&if usage.anomalies.is_empty() {
                    "required input/output token counts are incomplete".to_string()
                } else {
                    usage.anomalies.join("; ")
                }),
            },
        },
    };

    // Explicitly-free routes settle exact zero under their contract
    // regardless of the stream outcome; token validity stays independent.
    if matches!(
        authority.spend_bound,
        SpendBoundAuthority::ExplicitlyFree { .. }
    ) {
        return (SpendAccounting::ExplicitlyFree, tokens);
    }

    if !stream_completed {
        return (
            SpendAccounting::Unavailable {
                diagnostic: bound_diagnostic(
                    diagnostic.unwrap_or("provider stream did not complete after issue"),
                ),
            },
            tokens,
        );
    }

    let spend = match &authority.reconciliation {
        ChargeReconciliationAuthority::ProviderReportedFinalCharge { .. } => match usage {
            Some(usage) if !usage.spend_anomalies.is_empty() => SpendAccounting::Unavailable {
                diagnostic: bound_diagnostic(&usage.spend_anomalies.join("; ")),
            },
            Some(usage) if usage.reported_cost_usd_raw.is_some() => {
                SpendAccounting::ProviderReportedFinal {
                    raw_decimal: usage
                        .reported_cost_usd_raw
                        .clone()
                        .expect("guarded by is_some"),
                }
            }
            _ => SpendAccounting::Unavailable {
                diagnostic: bound_diagnostic(
                    "provider did not report a usable final charge for the attempt",
                ),
            },
        },
        ChargeReconciliationAuthority::DeterministicTariff { tariff } => {
            match usage
                .filter(|usage| usage.anomalies.is_empty())
                .and_then(|usage| {
                    tariff
                        .covered_dimensions
                        .as_slice()
                        .iter()
                        .map(|dimension| {
                            let units = match dimension {
                                BillableDimension::InputTokens => usage.input_tokens?,
                                BillableDimension::OutputTokens => usage.output_tokens?,
                                BillableDimension::ReasoningTokens => usage.reasoning_tokens?,
                                BillableDimension::CacheReadTokens => usage.cache_read_tokens?,
                                BillableDimension::CacheMissTokens => usage.cache_miss_tokens?,
                                BillableDimension::CacheWriteTokens => usage.cache_write_tokens?,
                                // The daemon adds the tariff's flat per-request
                                // rate itself; no provider observation is needed.
                                BillableDimension::PerRequest => return Some(None),
                            };
                            Some(Some(UnitCount {
                                dimension: *dimension,
                                units,
                            }))
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(|counts| counts.into_iter().flatten().collect())
                }) {
                Some(unit_counts) => SpendAccounting::TariffUnits { unit_counts },
                None => SpendAccounting::Unavailable {
                    diagnostic: bound_diagnostic(
                        "deterministic tariff settlement is missing a covered billable dimension",
                    ),
                },
            }
        }
        ChargeReconciliationAuthority::Unavailable => SpendAccounting::Unavailable {
            diagnostic: bound_diagnostic(
                "route declares no charge reconciliation authority for issued attempts",
            ),
        },
    };
    (spend, tokens)
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
    pub provider_config: ProviderConfig,
    pub provider_id: String,
    /// Profile name that matched during daemon preflight.
    pub matched_profile: Option<String>,
    /// SHA-256 of the canonical-JSON provider config from the snapshot.
    pub config_hash: String,
    /// Sealed-program opt-in: submit a provider-call effect record after
    /// each settled, completed call. The daemon independently re-reads the
    /// declaration from the admitted capsule at publication.
    pub provider_effects_recorded: bool,
    /// Sealed financial authority from the launch envelope, when present.
    pub financial_authority: Option<ProviderAccountingAuthority>,
    /// Daemon-minted accounting scope identities from the launch envelope.
    pub accounting_scope: Option<EnvelopeAccountingScope>,
    pub execution: ExecutionConfig,
    pub model_name: String,
    pub thread_id: String,
    pub definition_ref: String,
    pub root_raw_content_digest: String,
    pub effective_definition_digest: String,
    pub hooks: Vec<ryeos_runtime::CompiledHook>,
    pub outputs: Option<Vec<OutputSpec>>,
    pub return_nudge: ReturnNudge,
    pub continuation: ContinuationConfig,
    pub sampling: Option<SamplingConfig>,
    pub reasoning: Option<ReasoningConfig>,
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
            provider_effects_recorded,
            execution,
            model_name,
            thread_id,
            definition_ref,
            root_raw_content_digest,
            effective_definition_digest,
            hooks,
            outputs,
            return_nudge,
            continuation,
            sampling,
            reasoning,
            matched_profile,
            config_hash,
            financial_authority,
            accounting_scope,
        } = config;
        let initial_messages = initial_messages(messages, system_prompt.as_deref());

        let effective_caps = harness.effective_caps().to_vec();
        let dispatcher = Dispatcher::new(tools.clone(), effective_caps);
        let inventory_tool_names = launch_inventory(
            &tools,
            harness.effective_caps(),
            outputs.as_deref(),
            harness.run_clock().is_some(),
        )
        .into_iter()
        .map(|tool| tool.name)
        .collect();

        Self {
            messages: initial_messages,
            tools,
            inventory_tool_names,
            dispatcher,
            harness,
            budget,
            callback,
            continuation: ContinuationCheck::new(context_window, context_threshold_ratio)
                .with_images(
                    provider_config
                        .image_input
                        .as_ref()
                        .and_then(|image_input| image_input.accepts.clone()),
                ),
            result_guard: ResultGuard::new(),
            image_store: crate::provider_adapter::images::ImageStore::default(),
            image_run: crate::tool_images::ImageRunState::default(),
            provider_config,
            provider_id,
            matched_profile,
            config_hash,
            financial_authority,
            accounting_scope,
            context_window,
            provider_effects_recorded,
            execution,
            model_name,
            thread_id,
            definition_ref,
            root_raw_content_digest,
            effective_definition_digest,
            initial_turn: 0,
            hooks,
            directive_outputs: outputs,
            return_nudge,
            return_nudge_sent: false,
            wrap_up_delivered: false,
            continuation_config: continuation,
            sampling,
            reasoning,
            // Retain the existing per-process client and pool size; no
            // protocol or keepalive tuning is added without measurement. The
            // custom resolver preserves reqwest's default getaddrinfo behavior
            // while exposing DNS timing. The connector layer records only the
            // honest aggregate connection-establishment boundary, which can
            // include DNS, TCP, proxy negotiation, and TLS. Response versions
            // are observed after negotiation rather than forced here.
            http_client: reqwest::Client::builder()
                .pool_max_idle_per_host(8)
                .dns_resolver(std::sync::Arc::new(
                    crate::provider_transport_timing::InstrumentedDnsResolver,
                ))
                .connector_layer(
                    crate::provider_transport_timing::ConnectionEstablishmentTimingLayer,
                )
                .timeout(std::time::Duration::from_secs(600))
                // The prepared request is a sealed contract: exact bytes,
                // frozen credential, exact endpoint. Following a redirect
                // would replay the credential to a host the sealed provider
                // config never authorized, so a 3xx is an error, not a hop.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client builder"),
            resume_pending_tools: Vec::new(),
            resume_owed_siblings: Vec::new(),
            turn_malformed_calls: Vec::new(),
            consecutive_length_turns: 0,
            resume_disposition: ResumeDisposition::ContinueProvider,
            resume_active_provider_turn: None,
            provider_retry_testimony_digests: std::collections::BTreeSet::new(),
            compaction: CompactionConfig::default(),
            compaction_memo: None,
            start_with_overflow_compaction: false,
            pressure_hook: None,
            summary_job: None,
            compaction_retry_at_turn: 0,
            chain_depth: 0,
            chain_depth_limit: u32::MAX,
            start_over_ceiling: false,
        }
    }

    /// Set how many continuation predecessors a chain may have.
    pub fn set_chain_depth_limit(&mut self, limit: u32) {
        self.chain_depth_limit = limit;
    }

    /// Set the compaction policy the signed runtime config carries.
    pub fn set_compaction(&mut self, policy: CompactionConfig) {
        self.compaction = policy;
    }

    pub fn from_resume(resume: ResumeState, mut config: RunnerConfig) -> anyhow::Result<Self> {
        let (input_tokens, output_tokens, spend_usd, retained_spawns) = resume
            .thread_usage
            .as_ref()
            .map(|usage| {
                (
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.spend_usd,
                    usage.spawns_used,
                )
            })
            .unwrap_or((0, 0, ryeos_runtime::envelope::UsdNanos::ZERO, 0));
        let resumed_tokens = input_tokens
            .checked_add(output_tokens)
            .ok_or_else(|| anyhow::anyhow!("resume usage token count overflow"))?;
        RuntimeCost {
            input_tokens,
            output_tokens,
            total_usd: spend_usd,
            basis: None,
        }
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid resume usage: {error}"))?;
        config
            .budget
            .reseed(input_tokens, output_tokens, spend_usd)
            .map_err(|error| anyhow::anyhow!("invalid resume budget: {error}"))?;
        let result_guard = resume.result_guard;
        let image_run = resume.images;
        config.messages = resume.messages;
        let mut pending_tools = resume.pending_tools;
        let owed_siblings = resume.owed_siblings;
        let started_tool_occurrences = resume.started_tool_occurrences;
        let resume_disposition = resume.disposition;
        let resume_active_provider_turn = resume.active_provider_turn;
        let provider_retry_testimony_digests = resume.provider_retry_testimony_digests;
        let wrap_up_delivered = resume.deadline_wrap_up_delivered;
        let trailing_length_turns = resume.trailing_length_turns;
        let last_turn_coordinate = resume.last_turn_coordinate;
        let compaction_memo = resume.compaction.clone();
        let start_with_overflow_compaction = resume.predecessor_rejected_turn.is_some();
        let chain_depth = resume.chain_depth;
        let start_over_ceiling = resume.start_over_ceiling;
        if let Some(active_turn) = resume_active_provider_turn {
            let expected = last_turn_coordinate
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("resume provider-turn coordinate overflow"))?;
            if active_turn != expected
                || !pending_tools.is_empty()
                || !owed_siblings.is_empty()
                || !matches!(resume_disposition, ResumeDisposition::ContinueProvider)
            {
                anyhow::bail!(
                    "resume active provider turn {active_turn} contradicts retained runtime state"
                );
            }
        }
        let mut runner = Self::new(config);
        runner.result_guard = result_guard;
        runner.image_run = image_run;
        if let ResumeDisposition::FinalizeDirectiveReturn { outputs } = &resume_disposition
            && let Some(error) =
                directive_output_validation_error(runner.directive_outputs.as_deref(), outputs)
        {
            anyhow::bail!("resume: settled directive_return no longer validates: {error}");
        }
        // A compaction's replay window opens after the run's earlier tool
        // starts and spawns; they are counted in the record.
        let starts_before_window = resume.tool_starts_before_window;
        let started_count = u32::try_from(started_tool_occurrences.len())
            .map_err(|_| anyhow::anyhow!("resume tool-start count exceeds u32"))?
            .checked_add(starts_before_window)
            .ok_or_else(|| anyhow::anyhow!("resume tool-start count exceeds u32"))?;
        let admitted_count = runner
            .harness
            .recover_admitted_tool_call_count(started_count);
        let admitted_len = usize::try_from(admitted_count.saturating_sub(starts_before_window))
            .expect("u32 admitted tool count always fits usize on supported targets");
        let mut admitted_operations = std::collections::BTreeSet::new();
        let mut spawn_counted_operations = std::collections::BTreeSet::new();
        let mut derived_spawns = resume.spawns_before_window;
        for started in &started_tool_occurrences[..admitted_len] {
            admitted_operations.insert(started.operation_id.clone());
            let Ok(resolved) = runner.dispatcher.resolve(
                &started.call.name,
                &started.call.arguments.to_string(),
                started.call.id.clone(),
            ) else {
                continue;
            };
            if matches!(
                resolved.dispatch_kind,
                DispatchKind::DirectiveChild | DispatchKind::GraphChild
            ) {
                derived_spawns = derived_spawns
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("resume spawn counter overflow"))?;
                spawn_counted_operations.insert(started.operation_id.clone());
            }
        }
        if derived_spawns < retained_spawns {
            anyhow::bail!(
                "resume event braid accounts for {derived_spawns} child starts, below retained usage authority {retained_spawns}"
            );
        }
        runner.harness.reseed(
            resume.turns_completed,
            resumed_tokens,
            spend_usd,
            derived_spawns,
        );
        runner.harness.reseed_tool_calls(admitted_count);
        for occurrence in &mut pending_tools {
            if occurrence.start_recorded && occurrence.call.name != DIRECTIVE_RETURN_TOOL {
                occurrence.counted = admitted_operations.contains(&occurrence.operation_id);
                occurrence.spawn_counted =
                    spawn_counted_operations.contains(&occurrence.operation_id);
            }
        }
        runner.initial_turn = last_turn_coordinate;
        runner.resume_pending_tools = pending_tools;
        runner.resume_owed_siblings = owed_siblings;
        runner.resume_disposition = resume_disposition;
        runner.resume_active_provider_turn = resume_active_provider_turn;
        runner.provider_retry_testimony_digests = provider_retry_testimony_digests;
        runner.wrap_up_delivered = wrap_up_delivered;
        runner.consecutive_length_turns = trailing_length_turns;
        runner.compaction_memo = compaction_memo;
        runner.chain_depth = chain_depth;
        runner.start_over_ceiling = start_over_ceiling;
        runner.start_with_overflow_compaction = start_with_overflow_compaction;
        runner.return_nudge_sent = recovered_return_nudge_seen(
            &runner.messages,
            &runner.return_nudge,
            runner.directive_outputs.as_deref(),
        );
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
    /// Estimated tokens of live context at which a compaction starts.
    fn compaction_trigger_tokens(&self) -> u64 {
        (self.context_window as f64 * self.compaction.trigger_ratio) as u64
    }

    /// Whether the compaction policy asks for a compaction at `live_context`:
    /// the trigger is reached, enough turns have settled since the last one,
    /// and a last one that failed to shrink the conversation has been outgrown
    /// by the same share. All of it is read from the record's compaction.
    fn compaction_due(&self, live_context: u64) -> bool {
        if !self.compaction.enabled
            || live_context < self.compaction_trigger_tokens()
            || self.harness.turns_used() < self.compaction_retry_at_turn
        {
            return false;
        }
        let Some(memo) = &self.compaction_memo else {
            return true;
        };
        let settled_since = self.harness.turns_used().saturating_sub(memo.settled_turns);
        if settled_since < self.compaction.min_turns_between {
            return false;
        }
        if memo.ineffective {
            let outgrown = memo.size_after as f64 * (1.0 + self.compaction.min_reduction_ratio);
            return live_context as f64 >= outgrown;
        }
        true
    }

    /// Whether the policy answers a refusal carrying `code` with a compaction
    /// in a successor, from the record alone: the policy is on, the directive
    /// allows continuation, the code is one the policy names, the chain has
    /// recoveries left, and the request has not already been retried after one.
    fn overflow_recovery(&self, code: &str) -> OverflowRecovery {
        if !self.compaction.enabled
            || !self.continuation_config.enabled()
            || !self
                .compaction
                .overflow_codes
                .iter()
                .any(|named| named == code)
        {
            return OverflowRecovery::NotApplicable;
        }
        if let Some(memo) = &self.compaction_memo {
            // A compaction for an overflow, with no turn completed since:
            // this refusal is the retried request's. It stands.
            if memo.trigger == "overflow"
                && self.harness.turns_used().saturating_sub(1) <= memo.settled_turns
            {
                return OverflowRecovery::NotApplicable;
            }
            if memo.overflow_recoveries >= self.compaction.max_overflow_recoveries_per_chain {
                return OverflowRecovery::Exhausted;
            }
        } else if self.compaction.max_overflow_recoveries_per_chain == 0 {
            return OverflowRecovery::Exhausted;
        }
        OverflowRecovery::Allowed
    }

    fn pressure_occurrence(&self, turn: u32) -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::new(
            "directive",
            CONTEXT_PRESSURE_EVENT,
            self.definition_ref.clone(),
            self.root_raw_content_digest.clone(),
            self.effective_definition_digest.clone(),
        )
        .with_counter_coordinate("turn", turn)
    }

    /// The `context_pressure` hook context: numbers only, no conversation.
    fn pressure_context(
        &self,
        trigger: &str,
        turn: u32,
        live_context: u64,
        rejection: Option<&RequestRejection>,
    ) -> Value {
        let mut event = json!({
            "name": CONTEXT_PRESSURE_EVENT,
            "trigger": trigger,
            "turn": turn,
            "live_context_tokens": live_context,
            "trigger_tokens": self.compaction_trigger_tokens(),
            "threshold_tokens": self.continuation.threshold(),
            "window_tokens": self.context_window,
            "compactions_so_far": self.compaction_memo.as_ref().map_or(0, |memo| memo.compactions),
            "last_compaction_turn": self.compaction_memo.as_ref().map(|memo| memo.settled_turns),
        });
        if let Some(rejection) = rejection {
            event["code"] = json!(rejection.code);
            event["status"] = json!(rejection.status);
        }
        json!({ "event": event })
    }

    /// The execution settings of a compaction's summary call: the run's, with
    /// the output cap at the policy's.
    fn summary_call_execution(&self) -> ExecutionConfig {
        let mut execution = self.execution.clone();
        let cap = self.compaction.summary.max_output_tokens;
        execution.max_provider_output_tokens_per_turn =
            match execution.max_provider_output_tokens_per_turn {
                0 => cap,
                existing => existing.min(cap),
            };
        execution
    }

    /// Load the conversation as the record holds it and choose the cut. The
    /// summary call's request is the replaced range, rendered and bounded.
    /// `false` when there is nothing to replace.
    async fn begin_summary_call(&mut self, trigger: &str) -> Result<bool, CompactionFailure> {
        let basis = crate::resume::load_compaction_basis(&self.callback, &self.thread_id)
            .await
            .map_err(|error| CompactionFailure::Failed(format!("{error:#}")))?;
        let Some(plan) = crate::compaction::plan(&basis.folded, self.compaction.keep_turns) else {
            return Ok(false);
        };
        // The record carries the pinned inputs: refuse before a paid call that
        // could not be recorded.
        let summary = &self.compaction.summary;
        let pinned_bytes =
            serde_json::to_vec(&plan.pinned_inputs).map_or(usize::MAX, |bytes| bytes.len());
        let reserve = usize::try_from(summary.max_output_tokens.saturating_mul(8))
            .unwrap_or(usize::MAX)
            .saturating_add(16 * 1024);
        if pinned_bytes.saturating_add(reserve)
            > ryeos_runtime::cognition::MAX_COMPACTION_RECORD_BYTES
        {
            return Err(CompactionFailure::Failed(format!(
                "the pinned inputs are {pinned_bytes} bytes, too large for a compaction record to carry"
            )));
        }
        let input_budget = ((self.context_window as f64 * summary.input_ratio) as u64)
            .saturating_sub(summary.max_output_tokens);
        let rendered =
            crate::compaction::render_for_summary(&basis.folded[..plan.cut], input_budget);
        self.summary_job = Some(SummaryJob {
            trigger: trigger.to_string(),
            messages: vec![
                ProviderMessage::system(summary.instruction.clone()),
                ProviderMessage::user(rendered.text),
            ],
            omitted_from_input: rendered.omitted,
            result: None,
            truncated: false,
        });
        Ok(true)
    }

    /// Write the compaction record and make the conversation the fold over it.
    /// `summary` is the settled summary call's text, or `None` for the policy's
    /// fixed note. The conversation is replaced by rebuilding it from the
    /// record, never patched in place, so memory and record cannot diverge.
    async fn finish_compaction(
        &mut self,
        trigger: &str,
        summary: Option<crate::compaction::Summary>,
    ) -> Result<CompactionOutcome, CompactionFailure> {
        let images = self
            .provider_config
            .image_input
            .as_ref()
            .and_then(|image_input| image_input.accepts.clone());
        let basis = crate::resume::load_compaction_basis(&self.callback, &self.thread_id)
            .await
            .map_err(|error| CompactionFailure::Failed(format!("{error:#}")))?;
        let Some(plan) = crate::compaction::plan(&basis.folded, self.compaction.keep_turns) else {
            return Ok(CompactionOutcome::NothingToReplace);
        };
        let summary = summary.unwrap_or_else(|| crate::compaction::fixed_note(&plan));
        let record = crate::compaction::build_record(
            &basis,
            &plan,
            summary,
            trigger,
            &self.compaction,
            images.as_ref(),
        )
        .map_err(|error| CompactionFailure::Failed(format!("{error:#}")))?;
        let payload = ryeos_runtime::cognition::to_payload(&record)
            .map_err(|error| CompactionFailure::Failed(format!("{error:#}")))?;
        self.callback
            .append_runtime_event(RuntimeEventType::CognitionCompaction, payload)
            .await
            .map_err(|error| {
                CompactionFailure::Unknown(format!(
                    "resume-critical compaction record append failed: {error:#}"
                ))
            })?;
        let rebuilt = crate::resume::load_resume_state(
            &self.callback,
            &self.thread_id,
            crate::resume::CarryPolicy {
                carry_turns: u32::MAX,
                start_tokens: u64::MAX,
                ceiling_tokens: u64::MAX,
            },
            crate::resume::ResumeMode::NativeSameThread,
            images.as_ref(),
        )
        .await
        .map_err(|error| {
            CompactionFailure::Unknown(format!(
                "resume-critical compaction rebuild failed: {error:#}"
            ))
        })?;
        let system = self
            .messages
            .first()
            .filter(|message| message.role() == "system")
            .cloned();
        self.messages = system.into_iter().chain(rebuilt.messages).collect();
        self.result_guard = rebuilt.result_guard;
        self.wrap_up_delivered = rebuilt.deadline_wrap_up_delivered;
        self.compaction_memo = Some(CompactionMemo::of(&record));
        self.start_with_overflow_compaction = false;
        self.start_over_ceiling = false;
        tracing::info!(
            trigger,
            size_before = record.size_before,
            size_after = record.size_after,
            ineffective = record.ineffective,
            "compacted the conversation"
        );
        Ok(CompactionOutcome::Compacted)
    }

    /// Where a run goes after a compaction attempt.
    fn after_compaction(
        &mut self,
        trigger: &str,
        outcome: Result<CompactionOutcome, CompactionFailure>,
    ) -> State {
        let overflow = trigger == "overflow";
        let onward = if overflow {
            State::CheckingLimits
        } else {
            State::ContinuationBoundary
        };
        let overflow_code = || {
            self.compaction
                .overflow_codes
                .first()
                .cloned()
                .unwrap_or_else(|| "provider_context_length_exceeded".to_string())
        };
        match outcome {
            Ok(CompactionOutcome::Compacted) => onward,
            Ok(CompactionOutcome::NothingToReplace) if overflow => State::Errored {
                error: format!(
                    "{}: the request was refused as too long and the conversation holds no turn older than the {} kept turns to replace",
                    overflow_code(),
                    self.compaction.keep_turns
                ),
            },
            Ok(CompactionOutcome::NothingToReplace) => {
                self.compaction_retry_at_turn = self
                    .harness
                    .turns_used()
                    .saturating_add(self.compaction.keep_turns.max(1));
                onward
            }
            Err(CompactionFailure::Unknown(error)) => State::RecoveryRequired { error },
            Err(CompactionFailure::Failed(error)) if overflow => State::Errored {
                error: format!("{}: compaction failed: {error}", overflow_code()),
            },
            Err(CompactionFailure::Failed(error)) => {
                tracing::warn!(%error, "compaction failed; the run carries on uncompacted");
                // Not tried again every turn: a paid summary call must not
                // repeat after each settled turn.
                self.compaction_retry_at_turn = self
                    .harness
                    .turns_used()
                    .saturating_add(self.compaction.keep_turns.max(1));
                onward
            }
        }
    }

    /// The `time_left` tool's answer, read from the run clock now.
    fn time_left_report(&self) -> Value {
        crate::run_clock::time_left_report(
            self.harness.run_clock().as_ref(),
            lillux::time::timestamp_millis(),
        )
    }

    /// Whether a call to `tool_name` arriving now is refused at the hard stop.
    fn tool_refused_at_hard_stop(&self, tool_name: &str) -> bool {
        refuse_tool_at_hard_stop(
            self.harness.run_clock().as_ref(),
            tool_name,
            lillux::time::timestamp_millis(),
        )
    }

    /// The run deadline's boundary at the start of a turn. At the hard stop
    /// the run stops; in the wrap-up phase the model is told once, as a
    /// durable harness input, that time is short. A recovered provider turn
    /// takes no new input, since its transcript is already fixed.
    async fn run_deadline_boundary(&mut self) -> Result<RunDeadlineBoundary, String> {
        let Some(clock) = self.harness.run_clock() else {
            return Ok(RunDeadlineBoundary::Proceed);
        };
        let now_ms = lillux::time::timestamp_millis();
        match clock.phase_at(now_ms) {
            RunPhase::HardStop => Ok(RunDeadlineBoundary::HardStop),
            RunPhase::WrapUp
                if !self.wrap_up_delivered && self.resume_active_provider_turn.is_none() =>
            {
                let text = wrap_up_message(
                    clock.remaining_ms(now_ms),
                    self.directive_outputs
                        .as_deref()
                        .is_some_and(|outputs| !outputs.is_empty()),
                );
                // Deliberately not pinned: its text is a time-left figure, which
                // is stale in a successor. A successor whose carried conversation
                // lacks the notice is told again.
                let input = ryeos_runtime::CognitionInput::text("harness", &text)
                    .with_kind(ryeos_runtime::cognition::KIND_DEADLINE_WRAP_UP);
                self.callback
                    .emit_cognition_input(&input)
                    .await
                    .map_err(|e| {
                        format!("resume-critical deadline wrap-up input append failed: {e}")
                    })?;
                self.messages.push(ProviderMessage::user(text));
                self.wrap_up_delivered = true;
                Ok(RunDeadlineBoundary::Proceed)
            }
            RunPhase::WrapUp | RunPhase::Running => Ok(RunDeadlineBoundary::Proceed),
        }
    }

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
            self.messages.push(ProviderMessage::user(s.content.clone()));
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

    pub async fn run(&mut self) -> anyhow::Result<RuntimeResult> {
        let mut guard = RunGuard { finalized: false };
        // At the run deadline's hard stop this cuts whatever model call is in
        // flight; the stop itself is decided at the next limits check.
        let _deadline_timer = self
            .harness
            .run_clock()
            .map(|clock| DeadlineInterruptTimer::start(clock, self.harness.interrupted_flag()));
        // The entrypoint has already attached the process and durably moved
        // the thread to running before it constructs the runner.
        // Ledger routes (Paid / ExplicitlyFree sealed authority): the daemon
        // ledger owns the attempt budget lifecycle. Cloned once so the
        // admission/settlement helpers never pin an immutable `self` borrow
        // across the mutable reporting-settlement paths.
        let ledger_authority: Option<ProviderAccountingAuthority> = self
            .financial_authority
            .as_ref()
            .filter(|authority| {
                matches!(
                    authority.spend_bound,
                    SpendBoundAuthority::Paid { .. } | SpendBoundAuthority::ExplicitlyFree { .. }
                )
            })
            .cloned();
        // Hard budget mode is only meaningful when the daemon reservation
        // ledger backs the route. main.rs enforces this at startup; this is
        // the fail-closed backstop for alternate construction paths.
        let mut state = if self.provider_effects_recorded && ledger_authority.is_none() {
            State::Errored {
                error: "external_effect_authority_invalid: recorded provider calls require a \
                        Paid or ExplicitlyFree sealed financial authority so replay evidence can \
                        be published only at the terminal accounting boundary"
                    .to_string(),
            }
        } else if self.execution.accounting.budget_mode
            == crate::directive::AccountingBudgetMode::Hard
            && ledger_authority.is_none()
        {
            State::Errored {
                error: "provider_accounting_policy_invalid: accounting.budget_mode=hard \
                        requires a Paid or ExplicitlyFree sealed financial authority; this \
                        route is advisory-only"
                    .to_string(),
            }
        } else if self.resume_active_provider_turn.is_some() && ledger_authority.is_none() {
            State::Errored {
                error: "provider_attempt_outcome_unknown: an unfinished provider turn has no \
                        daemon-owned attempt ledger; native recovery refuses a second contact"
                    .to_string(),
            }
        } else if self.compaction.enabled
            && self.resume_active_provider_turn.is_some()
            && self.compaction_due(
                self.continuation
                    .estimate_live_context_tokens(&self.messages),
            )
        {
            // A turn started and never completed while the conversation was at
            // the compaction trigger: it was the compaction's summary call, as
            // a regular turn would have compacted first. It is re-issued at
            // its own coordinate, by the ledger's rules for an unknown outcome.
            State::Compacting {
                trigger: "pressure".to_string(),
            }
        } else if self.compaction.enabled
            && self.start_with_overflow_compaction
            && self.resume_pending_tools.is_empty()
        {
            // The predecessor's request was refused as too long: this thread's
            // first act is the compaction, and its first request is the
            // retried one.
            State::Compacting {
                trigger: "overflow".to_string(),
            }
        } else if self.compaction.enabled
            && self.start_over_ceiling
            && self.resume_pending_tools.is_empty()
        {
            // The compacted start is still over the continuation threshold:
            // compact it again before the first request.
            State::Compacting {
                trigger: "pressure".to_string(),
            }
        } else if !self.resume_pending_tools.is_empty() {
            let pending = std::mem::take(&mut self.resume_pending_tools);
            if batch_is_lifecycle_bearing(&pending) {
                State::DispatchingTools { pending, index: 0 }
            } else {
                State::DispatchingToolBatch { pending }
            }
        } else {
            match std::mem::take(&mut self.resume_disposition) {
                ResumeDisposition::ContinueProvider => State::CheckingLimits,
                ResumeDisposition::ParseRetainedResponse => State::ParsingResponse,
                ResumeDisposition::AfterSettledToolBatch { turn } => State::FiringHooks {
                    occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                        "directive",
                        "after_step",
                        self.definition_ref.clone(),
                        self.root_raw_content_digest.clone(),
                        self.effective_definition_digest.clone(),
                    )
                    .with_counter_coordinate("turn", turn),
                    context: json!({"turn": turn}),
                    resume_to: Box::new(State::CheckingContinuation),
                },
                ResumeDisposition::CheckContinuation => State::CheckingContinuation,
                ResumeDisposition::SettleOwedSiblings => State::SettlingOwedSiblings {
                    siblings: std::mem::take(&mut self.resume_owed_siblings),
                },
                ResumeDisposition::FinalizeDirectiveReturn { outputs } => {
                    State::FinalizingDirectiveReturn { outputs }
                }
                ResumeDisposition::FinishCompaction { trigger, summary } => {
                    self.summary_job = Some(SummaryJob {
                        trigger,
                        messages: Vec::new(),
                        omitted_from_input: 0,
                        result: Some(summary),
                        truncated: false,
                    });
                    State::FinishingCompaction
                }
            }
        };
        let mut turn = self.initial_turn;
        // Diagnostic locator for terminal failure payloads: the most recent
        // admitted attempt (daemon attempt ID on ledger routes, the local
        // coordinate string otherwise).
        let mut last_attempt_id: Option<String> = None;
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
                    match self.run_deadline_boundary().await {
                        Ok(RunDeadlineBoundary::Proceed) => {}
                        Ok(RunDeadlineBoundary::HardStop) => {
                            state = State::DeadlineStopped;
                            continue;
                        }
                        Err(error) => {
                            state = State::RecoveryRequired { error };
                            continue;
                        }
                    }
                    // Steer drain (between-turns boundary): fold any operator
                    // inputs staged since the last cognition before calling the
                    // provider. Also folds the input that accompanied a live
                    // interrupt (the interrupt arm routes back through here).
                    // Limits already passed above, so a turn WILL start — a fold
                    // here is always answered. Resume-critical: a poll error stops
                    // the loop rather than answering with input we may have dropped.
                    if self.resume_active_provider_turn.is_none()
                        && let Err(e) = self.poll_pending_input().await
                    {
                        state = State::RecoveryRequired { error: e };
                        continue;
                    }
                    State::CallingProvider
                }

                State::CallingProvider => {
                    let turn_start = Instant::now();
                    // A compaction's summary call takes a turn coordinate but is
                    // not a model turn of the directive: it does not count
                    // against the turn limit.
                    if self.summary_job.is_none() {
                        self.harness.record_turn();
                    }
                    // Filter tools by effective_caps so the LLM only sees
                    // tools it can actually call (saves context, avoids the
                    // "model names a tool the dispatcher would reject" path).
                    // The summary call offers none.
                    let visible_tools_owned = if self.summary_job.is_some() {
                        Vec::new()
                    } else {
                        visible_provider_tools(
                            &self.tools,
                            self.harness.effective_caps(),
                            self.directive_outputs.as_deref(),
                            self.harness.tool_calls_available(),
                            self.harness.run_clock().is_some(),
                        )
                    };
                    if let Some(active_turn) = self.resume_active_provider_turn.take() {
                        turn = active_turn;
                    } else {
                        turn += 1;
                        // The turn marker names the offered tools only when
                        // they differ from the frame's inventory.
                        let offered: Vec<String> = visible_tools_owned
                            .iter()
                            .map(|tool| tool.name.clone())
                            .collect();
                        let tools_offered =
                            (offered != self.inventory_tool_names).then_some(offered);
                        if let Err(e) = self.callback.emit_turn_start(turn, tools_offered).await {
                            state = State::RecoveryRequired {
                                error: format!(
                                    "resume-critical callback emit_turn_start failed: {e}"
                                ),
                            };
                            continue;
                        }
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
                    // §1 retry loop. A provider failure with an outcome-safe
                    // classification — a configured status response or a
                    // connection-establishment failure — is re-attempted with
                    // capped exponential backoff. Timeout, post-connect send
                    // failure, and mid-stream cut have unknown outcomes and are
                    // re-attempted only within the sealed unknown-outcome
                    // ceiling; identical message bytes are not an idempotency
                    // key, so that ceiling needs an explicit author declaration.
                    // Anything not
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
                    let mut turn_replayed_from: Option<String> = None;
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
                        let attempt_number = match attempt.checked_add(1) {
                            Some(attempt_number) => attempt_number,
                            None => {
                                break Err(anyhow::anyhow!("provider attempt coordinate overflow"));
                            }
                        };
                        last_attempt_id =
                            Some(format!("{}:{turn}:{attempt_number}", self.thread_id));
                        if let Err(error) = self.load_request_images().await {
                            break Err(error);
                        }
                        let summary_execution: Option<ExecutionConfig> = self
                            .summary_job
                            .as_ref()
                            .map(|_| self.summary_call_execution());
                        let request_execution =
                            summary_execution.as_ref().unwrap_or(&self.execution);
                        let request_messages: &[ProviderMessage] = match &self.summary_job {
                            Some(job) => &job.messages,
                            None => &self.messages,
                        };
                        let call_input = crate::provider_adapter::StreamingCallInput {
                            client: &self.http_client,
                            provider: &self.provider_config,
                            provider_id: &self.provider_id,
                            matched_profile: self.matched_profile.as_deref(),
                            config_hash: &self.config_hash,
                            images: &self.image_store,
                            execution: request_execution,
                            model: &self.model_name,
                            messages: request_messages,
                            tools: &visible_tools_owned,
                            callback: &self.callback,
                            turn,
                            attempt: attempt_number,
                            sampling: self.sampling.as_ref(),
                            reasoning: self.reasoning.as_ref(),
                            provider_request_body_bytes_limit: self
                                .harness
                                .provider_request_body_bytes_limit(),
                            cancel_flag: Some(cancel_flag.clone()),
                            interrupt_flag: Some(interrupt_flag.clone()),
                        };
                        // Prepare the immutable request BEFORE budget admission:
                        // the reservation binds to these exact bytes (§9.2) and
                        // transport later sends them verbatim.
                        let prepared =
                            match crate::provider_adapter::prepare_provider_request(&call_input) {
                                Ok(prepared) => prepared,
                                Err(error) => break Err(error),
                            };
                        // The daemon owns the atomic replay-or-reserve
                        // boundary. Only its proven replay may bypass contact;
                        // transport failure is never interpreted as a miss.
                        let mut ledger_attempt = None;
                        let replayed = if let Some(authority) = ledger_authority.as_ref() {
                            let verified = match crate::spend_verifier::verify_prepared_spend_bound(
                                &prepared,
                                authority,
                                &self.provider_config,
                                self.context_window,
                                request_execution.max_provider_output_tokens_per_turn,
                            ) {
                                Ok(verified) => verified,
                                Err(error) => {
                                    break Err(anyhow::anyhow!(
                                        "provider_spend_bound_unverified: {error:#}"
                                    ));
                                }
                            };
                            match self
                                .admit_ledger_attempt(
                                    turn,
                                    attempt_number,
                                    &prepared,
                                    authority,
                                    verified,
                                    &cancel_flag,
                                    &interrupt_flag,
                                )
                                .await
                            {
                                Ok(LedgerAdmission::Replayed {
                                    record_hash,
                                    response,
                                }) => Some((record_hash, response)),
                                Ok(LedgerAdmission::Admitted(ledger)) => {
                                    last_attempt_id = Some(ledger.attempt_id.clone());
                                    ledger_attempt = Some(ledger);
                                    None
                                }
                                Ok(LedgerAdmission::RetryAdvanced(advance)) => {
                                    if let Err(error) =
                                        self.validate_retry_advance(&advance, turn, attempt_number)
                                    {
                                        break Err(error);
                                    }
                                    last_attempt_id = Some(advance.prior_attempt_id.clone());
                                    if let Err(error) =
                                        self.emit_provider_retry_testimony(&advance).await
                                    {
                                        tracing::warn!(
                                            decision_digest = %advance.decision_digest.as_str(),
                                            %error,
                                            "recovered provider retry testimony outcome is unknown; requesting same-thread recovery"
                                        );
                                        return Err(ryeos_runtime::process_outcome::RuntimeRecoveryRequired::retained_progress_outcome_unknown(
                                            self.thread_id.clone(),
                                        ).into());
                                    }
                                    attempt = advance.decision.failed_attempt_number;
                                    tracing::warn!(
                                        turn,
                                        attempt,
                                        next_attempt = advance.decision.next_attempt_number,
                                        backoff_ms = advance.decision.backoff_ms,
                                        decision_digest = %advance.decision_digest.as_str(),
                                        "recovered the exact durable provider retry advancement"
                                    );
                                    if let Some(terminal) = provider_retry_wait_terminal(
                                        wait_for_provider_retry_deadline(
                                            advance.not_before_ms,
                                            &cancel_flag,
                                            &interrupt_flag,
                                        )
                                        .await,
                                    ) {
                                        break Ok(terminal);
                                    }
                                    continue;
                                }
                                Ok(LedgerAdmission::RetryNotBefore(advance)) => {
                                    let failed_attempt_number =
                                        attempt_number.checked_sub(1).ok_or_else(|| {
                                            anyhow::anyhow!(
                                                "provider retry successor has no predecessor"
                                            )
                                        })?;
                                    if let Err(error) = self.validate_retry_advance(
                                        &advance,
                                        turn,
                                        failed_attempt_number,
                                    ) {
                                        break Err(error);
                                    }
                                    if advance.decision.next_attempt_number != attempt_number {
                                        break Err(anyhow::anyhow!(
                                            "provider retry deadline contradicts requested successor attempt {attempt_number}"
                                        ));
                                    }
                                    if let Err(error) =
                                        self.emit_provider_retry_testimony(&advance).await
                                    {
                                        tracing::warn!(
                                            decision_digest = %advance.decision_digest.as_str(),
                                            %error,
                                            "provider retry testimony outcome is unknown while waiting for ledger deadline"
                                        );
                                        return Err(ryeos_runtime::process_outcome::RuntimeRecoveryRequired::retained_progress_outcome_unknown(
                                            self.thread_id.clone(),
                                        ).into());
                                    }
                                    if let Some(terminal) = provider_retry_wait_terminal(
                                        wait_for_provider_retry_deadline(
                                            advance.not_before_ms,
                                            &cancel_flag,
                                            &interrupt_flag,
                                        )
                                        .await,
                                    ) {
                                        break Ok(terminal);
                                    }
                                    continue;
                                }
                                Ok(LedgerAdmission::Denied) => {
                                    break Err(anyhow::anyhow!(
                                        "budget_exceeded: the daemon ledger denied the provider \
                                         attempt reservation (insufficient available balance for \
                                         the route maximum); zero provider requests"
                                    ));
                                }
                                Ok(LedgerAdmission::CancelledBeforeIssue) => {
                                    break Ok(crate::provider_adapter::StreamOutcome::Cancelled {
                                        attempt:
                                            crate::provider_adapter::streaming::CutAttemptState {
                                                usage: None,
                                            },
                                    });
                                }
                                Ok(LedgerAdmission::InterruptedBeforeIssue) => {
                                    break Ok(crate::provider_adapter::StreamOutcome::Interrupted {
                                        partial_cognition:
                                            crate::provider_adapter::InterruptedCognition::default(),
                                        events: Vec::new(),
                                        attempt:
                                            crate::provider_adapter::streaming::CutAttemptState {
                                                usage: None,
                                            },
                                    });
                                }
                                Ok(LedgerAdmission::ReleasedByDaemon { detail }) => {
                                    break Err(anyhow::anyhow!(
                                        "provider_attempt_released_before_issue: {detail}; \
                                         zero provider connections"
                                    ));
                                }
                                Err(error) => break Err(error),
                            }
                        } else {
                            None
                        };
                        turn_replayed_from = replayed
                            .as_ref()
                            .map(|(record_hash, _)| record_hash.clone());
                        let (call, durable_retry_advance, prepared_retry) = if let Some((
                            record_hash,
                            response,
                        )) = replayed
                        {
                            tracing::info!(
                                %record_hash,
                                turn,
                                "provider call replayed from record; no reservation, no provider contact"
                            );
                            // Live watchers see the replayed content arrive as
                            // one ephemeral delta (chunk cadence is transport
                            // texture, not evidence); the durable record of the
                            // turn still comes from emit_turn_complete.
                            if let Some(text) = response.message.text() {
                                let _ = self
                                    .callback
                                    .append_runtime_event(
                                        RuntimeEventType::CognitionOut,
                                        json!({
                                            "turn": turn,
                                            "delta": text,
                                            "replayed": true,
                                        }),
                                    )
                                    .await;
                            }
                            (
                                Ok(crate::provider_adapter::StreamOutcome::Completed {
                                    response,
                                    events: Vec::new(),
                                }),
                                None,
                                None,
                            )
                        } else {
                            // Send the exact prepared bytes.
                            let call = match &prepared.transport {
                                crate::provider_adapter::PreparedProviderTransport::RemoteHttp {
                                    ..
                                } => crate::provider_adapter::send_prepared_streaming(
                                    &call_input,
                                    &prepared,
                                )
                                .await,
                                crate::provider_adapter::PreparedProviderTransport::AdmittedLocalWorker {
                                    ..
                                } => match ledger_attempt.as_ref() {
                                    Some(ledger) => {
                                        crate::provider_adapter::send_prepared_local_streaming(
                                            &call_input,
                                            &prepared,
                                            &ledger.attempt_id,
                                            &ledger.request_hash,
                                            &ledger.coordinate,
                                        )
                                        .await
                                    }
                                    None => Err(anyhow::anyhow!(
                                        "admitted local worker transport requires a durably-issued accounting attempt"
                                    )),
                                },
                            };
                            // Classify and close every retry prerequisite before
                            // settlement. The resulting closed decision is then
                            // committed atomically with the predecessor's terminal
                            // financial state; no post-settlement event append is
                            // allowed to become retry authority.
                            let mut prepared_retry = if let Err(error) = &call {
                                // A refusal the entry names is final: it is
                                // never retried, whatever the retry status
                                // codes say.
                                if let Some((delay, reason)) =
                                    retry_classification(error, attempt, &self.execution).filter(
                                        |_| {
                                            request_rejection(&self.provider_config, error)
                                                .is_none()
                                        },
                                    )
                                {
                                    let usage_required = retry_requires_attempt_usage(
                                        &reason,
                                        self.accounting_fail_closed(),
                                    );
                                    let ambiguous_without_accounting = error
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
                                        break Err(anyhow::anyhow!(
                                            "provider_accounting_invalid: refusing an ambiguous retry because the provider may have accepted work but required attempt usage is unavailable; original error: {error:#}"
                                        ));
                                    }
                                    let mid_stream_usage = error
                                        .downcast_ref::<crate::provider_adapter::ProviderStreamError>()
                                        .and_then(|error| match error {
                                            crate::provider_adapter::ProviderStreamError::MidStream(
                                                mid_stream,
                                            ) => Some(mid_stream.usage.as_ref()),
                                            _ => None,
                                        });
                                    if let Some(usage) = mid_stream_usage {
                                        self.settle_available_attempt_accounting(
                                            turn,
                                            usage,
                                            turn_start.elapsed().as_millis() as u64,
                                        )
                                        .await
                                        .map_err(|settlement_error| {
                                            anyhow::anyhow!(
                                                "provider attempt accounting settlement failed before retry: {settlement_error:#}"
                                            )
                                        })?;
                                        if usage_required
                                            && !usage.is_some_and(|usage| usage.is_valid())
                                        {
                                            break Err(anyhow::anyhow!(
                                                "provider_accounting_invalid: mid-stream attempt cannot be retried because required final usage is unavailable; original error: {error:#}"
                                            ));
                                        }
                                    }
                                    if let Err(limit) = self.harness.check_retry_limits() {
                                        break Err(anyhow::anyhow!(
                                            "provider retry refused after closing the prior attempt: {limit}; original error: {error:#}"
                                        ));
                                    }
                                    Some(provider_retry_decision(
                                        turn,
                                        attempt_number,
                                        delay,
                                        reason,
                                        error,
                                        &self.execution,
                                    )?)
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            // One terminal daemon settlement per issued attempt.
                            // A retry is a new physical attempt and its exact
                            // successor coordinate is stored in this same commit.
                            let mut durable_retry_advance = None;
                            if let Some(ledger) = ledger_attempt {
                                let (stream_completed, ledger_usage, ledger_diag): (
                                    bool,
                                    Option<&crate::provider_adapter::http::TokenUsage>,
                                    Option<String>,
                                ) = match &call {
                                    Ok(crate::provider_adapter::StreamOutcome::Completed {
                                        response,
                                        ..
                                    }) => (true, response.usage.as_ref(), None),
                                    Ok(crate::provider_adapter::StreamOutcome::Cancelled {
                                        attempt: cut,
                                    }) => (
                                        false,
                                        cut.usage.as_ref(),
                                        Some("cancelled after the issue boundary".to_string()),
                                    ),
                                    Ok(crate::provider_adapter::StreamOutcome::Interrupted {
                                        attempt: cut,
                                        ..
                                    }) => (
                                        false,
                                        cut.usage.as_ref(),
                                        Some("interrupted after the issue boundary".to_string()),
                                    ),
                                    Err(error) => (
                                        false,
                                        carried_usage_from_error(error),
                                        Some(format!(
                                            "provider attempt failed after the issue boundary: \
                                             {error:#}"
                                        )),
                                    ),
                                };
                                let authority = ledger_authority
                                    .as_ref()
                                    .expect("ledger attempt implies ledger authority");
                                let (spend, tokens) = build_ledger_settlement(
                                    authority,
                                    stream_completed,
                                    ledger_usage,
                                    ledger_diag.as_deref(),
                                );
                                let answer = match &call {
                                    Ok(crate::provider_adapter::StreamOutcome::Completed {
                                        response,
                                        ..
                                    }) => match crate::directive::normalize_provider_call_answer(
                                        &response.message,
                                        response.finish_reason.as_deref(),
                                    ) {
                                        Ok(answer) => Some(answer),
                                        Err(error) => break Err(error),
                                    },
                                    _ => None,
                                };
                                durable_retry_advance = match self
                                    .settle_ledger_attempt(
                                        &ledger,
                                        spend,
                                        tokens,
                                        answer,
                                        prepared_retry.clone(),
                                    )
                                    .await
                                {
                                    Ok(advance) => advance,
                                    Err(settle_error) => {
                                        // Terminal settlement unprovable: no
                                        // retry, fail-safe native recovery.
                                        break Err(anyhow::anyhow!(
                                            "provider_attempt_settlement_unprovable: {settle_error:#}"
                                        ));
                                    }
                                };
                                if prepared_retry.is_some() && durable_retry_advance.is_none() {
                                    // The ledger may refuse advancement when
                                    // settlement disproves/quarantines its
                                    // authority. The attempt remains honestly
                                    // terminal, but the original provider error
                                    // is not retryable anymore.
                                    prepared_retry = None;
                                }
                            }
                            (call, durable_retry_advance, prepared_retry)
                        };
                        match call {
                            Err(e) => {
                                let Some(decision) = prepared_retry else {
                                    if provider_outcome_is_uncertain(&e) {
                                        break Err(anyhow::anyhow!(
                                            "provider_outcome_uncertain: the provider may have accepted the issued request and this failure was not retried; a retry after an unknown outcome needs the directive to declare `provider_contact: repeatable` on a remote provider route and `execution.unknown_outcome_retry` to name this failure class within its first max_retries attempts (sealed ceiling {}); original error: {e:#}",
                                            self.execution
                                                .unknown_outcome_retry
                                                .as_ref()
                                                .map_or(0, |policy| policy.max_retries)
                                        ));
                                    }
                                    break Err(e);
                                };
                                let Some(advance) = durable_retry_advance else {
                                    break Err(anyhow::anyhow!(
                                        "provider_retry_requires_durable_attempt_authority: the provider failure was retryable, but no atomic ledger advancement exists; original failure digest={}",
                                        decision.failure_digest.as_str()
                                    ));
                                };
                                self.validate_retry_advance(&advance, turn, attempt_number)?;
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "provider_retry_settled",
                                )
                                .await;
                                if let Err(error) =
                                    self.emit_provider_retry_testimony(&advance).await
                                {
                                    tracing::warn!(
                                        decision_digest = %advance.decision_digest.as_str(),
                                        %error,
                                        "provider retry testimony outcome is unknown; requesting same-thread recovery"
                                    );
                                    return Err(ryeos_runtime::process_outcome::RuntimeRecoveryRequired::retained_progress_outcome_unknown(
                                        self.thread_id.clone(),
                                    ).into());
                                }
                                attempt = advance.decision.failed_attempt_number;
                                tracing::warn!(
                                    turn,
                                    attempt,
                                    max_retries = self.execution.retries,
                                    backoff_ms = advance.decision.backoff_ms,
                                    reason = ?advance.decision.reason,
                                    failure_digest = %advance.decision.failure_digest.as_str(),
                                    "provider call failed with a durably admitted retry; backing off before the exact successor"
                                );
                                warnings.push(format!(
                                    "provider retry {attempt}/{max} on turn {turn} after {ms}ms backoff (decision {decision_digest})",
                                    max = self.execution.retries,
                                    ms = advance.decision.backoff_ms,
                                    decision_digest = advance.decision_digest.as_str(),
                                ));
                                if let Some(terminal) = provider_retry_wait_terminal(
                                    wait_for_provider_retry_deadline(
                                        advance.not_before_ms,
                                        &cancel_flag,
                                        &interrupt_flag,
                                    )
                                    .await,
                                ) {
                                    break Ok(terminal);
                                }
                                continue;
                            }
                            ok => break ok,
                        }
                    };
                    if self.provider_effects_recorded
                        && matches!(
                            &stream_result,
                            Ok(crate::provider_adapter::StreamOutcome::Completed { .. })
                        )
                    {
                        block_at_directive_recovery_test_cut(
                            &self.callback,
                            "provider_effect_settled",
                        )
                        .await;
                    }
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
                                if !usage.spend_anomalies.is_empty() {
                                    // This one silently changes money: the
                                    // reported charge is distrusted and the
                                    // attempt settles at the reserved
                                    // maximum. Say so where the operator
                                    // can see it.
                                    tracing::warn!(
                                        turn,
                                        anomalies = ?usage.spend_anomalies,
                                        "provider-reported spend is untrustworthy; the attempt \
                                         settles conservatively at the reserved maximum"
                                    );
                                    warnings.push(format!(
                                        "provider_spend_anomaly on turn {turn} (settled at \
                                         reserved maximum): {}",
                                        usage.spend_anomalies.join("; ")
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
                                        state = State::Errored {
                                            error: format!(
                                                "independent provider-reported spend settlement failed: {error:#}"
                                            ),
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
                            // A replayed turn has no usage by construction:
                            // nothing was reserved, issued, or billed, so the
                            // fail-closed snapshot requirement does not apply.
                            // Its accounting fact is `replayed_from`, carried
                            // in the durable turn evidence.
                            if usage_required
                                && valid_usage.is_none()
                                && turn_replayed_from.is_none()
                            {
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
                                continue;
                            }
                            if resp.usage.is_some() && valid_usage.is_none() {
                                warnings.push(format!(
                                    "provider accounting unavailable on turn {turn}; malformed \
                                     token counts were not settled as zero"
                                ));
                            } else if resp.usage.is_none() && turn_replayed_from.is_none() {
                                warnings.push(format!(
                                    "provider accounting unavailable on turn {turn}; no usage \
                                     snapshot was reported and no token counts were settled"
                                ));
                            }
                            let token_counts =
                                valid_usage.and_then(|usage| usage.complete_token_counts());
                            let (input_tok, output_tok) = token_counts.unwrap_or((0, 0));
                            let cost = if settled_spend_only {
                                // The spend-only settlement above already
                                // quantized this exact figure successfully.
                                let usd = resp
                                    .usage
                                    .as_ref()
                                    .and_then(|usage| usage.reported_cost_usd)
                                    .map(|reported| {
                                        ryeos_runtime::envelope::UsdNanos::quantize_reported_f64_round_up(reported)
                                            .expect("spend-only settlement quantized this figure")
                                            .0
                                    })
                                    .unwrap_or(ryeos_runtime::envelope::UsdNanos::ZERO);
                                CostBreakdown {
                                    usd,
                                    source: PricingSource::ProviderReported,
                                }
                            } else {
                                let cost_result = self.compute_cost_for_usage(valid_usage);
                                match cost_result {
                                    Ok(cost) => cost,
                                    Err(error) => {
                                        state = State::Errored {
                                            error: format!("turn pricing failed: {error:#}"),
                                        };
                                        continue;
                                    }
                                }
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
                                let settlement_result = self
                                    .settle_provider_usage(
                                        turn,
                                        usage,
                                        usd,
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await;
                                if let Err(e) = settlement_result {
                                    state = State::Errored {
                                        error: format!("provider usage settlement failed: {e:#}"),
                                    };
                                    continue;
                                }
                            }
                            let ended_at_output_limit = crate::directive::normalize_finish_reason(
                                resp.finish_reason.as_deref(),
                            ) == FinishReason::Length;
                            if ended_at_output_limit
                                && self.summary_job.is_none()
                                && self.consecutive_length_turns >= self.output_limit_allowance()
                            {
                                state = State::Errored {
                                    error: OUTPUT_LIMIT_ERROR.to_string(),
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
                            if let Err(error) =
                                crate::directive::ensure_distinct_call_ids(&resp.message)
                            {
                                state = State::Errored {
                                    error: format!("{error}"),
                                };
                                continue;
                            }
                            // A turn that owes a replayable reasoning value under
                            // a `refuse` rule and holds none fails here, after its
                            // usage is settled and before it is recorded or any
                            // tool it proposed is dispatched.
                            if self.summary_job.is_none()
                                && let Some(missing) = self
                                .provider_config
                                .schemas
                                .as_ref()
                                .and_then(|schemas| schemas.reasoning.as_ref())
                                .and_then(|reasoning| {
                                    crate::provider_adapter::reasoning_replay::owed_missing_in_turn(
                                        &self.messages,
                                        &resp.message,
                                        reasoning,
                                    )
                                })
                            {
                                state = State::Errored {
                                    error: missing.to_string(),
                                };
                                continue;
                            }
                            let summary_finish = self.summary_job.as_ref().map(|job| {
                                ryeos_runtime::cognition::compaction_summary_finish(&job.trigger)
                            });
                            let finish = match summary_finish.as_deref() {
                                Some(finish) => finish,
                                None => finish_label(crate::directive::normalize_finish_reason(
                                    resp.finish_reason.as_deref(),
                                )),
                            };
                            let provider_accounting = Some(json!({
                                "attempt_id": format!("{}:{turn}:{}", self.thread_id, attempt + 1),
                                "attempt_number": attempt + 1,
                                "input_tokens": resp.usage.as_ref().and_then(|usage| usage.input_tokens),
                                "output_tokens": resp.usage.as_ref().and_then(|usage| usage.output_tokens),
                                "reasoning_tokens": resp.usage.as_ref().and_then(|usage| usage.reasoning_tokens),
                                "cache_read_tokens": resp.usage.as_ref().and_then(|usage| usage.cache_read_tokens),
                                "cache_write_tokens": resp.usage.as_ref().and_then(|usage| usage.cache_write_tokens),
                                "cache_miss_tokens": resp.usage.as_ref().and_then(|usage| usage.cache_miss_tokens),
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
                                "replayed_from": &turn_replayed_from,
                            }));
                            // A summary call is recorded as its text alone: it
                            // offered no tools, so a call in its answer is not
                            // carried and never owed a result.
                            let recorded_parts: Vec<Part> = if self.summary_job.is_some() {
                                resp.message
                                    .parts
                                    .iter()
                                    .filter(|part| matches!(part, Part::Text(_)))
                                    .cloned()
                                    .collect()
                            } else {
                                resp.message.parts.clone()
                            };
                            if self.summary_job.is_some() && recorded_parts.is_empty() {
                                state = State::Errored {
                                    error: "compaction_summary_failed: the summary call returned no text"
                                        .to_string(),
                                };
                                continue;
                            }
                            let malformed_calls = if self.summary_job.is_some() {
                                Vec::new()
                            } else {
                                match malformed_calls_of_turn(&events, &resp.message) {
                                    Ok(calls) => calls,
                                    Err(error) => {
                                        state = State::Errored {
                                            error: format!("{error:#}"),
                                        };
                                        continue;
                                    }
                                }
                            };
                            if let Err(e) = self
                                .callback
                                .emit_turn_complete(
                                    turn,
                                    finish,
                                    token_counts,
                                    recorded_parts.clone(),
                                    provider_accounting,
                                    malformed_calls.clone(),
                                )
                                .await
                            {
                                state = State::RecoveryRequired {
                                    error: format!(
                                        "resume-critical callback emit_turn_complete failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            if let Some(job) = self.summary_job.as_mut() {
                                // The summary is not a conversation turn: it
                                // goes into the compaction record, not into the
                                // conversation.
                                job.result = Some(recorded_parts);
                                job.truncated = ended_at_output_limit;
                                state = State::FinishingCompaction;
                                continue;
                            }
                            self.turn_malformed_calls = malformed_calls;
                            self.consecutive_length_turns = if ended_at_output_limit {
                                self.consecutive_length_turns.saturating_add(1)
                            } else {
                                0
                            };
                            self.messages.push(resp.message.clone());
                            if resp.message.has_tool_calls() {
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "tool_proposal_recorded",
                                )
                                .await;
                            } else if resp.message.content_value().is_some() {
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "final_cognition_recorded",
                                )
                                .await;
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
                            if let Err(error) = self
                                .settle_available_attempt_accounting(
                                    turn,
                                    cut.usage.as_ref(),
                                    turn_start.elapsed().as_millis() as u64,
                                )
                                .await
                            {
                                state = State::Errored {
                                    error: format!(
                                        "cancelled provider attempt accounting settlement failed: {error:#}"
                                    ),
                                };
                                continue;
                            }
                            // SIGTERM remains a cancellation, not a provider
                            // failure, after independently settling any usage
                            // received before the signal.
                            State::Cancelled
                        }
                        Ok(crate::provider_adapter::StreamOutcome::Interrupted {
                            partial_cognition,
                            events,
                            attempt: cut,
                        }) => {
                            // A live interrupt that cuts a compaction's summary
                            // call ends the compaction: the partial summary is
                            // not a conversation turn.
                            if self.summary_job.is_some() {
                                let settled = self
                                    .settle_available_attempt_accounting(
                                        turn,
                                        cut.usage.as_ref(),
                                        turn_start.elapsed().as_millis() as u64,
                                    )
                                    .await;
                                state = State::Errored {
                                    error: match settled {
                                        Ok(_) => "interrupted: the compaction summary call was interrupted".to_string(),
                                        Err(error) => format!(
                                            "interrupted: the compaction summary call was interrupted; its accounting settlement failed: {error:#}"
                                        ),
                                    },
                                };
                                continue;
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
                            // Account any provider usage only after refunding
                            // the logical turn. The cumulative usage snapshot
                            // must retain the charge while honestly showing
                            // that an interrupted cognition did not consume a
                            // completed-turn slot.
                            if let Err(error) = self
                                .settle_available_attempt_accounting(
                                    turn,
                                    cut.usage.as_ref(),
                                    turn_start.elapsed().as_millis() as u64,
                                )
                                .await
                            {
                                state = State::Errored {
                                    error: format!(
                                        "interrupted provider attempt accounting settlement failed: {error:#}"
                                    ),
                                };
                                continue;
                            }

                            // Seal the partial as a transcript-bearing
                            // cognition_out with finish "interrupted" (the
                            // complete text and reasoning parts, no tool
                            // calls) so the braid holds an honest, foldable
                            // consequence. The recorded parts are the message
                            // the redirected call sees.
                            let interrupted_message = partial_cognition.into_provider_message();
                            if let Err(e) = self
                                .callback
                                .emit_turn_interrupted(turn, interrupted_message.parts.clone())
                                .await
                            {
                                state = State::RecoveryRequired {
                                    error: format!(
                                        "resume-critical callback emit_turn_interrupted failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            self.messages.push(interrupted_message);

                            // Back to the between-turns boundary: CheckingLimits
                            // folds the queued input (DECISION 2: if the poll is
                            // empty, a fresh cognition still runs with no new input)
                            // and runs the redirect cognition. Does NOT finalize.
                            State::CheckingLimits
                        }
                        Err(e) => {
                            let rejection = request_rejection(&self.provider_config, &e);
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
                                            let settlement_result = self
                                                .settle_provider_spend_only(
                                                    turn,
                                                    reported_cost_usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await;
                                            if let Err(settlement_error) = settlement_result {
                                                detail.push_str(&format!(
                                                    "; reported-spend settlement also failed: {settlement_error:#}"
                                                ));
                                            }
                                        }
                                    } else {
                                        let settled = match self.compute_cost_for_usage(Some(usage))
                                        {
                                            Ok(cost) => {
                                                self.settle_provider_usage(
                                                    turn,
                                                    usage,
                                                    cost.usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await
                                            }
                                            Err(error) => Err(error),
                                        };
                                        if let Err(settlement_error) = settled {
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
                                            let settlement_result = self
                                                .settle_provider_spend_only(
                                                    turn,
                                                    reported_cost_usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await;
                                            if let Err(settlement_error) = settlement_result {
                                                detail.push_str(&format!(
                                                    "; reported-spend settlement also failed: {settlement_error:#}"
                                                ));
                                            }
                                        }
                                    } else {
                                        let (input_tokens, output_tokens) = usage
                                            .complete_token_counts()
                                            .expect("valid provider usage has complete token counts");
                                        let settled = match self.compute_cost_for_usage(Some(usage))
                                        {
                                            Ok(cost) => {
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
                                                self
                                                    .settle_provider_usage(
                                                    turn,
                                                    usage,
                                                    cost.usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await
                                            }
                                            Err(error) => Err(error),
                                        };
                                        if let Err(settlement_error) = settled {
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
                                crate::provider_adapter::ProviderStreamError::MidStream(
                                    mid_stream,
                                ) => Some((
                                    mid_stream.accepted_bytes,
                                    mid_stream.accepted_output_events,
                                    mid_stream.live_output_events_emitted,
                                    mid_stream.usage.clone(),
                                    mid_stream.generation_header_id.clone(),
                                    mid_stream.response_id.clone(),
                                    mid_stream.requested_output_tokens,
                                    mid_stream.detail.clone(),
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
                                        let settled = match self.compute_cost_for_usage(Some(usage))
                                        {
                                            Ok(cost) => {
                                                self.settle_provider_usage(
                                                    turn,
                                                    usage,
                                                    cost.usd,
                                                    turn_start.elapsed().as_millis() as u64,
                                                )
                                                .await
                                            }
                                            Err(error) => Err(error),
                                        };
                                        if let Err(settlement_error) = settled {
                                            detail.push_str(&format!(
                                                "; accounting settlement also failed: {settlement_error:#}"
                                            ));
                                        }
                                    } else if let Some(reported_cost_usd) =
                                        usage.reported_cost_usd
                                    {
                                        let settlement_result = self
                                            .settle_provider_spend_only(
                                                turn,
                                                reported_cost_usd,
                                                turn_start.elapsed().as_millis() as u64,
                                            )
                                            .await;
                                        if let Err(settlement_error) = settlement_result {
                                            detail.push_str(&format!(
                                                "; reported-spend settlement also failed: {settlement_error:#}"
                                            ));
                                        }
                                    }
                                }
                                detail
                            } else if let Some(rejection) = &rejection {
                                format!("{}: {e:#}", rejection.code)
                            } else {
                                format!("{e:#}")
                            };
                            // The compaction policy may answer a refusal that names
                            // an overflow code: this thread settles `continued` and
                            // its successor compacts first, then makes the request
                            // again. A refusal of a compaction's own summary call is
                            // never answered this way.
                            let recovery = match &rejection {
                                Some(rejection) if self.summary_job.is_none() => {
                                    self.overflow_recovery(&rejection.code)
                                }
                                _ => OverflowRecovery::NotApplicable,
                            };
                            match rejection {
                                // A named refusal reaches the directive's
                                // `provider_error` hooks, with its code, before
                                // the failure settles. The hooks observe; the
                                // failure itself is the typed code.
                                Some(rejection) => {
                                    let after = match recovery {
                                        OverflowRecovery::Allowed => {
                                            let live_context = self
                                                .continuation
                                                .estimate_live_context_tokens(&self.messages);
                                            self.pressure_hook = Some(PressureHook {
                                                compact: State::Continued,
                                                skip: State::Errored {
                                                    error: error.clone(),
                                                },
                                            });
                                            State::FiringHooks {
                                                occurrence: self.pressure_occurrence(turn),
                                                context: self.pressure_context(
                                                    "overflow",
                                                    turn,
                                                    live_context,
                                                    Some(&rejection),
                                                ),
                                                resume_to: Box::new(State::Continued),
                                            }
                                        }
                                        OverflowRecovery::Exhausted => State::Errored {
                                            error: format!(
                                                "compaction_recovery_exhausted: {} overflow recoveries are spent in this chain; the refusal was: {error}",
                                                self.compaction.max_overflow_recoveries_per_chain
                                            ),
                                        },
                                        OverflowRecovery::NotApplicable => State::Errored { error },
                                    };
                                    State::FiringHooks {
                                        occurrence:
                                            ryeos_runtime::callback::HookDispatchOccurrence::new(
                                                "directive",
                                                "provider_error",
                                                self.definition_ref.clone(),
                                                self.root_raw_content_digest.clone(),
                                                self.effective_definition_digest.clone(),
                                            )
                                            .with_counter_coordinate("turn", turn),
                                        context: json!({
                                            "event": {
                                                "name": "provider_error",
                                                "code": rejection.code,
                                                "status": rejection.status,
                                                "turn": turn,
                                                "retryable": false,
                                            }
                                        }),
                                        resume_to: Box::new(after),
                                    }
                                }
                                None => State::Errored { error },
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
                            StreamEvent::ReasoningArtefact { capture_id, value } => {
                                tracing::trace!(
                                    capture_id = %capture_id,
                                    bytes = value.to_string().len(),
                                    "reasoning artefact captured"
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
                                    cache_miss = ?update.cache_miss_tokens,
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
                            if msg.has_tool_calls() {
                                let occurrences = msg
                                    .calls()
                                    .iter()
                                    .enumerate()
                                    .map(|(call_index, call)| ToolOccurrence {
                                        call: call.clone(),
                                        operation_id: crate::resume::directive_tool_operation_id(
                                            &self.thread_id,
                                            turn,
                                            call_index,
                                        ),
                                        turn,
                                        call_index,
                                        start_recorded: false,
                                        counted: false,
                                        spawn_counted: false,
                                        malformed: self
                                            .turn_malformed_calls
                                            .iter()
                                            .find(|malformed| {
                                                usize::try_from(malformed.call_index).ok()
                                                    == Some(call_index)
                                            })
                                            .cloned(),
                                    })
                                    .collect::<Vec<_>>();
                                // Lifecycle-bearing batches keep the
                                // strict serial path; plain batches
                                // dispatch through the bounded window.
                                if batch_is_lifecycle_bearing(&occurrences) {
                                    State::DispatchingTools {
                                        pending: occurrences,
                                        index: 0,
                                    }
                                } else {
                                    State::DispatchingToolBatch {
                                        pending: occurrences,
                                    }
                                }
                            } else if self.consecutive_length_turns > 0 {
                                // The turn ended at the output limit and made no
                                // tool call: its text is incomplete, so it must
                                // not finalize. Record the policy's message as a
                                // harness input and take another turn, within the
                                // policy's bound and the turn limit.
                                let message = self
                                    .execution
                                    .on_output_limit
                                    .as_ref()
                                    .map(|policy| policy.message.clone());
                                match message {
                                    Some(message) if self.can_start_another_turn() => {
                                        let input = ryeos_runtime::CognitionInput::text(
                                            "harness", &message,
                                        )
                                        .with_kind("output_limit");
                                        if let Err(e) =
                                            self.callback.emit_cognition_input(&input).await
                                        {
                                            State::RecoveryRequired {
                                                error: format!(
                                                    "resume-critical output_limit input append failed: {e}"
                                                ),
                                            }
                                        } else {
                                            self.messages.push(ProviderMessage::user(message));
                                            State::CheckingLimits
                                        }
                                    }
                                    _ => State::Errored {
                                        error: OUTPUT_LIMIT_ERROR.to_string(),
                                    },
                                }
                            } else if let Some(content) = msg.content_value() {
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
                                        Err(e) => State::RecoveryRequired { error: e },
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
                        let occurrence = pending[index].clone();
                        let tc = &occurrence.call;
                        if !occurrence.start_recorded {
                            if let Err(e) = self
                                .callback
                                .emit_tool_dispatch(
                                    &ryeos_runtime::callback_client::ToolDispatchEvent {
                                        operation_id: &occurrence.operation_id,
                                        tool: &tc.name,
                                        call_id: tc.id.as_deref().unwrap_or_default(),
                                        item_ref: self.dispatcher.item_ref(&tc.name).as_deref(),
                                        turn: occurrence.turn,
                                        call_index: occurrence.call_index,
                                        effective_caps: self.harness.effective_caps(),
                                    },
                                )
                                .await
                            {
                                state = State::RecoveryRequired {
                                    error: format!(
                                        "resume-critical callback emit_tool_dispatch failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            if tc.name != DIRECTIVE_RETURN_TOOL
                                && !self.harness.tool_calls_available()
                            {
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "tool_limit_refusal_start_recorded",
                                )
                                .await;
                            }
                        }

                        // directive_return: lifecycle signal, not a tool.
                        // Bypass permission check and dispatch entirely;
                        // the runner handles output validation,
                        // event emission, and finalization inline.
                        if tc.name == DIRECTIVE_RETURN_TOOL {
                            State::ProcessingDirectiveReturn {
                                call_id: tc.id.clone().unwrap_or_default(),
                                operation_id: occurrence.operation_id,
                                raw_args: tc.arguments.to_string(),
                                malformed: occurrence.malformed,
                                siblings: pending[index + 1..].to_vec(),
                            }
                        } else if self.tool_refused_at_hard_stop(&tc.name) {
                            // Refused before it takes tool-call budget; the
                            // settled result keeps the turn input complete and
                            // the next turn start stops at the hard stop.
                            let call_id = tc.id.clone().unwrap_or_default();
                            let tool_name = tc.name.clone();
                            let rendered = Self::rendered_error_envelope(
                                &tool_name,
                                tool_deadline_elapsed_error(),
                            );
                            if let Err(error) = self
                                .settle_tool_result(occurrence.operation_id, call_id, rendered)
                                .await
                            {
                                state = State::RecoveryRequired { error };
                                continue;
                            }
                            state_after_serial_tool_result(
                                pending,
                                index,
                                turn,
                                &self.definition_ref,
                                &self.root_raw_content_digest,
                                &self.effective_definition_digest,
                            )
                        } else if tc.name != TIME_LEFT_TOOL
                            && !occurrence.counted
                            && !self.harness.try_record_tool_call()
                        {
                            let call_id = tc.id.clone().unwrap_or_default();
                            let tool_name = tc.name.clone();
                            let rendered =
                                Self::rendered_error_envelope(&tool_name, tool_call_limit_error());
                            if let Err(error) = self
                                .settle_tool_result(occurrence.operation_id, call_id, rendered)
                                .await
                            {
                                state = State::RecoveryRequired { error };
                                continue;
                            }
                            state_after_serial_tool_result(
                                pending,
                                index,
                                turn,
                                &self.definition_ref,
                                &self.root_raw_content_digest,
                                &self.effective_definition_digest,
                            )
                        } else {
                            // Permission check deferred to the dispatcher,
                            // which uses the canonical ref (not the LLM-
                            // emitted tool name) for cap matching.
                            State::ProcessingToolResult {
                                call_id: tc.id.clone().unwrap_or_default(),
                                tool_name: tc.name.clone(),
                                operation_id: occurrence.operation_id,
                                raw_args: tc.arguments.to_string(),
                                malformed: occurrence.malformed,
                                pending,
                                index,
                                spawn_counted: occurrence.spawn_counted,
                            }
                        }
                    }
                }

                State::ProcessingToolResult {
                    call_id,
                    tool_name,
                    operation_id,
                    raw_args,
                    malformed,
                    pending,
                    index,
                    spawn_counted,
                } => {
                    // `time_left` is answered in process from the run clock; it
                    // never reaches the daemon.
                    let local_report =
                        (tool_name == TIME_LEFT_TOOL).then(|| self.time_left_report());
                    let tool_result: RenderedToolResult = match local_report {
                        Some(report) => self.rendered_success(&tool_name, &report),
                        None => match self
                            .preflight_tool_call(
                                &operation_id,
                                &tool_name,
                                &raw_args,
                                &call_id,
                                spawn_counted,
                                malformed.as_ref(),
                            )
                            .await
                        {
                            Ok(request) => match self.callback.dispatch_action(*request).await {
                                Ok(response) => {
                                    block_at_directive_recovery_test_cut(
                                        &self.callback,
                                        "tool_settled",
                                    )
                                    .await;
                                    self.rendered_success(&tool_name, &response.result)
                                }
                                Err(e) if e.runtime_action_outcome_unknown() => {
                                    state = State::RecoveryRequired {
                                        error: format!(
                                            "runtime action {operation_id} outcome is unknown: {e}"
                                        ),
                                    };
                                    continue;
                                }
                                Err(e) => Self::rendered_error_envelope(
                                    &tool_name,
                                    callback_error_envelope(&e),
                                ),
                            },
                            Err(body) => Self::rendered_error_envelope(&tool_name, body),
                        },
                    };

                    if let Err(error) = self
                        .settle_tool_result(operation_id, call_id, tool_result)
                        .await
                    {
                        state = State::RecoveryRequired { error };
                        continue;
                    }

                    state_after_serial_tool_result(
                        pending,
                        index,
                        turn,
                        &self.definition_ref,
                        &self.root_raw_content_digest,
                        &self.effective_definition_digest,
                    )
                }

                State::DispatchingToolBatch { pending } => {
                    if self.harness.is_cancelled() {
                        state = State::Cancelled;
                        continue;
                    }

                    // Phase A — serial, in call order. Braid dispatch intents,
                    // resolution, spawn accounting, and risk policy keep their
                    // serial ordering and their `&mut` access; only the daemon
                    // dispatch awaits overlap in phase B.
                    enum BatchWork {
                        /// Admitted for concurrent dispatch; taken at spawn.
                        Dispatch(Option<Box<ryeos_runtime::callback::DispatchActionRequest>>),
                        /// Preflight refusal: an error-envelope body that never
                        /// passes the result guard, exactly like the serial path.
                        Immediate(String),
                        /// Answered in process (`time_left`); folded in call
                        /// order like a dispatched result.
                        Local(Value),
                    }
                    let mut prepared: Vec<(String, String, String, BatchWork)> =
                        Vec::with_capacity(pending.len());
                    let mut recovery_required: Option<String> = None;
                    let mut admission_cancelled = false;
                    for occurrence in &pending {
                        let tc = &occurrence.call;
                        if self.harness.is_cancelled() {
                            admission_cancelled = true;
                            break;
                        }
                        if !occurrence.start_recorded {
                            if let Err(e) = self
                                .callback
                                .emit_tool_dispatch(
                                    &ryeos_runtime::callback_client::ToolDispatchEvent {
                                        operation_id: &occurrence.operation_id,
                                        tool: &tc.name,
                                        call_id: tc.id.as_deref().unwrap_or_default(),
                                        item_ref: self.dispatcher.item_ref(&tc.name).as_deref(),
                                        turn: occurrence.turn,
                                        call_index: occurrence.call_index,
                                        effective_caps: self.harness.effective_caps(),
                                    },
                                )
                                .await
                            {
                                recovery_required = Some(format!(
                                    "resume-critical callback emit_tool_dispatch failed: {e}"
                                ));
                                break;
                            }
                            if tc.name != DIRECTIVE_RETURN_TOOL
                                && !self.harness.tool_calls_available()
                            {
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "tool_limit_refusal_start_recorded",
                                )
                                .await;
                            }
                        }
                        let call_id = tc.id.clone().unwrap_or_default();
                        let raw_args = tc.arguments.to_string();
                        // `time_left` is free: it neither needs nor takes tool-call budget.
                        let work = if tc.name == TIME_LEFT_TOOL {
                            BatchWork::Local(self.time_left_report())
                        } else if self.tool_refused_at_hard_stop(&tc.name) {
                            BatchWork::Immediate(tool_deadline_elapsed_error())
                        } else if occurrence.counted || self.harness.try_record_tool_call() {
                            {
                                match self
                                    .preflight_tool_call(
                                        &occurrence.operation_id,
                                        &tc.name,
                                        &raw_args,
                                        &call_id,
                                        occurrence.spawn_counted,
                                        occurrence.malformed.as_ref(),
                                    )
                                    .await
                                {
                                    Ok(request) => BatchWork::Dispatch(Some(request)),
                                    Err(body) => BatchWork::Immediate(body),
                                }
                            }
                        } else {
                            BatchWork::Immediate(tool_call_limit_error())
                        };
                        prepared.push((
                            occurrence.operation_id.clone(),
                            call_id,
                            tc.name.clone(),
                            work,
                        ));
                    }
                    if let Some(error) = recovery_required {
                        state = State::RecoveryRequired { error };
                        continue;
                    }
                    // Phase B — bounded concurrent dispatch. Each task owns a
                    // callback clone and its sealed request; no task touches
                    // runner state. Cancellation stops admission; in-flight
                    // children are joined (they are daemon-managed threads
                    // under this thread's lineage, so a cancelled parent
                    // settlement cascades to them).
                    let width = self.execution.tool_concurrency.clamp(1, 16) as usize;
                    let dispatch_indices: Vec<usize> = prepared
                        .iter()
                        .enumerate()
                        .filter_map(|(index, (_, _, _, work))| {
                            matches!(work, BatchWork::Dispatch(_)).then_some(index)
                        })
                        .collect();
                    let mut outcomes: std::collections::BTreeMap<
                        usize,
                        Result<
                            ryeos_runtime::callback_contract::CallbackDispatchResponse,
                            ryeos_runtime::callback::CallbackError,
                        >,
                    > = std::collections::BTreeMap::new();
                    {
                        let mut join = tokio::task::JoinSet::new();
                        let mut next = 0usize;
                        loop {
                            while !admission_cancelled
                                && join.len() < width
                                && next < dispatch_indices.len()
                            {
                                if self.harness.is_cancelled() {
                                    admission_cancelled = true;
                                    break;
                                }
                                let index = dispatch_indices[next];
                                next += 1;
                                let request = match &mut prepared[index].3 {
                                    BatchWork::Dispatch(slot) => {
                                        slot.take().expect("dispatch slot spawns once")
                                    }
                                    BatchWork::Immediate(_) | BatchWork::Local(_) => {
                                        unreachable!("dispatch_indices selects only Dispatch work")
                                    }
                                };
                                let callback = self.callback.clone();
                                join.spawn(tracing::Instrument::instrument(
                                    async move {
                                        let result = callback.dispatch_action(*request).await;
                                        (index, result)
                                    },
                                    tracing::Span::current(),
                                ));
                            }
                            let joined = join.join_next().await;
                            match joined {
                                Some(Ok((index, result))) => {
                                    outcomes.insert(index, result);
                                }
                                Some(Err(join_error)) => {
                                    // A task-level failure (panic/abort) is
                                    // run-fatal, but the siblings' completed
                                    // work still folds: stop admitting, keep
                                    // draining, and settle every gathered
                                    // outcome before erroring.
                                    recovery_required.get_or_insert(format!(
                                        "tool dispatch task failed: {join_error}"
                                    ));
                                    admission_cancelled = true;
                                }
                                None => break,
                            }
                        }
                    }

                    // Phase C — fold strictly by call order. The result guard,
                    // result events, and transcript pushes run serially here,
                    // so dedup/truncation decisions and the provider-visible
                    // ordering are identical to a serial run. Under a
                    // cancellation only calls that actually dispatched fold;
                    // unadmitted calls are dropped exactly as the serial path
                    // drops undispatched ones.
                    for (index, (operation_id, call_id, tool_name, work)) in
                        prepared.into_iter().enumerate()
                    {
                        let rendered = match work {
                            BatchWork::Immediate(body) => {
                                Self::rendered_error_envelope(&tool_name, body)
                            }
                            BatchWork::Local(report) => self.rendered_success(&tool_name, &report),
                            BatchWork::Dispatch(unstarted_request) => match outcomes.remove(&index)
                            {
                                Some(Ok(response)) => {
                                    block_at_directive_recovery_test_cut(
                                        &self.callback,
                                        "tool_settled",
                                    )
                                    .await;
                                    self.rendered_success(&tool_name, &response.result)
                                }
                                Some(Err(error)) if error.runtime_action_outcome_unknown() => {
                                    recovery_required = Some(format!(
                                        "runtime action {operation_id} outcome is unknown: {error}"
                                    ));
                                    break;
                                }
                                Some(Err(e)) => Self::rendered_error_envelope(
                                    &tool_name,
                                    callback_error_envelope(&e),
                                ),
                                None if unstarted_request.is_some() => {
                                    // The request remained in its slot, proving
                                    // no callback task contacted the daemon.
                                    let body = serde_json::to_string(&json!({
                                        "error": "batch aborted before this call dispatched"
                                    }))
                                    .unwrap_or_else(|_| {
                                        "{\"error\":\"not dispatched\"}".to_string()
                                    });
                                    Self::rendered_error_envelope(&tool_name, body)
                                }
                                None => {
                                    recovery_required = Some(format!(
                                        "runtime action {operation_id} task ended without a provable outcome"
                                    ));
                                    break;
                                }
                            },
                        };
                        if let Err(error) = self
                            .settle_tool_result(operation_id, call_id, rendered)
                            .await
                        {
                            recovery_required.get_or_insert(error);
                            break;
                        }
                    }
                    if let Some(error) = recovery_required {
                        state = State::RecoveryRequired { error };
                        continue;
                    }
                    if admission_cancelled || self.harness.is_cancelled() {
                        state = State::Cancelled;
                        continue;
                    }
                    State::FiringHooks {
                        occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                            "directive",
                            "after_step",
                            self.definition_ref.clone(),
                            self.root_raw_content_digest.clone(),
                            self.effective_definition_digest.clone(),
                        )
                        .with_counter_coordinate("turn", turn),
                        context: json!({"turn": turn}),
                        resume_to: Box::new(State::CheckingContinuation),
                    }
                }

                State::ProcessingDirectiveReturn {
                    call_id,
                    operation_id,
                    raw_args,
                    malformed,
                    siblings,
                } => {
                    // directive_return is a lifecycle signal, not a
                    // dispatchable tool. The LLM calls it using the
                    // provider's tool-call convention; the runtime
                    // recognizes it by name and handles inline:
                    //   1. Parse arguments (typed-fail-loud)
                    //   2. Validate against declared outputs
                    //   3. Fire tool_call_result for chain visibility
                    //   4. Publish directive_outputs artifact
                    //   5. Finalize thread
                    let parsed_arguments = match &malformed {
                        Some(malformed) => Err(malformed_call_error(malformed)),
                        None => crate::adapter::parse_tool_arguments(&raw_args),
                    };
                    let tool_result_content = match parsed_arguments {
                        Ok(args) => {
                            let validation_error = directive_output_validation_error(
                                self.directive_outputs.as_deref(),
                                &args,
                            );

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
                                let outputs_result = RenderedToolResult {
                                    tool: DIRECTIVE_RETURN_TOOL.to_string(),
                                    content: outputs_json,
                                    raw_size: outputs_size,
                                    truncated: false,
                                    duplicate_of: None,
                                    truncated_reason: None,
                                    status: ToolResultStatus::Ok,
                                    images: Vec::new(),
                                    dropped: Vec::new(),
                                };
                                if let Err(e) = self
                                    .callback
                                    .emit_tool_result(
                                        &outputs_result.payload(&operation_id, &call_id),
                                    )
                                    .await
                                {
                                    state = State::RecoveryRequired {
                                        error: format!(
                                            "resume-critical callback emit_tool_result failed: {e}"
                                        ),
                                    };
                                    continue;
                                }
                                block_at_directive_recovery_test_cut(
                                    &self.callback,
                                    "directive_return_recorded",
                                )
                                .await;
                                state = State::FinalizingDirectiveReturn { outputs: args };
                                continue;
                            }
                        }
                        Err(e) => serde_json::to_string(&json!({"error": e}))
                            .unwrap_or_else(|_| "{\"error\":\"malformed arguments\"}".to_string()),
                    };

                    // Validation or parse failure: fire tool_call_result,
                    // push error as tool message, and let the LLM retry.
                    // (Non-fatal — the LLM can correct its outputs.) The calls
                    // after it in the batch are not run; each is answered with
                    // a refusal so the next request holds a result for every
                    // call the assistant message names.
                    let failure_size = tool_result_content.len() as u64;
                    let failure_result = RenderedToolResult {
                        tool: DIRECTIVE_RETURN_TOOL.to_string(),
                        content: tool_result_content.clone(),
                        raw_size: failure_size,
                        truncated: false,
                        duplicate_of: None,
                        truncated_reason: None,
                        status: ToolResultStatus::Error,
                        images: Vec::new(),
                        dropped: Vec::new(),
                    };
                    if let Err(e) = self
                        .callback
                        .emit_tool_result(&failure_result.payload(&operation_id, &call_id))
                        .await
                    {
                        state = State::RecoveryRequired {
                            error: format!("resume-critical callback emit_tool_result failed: {e}"),
                        };
                        continue;
                    }
                    block_at_directive_recovery_test_cut(
                        &self.callback,
                        "failed_directive_return_recorded",
                    )
                    .await;
                    self.messages.push(ProviderMessage::tool_result_with_status(
                        call_id,
                        tool_result_content,
                        ToolResultStatus::Error,
                    ));
                    State::SettlingOwedSiblings { siblings }
                }

                State::SettlingOwedSiblings { siblings } => {
                    let mut recovery_required = None;
                    for sibling in &siblings {
                        if let Err(error) = self.refuse_sibling_of_failed_return(sibling).await {
                            recovery_required = Some(error);
                            break;
                        }
                    }
                    match recovery_required {
                        Some(error) => State::RecoveryRequired { error },
                        None => State::CheckingContinuation,
                    }
                }

                State::FiringHooks {
                    occurrence,
                    context,
                    resume_to,
                } => {
                    let event = occurrence.event().to_string();
                    // What a `context_pressure` hook's action chooses between.
                    let pressure_hook = if event == CONTEXT_PRESSURE_EVENT {
                        self.pressure_hook.take()
                    } else {
                        None
                    };
                    let callback = self.callback.clone();
                    let thread_id = self.thread_id.clone();

                    let dispatcher: ryeos_runtime::hooks_eval::HookDispatcher =
                        Box::new(move |action, _project_path, hook_dispatch| {
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
                                            action: payload,
                                            hook_dispatch: Some(hook_dispatch),
                                            effect_dispatch: None,
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
                        "",
                        &dispatcher,
                    )
                    .await;
                    let hook_cost = match &hook_run {
                        Ok(result) => result.cost.as_ref(),
                        Err(error) => error.cost.as_ref(),
                    };
                    if let Some(cost) = hook_cost
                        && let Err(error) = self.budget.accumulate(cost)
                    {
                        // A `provider_error` hook only observes a failure that
                        // is already typed; its own cost violation must not
                        // replace that failure's code. A `context_pressure`
                        // hook's cost violation leaves the policy's default.
                        if event == "provider_error" || event == CONTEXT_PRESSURE_EVENT {
                            tracing::warn!(%error, "provider_error hook cost violates accounting bounds; the typed failure stands");
                            state = *resume_to;
                            continue;
                        }
                        state = State::Errored {
                            error: format!(
                                "hook event `{event}` cost violates accounting bounds: {error}"
                            ),
                        };
                        continue;
                    }

                    match hook_run {
                        Ok(hook_run) => {
                            let hook_result = hook_run.control;
                            // Hook events ("before_step", "after_step", …)
                            // are not in the daemon's event-vocabulary
                            // allow-list; map them to `cognition_reasoning`
                            // (ephemeral) and stash the original hook name
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
                                    HookAction::Compact if event == CONTEXT_PRESSURE_EVENT => {
                                        match pressure_hook {
                                            Some(hook) => hook.compact,
                                            None => *resume_to,
                                        }
                                    }
                                    HookAction::SkipCompaction if event == CONTEXT_PRESSURE_EVENT => {
                                        match pressure_hook {
                                            Some(hook) => hook.skip,
                                            None => *resume_to,
                                        }
                                    }
                                    HookAction::Compact | HookAction::SkipCompaction => State::Errored {
                                        error: format!(
                                            "hook event `{event}` returned invalid control action: {action:?} is valid only on context_pressure"
                                        ),
                                    },
                                    // Retrying would send the same request: refused here.
                                    HookAction::Retry if event == CONTEXT_PRESSURE_EVENT => {
                                        State::Errored {
                                            error: "hook event `context_pressure` returned invalid control action: retry would send the same request".to_string(),
                                        }
                                    }
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
                        // A `provider_error` hook only observes a failure that
                        // is already typed; a hook that itself fails must not
                        // replace that failure's code.
                        Err(e) if event == "provider_error" || event == CONTEXT_PRESSURE_EVENT => {
                            tracing::warn!(error = %e, event = %event, "observing hook failed; the policy default stands");
                            *resume_to
                        }
                        Err(e) => State::Errored {
                            error: format!("hook event `{event}` failed: {e}"),
                        },
                    }
                }

                State::CheckingContinuation => {
                    let live_context = self
                        .continuation
                        .estimate_live_context_tokens(&self.messages);
                    if self.compaction_due(live_context) {
                        // The trigger is reached: the directive's hooks may
                        // choose, and with no choice the policy decides.
                        let compact = || State::Compacting {
                            trigger: "pressure".to_string(),
                        };
                        let by_policy =
                            if self.compaction.on_pressure == CompactionOnPressure::Compact {
                                compact()
                            } else {
                                State::ContinuationBoundary
                            };
                        self.pressure_hook = Some(PressureHook {
                            compact: compact(),
                            skip: State::ContinuationBoundary,
                        });
                        State::FiringHooks {
                            occurrence: self.pressure_occurrence(turn),
                            context: self.pressure_context("pressure", turn, live_context, None),
                            resume_to: Box::new(by_policy),
                        }
                    } else {
                        State::ContinuationBoundary
                    }
                }

                State::Compacting { trigger } => {
                    let outcome = match self.compaction.summary.mode {
                        SummaryMode::None => self.finish_compaction(&trigger, None).await,
                        SummaryMode::Model => match self.begin_summary_call(&trigger).await {
                            Ok(true) => {
                                state = State::CallingProvider;
                                continue;
                            }
                            Ok(false) => Ok(CompactionOutcome::NothingToReplace),
                            Err(failure) => Err(failure),
                        },
                    };
                    self.after_compaction(&trigger, outcome)
                }

                State::FinishingCompaction => {
                    let Some(job) = self.summary_job.take() else {
                        state = State::Errored {
                            error: "compaction_failed: no compaction summary to finish".to_string(),
                        };
                        continue;
                    };
                    // Only text is a summary: a call the model made although no
                    // tools were offered is not carried.
                    if job.truncated {
                        state = State::Errored {
                            error: "compaction_summary_failed: the summary was cut off at the output limit and is not carried"
                                .to_string(),
                        };
                        continue;
                    }
                    let parts: Vec<Part> = job
                        .result
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|part| matches!(part, Part::Text(_)))
                        .collect();
                    let has_text = parts.iter().any(
                        |part| matches!(part, Part::Text(text) if !text.text.trim().is_empty()),
                    );
                    if !has_text {
                        state = State::Errored {
                            error: "compaction_summary_failed: the summary call returned no text"
                                .to_string(),
                        };
                        continue;
                    }
                    let outcome = self
                        .finish_compaction(
                            &job.trigger,
                            Some(crate::compaction::Summary {
                                parts,
                                model_call: true,
                                turn,
                                omitted_from_input: job.omitted_from_input,
                            }),
                        )
                        .await;
                    self.after_compaction(&job.trigger, outcome)
                }

                State::ContinuationBoundary => {
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
                                occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                                    "directive",
                                    "continuation",
                                    self.definition_ref.clone(),
                                    self.root_raw_content_digest.clone(),
                                    self.effective_definition_digest.clone(),
                                )
                                .with_counter_coordinate("turn", turn),
                                context: self.continuation_hook_context(live_context, threshold),
                                resume_to: Box::new(State::Continued),
                            }
                        } else {
                            // Over the window with no way to continue: the run
                            // ends with a typed failure, not with its last
                            // message as if it were an answer. A directive that
                            // wants the old stop writes it into a hook.
                            State::Errored {
                                error: format!(
                                    "context_window_exhausted: live context of {live_context} estimated tokens reached the {threshold}-token continuation threshold and continuation is not enabled"
                                ),
                            }
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
                    // this segment. When outputs are declared:
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
                            // braid-visible and exactly reconstructable.
                            let nudge_input =
                                ryeos_runtime::CognitionInput::text("harness", &nudge)
                                    .with_kind("nudge");
                            if let Err(e) = self.callback.emit_cognition_input(&nudge_input).await {
                                state = State::RecoveryRequired {
                                    error: format!(
                                        "resume-critical return_nudge stimulus append failed: {e}"
                                    ),
                                };
                                continue;
                            }
                            self.messages.push(ProviderMessage::user(nudge));
                            state = State::CheckingLimits;
                            continue;
                        }
                        warnings.push(format!(
                            "declared outputs ({}) were never emitted via directive_return; \
                             settling with empty outputs",
                            declared_outputs.join(", ")
                        ));
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
                        return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                    }
                    let runtime_result = self.finalize(result);
                    guard.finalized = true;
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }

                State::FinalizingDirectiveReturn { outputs } => {
                    let completion = TerminalCompletion {
                        status: ThreadTerminalStatus::Completed,
                        outcome_code: Some("success".to_string()),
                        result: Some(json!(DIRECTIVE_RETURN_TOOL)),
                        error: None,
                        cost: Some(serde_json::to_value(self.budget.cost()).expect(
                            "validated directive cost must serialize for terminal settlement",
                        )),
                        outputs: outputs.clone(),
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
                        return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                    }
                    let mut runtime_result = self.finalize(json!(DIRECTIVE_RETURN_TOOL));
                    runtime_result.outputs = outputs;
                    guard.finalized = true;
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }

                State::Continued => {
                    if self.chain_depth >= self.chain_depth_limit {
                        state = State::Errored {
                            error: format!(
                                "continuation_chain_limit: this thread has {} continuation predecessors, the chain's limit of {}; the chain will not continue",
                                self.chain_depth, self.chain_depth_limit
                            ),
                        };
                        continue;
                    }
                    // Cut off by a limit mid-task: hand off to a chain-fold
                    // successor. Continuation is autonomous by construction — no
                    // reason/gate/mode, only an optional free-form string for logs.
                    //
                    // Do NOT swallow: a failed handoff must not settle the thread
                    // `continued` with no recorded successor. Surface as terminal
                    // `failed`.
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
                        // The daemon's refusal at the chain's depth limit carries
                        // its code: the run fails with it, typed.
                        if format!("{e}").contains("continuation_chain_limit:") {
                            state = State::Errored {
                                error: format!(
                                    "continuation_chain_limit: the chain reached its continuation depth limit and will not continue: {e}"
                                ),
                            };
                            continue;
                        }
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
                        return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                    }

                    guard.finalized = true;
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }

                State::Errored { error } => {
                    // A failed compaction summary call fails the run with its
                    // own typed code. This is a deliberate choice, not the
                    // carry-on-uncompacted behaviour a gentler design would
                    // have: the failed call burned a turn coordinate, and
                    // carrying on would need a failure-final record that every
                    // recovery reader skips.
                    let error = if self.summary_job.take().is_some() {
                        format!("compaction_summary_failed: {error}")
                    } else {
                        error
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
                        last_attempt_id.as_deref(),
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
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }

                State::RecoveryRequired { error } => {
                    tracing::warn!(%error, "directive requires same-thread native recovery");
                    return Err(
                        ryeos_runtime::process_outcome::RuntimeRecoveryRequired::retained_progress_outcome_unknown(
                            self.thread_id.clone(),
                        )
                        .into(),
                    );
                }

                State::DeadlineStopped => {
                    let stopped_at_ms = lillux::time::timestamp_millis();
                    let result = json!({
                        "outcome": "deadline",
                        "phase": "hard_stop",
                        "deadline_at_ms": self.harness.run_clock().map(|clock| clock.end_at_ms()),
                        "stopped_at_ms": stopped_at_ms,
                        "turn": turn,
                        "wrap_up_delivered": self.wrap_up_delivered,
                    });
                    // The runtime settles its own deadline stop over the
                    // callback, so the thread records the structured result
                    // as the runtime's testimony rather than a fallback
                    // wrapper. The callback and stdout carry the identical
                    // result, cost and warnings, which the daemon cross-checks.
                    let cost = self.budget.cost();
                    let completion = TerminalCompletion {
                        status: ThreadTerminalStatus::TimedOut,
                        outcome_code: Some("deadline".to_string()),
                        result: Some(result.clone()),
                        error: None,
                        cost: Some(serde_json::to_value(&cost).expect(
                            "validated directive cost must serialize for terminal settlement",
                        )),
                        outputs: json!({}),
                        warnings: warnings.clone(),
                    };
                    let finalized = self
                        .callback
                        .finalize_thread(completion)
                        .await
                        .map_err(|error| error.to_string());
                    let runtime_result = deadline_stop_result(
                        &self.thread_id,
                        result,
                        cost,
                        finalized,
                        &mut warnings,
                    );
                    guard.finalized = true;
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }

                State::Cancelled => {
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
                    return Ok(Self::attach_warnings(runtime_result, &mut warnings));
                }
            };
        }
    }

    /// Serial per-call preflight shared by the serial and batch dispatch
    /// paths: resolution, spawn accounting, risk policy, and the blocked-call
    /// advisory event. Returns the sealed dispatch request, or the immediate
    /// error-envelope body for a refused call. `&mut` because spawn accounting
    /// lives on the harness; MUST run in call order.
    async fn preflight_tool_call(
        &mut self,
        operation_id: &str,
        tool_name: &str,
        raw_args: &str,
        call_id: &str,
        spawn_already_counted: bool,
        malformed: Option<&ryeos_runtime::MalformedCall>,
    ) -> Result<Box<ryeos_runtime::callback::DispatchActionRequest>, String> {
        if let Some(malformed) = malformed {
            return Err(serde_json::to_string(
                &json!({ "error": malformed_call_error(malformed) }),
            )
            .unwrap_or_else(|_| "{\"error\":\"malformed arguments\"}".to_string()));
        }
        match self
            .dispatcher
            .resolve(tool_name, raw_args, Some(call_id.to_string()))
        {
            Ok(dispatch_result) => {
                // Record spawn for child executions (directive/graph)
                match dispatch_result.dispatch_kind {
                    DispatchKind::DirectiveChild | DispatchKind::GraphChild
                        if !spawn_already_counted =>
                    {
                        self.harness.record_spawn();
                    }
                    DispatchKind::DirectiveChild | DispatchKind::GraphChild => {}
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
                    let body_str = serde_json::to_string(&json!({
                        "error": format!(
                            "blocked by risk policy: {}",
                            dispatch_result.canonical_ref
                        )
                    }))
                    .unwrap_or_else(|_| "{\"error\":\"blocked\"}".to_string());
                    Err(body_str)
                } else {
                    Ok(Box::new(ryeos_runtime::callback::DispatchActionRequest {
                        thread_id: self.thread_id.clone(),
                        action: ryeos_runtime::callback::ActionPayload {
                            product_selections: Vec::new(),
                            operation_id: Some(operation_id.to_string()),
                            item_id: dispatch_result.canonical_ref.clone(),
                            ref_bindings: std::collections::BTreeMap::new(),
                            params: dispatch_result.arguments.clone(),
                            thread: "inline".to_string(),
                            // Directive tool-calls dispatch `tool:` refs at
                            // their default method; no method selector.
                            call: None,
                            facets: None,
                            launch_window: None,
                        },
                        hook_dispatch: None,
                        effect_dispatch: None,
                    }))
                }
            }
            Err(e) => Err(serde_json::to_string(&json!({ "error": e }))
                .unwrap_or_else(|_| "{\"error\":\"resolve failed\"}".to_string())),
        }
    }

    /// Reads into the image store the bytes of the images the next request
    /// carries that it does not hold: after a resume or a continuation, or
    /// when memory pressure dropped them. A live run records its images into
    /// the store as they happen, so this reads nothing then.
    async fn load_request_images(&mut self) -> anyhow::Result<()> {
        let Some(accepts) = self
            .provider_config
            .image_input
            .as_ref()
            .and_then(|image_input| image_input.accepts.as_ref())
        else {
            return Ok(());
        };
        // An image the entry cannot take by size is replaced by a note in
        // the conversation, never read back, and the run goes on.
        crate::provider_adapter::images::replace_unsendable_images(
            &mut self.messages,
            accepts,
            crate::tool_images::IMAGE_OVERSIZE,
        );
        let retained: Vec<String> = crate::provider_adapter::images::retained_images(
            &self.messages,
            accepts.max_images_per_request,
        )
        .into_iter()
        .map(|image| image.attachment.clone())
        .collect();
        // Pin the window before loading so reading one image back cannot
        // evict another the request needs.
        self.image_store.pin(retained.iter().cloned());
        let missing: std::collections::BTreeSet<String> = retained
            .into_iter()
            .filter(|attachment| !self.image_store.contains(attachment))
            .collect();
        for attachment in missing {
            let (media_type, bytes) = self
                .callback
                .read_attachment(&attachment)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "image_attachment_missing: could not read the bytes of image {attachment}: {error:#}"
                    )
                })?;
            self.image_store.insert(&attachment, &media_type, &bytes);
        }
        Ok(())
    }

    /// Render a successful dispatch response into the model-visible result.
    /// Applies the cross-result guard (truncation + duplicate collapse), so it
    /// MUST be called in call order — the guard is ordering-sensitive state.
    fn rendered_success(&mut self, tool_name: &str, response_result: &Value) -> RenderedToolResult {
        // A tool the daemon stopped at the run deadline did not succeed and did
        // not plainly fail: it was cut mid-run. The model sees that as an
        // error whose outcome is unknown, never as a result.
        if response_result.get("outcome_code").and_then(Value::as_str)
            == Some(TOOL_INTERRUPTED_OUTCOME)
        {
            return Self::rendered_error_envelope(tool_name, tool_interrupted_envelope());
        }
        // Model-visible bytes are ONLY the leaf dispatcher's `result` — never
        // the wrapping `thread` snapshot. Without this, the LLM saw the whole
        // {thread, result} envelope and the child-thread metadata leaked into
        // every tool-result message.
        let raw_bytes = serde_json::to_vec(response_result).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize dispatch result: {e}");
            Vec::new()
        });
        let raw_size = raw_bytes.len() as u64;
        let parts = match crate::tool_images::parse_envelope(response_result) {
            crate::tool_images::Envelope::NotParts => None,
            crate::tool_images::Envelope::Invalid(message) => {
                return Self::rendered_error_envelope(
                    tool_name,
                    tool_error_body(crate::tool_images::ENVELOPE_INVALID, &message),
                );
            }
            crate::tool_images::Envelope::Parts(parts) => Some(parts),
        };
        let Some(parts) = parts else {
            let guarded = self.result_guard.process_bytes(&raw_bytes);
            return RenderedToolResult {
                tool: tool_name.to_string(),
                content: String::from_utf8_lossy(&guarded.bytes).to_string(),
                raw_size,
                truncated: guarded.truncated,
                duplicate_of: guarded.duplicate_of,
                truncated_reason: guarded.truncated.then_some("result_guard"),
                status: ToolResultStatus::Ok,
                images: Vec::new(),
                dropped: Vec::new(),
            };
        };
        match crate::tool_images::admit_parts(
            parts,
            self.provider_config.image_input.as_ref(),
            &self.execution.tool_images,
            &mut self.image_run,
        ) {
            crate::tool_images::Verdict::Refused { code, message } => {
                Self::rendered_error_envelope(tool_name, tool_error_body(code, &message))
            }
            crate::tool_images::Verdict::Settled(settled) => RenderedToolResult::from_settled(
                tool_name,
                raw_size,
                settled,
                &mut self.result_guard,
            ),
        }
    }

    /// Render a preflight or dispatch failure as the standard error envelope
    /// the model sees. Error envelopes never pass the result guard.
    fn rendered_error_envelope(tool_name: &str, body: String) -> RenderedToolResult {
        let raw_size = body.len() as u64;
        let (content, truncated) = if body.len() <= MAX_ERROR_ENVELOPE_BYTES {
            (body, false)
        } else {
            let digest = lillux::sha256_hex(body.as_bytes());
            (
                serde_json::to_string(&json!({
                    "error": {
                        "code": "error_envelope_truncated",
                        "message": "The action failed with an oversized error response.",
                        "response_digest": digest,
                    }
                }))
                .expect("fixed oversized error envelope serializes"),
                true,
            )
        };
        RenderedToolResult {
            tool: tool_name.to_string(),
            raw_size,
            content,
            truncated,
            duplicate_of: None,
            truncated_reason: truncated.then_some("oversized_error"),
            status: ToolResultStatus::Error,
            images: Vec::new(),
            dropped: Vec::new(),
        }
    }

    /// Record the start (unless the record already holds it) and a
    /// `tool_not_run_after_failed_return` error result for one call that sat
    /// after a failed `directive_return`. The call is never run and takes no
    /// tool-call budget.
    async fn refuse_sibling_of_failed_return(
        &mut self,
        occurrence: &ToolOccurrence,
    ) -> Result<(), String> {
        let tc = &occurrence.call;
        if !occurrence.start_recorded {
            self.callback
                .emit_tool_dispatch(&ryeos_runtime::callback_client::ToolDispatchEvent {
                    operation_id: &occurrence.operation_id,
                    tool: &tc.name,
                    call_id: tc.id.as_deref().unwrap_or_default(),
                    item_ref: self.dispatcher.item_ref(&tc.name).as_deref(),
                    turn: occurrence.turn,
                    call_index: occurrence.call_index,
                    effective_caps: self.harness.effective_caps(),
                })
                .await
                .map_err(|e| format!("resume-critical callback emit_tool_dispatch failed: {e}"))?;
        }
        let rendered =
            Self::rendered_error_envelope(&tc.name, tool_not_run_after_failed_return_error());
        self.settle_tool_result(
            occurrence.operation_id.clone(),
            tc.id.clone().unwrap_or_default(),
            rendered,
        )
        .await
    }

    /// Emit the result event and append the transcript message for one settled
    /// tool call. Shared by the serial and batch paths so truncation flags,
    /// event shape, and message shape cannot diverge. An emission failure is
    /// resume-critical and returned as the run-fatal error string.
    async fn settle_tool_result(
        &mut self,
        operation_id: String,
        call_id: String,
        rendered: RenderedToolResult,
    ) -> Result<(), String> {
        let mut rendered = rendered;
        let mut payload = rendered.payload(&operation_id, &call_id);
        let uploads = rendered
            .images
            .iter()
            .filter_map(|image| image.upload.clone())
            .collect();
        if let Err(error) = self
            .callback
            .emit_tool_result_with_attachments(&payload, uploads)
            .await
        {
            // The node's attachment policy can be tighter than the signed
            // image limits. Its refusal appended nothing, so the result goes
            // in again without its images and the run goes on.
            if rendered.images.is_empty()
                || error
                    .downcast_ref::<ryeos_runtime::callback::AttachmentRefused>()
                    .is_none()
            {
                return Err(format!(
                    "resume-critical callback emit_tool_result failed: {error}"
                ));
            }
            tracing::warn!(%error, "node refused a tool result's attachments; dropping its images");
            for image in &rendered.images {
                self.image_run.images = self.image_run.images.saturating_sub(1);
                if image.upload.is_some() {
                    self.image_run.bytes =
                        self.image_run.bytes.saturating_sub(image.part.byte_length);
                    self.image_run.stored.remove(&image.part.attachment);
                }
            }
            rendered = rendered.without_images(crate::tool_images::IMAGE_LIMIT);
            payload = rendered.payload(&operation_id, &call_id);
            self.callback
                .emit_tool_result(&payload)
                .await
                .map_err(|e| format!("resume-critical callback emit_tool_result failed: {e}"))?;
        }
        for image in &rendered.images {
            self.image_store
                .insert(&image.part.attachment, &image.part.media_type, &image.bytes);
        }
        block_at_directive_recovery_test_cut(&self.callback, "tool_observation_recorded").await;
        self.messages.push(payload.message());
        Ok(())
    }

    fn continuation_hook_context(&self, live_context_tokens: u64, threshold_tokens: u64) -> Value {
        // One-way display floats: hook conditions compare numerically, and the
        // event payload is observability, not settlement authority.
        let remaining_spend_usd = self
            .budget
            .remaining_spend_usd()
            .map(|remaining| remaining.display_usd_lossy());
        json!({
            "event": {
                "name": "continuation",
                "reason": CONTINUATION_LOG_REASON,
                "live_context_tokens": live_context_tokens,
                "threshold_tokens": threshold_tokens,
                "messages": self.messages.clone(),
                // Hook conditions compare numerically: usage money is the
                // one-way display float, matching `budget_remaining` below.
                "usage": {
                    "input_tokens": self.budget.cost().input_tokens,
                    "output_tokens": self.budget.cost().output_tokens,
                    "total_usd": self.budget.cost().total_usd.display_usd_lossy(),
                },
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
        usd: ryeos_runtime::envelope::UsdNanos,
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

    /// Ledger admission for one physical attempt (§9.1): reserve against the
    /// verified prepared-request bound, release on a pre-issue runner signal,
    /// then durably mark issued. No provider request may start unless every
    /// step is proven; ambiguity fails closed with zero provider contact.
    async fn admit_ledger_attempt(
        &self,
        turn: u32,
        attempt_number: u32,
        prepared: &crate::provider_adapter::PreparedProviderRequest,
        authority: &ProviderAccountingAuthority,
        verified: VerifiedPreparedSpendBound,
        cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        interrupt_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<LedgerAdmission> {
        use std::sync::atomic::Ordering;

        let prepare_params = ProviderAttemptPrepareParams {
            thread_id: self.thread_id.clone(),
            turn,
            attempt_number,
            transport: match &prepared.transport {
                crate::provider_adapter::PreparedProviderTransport::RemoteHttp { method, url } => {
                    ryeos_provider_contract::PreparedTransportIntent::RemoteHttp {
                        method: method.as_str().to_owned(),
                        url: url.clone(),
                    }
                }
                crate::provider_adapter::PreparedProviderTransport::AdmittedLocalWorker {
                    execute,
                } => ryeos_provider_contract::PreparedTransportIntent::AdmittedLocalWorker {
                    execute: execute.clone(),
                },
            },
            request: ryeos_provider_contract::PreparedRequestProjection::from_coordinates(
                prepared.public_header_coordinates.clone(),
                prepared.credential_header_names.clone(),
                prepared.body_sha256.clone(),
                prepared.requested_output_ceiling,
            )?,
            verified_bound: verified,
        };
        let response = self.prepare_with_recovery(&prepare_params).await?;
        let (attempt_id, request_hash, coordinate, response_authority_digest, execution_budget_id) =
            match response {
                ProviderAttemptPrepareResponse::Replay {
                    record_hash,
                    answer,
                } => {
                    let message = answer.message;
                    return Ok(LedgerAdmission::Replayed {
                        record_hash,
                        response: crate::provider_adapter::http::AdapterResponse {
                            message,
                            usage: None,
                            finish_reason: answer.finish_reason,
                            generation_header_id: None,
                            response_id: None,
                            requested_output_tokens: prepared.requested_output_tokens,
                            observed_output: Default::default(),
                        },
                    });
                }
                ProviderAttemptPrepareResponse::RetryAdvanced { advance } => {
                    return Ok(LedgerAdmission::RetryAdvanced(advance));
                }
                ProviderAttemptPrepareResponse::RetryNotBefore { advance } => {
                    return Ok(LedgerAdmission::RetryNotBefore(advance));
                }
                ProviderAttemptPrepareResponse::ReservationDenied { .. } => {
                    return Ok(LedgerAdmission::Denied);
                }
                ProviderAttemptPrepareResponse::Reserved {
                    attempt_id,
                    request_hash,
                    coordinate,
                    authority_digest,
                    execution_budget_id,
                    ..
                } => (
                    attempt_id,
                    request_hash,
                    coordinate,
                    authority_digest,
                    execution_budget_id,
                ),
            };
        if response_authority_digest != authority.authority_digest {
            anyhow::bail!(
                "daemon reservation authority digest {} contradicts the sealed launch \
                 authority {}",
                response_authority_digest.as_str(),
                authority.authority_digest.as_str()
            );
        }
        if let Some(scope) = &self.accounting_scope
            && execution_budget_id != scope.execution_budget_id
        {
            anyhow::bail!(
                "daemon reservation execution budget `{}` contradicts the sealed launch \
                 scope `{}`",
                execution_budget_id,
                scope.execution_budget_id
            );
        }
        let ledger = LedgerAttempt {
            attempt_id,
            request_hash,
            authority_digest: response_authority_digest,
            coordinate,
        };
        // A signal in the reserve→issue window releases rather than crossing
        // the issue boundary: once issued, even a never-sent request is
        // conservatively charged the reserved maximum (§7.4).
        if cancel_flag.load(Ordering::Relaxed) {
            // The cancel is real either way: the reservation is provably
            // unissued and the terminal fence releases it. Never convert a
            // user cancel into a thread failure over release bookkeeping.
            if let Err(error) = self
                .release_ledger_attempt(&ledger, ReconciliationReason::ReleasedByRunner)
                .await
            {
                tracing::warn!(
                    attempt_id = %ledger.attempt_id,
                    %error,
                    "pre-issue release unprovable during cancel; the terminal fence releases \
                     the held reservation"
                );
            }
            return Ok(LedgerAdmission::CancelledBeforeIssue);
        }
        if interrupt_flag.load(Ordering::Relaxed) {
            if let Err(error) = self
                .release_ledger_attempt(&ledger, ReconciliationReason::ReleasedByRunner)
                .await
            {
                tracing::warn!(
                    attempt_id = %ledger.attempt_id,
                    %error,
                    "pre-issue release unprovable during interrupt; the terminal fence \
                     releases the held reservation"
                );
            }
            return Ok(LedgerAdmission::InterruptedBeforeIssue);
        }
        let issue_params = ProviderAttemptMarkIssuedParams {
            thread_id: self.thread_id.clone(),
            attempt_id: ledger.attempt_id.clone(),
            request_hash: ledger.request_hash.clone(),
            expected_state: AttemptBudgetState::Reserved,
        };
        match self.mark_issued_with_recovery(&issue_params).await? {
            AttemptBudgetState::Issued => Ok(LedgerAdmission::Admitted(ledger)),
            AttemptBudgetState::ReleasedUnissued => Ok(LedgerAdmission::ReleasedByDaemon {
                detail: format!(
                    "the daemon released reservation {} before issue (certificate expiry or \
                     credential unavailability)",
                    ledger.attempt_id
                ),
            }),
            AttemptBudgetState::Reserved => anyhow::bail!(
                "issue commit unprovable for attempt {}: the recorded state remains reserved \
                 after bounded recovery; zero provider connections (daemon reconciliation \
                 releases the held reservation)",
                ledger.attempt_id
            ),
            other => anyhow::bail!(
                "mark_issued returned unexpected state `{}` for attempt {}",
                other.as_str(),
                ledger.attempt_id
            ),
        }
    }

    /// Prepare replay-or-reservation with bounded exact retry. The daemon owns
    /// the atomic lookup/absence decision and reservation crossing, so a
    /// transport error can never be interpreted as a replay miss.
    async fn prepare_with_recovery(
        &self,
        params: &ProviderAttemptPrepareParams,
    ) -> anyhow::Result<ProviderAttemptPrepareResponse> {
        let mut last_error: Option<String> = None;
        for retry in 0..=LEDGER_RPC_RETRIES {
            let prepare_result = self.callback.provider_attempt_prepare(params).await;
            match prepare_result {
                Ok(response) => {
                    if matches!(
                        &response,
                        ProviderAttemptPrepareResponse::Reserved { replayed: true, .. }
                            | ProviderAttemptPrepareResponse::ReservationDenied {
                                replayed: true,
                                ..
                            }
                    ) {
                        tracing::warn!(
                            "provider-attempt prepare reply was lost; exact retry recovered the recorded outcome"
                        );
                    }
                    return Ok(response);
                }
                Err(error) => {
                    tracing::warn!(
                        retry,
                        error = %error,
                        "provider_attempt_prepare failed; retrying the exact same preparation"
                    );
                    last_error = Some(error.to_string());
                    if retry < LEDGER_RPC_RETRIES {
                        ledger_retry_backoff(retry).await;
                    }
                }
            }
        }
        Err(anyhow::anyhow!(
            "provider-attempt preparation unprovable after {} attempts, zero provider requests: {}",
            LEDGER_RPC_RETRIES + 1,
            last_error.unwrap_or_else(|| "no error recorded".to_string())
        ))
    }

    /// Mark issued with bounded exact-retry recovery, then a recorded-state
    /// read. Returns the proven daemon state; an unprovable state errors.
    async fn mark_issued_with_recovery(
        &self,
        params: &ProviderAttemptMarkIssuedParams,
    ) -> anyhow::Result<AttemptBudgetState> {
        let mut last_error: Option<String> = None;
        for retry in 0..=LEDGER_RPC_RETRIES {
            let mark_result = self.callback.provider_attempt_mark_issued(params).await;
            match mark_result {
                Ok(response) => {
                    if response.replayed {
                        tracing::warn!(
                            attempt_id = %params.attempt_id,
                            "issue reply was lost; exact idempotent retry recovered the \
                             recorded transition"
                        );
                    }
                    return Ok(response.state);
                }
                // A typed daemon refusal is a decision, not a transient
                // failure: retrying the same transition cannot change it.
                Err(ryeos_runtime::callback::CallbackError::ActionFailed {
                    code,
                    message,
                    retryable: false,
                }) if code != "request_failed" => {
                    anyhow::bail!(
                        "provider attempt issue refused ({code}): {message}; zero provider connections"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        retry,
                        attempt_id = %params.attempt_id,
                        error = %error,
                        "provider_attempt_mark_issued failed; retrying the exact same transition"
                    );
                    last_error = Some(error.to_string());
                    if retry < LEDGER_RPC_RETRIES {
                        ledger_retry_backoff(retry).await;
                    }
                }
            }
        }
        match self
            .callback
            .provider_attempt_get(&ProviderAttemptGetParams {
                thread_id: self.thread_id.clone(),
                attempt_id: params.attempt_id.clone(),
            })
            .await
        {
            Ok(Some(record)) if record.request_hash == params.request_hash => {
                if record.state == AttemptBudgetState::Reserved {
                    // The reservation is proven intact and unissued, so the
                    // earlier failures were transient (vault blip, anchor
                    // IO) — issuing is still legal and idempotent. One more
                    // paced attempt before declaring the issue unprovable.
                    ledger_retry_backoff(LEDGER_RPC_RETRIES + 1).await;
                    if let Ok(response) = self.callback.provider_attempt_mark_issued(params).await {
                        return Ok(response.state);
                    }
                }
                Ok(record.state)
            }
            Ok(Some(record)) => Err(anyhow::anyhow!(
                "recorded attempt {} carries request hash {} but this attempt issued {}; \
                 integrity conflict",
                params.attempt_id,
                record.request_hash,
                params.request_hash
            )),
            Ok(None) => Err(anyhow::anyhow!(
                "issue commit unprovable: attempt {} has no recorded state; zero provider \
                 connections",
                params.attempt_id
            )),
            Err(get_error) => Err(anyhow::anyhow!(
                "issue commit unprovable, zero provider connections: {}; recorded-state read \
                 also failed: {get_error}",
                last_error.unwrap_or_else(|| "no error recorded".to_string())
            )),
        }
    }

    /// Turns in a row that may end at the output limit under the signed policy.
    fn output_limit_allowance(&self) -> u32 {
        self.execution
            .on_output_limit
            .as_ref()
            .map_or(0, |policy| policy.max_consecutive)
    }

    fn validate_retry_advance(
        &self,
        advance: &ProviderRetryAdvance,
        turn: u32,
        failed_attempt_number: u32,
    ) -> anyhow::Result<()> {
        advance
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid provider retry advancement: {error}"))?;
        if advance.decision.turn != turn
            || advance.decision.failed_attempt_number != failed_attempt_number
            || advance.decision.next_attempt_number
                != failed_attempt_number
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("provider retry coordinate overflow"))?
        {
            anyhow::bail!(
                "provider retry advancement contradicts active turn {turn} attempt {failed_attempt_number}"
            );
        }
        let policy_digest = retry_policy_digest(&self.execution)?;
        if advance.decision.retry_policy_digest != policy_digest {
            anyhow::bail!(
                "provider retry advancement was admitted under a different execution policy"
            );
        }
        let expected_delay = retry_delay_for_reason(
            &advance.decision.reason,
            failed_attempt_number,
            &self.execution,
        )
        .ok_or_else(|| {
            anyhow::anyhow!("provider retry advancement is forbidden by the exact execution policy")
        })?;
        if u64::try_from(expected_delay.as_millis()).ok() != Some(advance.decision.backoff_ms) {
            anyhow::bail!("provider retry advancement backoff contradicts execution policy");
        }
        Ok(())
    }

    async fn emit_provider_retry_testimony(
        &mut self,
        advance: &ProviderRetryAdvance,
    ) -> anyhow::Result<()> {
        let digest = advance.decision_digest.as_str();
        if self.provider_retry_testimony_digests.contains(digest) {
            return Ok(());
        }
        self.callback
            .append_runtime_event(
                ryeos_runtime::RuntimeEventType::ProviderRetry,
                json!({
                    "turn": advance.decision.turn,
                    "attempt": advance.decision.failed_attempt_number,
                    "max_retries": self.execution.retries,
                    "backoff_ms": advance.decision.backoff_ms,
                    "decision_digest": digest,
                    "supersedes": retry_supersedes(&advance.decision),
                    "retry_advance": advance,
                }),
            )
            .await?;
        self.provider_retry_testimony_digests
            .insert(digest.to_string());
        Ok(())
    }

    /// Settle the issued attempt with bounded exact-retry recovery, then a
    /// recorded-state read. A terminal settlement that cannot be proven is an
    /// error — the caller must fail safe without admitting a retry.
    async fn settle_ledger_attempt(
        &self,
        ledger: &LedgerAttempt,
        spend: SpendAccounting,
        tokens: TokenAccounting,
        answer: Option<ryeos_provider_contract::ProviderCallAnswer>,
        retry: Option<ProviderRetryDecision>,
    ) -> anyhow::Result<Option<ProviderRetryAdvance>> {
        let params = ProviderAttemptSettleParams {
            thread_id: self.thread_id.clone(),
            attempt_id: ledger.attempt_id.clone(),
            request_hash: ledger.request_hash.clone(),
            authority_digest: ledger.authority_digest.clone(),
            coordinate: ledger.coordinate.clone(),
            spend,
            tokens,
            answer,
            retry,
        };
        let required_publication = params
            .answer
            .as_ref()
            .filter(|_| params.coordinate.admitted_effect_class.is_some())
            .map(|answer| {
                Ok::<_, anyhow::Error>((params.coordinate.cache_key()?, answer.digest()?))
            })
            .transpose()?;
        let mut last_error: Option<String> = None;
        for retry in 0..=LEDGER_RPC_RETRIES {
            let settlement_result = self.callback.provider_attempt_settle(&params).await;
            match settlement_result {
                Ok(response) => {
                    if !response.state.is_terminal() {
                        anyhow::bail!(
                            "settlement for attempt {} returned nonterminal state `{}`",
                            ledger.attempt_id,
                            response.state.as_str()
                        );
                    }
                    if response.replayed {
                        tracing::warn!(
                            attempt_id = %ledger.attempt_id,
                            "settlement reply was lost; exact idempotent retry recovered the \
                             recorded terminal transition"
                        );
                    }
                    if required_publication.is_some() && response.publication.is_none() {
                        anyhow::bail!(
                            "provider attempt {} reached financial terminal state without durable answer publication",
                            ledger.attempt_id
                        );
                    }
                    return Ok(response.retry_advance);
                }
                Err(error) => {
                    tracing::warn!(
                        retry,
                        attempt_id = %ledger.attempt_id,
                        error = %error,
                        "provider_attempt_settle failed; retrying the exact same settlement"
                    );
                    last_error = Some(error.to_string());
                    if retry < LEDGER_RPC_RETRIES {
                        ledger_retry_backoff(retry).await;
                    }
                }
            }
        }
        match self
            .callback
            .provider_attempt_get(&ProviderAttemptGetParams {
                thread_id: self.thread_id.clone(),
                attempt_id: ledger.attempt_id.clone(),
            })
            .await
        {
            Ok(Some(record))
                if record.state.is_terminal() && record.request_hash == params.request_hash =>
            {
                if let Some((cache_key, answer_digest)) = required_publication.as_ref() {
                    let proof = record.publication_proof.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "terminal settlement for attempt {} is unbanked",
                            ledger.attempt_id
                        )
                    })?;
                    if &proof.cache_key != cache_key || &proof.answer_digest != answer_digest {
                        anyhow::bail!(
                            "terminal settlement publication proof contradicts attempt {}",
                            ledger.attempt_id
                        );
                    }
                }
                tracing::warn!(
                    attempt_id = %ledger.attempt_id,
                    state = record.state.as_str(),
                    "settlement replies were lost; recorded terminal state proven by read"
                );
                Ok(record.retry_advance)
            }
            Ok(Some(record)) => Err(anyhow::anyhow!(
                "terminal settlement unprovable: attempt {} recorded state `{}`",
                ledger.attempt_id,
                record.state.as_str()
            )),
            Ok(None) => Err(anyhow::anyhow!(
                "terminal settlement unprovable: attempt {} has no recorded state",
                ledger.attempt_id
            )),
            Err(get_error) => Err(anyhow::anyhow!(
                "terminal settlement unprovable: {}; recorded-state read also failed: \
                 {get_error}",
                last_error.unwrap_or_else(|| "no error recorded".to_string())
            )),
        }
    }

    /// Release a reserved-but-unissued attempt with bounded exact-retry
    /// recovery, then a recorded-state read.
    async fn release_ledger_attempt(
        &self,
        ledger: &LedgerAttempt,
        reason: ReconciliationReason,
    ) -> anyhow::Result<()> {
        let params = ProviderAttemptReleaseUnissuedParams {
            thread_id: self.thread_id.clone(),
            attempt_id: ledger.attempt_id.clone(),
            request_hash: ledger.request_hash.clone(),
            reason,
        };
        let mut last_error: Option<String> = None;
        for retry in 0..=LEDGER_RPC_RETRIES {
            let release_result = self
                .callback
                .provider_attempt_release_unissued(&params)
                .await;
            match release_result {
                Ok(response) => {
                    if response.state != AttemptBudgetState::ReleasedUnissued {
                        anyhow::bail!(
                            "release for attempt {} returned unexpected state `{}`",
                            ledger.attempt_id,
                            response.state.as_str()
                        );
                    }
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(
                        retry,
                        attempt_id = %ledger.attempt_id,
                        error = %error,
                        "provider_attempt_release_unissued failed; retrying the exact release"
                    );
                    last_error = Some(error.to_string());
                    if retry < LEDGER_RPC_RETRIES {
                        ledger_retry_backoff(retry).await;
                    }
                }
            }
        }
        match self
            .callback
            .provider_attempt_get(&ProviderAttemptGetParams {
                thread_id: self.thread_id.clone(),
                attempt_id: ledger.attempt_id.clone(),
            })
            .await
        {
            Ok(Some(record)) if record.state == AttemptBudgetState::ReleasedUnissued => Ok(()),
            Ok(Some(record)) => Err(anyhow::anyhow!(
                "release unprovable: attempt {} recorded state `{}`",
                ledger.attempt_id,
                record.state.as_str()
            )),
            Ok(None) => Err(anyhow::anyhow!(
                "release unprovable: attempt {} has no recorded state",
                ledger.attempt_id
            )),
            Err(get_error) => Err(anyhow::anyhow!(
                "release unprovable: {}; recorded-state read also failed: {get_error}",
                last_error.unwrap_or_else(|| "no error recorded".to_string())
            )),
        }
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
            let cost = self.compute_cost_for_usage(Some(usage))?;
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
        // Entry boundary: quantize the provider-reported float once, rounding up.
        let usd = ryeos_runtime::envelope::UsdNanos::quantize_reported_f64_round_up(usd)
            .map_err(|error| anyhow::anyhow!("provider-reported spend is invalid: {error}"))?
            .0;
        let mut proposed_cost = self.budget.cost();
        proposed_cost
            .checked_accumulate(&provider_reported_spend_runtime_cost(usd))
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
                let replay = self
                    .callback
                    .replay_thread(
                        &self.thread_id,
                        Some(&[RuntimeEventType::ThreadUsage.as_str()]),
                    )
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

    fn compute_cost(&self, input_tokens: u64, output_tokens: u64) -> anyhow::Result<CostBreakdown> {
        let Some(ref pricing) = self.provider_config.pricing else {
            return Ok(CostBreakdown {
                usd: ryeos_runtime::envelope::UsdNanos::ZERO,
                source: PricingSource::Unpriced,
            });
        };
        if pricing.explicitly_free {
            return Ok(CostBreakdown {
                usd: ryeos_runtime::envelope::UsdNanos::ZERO,
                source: PricingSource::ExplicitlyFree,
            });
        }
        // Distinguish a per-model entry from the provider-default fallback so
        // the caller can flag the (otherwise silent) fallback exactly once.
        let (rates, source) = if let Some(p) = pricing.models.get(&self.model_name) {
            (p.clone(), PricingSource::PerModel)
        } else {
            match (pricing.input_per_million, pricing.output_per_million) {
                (Some(i), Some(o)) if i.is_zero() && o.is_zero() => {
                    return Ok(CostBreakdown {
                        usd: ryeos_runtime::envelope::UsdNanos::ZERO,
                        source: PricingSource::Unpriced,
                    });
                }
                (Some(i), Some(o)) => (
                    ryeos_directive_definition::ModelPricing {
                        input_per_million: i,
                        output_per_million: o,
                        cache_read_per_million: pricing.cache_read_per_million,
                        cache_miss_per_million: pricing.cache_miss_per_million,
                    },
                    PricingSource::ProviderDefault,
                ),
                _ => {
                    return Ok(CostBreakdown {
                        usd: ryeos_runtime::envelope::UsdNanos::ZERO,
                        source: PricingSource::Unpriced,
                    });
                }
            }
        };
        // Rate-derived spend is exact from birth: rate × tokens in checked
        // i128 nanos, rounded toward positive infinity, never float math.
        let input_cost = ryeos_runtime::envelope::UsdNanos::rate_per_million_mul_units_round_up(
            rates.input_per_million,
            input_tokens,
        )
        .map_err(|error| anyhow::anyhow!("input token pricing overflowed: {error}"))?;
        let output_cost = ryeos_runtime::envelope::UsdNanos::rate_per_million_mul_units_round_up(
            rates.output_per_million,
            output_tokens,
        )
        .map_err(|error| anyhow::anyhow!("output token pricing overflowed: {error}"))?;
        Ok(CostBreakdown {
            usd: input_cost
                .checked_add(output_cost)
                .map_err(|error| anyhow::anyhow!("turn pricing overflowed: {error}"))?,
            source,
        })
    }

    fn compute_cost_for_usage(
        &self,
        usage: Option<&crate::provider_adapter::http::TokenUsage>,
    ) -> anyhow::Result<CostBreakdown> {
        let (input_tokens, output_tokens) = usage
            .and_then(|usage| usage.complete_token_counts())
            .unwrap_or((0, 0));
        if let Some(reported) = usage.and_then(|usage| usage.reported_cost_usd) {
            // The provider float quantizes into nanos here, at its entry; a
            // non-money figure (negative, non-finite) falls through to the
            // rate-derived path, as before.
            if let Ok((usd, _rounded)) =
                ryeos_runtime::envelope::UsdNanos::quantize_reported_f64_round_up(reported)
            {
                return Ok(CostBreakdown {
                    usd,
                    source: if usd.is_zero()
                        && usage.is_some_and(|usage| usage.is_byok == Some(true))
                    {
                        PricingSource::ByokUntracked
                    } else {
                        PricingSource::ProviderReported
                    },
                });
            }
        }
        if let Some(usage) = usage
            && usage.cache_partition_of_input
            && usage.anomalies.is_empty()
            && let (Some(cache_read_tokens), Some(cache_miss_tokens)) =
                (usage.cache_read_tokens, usage.cache_miss_tokens)
            && let Some(pricing) = self.provider_config.pricing.as_ref()
            && !pricing.explicitly_free
            && let Some(rates) = pricing.for_model(&self.model_name)
            && let (Some(cache_read_rate), Some(cache_miss_rate)) =
                (rates.cache_read_per_million, rates.cache_miss_per_million)
        {
            let cache_read_cost =
                ryeos_runtime::envelope::UsdNanos::rate_per_million_mul_units_round_up(
                    cache_read_rate,
                    cache_read_tokens,
                )
                .map_err(|error| anyhow::anyhow!("cache-read token pricing overflowed: {error}"))?;
            let cache_miss_cost =
                ryeos_runtime::envelope::UsdNanos::rate_per_million_mul_units_round_up(
                    cache_miss_rate,
                    cache_miss_tokens,
                )
                .map_err(|error| anyhow::anyhow!("cache-miss token pricing overflowed: {error}"))?;
            let output_cost =
                ryeos_runtime::envelope::UsdNanos::rate_per_million_mul_units_round_up(
                    rates.output_per_million,
                    output_tokens,
                )
                .map_err(|error| anyhow::anyhow!("output token pricing overflowed: {error}"))?;
            let usd = cache_read_cost
                .checked_add(cache_miss_cost)
                .and_then(|subtotal| subtotal.checked_add(output_cost))
                .map_err(|error| anyhow::anyhow!("cache-aware turn pricing overflowed: {error}"))?;
            return Ok(CostBreakdown {
                usd,
                source: if pricing.models.contains_key(&self.model_name) {
                    PricingSource::PerModel
                } else {
                    PricingSource::ProviderDefault
                },
            });
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
            spend = format!("${}", self.harness.spend_used().to_canonical_string()),
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

/// Represent a provider-reported charge in the runtime's own cost.
///
/// `RuntimeCost::basis` describes cost ownership, not the evidence used to
/// determine the charge: `None` is cost incurred by this thread and `rollup`
/// is cost aggregated from children. The accounting ledger independently
/// records the charge source as `ChargeBasis::ProviderReported`.
fn provider_reported_spend_runtime_cost(usd: ryeos_runtime::envelope::UsdNanos) -> RuntimeCost {
    RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: usd,
        basis: None,
    }
}

/// The frame-stability check of a resume or continuation: when the provider
/// entry refuses a change of a component in which the launch frame differs
/// from the recorded one, the launch ends here with `frame_change_refused`
/// and nothing about the new frame or its inputs is recorded. `None` means the
/// launch may go on.
pub(crate) async fn refuse_frame_change(
    callback: &CallbackClient,
    thread_id: &str,
    provider: &ryeos_directive_definition::ProviderConfig,
    recorded: &ryeos_runtime::cognition::CognitionFramePayload,
    launch: &ryeos_runtime::cognition::CognitionFramePayload,
    cost: ryeos_runtime::envelope::RuntimeCost,
) -> Option<anyhow::Result<RuntimeResult>> {
    let refused = provider
        .schemas
        .as_ref()
        .and_then(|schemas| schemas.frame_stability.as_ref())
        .map_or(&[][..], |stability| stability.refuse_change_of.as_slice());
    let component = crate::launch_records::refused_frame_change(recorded, launch, refused)?;
    let error = format!(
        "frame_change_refused: the provider entry refuses a changed {} within one \
         conversation (recorded frame_seq {})",
        component.as_str(),
        recorded.frame_seq
    );
    Some(refuse_launch(callback, thread_id, &error, cost).await)
}

/// Ends a launch that is refused before the runner starts: records the
/// failure on the thread and returns the terminal result. Used where the
/// refusal is decided from the recorded chain and the launch alone.
pub(crate) async fn refuse_launch(
    callback: &CallbackClient,
    thread_id: &str,
    error: &str,
    cost: ryeos_runtime::envelope::RuntimeCost,
) -> anyhow::Result<RuntimeResult> {
    let failure = runtime_failure_payload(error, thread_id, 0, None);
    callback.emit_error(error).await?;
    callback
        .finalize_thread(TerminalCompletion {
            status: ThreadTerminalStatus::Failed,
            outcome_code: Some("failed".to_string()),
            result: None,
            error: Some(failure.clone()),
            cost: Some(
                serde_json::to_value(&cost)
                    .expect("validated directive cost must serialize for terminal settlement"),
            ),
            outputs: json!({}),
            warnings: Vec::new(),
        })
        .await?;
    Ok(RuntimeResult {
        success: false,
        status: RuntimeResultStatus::Failed,
        thread_id: thread_id.to_string(),
        result: Some(failure),
        outputs: json!({}),
        cost: Some(cost),
        warnings: Vec::new(),
    })
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
    use crate::harness::Harness;
    use ryeos_directive_definition::ModelPricing;
    use ryeos_runtime::callback_client::CallbackClient;
    use ryeos_runtime::envelope::{EnvelopeCallback, EnvelopePolicy, HardLimits};
    use std::path::PathBuf;

    fn make_callback_env() -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
        }
    }

    fn make_callback() -> CallbackClient {
        CallbackClient::new(&make_callback_env(), "T-test", "tat-test")
    }

    fn usd(canonical: &str) -> ryeos_accounting::UsdNanos {
        ryeos_accounting::UsdNanos::parse_canonical(canonical).unwrap()
    }

    fn tool_use_event(name: &str, malformed: Option<(usize, &str)>) -> StreamEvent {
        StreamEvent::ToolUse {
            id: Some(format!("call-{name}")),
            name: name.to_string(),
            arguments: json!({}),
            stream_key: format!("k-{name}"),
            argument_bytes: 0,
            malformed_args: malformed.map(|(raw_len, sha256)| crate::directive::MalformedArgs {
                sha256: sha256.to_string(),
                raw_len,
            }),
            artefact: None,
        }
    }

    fn assistant_calls(names: &[&str]) -> ProviderMessage {
        ProviderMessage::assistant(
            Some("working".to_string()),
            names
                .iter()
                .map(|name| crate::directive::ToolCall {
                    id: Some(format!("call-{name}")),
                    name: (*name).to_string(),
                    arguments: json!({}),
                })
                .collect(),
            None,
        )
    }

    #[test]
    fn malformed_calls_are_attributed_by_tool_call_ordinal_in_stream_order() {
        let sha = "ab".repeat(32);
        let events = vec![
            StreamEvent::Delta("working".to_string()),
            tool_use_event("first", None),
            tool_use_event("second", Some((17, &sha))),
            tool_use_event("third", None),
        ];
        let message = assistant_calls(&["first", "second", "third"]);
        let malformed = malformed_calls_of_turn(&events, &message).unwrap();
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0].call_index, 1);
        assert_eq!(malformed[0].raw_len, 17);
        assert_eq!(malformed[0].sha256, sha);
        assert_eq!(
            malformed_call_error(&malformed[0]),
            format!(
                "arguments were not valid JSON: 17 bytes, sha256 {sha}; resend the call with complete arguments"
            )
        );

        // A clean turn, and a replayed turn with no stream events, record nothing.
        assert!(
            malformed_calls_of_turn(&events[..2], &assistant_calls(&["first"]))
                .unwrap()
                .is_empty()
        );
        assert!(malformed_calls_of_turn(&[], &message).unwrap().is_empty());
    }

    #[test]
    fn malformed_attribution_refuses_a_stream_that_disagrees_with_the_message() {
        let sha = "cd".repeat(32);
        let events = vec![tool_use_event("only", Some((3, &sha)))];
        let error =
            malformed_calls_of_turn(&events, &assistant_calls(&["only", "extra"])).unwrap_err();
        assert!(error.to_string().contains("cannot attribute"));
    }

    fn tool_call(name: &str) -> ToolOccurrence {
        ToolOccurrence {
            call: crate::directive::ToolCall {
                id: Some(format!("call-{name}")),
                name: name.to_string(),
                arguments: serde_json::json!({}),
            },
            operation_id: "1".repeat(64),
            turn: 1,
            call_index: 0,
            start_recorded: false,
            counted: false,
            spawn_counted: false,
            malformed: None,
        }
    }

    #[test]
    fn lifecycle_bearing_batches_are_detected_anywhere_in_the_batch() {
        assert!(!batch_is_lifecycle_bearing(&[]));
        assert!(!batch_is_lifecycle_bearing(&[
            tool_call("a"),
            tool_call("b")
        ]));
        assert!(batch_is_lifecycle_bearing(&[tool_call(
            DIRECTIVE_RETURN_TOOL
        )]));
        assert!(batch_is_lifecycle_bearing(&[
            tool_call("a"),
            tool_call(DIRECTIVE_RETURN_TOOL),
            tool_call("b"),
        ]));
    }

    #[test]
    fn a_dropped_image_note_survives_truncation_and_duplicate_collapse() {
        let settled = |text: String| crate::tool_images::Settled {
            text,
            images: Vec::new(),
            dropped: vec![(1, crate::tool_images::IMAGE_OVERSIZE)],
        };
        let note = "[image 1 not used: image_oversize]";
        let mut guard = ResultGuard::new();
        // Over the guard's ceiling: the text is cut, the note is not.
        let big = "x".repeat(150 * 1024);
        let first = RenderedToolResult::from_settled("t", 1, settled(big.clone()), &mut guard);
        assert!(first.truncated);
        assert_eq!(first.truncated_reason, Some("result_guard"));
        assert!(
            first.content.ends_with(&format!("\n{note}")),
            "{}",
            first.content.len()
        );
        assert!(first.content.contains("[...truncated at 100KB]"));
        // The same text again collapses to the duplicate marker, and the note
        // still follows it.
        let second = RenderedToolResult::from_settled("t", 1, settled(big), &mut guard);
        assert!(second.duplicate_of.is_some());
        assert!(second.content.starts_with("[duplicate result omitted"));
        assert!(second.content.ends_with(&format!("\n{note}")));
        // The drop is recorded in its own field, not as a truncation.
        let small = RenderedToolResult::from_settled("t", 1, settled("hi".into()), &mut guard);
        assert!(!small.truncated);
        assert_eq!(small.truncated_reason, None);
        let payload = small.payload(&"a".repeat(64), "call-1");
        payload.validate().unwrap();
        assert_eq!(payload.guard.dropped_images.len(), 1);
        assert_eq!(payload.guard.dropped_images[0].code, "image_oversize");
        assert_eq!(payload.text(), format!("hi\n{note}"));
    }

    #[test]
    fn dropping_a_results_images_notes_each_at_its_own_position() {
        let image = |seed: u8| RenderedImage {
            part: ryeos_provider_contract::ImagePart {
                media_type: "image/png".to_string(),
                attachment: format!("{seed:02x}").repeat(32),
                byte_length: 4,
                width: Some(1),
                height: Some(1),
            },
            bytes: vec![seed],
            upload: None,
        };
        let settled = crate::tool_images::Settled {
            text: "shot".to_string(),
            images: Vec::new(),
            dropped: vec![(2, crate::tool_images::IMAGE_OVERSIZE)],
        };
        let mut rendered =
            RenderedToolResult::from_settled("t", 1, settled, &mut ResultGuard::new());
        // Positions 1 and 3 were admitted around the dropped second image.
        rendered.images = vec![image(1), image(3)];
        let dropped = rendered.without_images(crate::tool_images::IMAGE_LIMIT);
        assert!(dropped.images.is_empty());
        assert_eq!(
            dropped.content,
            "shot\n[image 1 not used: image_limit]\n[image 2 not used: image_oversize]\n[image 3 not used: image_limit]"
        );
        let payload = dropped.payload(&"a".repeat(64), "call-1");
        payload.validate().unwrap();
        assert!(payload.attachment_refs.is_empty());
        assert_eq!(payload.guard.dropped_images.len(), 3);
    }

    #[test]
    fn error_envelopes_are_bounded_and_retain_honest_metadata() {
        let small = Runner::rendered_error_envelope("t", "{\"error\":\"no\"}".to_string());
        assert!(!small.truncated);
        assert_eq!(small.status, ToolResultStatus::Error);
        assert_eq!(small.truncated_reason, None);
        assert_eq!(small.raw_size, small.content.len() as u64);

        let oversized_body = "secret".repeat(MAX_ERROR_ENVELOPE_BYTES);
        let oversized = Runner::rendered_error_envelope("t", oversized_body.clone());
        assert!(oversized.truncated);
        assert_eq!(oversized.status, ToolResultStatus::Error);
        assert_eq!(oversized.truncated_reason, Some("oversized_error"));
        let recorded = oversized.payload(&"a".repeat(64), "call-1");
        recorded.validate().unwrap();
        assert_eq!(
            recorded.guard.result_size_bytes,
            oversized_body.len() as u64
        );
        assert_eq!(oversized.raw_size, oversized_body.len() as u64);
        assert!(oversized.content.len() < MAX_ERROR_ENVELOPE_BYTES);
        assert!(!oversized.content.contains("secretsecret"));
        let parsed: Value = serde_json::from_str(&oversized.content).unwrap();
        assert_eq!(parsed["error"]["code"], "error_envelope_truncated");
        assert!(lillux::valid_hash(
            parsed["error"]["response_digest"].as_str().unwrap()
        ));
    }

    #[test]
    fn a_failed_callback_finalize_keeps_the_deadline_stop_timed_out() {
        let result = json!({"outcome": "deadline", "phase": "hard_stop"});
        let cost = || provider_reported_spend_runtime_cost(usd("0.125"));
        let mut warnings = vec!["earlier".to_string()];
        let stopped = deadline_stop_result(
            "T-1",
            result.clone(),
            cost(),
            Err("unknown variant `timed_out`".to_string()),
            &mut warnings,
        );
        assert_eq!(stopped.status, RuntimeResultStatus::TimedOut);
        assert!(!stopped.success);
        assert_eq!(stopped.result, Some(result.clone()));
        assert_eq!(warnings.len(), 2);
        assert!(warnings[1].contains("unknown variant `timed_out`"));
        // A landed finalize adds no warning and reports the same status.
        let mut clean = Vec::new();
        let landed = deadline_stop_result("T-1", result, cost(), Ok(()), &mut clean);
        assert_eq!(landed.status, RuntimeResultStatus::TimedOut);
        assert!(clean.is_empty());
    }

    #[test]
    fn a_tool_cut_at_the_deadline_reaches_the_model_as_an_interrupted_unknown_outcome() {
        let body: Value = serde_json::from_str(&tool_interrupted_envelope()).unwrap();
        assert_eq!(body["error"]["code"], "tool_interrupted_at_deadline");
        assert_eq!(body["error"]["outcome"], "unknown");
        assert_eq!(body["error"]["retryable"], false);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("side effects may have partially happened")
        );
    }

    #[test]
    fn a_tool_refused_at_the_hard_stop_is_distinct_and_not_retryable() {
        let body: Value = serde_json::from_str(&tool_deadline_elapsed_error()).unwrap();
        assert_eq!(body["error"]["code"], "tool_deadline_elapsed");
        assert_ne!(body["error"]["code"], TOOL_INTERRUPTED_OUTCOME);
        assert_eq!(body["error"]["retryable"], false);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not retried")
        );
    }

    #[test]
    fn tool_dispatch_is_refused_from_the_hard_stop_on_with_the_injected_clock() {
        use crate::run_clock::RunClock;
        // Run end 100_000 ms, reserve 10_000 ms: the hard stop is 90_000 ms.
        let mut limits = HardLimits::default();
        limits.deadline.reserve_seconds = 10;
        limits.deadline.at_ms = Some(100_000);
        let clock = RunClock::from_limits(&limits).expect("a run clock");
        assert_eq!(clock.hard_stop_at_ms(), 90_000);
        let refuse = |tool: &str, now: i64| refuse_tool_at_hard_stop(Some(&clock), tool, now);
        assert!(!refuse("marker", 89_999));
        // At the boundary and after it, the same comparison as the turn start.
        assert!(refuse("marker", 90_000));
        assert!(refuse("marker", 99_999));
        // `time_left` is local and always answers.
        assert!(!refuse(TIME_LEFT_TOOL, 95_000));
        // A run without a deadline never refuses.
        assert!(!refuse_tool_at_hard_stop(None, "marker", i64::MAX));
    }

    #[test]
    fn callback_error_envelope_does_not_expose_daemon_message() {
        let error = ryeos_runtime::callback::CallbackError::ActionFailed {
            code: "policy_denied".to_string(),
            message: "vault token and /private/path".to_string(),
            retryable: false,
        };
        let body = callback_error_envelope(&error);
        assert!(!body.contains("vault token"));
        assert!(!body.contains("/private/path"));
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], "policy_denied");
        assert_eq!(parsed["error"]["retryable"], false);
    }

    #[test]
    fn provider_reported_spend_is_own_runtime_cost_not_a_rollup_basis() {
        let cost = provider_reported_spend_runtime_cost(usd("0.125"));
        cost.validate().unwrap();
        assert_eq!(cost.input_tokens, 0);
        assert_eq!(cost.output_tokens, 0);
        assert_eq!(cost.total_usd, usd("0.125"));
        assert_eq!(cost.basis, None);
    }

    #[test]
    fn tool_call_limit_error_is_structured_and_directs_finalization() {
        let body: Value = serde_json::from_str(&tool_call_limit_error()).unwrap();
        assert_eq!(body["error"]["code"], "tool_call_limit_exceeded");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("results already gathered"))
        );
    }

    #[test]
    fn serial_limit_result_preserves_remaining_call_order() {
        let mut first = tool_call("one");
        first.call.id = Some("first".to_string());
        let mut second = tool_call("two");
        second.call.id = Some("second".to_string());
        let pending = vec![first, second];
        match state_after_serial_tool_result(
            pending,
            0,
            4,
            "directive:test",
            "root-hash",
            "effective-hash",
        ) {
            State::DispatchingTools { pending, index } => {
                assert_eq!(index, 1);
                assert_eq!(pending[index].call.id.as_deref(), Some("second"));
            }
            _ => panic!("first settled call must advance to the next call"),
        }
    }

    #[test]
    fn serial_limit_final_result_builds_generic_after_step_occurrence() {
        let mut only = tool_call("one");
        only.call.id = Some("only".to_string());
        let pending = vec![only];
        match state_after_serial_tool_result(
            pending,
            0,
            4,
            "directive:test",
            "root-hash",
            "effective-hash",
        ) {
            State::FiringHooks { occurrence, .. } => {
                assert_eq!(occurrence.owner_kind, "directive");
                assert_eq!(occurrence.event(), "after_step");
                assert_eq!(occurrence.counter_coordinate("turn"), Some(4));
            }
            _ => panic!("the final settled call must fire the after_step hooks"),
        }
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
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                explicitly_free: false,
                input_per_million: Some(usd("3")),
                output_per_million: Some(usd("15")),
                cache_read_per_million: Some(usd("0.3")),
                cache_miss_per_million: Some(usd("3")),
                models: Default::default(),
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        // Model not in the (empty) per-model table → provider-default rates.
        let cost = runner.compute_cost(1_000_000, 500_000).unwrap();
        assert_eq!(cost.usd, usd("10.5"));
        assert_eq!(cost.source, PricingSource::ProviderDefault);

        let usage = crate::provider_adapter::http::TokenUsage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            reported_cost_usd: Some(7.25),
            ..Default::default()
        };
        let reported = runner.compute_cost_for_usage(Some(&usage)).unwrap();
        assert_eq!(reported.usd, usd("7.25"));
        assert_eq!(reported.source, PricingSource::ProviderReported);

        let cache_partitioned = crate::provider_adapter::http::TokenUsage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            cache_read_tokens: Some(800_000),
            cache_miss_tokens: Some(200_000),
            cache_partition_of_input: true,
            ..Default::default()
        };
        let cached = runner
            .compute_cost_for_usage(Some(&cache_partitioned))
            .unwrap();
        assert_eq!(cached.usd, usd("8.34"));
        assert_eq!(cached.source, PricingSource::ProviderDefault);
    }

    #[test]
    fn finalize_extracts_string() {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        let result = runner.finalize(json!("Hello world"));
        assert!(result.success);
        assert_eq!(result.result.unwrap(), "Hello world");
        assert_eq!(result.status, RuntimeResultStatus::Completed);
    }

    #[test]
    fn system_prompt_prepended() {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![ProviderMessage::user("hello")],
            tools: vec![],
            system_prompt: Some("You are helpful".to_string()),
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        assert_eq!(runner.messages.len(), 2);
        assert_eq!(runner.messages[0].role(), "system");
        assert_eq!(runner.messages[1].role(), "user");
    }

    #[test]
    fn directive_outputs_stored_from_config() {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
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
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
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
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };
        let mut budget = BudgetTracker::new(usd("1"));
        budget.report(10, 5, usd("0.25")).unwrap();
        let runner = Runner::new(RunnerConfig {
            messages: vec![ProviderMessage::assistant_text("hello")],
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
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
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
    fn hook_dispatch_result_rejects_noncanonical_or_contradictory_runtime_status() {
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
                Ok(output) => assert!(
                    output
                        .failure
                        .is_some_and(|failure| failure.contains("hook_child_failed"))
                ),
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
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: Some(SamplingConfig {
                temperature: Some(0.3),
                seed: Some(42),
            }),
            reasoning: None,
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
                input_per_million: usd("0.8"),
                output_per_million: usd("4"),
                cache_read_per_million: None,
                cache_miss_per_million: None,
            },
        );
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                explicitly_free: false,
                input_per_million: Some(usd("0")), // would yield $0 if used
                output_per_million: Some(usd("0")),
                cache_read_per_million: None,
                cache_miss_per_million: None,
                models,
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let mut runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("100")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "claude-haiku-4-5".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        // 1M input + 1M output → 0.80 + 4.00 = 4.80, exact in nanos
        let cost = runner.compute_cost(1_000_000, 1_000_000).unwrap();
        assert_eq!(cost.usd, usd("4.8"));
        assert_eq!(cost.source, PricingSource::PerModel);

        runner.model_name = "missing-paid-model".to_string();
        let missing = runner.compute_cost(1_000_000, 1_000_000).unwrap();
        assert!(missing.usd.is_zero());
        assert_eq!(
            missing.source,
            PricingSource::Unpriced,
            "zero provider defaults are an untracked sentinel, not free pricing"
        );
    }

    #[test]
    fn compute_cost_falls_back_to_provider_default_when_no_model_entry() {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: Some(PricingConfig {
                explicitly_free: false,
                input_per_million: Some(usd("1")),
                output_per_million: Some(usd("5")),
                cache_read_per_million: None,
                cache_miss_per_million: None,
                models: Default::default(),
            }),
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("100")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "zen".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "unknown-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        // Falls back to provider defaults: 1M input + 1M output → 1.0 + 5.0 = 6.0
        let cost = runner.compute_cost(1_000_000, 1_000_000).unwrap();
        assert_eq!(cost.usd, usd("6"));
        assert_eq!(cost.source, PricingSource::ProviderDefault);
    }

    #[test]
    fn compute_cost_unpriced_when_no_pricing_config() {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };

        let runner = Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&make_policy(), 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: make_callback(),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "openai".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "test-model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        });

        // No pricing configured: nonzero tokens but $0 cost, flagged Unpriced so
        // the run loop can warn that spend is untracked (not free).
        let cost = runner.compute_cost(1_000_000, 1_000_000).unwrap();
        assert!(cost.usd.is_zero());
        assert_eq!(cost.source, PricingSource::Unpriced);
    }

    // ── §1 retry classification ──────────────────────────────────────

    fn retry_cfg() -> ExecutionConfig {
        ExecutionConfig {
            retries: 2,
            retry_status_codes: vec![429, 500, 502, 503],
            never_retry: vec!["401".into(), "403".into(), "404".into()],
            backoff_base_ms: 1000,
            ..ExecutionConfig::default()
        }
    }

    fn provider_with_rejections(rows: &str) -> ProviderConfig {
        serde_yaml::from_str(&format!(
            "family: chat_completions\ntransport:\n  kind: remote_http\n  base_url: \"http://localhost\"\n{rows}"
        ))
        .expect("provider config")
    }

    fn status_err_with_body(code: u16, body: &str) -> anyhow::Error {
        anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Status {
            code,
            detail: format!("provider returned {code}: {body}"),
            body: body.to_string(),
        })
    }

    const REJECTION_ROWS: &str = "request_rejections:\n  - code: provider_context_length_exceeded\n    statuses: [400, 413]\n    body_contains_any: [\"context length\", \"too long\"]\n";

    #[test]
    fn a_refused_request_the_entry_names_gets_its_code() {
        let provider = provider_with_rejections(REJECTION_ROWS);
        assert_eq!(
            request_rejection(
                &provider,
                &status_err_with_body(400, "maximum CONTEXT LENGTH exceeded")
            ),
            Some(RequestRejection {
                code: "provider_context_length_exceeded".to_string(),
                status: 400,
            })
        );
    }

    #[test]
    fn a_refusal_the_entry_does_not_name_stays_generic() {
        let provider = provider_with_rejections(REJECTION_ROWS);
        assert_eq!(
            request_rejection(&provider, &status_err_with_body(400, "malformed json")),
            None
        );
        assert_eq!(
            request_rejection(&provider, &status_err_with_body(500, "context length")),
            None
        );
        // An entry with no rows names nothing.
        let plain = provider_with_rejections("");
        assert_eq!(
            request_rejection(&plain, &status_err_with_body(400, "context length")),
            None
        );
        // A failure with no status never matches.
        let timeout = anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Timeout {
            detail: "context length".to_string(),
        });
        assert_eq!(request_rejection(&provider, &timeout), None);
    }

    #[test]
    fn a_named_refusal_carries_its_code_as_the_failure_code() {
        let error = format!(
            "{}: provider returned 400",
            "provider_context_length_exceeded"
        );
        let failure: ryeos_runtime::RuntimeFailure =
            serde_json::from_value(runtime_failure_payload(&error, "T-child", 3, None)).unwrap();
        assert_eq!(failure.code, "provider_context_length_exceeded");
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
            body: String::new(),
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
    fn retry_backoff_retries_only_connect_phase_send_failure() {
        use std::time::Duration;
        // A connect-phase failure did not establish a connection. A later send
        // failure has an uncertain provider outcome and must not be retried.
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
        assert_eq!(retry_backoff(&send_err(false), 1, &cfg), None);
        // Still bounded by the retry budget.
        assert_eq!(retry_backoff(&send_err(true), 2, &cfg), None);
    }

    fn timeout_err() -> anyhow::Error {
        anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Timeout {
            detail: "timed out".into(),
        })
    }

    fn post_connect_send_err() -> anyhow::Error {
        anyhow::Error::new(crate::provider_adapter::ProviderStreamError::Send {
            connect: false,
            detail: "send failed after connection".into(),
        })
    }

    fn mid_stream_err() -> anyhow::Error {
        anyhow::Error::new(crate::provider_adapter::ProviderStreamError::MidStream(
            Box::new(crate::provider_adapter::streaming::ProviderMidStreamError {
                live_output_events_emitted: 42,
                accepted_bytes: 128,
                accepted_output_events: 3,
                usage: None,
                generation_header_id: None,
                response_id: None,
                requested_output_tokens: Some(32_768),
                detail: "stream chunk error: operation timed out".into(),
            }),
        ))
    }

    fn unknown_policy_cfg(
        retries: u32,
        max_retries: u32,
        on: Vec<UnknownOutcomeClass>,
    ) -> ExecutionConfig {
        ExecutionConfig {
            retries,
            unknown_outcome_retry: Some(ryeos_directive_definition::UnknownOutcomeRetryPolicy {
                max_retries,
                on,
            }),
            ..retry_cfg()
        }
    }

    #[test]
    fn unknown_outcomes_are_never_retried_without_a_policy() {
        let cfg = retry_cfg();
        for attempt in 0..3 {
            assert_eq!(retry_backoff(&timeout_err(), attempt, &cfg), None);
            assert_eq!(retry_backoff(&post_connect_send_err(), attempt, &cfg), None);
            assert_eq!(retry_backoff(&mid_stream_err(), attempt, &cfg), None);
        }
    }

    #[test]
    fn unknown_outcomes_retry_only_for_named_classes_in_the_first_n_attempts() {
        use std::time::Duration;
        let all = vec![
            UnknownOutcomeClass::Timeout,
            UnknownOutcomeClass::Send,
            UnknownOutcomeClass::MidStream,
        ];
        let cfg = unknown_policy_cfg(5, 2, all);
        for err in [timeout_err(), post_connect_send_err(), mid_stream_err()] {
            assert_eq!(
                retry_backoff(&err, 0, &cfg),
                Some(Duration::from_millis(1000))
            );
            assert_eq!(
                retry_backoff(&err, 1, &cfg),
                Some(Duration::from_millis(2000))
            );
            // The third failed attempt is past max_retries.
            assert_eq!(retry_backoff(&err, 2, &cfg), None);
        }

        let only_mid = unknown_policy_cfg(5, 2, vec![UnknownOutcomeClass::MidStream]);
        assert_eq!(retry_backoff(&timeout_err(), 0, &only_mid), None);
        assert_eq!(retry_backoff(&post_connect_send_err(), 0, &only_mid), None);
        assert!(retry_backoff(&mid_stream_err(), 0, &only_mid).is_some());

        // `retries` caps first even when max_retries is not validated here.
        let capped = unknown_policy_cfg(1, 5, vec![UnknownOutcomeClass::Timeout]);
        assert!(retry_backoff(&timeout_err(), 0, &capped).is_some());
        assert_eq!(retry_backoff(&timeout_err(), 1, &capped), None);
    }

    #[test]
    fn unknown_outcome_classification_carries_the_closed_reason() {
        let cfg = unknown_policy_cfg(
            3,
            3,
            vec![
                UnknownOutcomeClass::Timeout,
                UnknownOutcomeClass::Send,
                UnknownOutcomeClass::MidStream,
            ],
        );
        assert!(matches!(
            retry_classification(&timeout_err(), 0, &cfg),
            Some((_, ProviderRetryReason::Timeout))
        ));
        assert!(matches!(
            retry_classification(&post_connect_send_err(), 0, &cfg),
            Some((
                _,
                ProviderRetryReason::Send {
                    connect_phase: false
                }
            ))
        ));
        assert!(matches!(
            retry_classification(&mid_stream_err(), 0, &cfg),
            Some((
                _,
                ProviderRetryReason::MidStream {
                    live_output_events_emitted: 42
                }
            ))
        ));
    }

    #[test]
    fn stored_unknown_outcome_decision_revalidates_only_under_the_same_policy() {
        let cfg = unknown_policy_cfg(3, 2, vec![UnknownOutcomeClass::MidStream]);
        let reason = ProviderRetryReason::MidStream {
            live_output_events_emitted: 1,
        };
        assert!(retry_delay_for_reason(&reason, 2, &cfg).is_some());
        assert!(retry_delay_for_reason(&reason, 3, &cfg).is_none());
        let narrowed = unknown_policy_cfg(3, 2, vec![UnknownOutcomeClass::Timeout]);
        assert!(retry_delay_for_reason(&reason, 1, &narrowed).is_none());
        assert_ne!(
            retry_policy_digest(&cfg).unwrap(),
            retry_policy_digest(&narrowed).unwrap()
        );
    }

    #[test]
    fn backoff_is_capped_by_backoff_max_ms() {
        use std::time::Duration;
        let cfg = ExecutionConfig {
            retries: 10,
            backoff_base_ms: 1000,
            backoff_max_ms: 5000,
            ..retry_cfg()
        };
        let delays: Vec<_> = (0..6)
            .map(|attempt| retry_backoff(&status_err(503), attempt, &cfg).unwrap())
            .collect();
        assert_eq!(
            delays,
            [1000, 2000, 4000, 5000, 5000, 5000].map(Duration::from_millis)
        );
    }

    #[test]
    fn unknown_outcome_retry_is_not_blocked_by_fail_closed_usage_but_known_retries_are() {
        for unknown in [
            ProviderRetryReason::Timeout,
            ProviderRetryReason::Send {
                connect_phase: false,
            },
            ProviderRetryReason::MidStream {
                live_output_events_emitted: 3,
            },
        ] {
            assert!(!retry_requires_attempt_usage(&unknown, true));
            assert!(!retry_requires_attempt_usage(&unknown, false));
        }
        for known in [
            ProviderRetryReason::Status { code: 503 },
            ProviderRetryReason::Send {
                connect_phase: true,
            },
        ] {
            assert!(retry_requires_attempt_usage(&known, true));
            assert!(!retry_requires_attempt_usage(&known, false));
        }
        // Without an admitted ceiling classification yields no reason at all,
        // so the unknown-outcome failure reaches the uncertainty error, never a
        // retry, whatever the budget.
        let refused = retry_cfg();
        assert!(retry_classification(&timeout_err(), 0, &refused).is_none());
        let admitted = unknown_policy_cfg(2, 2, vec![UnknownOutcomeClass::Timeout]);
        assert!(retry_classification(&timeout_err(), 0, &admitted).is_some());
    }

    #[test]
    fn statuses_504_and_529_are_retryable_by_default() {
        let cfg = ExecutionConfig::default();
        assert!(retry_backoff(&status_err(504), 0, &cfg).is_some());
        assert!(retry_backoff(&status_err(529), 0, &cfg).is_some());
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
    fn provider_outcome_uncertainty_starts_only_after_possible_contact() {
        use crate::provider_adapter::ProviderStreamError;

        let connect = anyhow::Error::new(ProviderStreamError::Send {
            connect: true,
            detail: "connect failed".into(),
        });
        let send = anyhow::Error::new(ProviderStreamError::Send {
            connect: false,
            detail: "send failed after connection".into(),
        });
        let timeout = anyhow::Error::new(ProviderStreamError::Timeout {
            detail: "response timed out".into(),
        });
        let mid_stream = anyhow::Error::new(ProviderStreamError::MidStream(Box::new(
            crate::provider_adapter::streaming::ProviderMidStreamError {
                live_output_events_emitted: 0,
                accepted_bytes: 0,
                accepted_output_events: 0,
                usage: None,
                generation_header_id: None,
                response_id: None,
                requested_output_tokens: None,
                detail: "stream cut".into(),
            },
        )));

        assert!(!provider_outcome_is_uncertain(&connect));
        assert!(provider_outcome_is_uncertain(&send));
        assert!(provider_outcome_is_uncertain(&timeout));
        assert!(provider_outcome_is_uncertain(&mid_stream));
    }

    #[test]
    fn retry_backoff_disabled_when_retries_zero() {
        let cfg = ExecutionConfig {
            retries: 0,
            ..retry_cfg()
        };
        assert_eq!(retry_backoff(&status_err(429), 0, &cfg), None);
    }

    // ── Ledger lifecycle: request hash, settlement payloads, and a
    //    fault-injectable daemon-ledger mock (RuntimeCallbackAPI) ─────

    use ryeos_accounting::{ClosedBillableDimensionSet, FinalityContract};
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, RuntimeCallbackAPI};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn digest_of(tag: &str) -> HexDigest {
        HexDigest::new(lillux::cas::sha256_hex(tag.as_bytes())).unwrap()
    }

    #[test]
    fn request_hash_is_stable_and_body_sensitive() {
        let hash = |coordinate_key: &str| {
            ryeos_accounting::rpc::provider_attempt_request_hash("T-test", 3, 2, coordinate_key)
        };
        assert_eq!(hash("bodybody"), hash("bodybody"), "same inputs, same hash");
        assert_ne!(
            hash("bodybody"),
            hash("otherbody"),
            "changed body changes hash"
        );
        assert_ne!(
            ryeos_accounting::rpc::provider_attempt_request_hash("T-test", 3, 3, "bodybody",),
            hash("bodybody"),
            "changed coordinate changes hash"
        );
    }

    fn base_ledger_authority() -> ryeos_accounting::ProviderAccountingAuthority {
        ryeos_accounting::ProviderAccountingAuthority {
            authority_digest: digest_of("seed"),
            config_hash: "test_hash".to_string(),
            config_value_digest: digest_of("cfg-value"),
            billing_principal_digest: digest_of("principal"),
            credential_authority_generation: "cred-gen-1".to_string(),
            pricing_contract_subject_digest: digest_of("subject"),
            provider_id: "route".to_string(),
            model_name: "model".to_string(),
            matched_profile: None,
            spend_bound: SpendBoundAuthority::ExplicitlyFree {
                contract_digest: digest_of("free"),
            },
            reconciliation: ChargeReconciliationAuthority::Unavailable,
        }
        .sealed()
        .unwrap()
    }

    fn paid_authority(
        reconciliation: ChargeReconciliationAuthority,
    ) -> ryeos_accounting::ProviderAccountingAuthority {
        let mut authority = base_ledger_authority();
        authority.spend_bound = SpendBoundAuthority::Paid {
            maximum: usd("0.5"),
            certificate: ryeos_accounting::SpendBoundCertificate::ProviderEnforcedChargeCap {
                request_cap_contract_digest: digest_of("cap"),
                currency: ryeos_accounting::Currency::Usd,
            },
        };
        authority.reconciliation = reconciliation;
        authority.sealed().unwrap()
    }

    fn reported_final_reconciliation() -> ChargeReconciliationAuthority {
        ChargeReconciliationAuthority::ProviderReportedFinalCharge {
            schema_digest: digest_of("schema"),
            covered_dimensions: ClosedBillableDimensionSet::new(vec![
                ryeos_accounting::BillableDimension::InputTokens,
                ryeos_accounting::BillableDimension::OutputTokens,
            ])
            .unwrap(),
            finality_contract: FinalityContract {
                final_on_response: true,
                max_reported_fraction_digits: 9,
                byok_zero_is_final: false,
            },
        }
    }

    fn tariff_reconciliation() -> ChargeReconciliationAuthority {
        ChargeReconciliationAuthority::DeterministicTariff {
            tariff: ryeos_accounting::SpendTariffDocument {
                schema_version: ryeos_accounting::SPEND_TARIFF_SCHEMA_VERSION,
                currency: ryeos_accounting::Currency::Usd,
                pricing_generation: "gen-1".to_string(),
                input_per_million: Some(ryeos_accounting::UsdNanos::parse_canonical("3").unwrap()),
                output_per_million: Some(
                    ryeos_accounting::UsdNanos::parse_canonical("15").unwrap(),
                ),
                reasoning_per_million: None,
                cache_read_per_million: None,
                cache_miss_per_million: None,
                cache_write_per_million: None,
                per_request: None,
                covered_dimensions: ClosedBillableDimensionSet::new(vec![
                    ryeos_accounting::BillableDimension::InputTokens,
                    ryeos_accounting::BillableDimension::OutputTokens,
                ])
                .unwrap(),
                expires_at_ms: None,
            },
        }
    }

    fn valid_usage() -> crate::provider_adapter::http::TokenUsage {
        crate::provider_adapter::http::TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(40),
            reasoning_tokens: Some(8),
            reported_cost_usd: Some(0.0123),
            reported_cost_usd_raw: Some("0.01230000009".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn ledger_settlement_provider_reported_final_charge() {
        let authority = paid_authority(reported_final_reconciliation());
        let usage = valid_usage();
        let (spend, tokens) = build_ledger_settlement(&authority, true, Some(&usage), None);
        assert_eq!(
            spend,
            SpendAccounting::ProviderReportedFinal {
                raw_decimal: "0.01230000009".to_string()
            }
        );
        assert_eq!(
            tokens,
            TokenAccounting::Reported {
                input_tokens: 100,
                output_tokens: 40,
                reasoning_tokens: Some(8),
            }
        );
    }

    #[test]
    fn ledger_settlement_deterministic_tariff_units() {
        let authority = paid_authority(tariff_reconciliation());
        let usage = valid_usage();
        let (spend, tokens) = build_ledger_settlement(&authority, true, Some(&usage), None);
        assert_eq!(
            spend,
            SpendAccounting::TariffUnits {
                unit_counts: vec![
                    UnitCount {
                        dimension: BillableDimension::InputTokens,
                        units: 100
                    },
                    UnitCount {
                        dimension: BillableDimension::OutputTokens,
                        units: 40
                    }
                ]
            }
        );
        assert!(matches!(tokens, TokenAccounting::Reported { .. }));

        let mut regressed = usage.clone();
        regressed
            .spend_anomalies
            .push("reported cost regressed".to_string());
        let (spend, tokens) = build_ledger_settlement(&authority, true, Some(&regressed), None);
        assert!(
            matches!(spend, SpendAccounting::TariffUnits { .. }),
            "an invalid provider-reported charge must not invalidate an independent tariff charge"
        );
        assert!(
            matches!(tokens, TokenAccounting::Reported { .. }),
            "spend validity must remain independent of complete token accounting"
        );
    }

    #[test]
    fn ledger_settlement_requires_and_preserves_cache_partition_dimensions() {
        let mut reconciliation = tariff_reconciliation();
        let ChargeReconciliationAuthority::DeterministicTariff { tariff } = &mut reconciliation
        else {
            unreachable!()
        };
        tariff.cache_read_per_million = Some(usd("0.3"));
        tariff.cache_miss_per_million = Some(usd("3"));
        tariff.input_per_million = None;
        tariff.covered_dimensions = ClosedBillableDimensionSet::new(vec![
            BillableDimension::OutputTokens,
            BillableDimension::CacheReadTokens,
            BillableDimension::CacheMissTokens,
        ])
        .unwrap();
        let authority = paid_authority(reconciliation);

        let mut usage = valid_usage();
        let (spend, _) = build_ledger_settlement(&authority, true, Some(&usage), None);
        assert!(matches!(spend, SpendAccounting::Unavailable { .. }));

        usage.cache_read_tokens = Some(60);
        usage.cache_miss_tokens = Some(40);
        let (spend, _) = build_ledger_settlement(&authority, true, Some(&usage), None);
        assert_eq!(
            spend,
            SpendAccounting::TariffUnits {
                unit_counts: vec![
                    UnitCount {
                        dimension: BillableDimension::OutputTokens,
                        units: 40,
                    },
                    UnitCount {
                        dimension: BillableDimension::CacheReadTokens,
                        units: 60,
                    },
                    UnitCount {
                        dimension: BillableDimension::CacheMissTokens,
                        units: 40,
                    },
                ],
            }
        );
    }

    #[test]
    fn ledger_settlement_explicitly_free_and_unavailable_cases() {
        // Explicitly-free settles exact zero under its contract even when
        // token accounting is independently absent.
        let free = base_ledger_authority();
        let (spend, tokens) = build_ledger_settlement(&free, true, None, None);
        assert_eq!(spend, SpendAccounting::ExplicitlyFree);
        assert_eq!(tokens, TokenAccounting::Unavailable);

        // A paid route whose stream did not complete never claims a final
        // charge — the daemon charges the reserved maximum.
        let authority = paid_authority(reported_final_reconciliation());
        let usage = valid_usage();
        let (spend, _) = build_ledger_settlement(
            &authority,
            false,
            Some(&usage),
            Some("cancelled after the issue boundary"),
        );
        assert!(
            matches!(spend, SpendAccounting::Unavailable { ref diagnostic }
            if diagnostic.contains("cancelled"))
        );

        // Malformed token usage under a deterministic tariff cannot settle
        // tariff units; tokens carry the anomaly diagnostic independently.
        let authority = paid_authority(tariff_reconciliation());
        let mut malformed = valid_usage();
        malformed
            .anomalies
            .push("output_tokens regressed".to_string());
        let (spend, tokens) = build_ledger_settlement(&authority, true, Some(&malformed), None);
        assert!(matches!(spend, SpendAccounting::Unavailable { .. }));
        assert!(matches!(tokens, TokenAccounting::Invalid { ref diagnostic }
            if diagnostic.contains("regressed")));
    }

    // ── Fault-injectable RuntimeCallbackAPI ledger mock ──────────────

    #[derive(Clone, Copy)]
    enum Script {
        Ok,
        /// Fail the first `n` calls with a transport-style error, then reply
        /// with the recorded transition (`replayed: true`), mirroring the
        /// daemon's exact idempotent recovery.
        ErrThenOk(u32),
        AlwaysErr,
        /// Every call answers with a typed, non-retryable daemon refusal.
        AlwaysRefused,
    }

    struct MethodState {
        script: Script,
        calls: u32,
    }

    impl MethodState {
        fn new(script: Script) -> Mutex<MethodState> {
            Mutex::new(MethodState { script, calls: 0 })
        }

        /// Returns `(should_error, replayed_recovery)` for the next call.
        fn next(&mut self) -> (bool, bool) {
            let call_index = self.calls;
            self.calls += 1;
            match self.script {
                Script::Ok => (false, false),
                Script::AlwaysErr | Script::AlwaysRefused => (true, false),
                Script::ErrThenOk(failures) => {
                    if call_index < failures {
                        (true, false)
                    } else {
                        (false, true)
                    }
                }
            }
        }
    }

    struct ScriptedLedger {
        authority_digest: String,
        calls: Mutex<Vec<(String, Value)>>,
        reserve: Mutex<MethodState>,
        reserve_state: AttemptBudgetState,
        issue: Mutex<MethodState>,
        issue_state: AttemptBudgetState,
        settle: Mutex<MethodState>,
        release: Mutex<MethodState>,
        /// Canned `provider_attempt_get` response (`None` → null).
        get_record: Option<Value>,
    }

    impl ScriptedLedger {
        fn new(authority_digest: &str) -> Self {
            Self {
                authority_digest: authority_digest.to_string(),
                calls: Mutex::new(Vec::new()),
                reserve: MethodState::new(Script::Ok),
                reserve_state: AttemptBudgetState::Reserved,
                issue: MethodState::new(Script::Ok),
                issue_state: AttemptBudgetState::Issued,
                settle: MethodState::new(Script::Ok),
                release: MethodState::new(Script::Ok),
                get_record: None,
            }
        }

        fn recorded(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| name == method)
                .map(|(_, params)| params.clone())
                .collect()
        }

        fn record(&self, method: &str, params: &Value) {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
        }

        fn transport_err(method: &str) -> CallbackError {
            CallbackError::Transport(anyhow::anyhow!("injected {method} transport failure"))
        }
    }

    #[async_trait::async_trait]
    impl RuntimeCallbackAPI for ScriptedLedger {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str) -> Result<Value, CallbackError> {
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
            event_type: &str,
            payload: Value,
        ) -> Result<Value, CallbackError> {
            self.record(
                "append_event",
                &json!({"event_type": event_type, "payload": payload}),
            );
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _: &str,
            _: Vec<Value>,
            _: ryeos_runtime::callback::EventAppendOptions,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
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

        async fn provider_attempt_prepare(
            &self,
            _thread_id: &str,
            params: Value,
        ) -> Result<Value, CallbackError> {
            self.record("reserve", &params);
            let (should_error, replayed) = self.reserve.lock().unwrap().next();
            if should_error {
                return Err(Self::transport_err("reserve"));
            }
            let outcome = if self.reserve_state == AttemptBudgetState::ReservationDenied {
                "reservation_denied"
            } else {
                "reserved"
            };
            let mut response = json!({
                "outcome": outcome,
                "attempt_id": "A-daemon-1",
                "request_hash": "rh-1",
                "coordinate": test_coordinate_value(&self.authority_digest),
                "authority_digest": self.authority_digest,
                "execution_budget_id": "B-exec-1",
                "replayed": replayed,
            });
            if outcome == "reserved" {
                response["reserved"] = json!("0.5");
            }
            Ok(response)
        }

        async fn provider_attempt_mark_issued(
            &self,
            _thread_id: &str,
            params: Value,
        ) -> Result<Value, CallbackError> {
            self.record("mark_issued", &params);
            let refused = matches!(self.issue.lock().unwrap().script, Script::AlwaysRefused);
            let (should_error, replayed) = self.issue.lock().unwrap().next();
            if should_error && refused {
                return Err(CallbackError::ActionFailed {
                    code: "capability_rejected".to_string(),
                    message: "recipient rule no longer lists this principal".to_string(),
                    retryable: false,
                });
            }
            if should_error {
                return Err(Self::transport_err("mark_issued"));
            }
            Ok(json!({ "state": self.issue_state, "replayed": replayed }))
        }

        async fn provider_attempt_settle(
            &self,
            _thread_id: &str,
            params: Value,
        ) -> Result<Value, CallbackError> {
            self.record("settle", &params);
            let (should_error, replayed) = self.settle.lock().unwrap().next();
            if should_error {
                return Err(Self::transport_err("settle"));
            }
            Ok(json!({
                "state": "reconciled",
                "budget_charge": "0.1",
                "released": "0.4",
                "charge_basis": "provider_reported",
                "replayed": replayed,
            }))
        }

        async fn provider_attempt_release_unissued(
            &self,
            _thread_id: &str,
            params: Value,
        ) -> Result<Value, CallbackError> {
            self.record("release_unissued", &params);
            let (should_error, replayed) = self.release.lock().unwrap().next();
            if should_error {
                return Err(Self::transport_err("release_unissued"));
            }
            Ok(json!({ "state": "released_unissued", "replayed": replayed }))
        }

        async fn provider_attempt_get(
            &self,
            _thread_id: &str,
            params: Value,
        ) -> Result<Value, CallbackError> {
            self.record("get", &params);
            Ok(self.get_record.clone().unwrap_or(Value::Null))
        }
    }

    fn ledger_runner(mock: Arc<ScriptedLedger>) -> Runner {
        ledger_runner_with_policy(mock, make_policy())
    }

    fn ledger_runner_with_policy(mock: Arc<ScriptedLedger>, policy: EnvelopePolicy) -> Runner {
        let provider = ProviderConfig {
            category: None,
            family: ProtocolFamily::ChatCompletions,
            transport: ProviderTransportConfig::RemoteHttp {
                base_url: "http://localhost".to_string(),
            },
            auth: Default::default(),
            headers: Default::default(),
            schemas: None,
            pricing: None,
            extra: Default::default(),
            body_template: None,
            body_extra: None,
            spend_authority: None,
            image_input: None,
            cache_marks: None,
            request_rejections: None,
            profiles: vec![],
        };
        Runner::new(RunnerConfig {
            messages: vec![],
            tools: vec![],
            system_prompt: None,
            harness: Harness::new(&policy, 0, None),
            continuation: ContinuationConfig::Flag(true),
            budget: BudgetTracker::new(usd("1")),
            callback: CallbackClient::from_inner(
                mock as Arc<dyn RuntimeCallbackAPI>,
                "T-test",
                "/project",
                "tat-test",
            ),
            context_window: 200_000,
            context_threshold_ratio: 0.9,
            provider_config: provider,
            provider_id: "route".to_string(),
            matched_profile: None,
            config_hash: "test_hash".to_string(),
            provider_effects_recorded: false,
            financial_authority: None,
            accounting_scope: None,
            execution: ExecutionConfig::default(),
            model_name: "model".to_string(),
            thread_id: "T-test".to_string(),
            definition_ref: "directive:test/fixture".to_string(),
            root_raw_content_digest: "root-raw-content-digest".to_string(),
            effective_definition_digest: "effective-definition-digest".to_string(),
            hooks: vec![],
            outputs: None,
            return_nudge: ReturnNudge::default(),
            sampling: None,
            reasoning: None,
        })
    }

    fn test_prepared() -> crate::provider_adapter::PreparedProviderRequest {
        let body_bytes = br#"{"messages":[]}"#.to_vec();
        let body_sha256 = lillux::cas::sha256_hex(&body_bytes);
        crate::provider_adapter::PreparedProviderRequest {
            transport: crate::provider_adapter::PreparedProviderTransport::RemoteHttp {
                method: reqwest::Method::POST,
                url: "http://localhost/chat/completions".to_string(),
            },
            header_names: vec![],
            public_header_coordinates: vec![],
            credential_header_names: vec![],
            body_sha256: body_sha256.clone(),
            body_bytes,
            requested_output_tokens: Some(1_024),
            requested_output_ceiling: 1_024,
            credential: None,
            headers: vec![],
            request_digest: ryeos_provider_contract::PreparedRequestProjection::from_coordinates(
                vec![],
                vec![],
                body_sha256,
                1_024,
            )
            .unwrap()
            .digest()
            .unwrap(),
            request_metrics: Default::default(),
        }
    }

    fn test_verified(
        authority: &ryeos_accounting::ProviderAccountingAuthority,
        prepared: &crate::provider_adapter::PreparedProviderRequest,
    ) -> VerifiedPreparedSpendBound {
        VerifiedPreparedSpendBound {
            prepared_request_digest: HexDigest::new(prepared.request_digest.clone()).unwrap(),
            authority_digest: authority.authority_digest.clone(),
            maximum: ryeos_accounting::UsdNanos::ZERO,
            commitments: ryeos_accounting::SpendBoundCommitments::ExplicitlyFree {
                contract_digest: digest_of("free"),
            },
            verifier_contract_digest: digest_of("verifier"),
        }
    }

    fn test_coordinate_value(authority_digest: &str) -> Value {
        json!({
            "outer_effective_definition_digest": "11".repeat(32),
            "transport": {"kind": "remote_http", "method": "POST", "url": "https://provider.invalid/v1"},
            "provider_family": "chat_completions",
            "provider_config_hash": "cfg",
            "provider_config_value_digest": "22".repeat(32),
            "provider_id": "route",
            "model_name": "model",
            "public_headers": [],
            "credential_header_names": [],
            "body_sha256": "33".repeat(32),
            "requested_output_ceiling": 1024,
            "credential_binding_hmac": "44".repeat(32),
            "credential_authority_generation": "generation",
            "authority_digest": authority_digest,
            "admitted_effect_class": null
        })
    }

    fn test_request_hash(
        _authority: &ryeos_accounting::ProviderAccountingAuthority,
        _prepared: &crate::provider_adapter::PreparedProviderRequest,
    ) -> String {
        "rh-1".to_string()
    }

    fn unset_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// A policy whose run ends `end_in_ms` from now, with a 10 s reserve and a
    /// 20 s wrap-up phase: the hard stop is 10 s before the end and the
    /// wrap-up notice starts 20 s before that.
    fn deadline_policy(end_in_ms: i64) -> EnvelopePolicy {
        let mut limits = HardLimits::default();
        limits.deadline.reserve_seconds = 10;
        limits.deadline.at_ms = Some(lillux::time::timestamp_millis() + end_in_ms);
        limits
            .runtime
            .insert(crate::run_clock::WRAP_UP_SECONDS_LIMIT.to_string(), 20);
        EnvelopePolicy {
            effective_caps: vec![],
            hard_limits: limits,
        }
    }

    #[tokio::test]
    async fn the_wrap_up_notice_is_recorded_once_and_only_in_its_phase() {
        // 100 s left: the wrap-up window opens in 70 s, so nothing is said yet.
        let mock = Arc::new(ScriptedLedger::new("authority"));
        let mut runner = ledger_runner_with_policy(mock.clone(), deadline_policy(100_000));
        assert!(matches!(
            runner.run_deadline_boundary().await,
            Ok(RunDeadlineBoundary::Proceed)
        ));
        assert!(mock.recorded("append_event").is_empty());

        // 25 s left: the hard stop is 15 s away and the wrap-up window opened 5 s ago.
        let mock = Arc::new(ScriptedLedger::new("authority"));
        let mut runner = ledger_runner_with_policy(mock.clone(), deadline_policy(25_000));
        for _ in 0..3 {
            assert!(matches!(
                runner.run_deadline_boundary().await,
                Ok(RunDeadlineBoundary::Proceed)
            ));
        }
        let appended = mock.recorded("append_event");
        assert_eq!(appended.len(), 1, "exactly one durable notice");
        assert_eq!(appended[0]["event_type"], "cognition_in");
        assert_eq!(appended[0]["payload"]["origin"], "harness");
        assert_eq!(appended[0]["payload"]["kind"], "deadline_wrap_up");
        assert!(
            appended[0]["payload"].get("pinned").is_none(),
            "the notice is a recent input, not pinned to the front of a successor"
        );
        assert_eq!(runner.messages.len(), 1);
        assert_eq!(runner.messages[0].role(), "user");
        assert!(
            runner.messages[0]
                .text()
                .unwrap()
                .starts_with("Deadline notice")
        );
    }

    #[tokio::test]
    async fn a_recovered_run_does_not_repeat_the_wrap_up_notice() {
        let mock = Arc::new(ScriptedLedger::new("authority"));
        let mut runner = ledger_runner_with_policy(mock.clone(), deadline_policy(25_000));
        runner.wrap_up_delivered = true;
        assert!(matches!(
            runner.run_deadline_boundary().await,
            Ok(RunDeadlineBoundary::Proceed)
        ));
        assert!(mock.recorded("append_event").is_empty());
        assert!(runner.messages.is_empty());
    }

    #[tokio::test]
    async fn the_hard_stop_and_a_run_without_a_deadline() {
        let mock = Arc::new(ScriptedLedger::new("authority"));
        // The end is 5 s away and the reserve is 10 s: the hard stop passed.
        let mut runner = ledger_runner_with_policy(mock.clone(), deadline_policy(5_000));
        assert!(matches!(
            runner.run_deadline_boundary().await,
            Ok(RunDeadlineBoundary::HardStop)
        ));
        assert!(mock.recorded("append_event").is_empty());

        let mut runner = ledger_runner(mock.clone());
        assert!(matches!(
            runner.run_deadline_boundary().await,
            Ok(RunDeadlineBoundary::Proceed)
        ));
    }

    #[tokio::test]
    async fn the_interrupt_timer_sets_the_flag_at_the_hard_stop_and_stops_with_its_owner() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut limits = HardLimits::default();
        limits.deadline.at_ms = Some(lillux::time::timestamp_millis() + 150);
        let clock = RunClock::from_limits(&limits).unwrap();
        let timer = DeadlineInterruptTimer::start(clock, flag.clone());
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        drop(timer);
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !flag.load(std::sync::atomic::Ordering::Relaxed),
            "a dropped timer stops setting the flag"
        );
    }

    #[tokio::test]
    async fn the_time_left_answer_follows_the_runner_clock() {
        let mock = Arc::new(ScriptedLedger::new("authority"));
        let runner = ledger_runner_with_policy(mock.clone(), deadline_policy(25_000));
        let report = runner.time_left_report();
        assert_eq!(report["phase"], "wrap_up");
        let remaining = report["remaining_ms"].as_u64().unwrap();
        assert!((14_000..=15_000).contains(&remaining), "{remaining}");
        assert_eq!(
            report["hard_stop_at_ms"].as_i64().unwrap() - report["now_ms"].as_i64().unwrap(),
            i64::try_from(remaining).unwrap()
        );

        let no_deadline = ledger_runner(mock).time_left_report();
        assert!(no_deadline["phase"].is_null());
        assert!(no_deadline["now_ms"].is_i64());
    }

    #[test]
    fn the_wrap_up_text_names_the_time_and_the_way_to_finish() {
        let with_outputs = wrap_up_message(12_400, true);
        assert!(with_outputs.contains("13 seconds"));
        assert!(with_outputs.contains("directive_return"));
        let without = wrap_up_message(5_000, false);
        assert!(without.contains("5 seconds"));
        assert!(!without.contains("directive_return"));
    }

    #[tokio::test]
    async fn ledger_admission_reserves_then_issues_once() {
        let authority = base_ledger_authority();
        let mock = Arc::new(ScriptedLedger::new(authority.authority_digest.as_str()));
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        match admission {
            LedgerAdmission::Admitted(ledger) => {
                assert_eq!(ledger.attempt_id, "A-daemon-1");
                assert_eq!(ledger.authority_digest, authority.authority_digest);
            }
            _ => panic!("expected Admitted"),
        }
        assert_eq!(mock.recorded("reserve").len(), 1);
        assert_eq!(mock.recorded("mark_issued").len(), 1);
        assert!(mock.recorded("release_unissued").is_empty());
    }

    #[tokio::test]
    async fn reservation_denied_makes_no_further_calls() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.reserve_state = AttemptBudgetState::ReservationDenied;
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(admission, LedgerAdmission::Denied));
        assert_eq!(mock.recorded("reserve").len(), 1);
        assert!(
            mock.recorded("mark_issued").is_empty(),
            "a denied reservation must never reach the issue boundary"
        );
    }

    #[tokio::test]
    async fn lost_reserve_reply_recovers_with_one_exact_retry() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.reserve = MethodState::new(Script::ErrThenOk(1));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(admission, LedgerAdmission::Admitted(_)));
        let reserves = mock.recorded("reserve");
        assert_eq!(reserves.len(), 2, "one failure, one exact retry");
        assert_eq!(
            reserves[0]["request_hash"], reserves[1]["request_hash"],
            "the retry must carry the byte-identical reservation intent"
        );
        assert_eq!(
            mock.recorded("mark_issued").len(),
            1,
            "a recovered reservation is a single reservation"
        );
    }

    #[tokio::test]
    async fn unprovable_reserve_fails_without_issue() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.reserve = MethodState::new(Script::AlwaysErr);
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let error = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unprovable"),
            "expected unprovable reservation, got: {error:#}"
        );
        assert_eq!(
            mock.recorded("reserve").len(),
            LEDGER_RPC_RETRIES + 1,
            "bounded exact retries only"
        );
        assert!(mock.recorded("mark_issued").is_empty());
    }

    #[tokio::test]
    async fn cancel_before_issue_releases_unissued() {
        let authority = base_ledger_authority();
        let mock = Arc::new(ScriptedLedger::new(authority.authority_digest.as_str()));
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);
        let cancelled = Arc::new(AtomicBool::new(true));

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &cancelled,
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(admission, LedgerAdmission::CancelledBeforeIssue));
        assert_eq!(mock.recorded("reserve").len(), 1);
        let releases = mock.recorded("release_unissued");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["reason"], "released_by_runner");
        assert!(
            mock.recorded("mark_issued").is_empty(),
            "a cancelled attempt must never cross the issue boundary"
        );
    }

    #[tokio::test]
    async fn daemon_release_at_issue_fails_closed() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.issue_state = AttemptBudgetState::ReleasedUnissued;
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(
            admission,
            LedgerAdmission::ReleasedByDaemon { .. }
        ));
    }

    #[tokio::test]
    async fn lost_issue_reply_recovers_with_one_exact_retry() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.issue = MethodState::new(Script::ErrThenOk(1));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let prepared = test_prepared();
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(admission, LedgerAdmission::Admitted(_)));
        let issues = mock.recorded("mark_issued");
        assert_eq!(issues.len(), 2, "one failure, one exact retry");
        assert_eq!(issues[0], issues[1], "issue retry must be byte-identical");
    }

    #[tokio::test]
    async fn lost_issue_replies_recover_from_recorded_issued_state() {
        let authority = base_ledger_authority();
        let prepared = test_prepared();
        let request_hash = test_request_hash(&authority, &prepared);
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.issue = MethodState::new(Script::AlwaysErr);
        mock.get_record = Some(json!({
            "attempt_id": "A-daemon-1",
            "turn": 1,
            "attempt_number": 1,
            "state": "issued",
            "request_hash": request_hash,
            "authority_digest": authority.authority_digest.clone(),
            "reserved": "0.5",
        }));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let verified = test_verified(&authority, &prepared);

        let admission = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap();
        assert!(matches!(admission, LedgerAdmission::Admitted(_)));
        assert_eq!(mock.recorded("mark_issued").len(), LEDGER_RPC_RETRIES + 1);
        assert_eq!(mock.recorded("get").len(), 1);
    }

    #[tokio::test]
    async fn typed_issue_refusal_ends_the_issue_without_retry() {
        let authority = base_ledger_authority();
        let prepared = test_prepared();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.issue = MethodState::new(Script::AlwaysRefused);
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let verified = test_verified(&authority, &prepared);

        let error = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .expect_err("a typed refusal at issue must end the attempt");
        assert!(
            format!("{error:#}").contains("provider attempt issue refused (capability_rejected)"),
            "{error:#}"
        );
        assert_eq!(mock.recorded("mark_issued").len(), 1);
        assert!(mock.recorded("get").is_empty());
        assert!(mock.recorded("release_unissued").is_empty());
    }

    #[tokio::test]
    async fn unprovable_issue_fails_before_provider_contact() {
        let authority = base_ledger_authority();
        let prepared = test_prepared();
        let request_hash = test_request_hash(&authority, &prepared);
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.issue = MethodState::new(Script::AlwaysErr);
        mock.get_record = Some(json!({
            "attempt_id": "A-daemon-1",
            "turn": 1,
            "attempt_number": 1,
            "state": "reserved",
            "request_hash": request_hash,
            "authority_digest": authority.authority_digest.clone(),
            "reserved": "0.5",
        }));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());
        let verified = test_verified(&authority, &prepared);

        let error = runner
            .admit_ledger_attempt(
                1,
                1,
                &prepared,
                &authority,
                verified,
                &unset_flag(),
                &unset_flag(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unprovable"));
        assert_eq!(
            mock.recorded("mark_issued").len(),
            LEDGER_RPC_RETRIES + 2,
            "a proven still-reserved attempt gets one final paced issue retry"
        );
        assert_eq!(mock.recorded("get").len(), 1);
    }

    fn test_ledger_attempt(
        authority: &ryeos_accounting::ProviderAccountingAuthority,
    ) -> LedgerAttempt {
        LedgerAttempt {
            attempt_id: "A-daemon-1".to_string(),
            request_hash: "rh-1".to_string(),
            authority_digest: authority.authority_digest.clone(),
            coordinate: serde_json::from_value(test_coordinate_value(
                authority.authority_digest.as_str(),
            ))
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn lost_settle_reply_recovers_with_exact_retry() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.settle = MethodState::new(Script::ErrThenOk(1));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());

        runner
            .settle_ledger_attempt(
                &test_ledger_attempt(&authority),
                SpendAccounting::ExplicitlyFree,
                TokenAccounting::Unavailable,
                None,
                None,
            )
            .await
            .unwrap();
        let settles = mock.recorded("settle");
        assert_eq!(settles.len(), 2, "one failure, one exact retry");
        assert_eq!(settles[0], settles[1], "retry is byte-identical");
    }

    #[tokio::test]
    async fn settle_recovered_by_state_read_when_replies_lost() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.settle = MethodState::new(Script::AlwaysErr);
        mock.get_record = Some(json!({
            "attempt_id": "A-daemon-1",
            "turn": 1,
            "attempt_number": 1,
            "state": "charged_reserved_maximum",
            "request_hash": "rh-1",
            "authority_digest": authority.authority_digest,
            "reserved": "0.5",
        }));
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());

        runner
            .settle_ledger_attempt(
                &test_ledger_attempt(&authority),
                SpendAccounting::Unavailable {
                    diagnostic: "test".to_string(),
                },
                TokenAccounting::Unavailable,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(mock.recorded("settle").len(), LEDGER_RPC_RETRIES + 1);
        assert_eq!(
            mock.recorded("get").len(),
            1,
            "recorded state proven by read"
        );
    }

    #[tokio::test]
    async fn unprovable_settle_is_a_hard_error() {
        let authority = base_ledger_authority();
        let mut mock = ScriptedLedger::new(authority.authority_digest.as_str());
        mock.settle = MethodState::new(Script::AlwaysErr);
        // get_record stays None → the recorded state cannot be proven.
        let mock = Arc::new(mock);
        let runner = ledger_runner(mock.clone());

        let error = runner
            .settle_ledger_attempt(
                &test_ledger_attempt(&authority),
                SpendAccounting::ExplicitlyFree,
                TokenAccounting::Unavailable,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unprovable"),
            "expected unprovable settlement, got: {error:#}"
        );
    }
}

#[cfg(test)]
mod frame_refusal_tests {
    use super::*;
    use crate::launch_records::{FrameSource, build_frame};
    use ryeos_directive_definition::{ProviderConfig, ReasoningConfig, ReasoningMode};
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, RuntimeCallbackAPI};
    use std::sync::{Arc, Mutex};

    /// Records what a refused launch writes: events and the terminal
    /// completion.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(String, Value)>>,
        finalized: Mutex<Vec<TerminalCompletion>>,
    }

    #[async_trait::async_trait]
    impl RuntimeCallbackAPI for Recorder {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            completion: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            self.finalized.lock().unwrap().push(completion);
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
            event_type: &str,
            payload: Value,
        ) -> Result<Value, CallbackError> {
            self.events
                .lock()
                .unwrap()
                .push((event_type.to_string(), payload));
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _: &str,
            _: Vec<Value>,
            _: ryeos_runtime::callback::EventAppendOptions,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
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

    fn provider(refuse: Option<Value>) -> ProviderConfig {
        let mut schemas = json!({
            "streaming": {"mode": "delta_merge"},
            "output_limit": {"path": "max_tokens", "semantics": "provider_native_output_tokens"},
        });
        if let Some(refuse) = refuse {
            schemas["frame_stability"] = json!({"refuse_change_of": refuse});
        }
        serde_json::from_value(json!({
            "family": "chat_completions",
            "transport": {"kind": "remote_http", "base_url": "https://example.invalid"},
            "schemas": schemas,
            "pricing": {"explicitly_free": true},
            "body_template": {"model": "{model}", "messages": "{messages}", "stream": "{stream}"}
        }))
        .expect("provider fixture")
    }

    fn frame(effort: &str) -> ryeos_runtime::cognition::CognitionFramePayload {
        build_frame(
            1,
            &FrameSource {
                system_prompt: Some("sys"),
                inventory: &[],
                model: "m",
                reasoning: Some(&ReasoningConfig {
                    mode: ReasoningMode::Enabled,
                    effort: Some(effort.to_string()),
                }),
                sampling: None,
                max_output_tokens: 0,
                provider_config_hash: &"ab".repeat(32),
            },
        )
        .unwrap()
        .payload
    }

    fn cost() -> ryeos_runtime::envelope::RuntimeCost {
        BudgetTracker::new(ryeos_accounting::UsdNanos::parse_canonical("1").unwrap()).cost()
    }

    async fn run(refuse: Option<Value>) -> (Option<anyhow::Result<RuntimeResult>>, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let callback = CallbackClient::from_inner(recorder.clone(), "T-refuse", "/tmp/p", "tat");
        let result = refuse_frame_change(
            &callback,
            "T-refuse",
            &provider(refuse),
            &frame("high"),
            &frame("low"),
            cost(),
        )
        .await;
        (result, recorder)
    }

    #[tokio::test]
    async fn a_refused_component_ends_the_launch_with_the_stable_code_and_one_failure_record() {
        let (result, recorder) = run(Some(json!(["reasoning"]))).await;
        let result = result.expect("refused").expect("terminal result");
        assert!(!result.success);
        assert_eq!(result.status, RuntimeResultStatus::Failed);
        let failure: ryeos_runtime::RuntimeFailure =
            serde_json::from_value(result.result.expect("failure payload")).unwrap();
        assert_eq!(failure.code, "frame_change_refused");
        assert!(failure.summary.contains("reasoning"), "{}", failure.summary);
        assert!(
            failure.summary.contains("frame_seq 1"),
            "{}",
            failure.summary
        );

        let events = recorder.events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "thread_failed");
        assert!(
            events[0].1["message"]
                .as_str()
                .unwrap()
                .starts_with("frame_change_refused")
        );
        let finalized = recorder.finalized.lock().unwrap();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].status, ThreadTerminalStatus::Failed);
    }

    #[tokio::test]
    async fn a_change_the_entry_does_not_refuse_records_nothing_and_lets_the_launch_go_on() {
        for refuse in [None, Some(json!(["system", "tools"]))] {
            let (result, recorder) = run(refuse).await;
            assert!(result.is_none());
            assert!(recorder.events.lock().unwrap().is_empty());
            assert!(recorder.finalized.lock().unwrap().is_empty());
        }
    }
}
