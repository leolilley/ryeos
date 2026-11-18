use std::collections::HashMap;

use ryeos_directive_definition::{ContinuationConfig, OutputSpec, ReturnNudge};
use ryeos_provider_contract::{Part, Role, ToolResultStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Provider-facing lifecycle tool intercepted by the directive runtime.
///
/// This is not a dispatchable RyeOS item. Keep its wire name centralized so
/// tool exposure, lifecycle detection, event recording, and replay cannot
/// silently diverge.
pub(crate) const DIRECTIVE_RETURN_TOOL: &str = "directive_return";
/// The built-in read-only tool that reports the run deadline's clock. The
/// runner answers it in process; it never reaches the daemon.
pub(crate) const TIME_LEFT_TOOL: &str = "time_left";

/// Directive-runtime continuation *behavior* config, loaded by the runtime from
/// `ryeos-runtime/continuation` (defaults if absent) — the same mechanism as
/// `ryeos-runtime/execution`. These govern the runtime's continuation boundary
/// and resume, so they live where the runtime loads its own config, NOT in the
/// executor's hard-limit resolution (which would force threading them back
/// through the launch envelope). Defaults are the one canonical place for these
/// values; `.ai/config/ryeos-runtime/continuation` overrides them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationRuntimeConfig {
    /// Kind-schema metadata header round-trip (parity with other typed configs).
    #[serde(default)]
    pub category: Option<String>,
    /// Fraction of the model context window at which the continuation boundary
    /// is reached.
    #[serde(default = "default_context_threshold_ratio")]
    pub context_threshold_ratio: f64,
    /// Default recent turns carried into a self-continue successor.
    #[serde(default = "default_carry_turns")]
    pub carry_turns_default: u32,
    /// Upper bound a directive's declared `carry_turns` is clamped to.
    #[serde(default = "default_carry_turns_cap")]
    pub carry_turns_cap: u32,
}

fn default_context_threshold_ratio() -> f64 {
    0.9
}
fn default_carry_turns() -> u32 {
    8
}
fn default_carry_turns_cap() -> u32 {
    32
}

impl Default for ContinuationRuntimeConfig {
    fn default() -> Self {
        Self {
            category: None,
            context_threshold_ratio: default_context_threshold_ratio(),
            carry_turns_default: default_carry_turns(),
            carry_turns_cap: default_carry_turns_cap(),
        }
    }
}

impl ContinuationRuntimeConfig {
    /// Resolve a directive's declared `carry_turns`: fall back to the configured
    /// default, then clamp to the configured cap.
    pub fn resolve_carry_turns(&self, declared: Option<u32>) -> u32 {
        declared
            .unwrap_or(self.carry_turns_default)
            .min(self.carry_turns_cap)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSchema {
    pub name: String,
    pub item_id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// Kind-schema metadata header (e.g. `"ryeos-runtime"`) surfaced so
    /// `deny_unknown_fields` keeps holding the line. Not consumed by
    /// the runtime.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_retry_status_codes")]
    pub retry_status_codes: Vec<u16>,
    #[serde(default)]
    pub never_retry: Vec<String>,
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// Provider-native output-token ceiling requested for a single model turn.
    /// Providers apply their own tokenizer; RyeOS does not treat final usage
    /// metadata as evidence that locally streamed output crossed this value.
    /// `0` disables the runtime ceiling while preserving any limit authored in
    /// the provider's signed request template/body defaults.
    #[serde(default = "default_max_provider_output_tokens_per_turn")]
    pub max_provider_output_tokens_per_turn: u64,
    /// Exact UTF-8 byte backstop for semantic output accepted from one provider
    /// stream (text, emitted reasoning, and tool arguments). This is an
    /// independent limit, not a token-to-byte conversion. `0` disables it.
    #[serde(default = "default_max_stream_output_bytes_per_turn")]
    pub max_stream_output_bytes_per_turn: u64,
    /// Maximum bytes buffered for one logical SSE event before its delimiter.
    /// This is a framing/memory-safety bound, not generated-output accounting.
    /// `0` disables the framing bound.
    #[serde(default = "default_max_provider_stream_frame_bytes")]
    pub max_provider_stream_frame_bytes: u64,
    #[serde(default)]
    pub tool_preload: bool,
    /// Maximum tool calls from ONE assistant message dispatched concurrently.
    /// Independent calls run through a bounded window; results fold back in
    /// call order, so the provider transcript is identical to a serial run,
    /// while the braid records the real shape: all of a batch's
    /// `tool_call_start` intents first, then results — consumers pair by
    /// `call_id`, never by adjacency. `1` serializes dispatch through the
    /// same path. Batches carrying `directive_return` always run serially.
    ///
    /// Range 1..=16. Each in-flight dispatch holds one dedicated daemon UDS
    /// connection for the child's whole duration against the node-wide
    /// connection budget (`MAX_UDS_CONNECTIONS`); raise this only with that
    /// budget and the fleet's concurrent directive count in mind.
    #[serde(default = "default_tool_concurrency")]
    pub tool_concurrency: u32,
    /// Upper bound, in milliseconds, on the delay before any provider retry.
    /// The delay is `backoff_base_ms * 2^attempt`, capped here.
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    /// Retry policy for provider failures that leave it unknown whether the
    /// provider acted (timeout, send failure after the connection, a stream
    /// cut after content began). Absent means such outcomes are never retried.
    /// Even when present, a retry also needs the directive's
    /// `provider_contact: repeatable` declaration on a remote provider route;
    /// launch preparation seals the resulting ceiling and the daemon enforces
    /// it. `max_retries` counts the first retries of a turn that may follow an
    /// unknown outcome and shares the early attempts with `retries`.
    #[serde(default)]
    pub unknown_outcome_retry: Option<ryeos_directive_definition::UnknownOutcomeRetryPolicy>,
    /// Recovery when the provider ends a turn at its output limit. Absent, or
    /// `max_consecutive: 0`, fails the thread as before. When enabled, the turn
    /// is recorded with `finish: "length"` and, if it carried no tool calls, a
    /// harness input carrying `message` starts the next turn.
    #[serde(default)]
    pub on_output_limit: Option<OutputLimitPolicy>,
    /// Execution-owned provider-attempt accounting policy. Wire parsing stays
    /// in the signed provider schema; budget/failure behavior belongs here.
    #[serde(default)]
    pub accounting: AttemptAccountingConfig,
    /// Limits on the images tools return, in addition to the node's attachment
    /// policy and the model's own limits.
    #[serde(default)]
    pub tool_images: ToolImagesConfig,
}

/// Signed ceilings on the images a run's tools may return. The per-run
/// counters are restored from the record on resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolImagesConfig {
    /// Most bytes one image may hold; keep it within the node's
    /// `thread_attachments` limit, which refuses a larger append.
    #[serde(default = "default_tool_image_max_bytes")]
    pub max_bytes_per_image: u64,
    #[serde(default = "default_tool_images_per_result")]
    pub max_images_per_result: u32,
    /// Most image bytes one result may hold, kept under the 4 MiB a tool's
    /// stored result may take once base64 inflates them.
    #[serde(default = "default_tool_image_bytes_per_result")]
    pub max_bytes_per_result: u64,
    #[serde(default = "default_tool_images_per_run")]
    pub max_images_per_run: u32,
    /// Most distinct image bytes a run may store.
    #[serde(default = "default_tool_image_bytes_per_run")]
    pub max_bytes_per_run: u64,
    /// Most pixels on either side of one image, read from its header. A
    /// truncated body is not detectable without decoding, so this bounds what
    /// the header claims, not what the bytes hold.
    #[serde(default = "default_tool_image_dimension")]
    pub max_image_dimension: u32,
}

fn default_tool_image_dimension() -> u32 {
    8000
}

fn default_tool_image_max_bytes() -> u64 {
    2 * 1024 * 1024
}

fn default_tool_images_per_result() -> u32 {
    4
}

/// A subprocess tool's whole result is stored in the thread's result, which
/// holds at most 4 MiB of compact JSON; base64 makes an image a third larger,
/// so 3 MB of image bytes leaves about 190 KB for the text and wrapper.
fn default_tool_image_bytes_per_result() -> u64 {
    3_000_000
}

fn default_tool_images_per_run() -> u32 {
    256
}

fn default_tool_image_bytes_per_run() -> u64 {
    48 * 1024 * 1024
}

impl Default for ToolImagesConfig {
    fn default() -> Self {
        Self {
            max_bytes_per_image: default_tool_image_max_bytes(),
            max_images_per_result: default_tool_images_per_result(),
            max_bytes_per_result: default_tool_image_bytes_per_result(),
            max_images_per_run: default_tool_images_per_run(),
            max_bytes_per_run: default_tool_image_bytes_per_run(),
            max_image_dimension: default_tool_image_dimension(),
        }
    }
}

impl ToolImagesConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.max_bytes_per_image == 0
            || self.max_images_per_result == 0
            || self.max_bytes_per_result == 0
            || self.max_images_per_run == 0
            || self.max_bytes_per_run == 0
            || self.max_image_dimension == 0
        {
            anyhow::bail!("execution.tool_images limits must all be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputLimitPolicy {
    /// How many turns in a row may end at the output limit before the thread
    /// fails. `0` disables recovery.
    pub max_consecutive: u32,
    /// The harness input recorded after a turn that ended at the limit with no
    /// tool call.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttemptAccountingConfig {
    #[serde(default)]
    pub failure_policy: AccountingFailurePolicy,
    #[serde(default)]
    pub budget_mode: AccountingBudgetMode,
}

impl Default for AttemptAccountingConfig {
    fn default() -> Self {
        Self {
            failure_policy: AccountingFailurePolicy::Auto,
            budget_mode: AccountingBudgetMode::Settled,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingFailurePolicy {
    #[default]
    Auto,
    Warn,
    FailClosed,
}

/// `Settled` reports post-attempt usage against a threshold; `Hard` requires
/// the daemon reservation ledger and is valid only for routes whose sealed
/// financial authority is `Paid` or `ExplicitlyFree` (checked at runtime
/// start — configuration alone cannot prove ledger eligibility).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingBudgetMode {
    #[default]
    Settled,
    Hard,
}

fn default_tool_concurrency() -> u32 {
    4
}

fn default_retries() -> u32 {
    2
}

fn default_retry_status_codes() -> Vec<u16> {
    vec![429, 500, 502, 503, 504, 529]
}

fn default_backoff_max_ms() -> u64 {
    60_000
}

fn default_backoff_base_ms() -> u64 {
    1000
}

fn default_timeout() -> u64 {
    300
}

fn default_max_provider_output_tokens_per_turn() -> u64 {
    32_768
}

fn default_max_stream_output_bytes_per_turn() -> u64 {
    131_072
}

fn default_max_provider_stream_frame_bytes() -> u64 {
    1_048_576
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            category: None,
            retries: default_retries(),
            retry_status_codes: default_retry_status_codes(),
            never_retry: vec![],
            backoff_base_ms: default_backoff_base_ms(),
            timeout_seconds: default_timeout(),
            max_provider_output_tokens_per_turn: default_max_provider_output_tokens_per_turn(),
            max_stream_output_bytes_per_turn: default_max_stream_output_bytes_per_turn(),
            max_provider_stream_frame_bytes: default_max_provider_stream_frame_bytes(),
            tool_preload: false,
            tool_concurrency: default_tool_concurrency(),
            backoff_max_ms: default_backoff_max_ms(),
            unknown_outcome_retry: None,
            on_output_limit: None,
            accounting: AttemptAccountingConfig::default(),
            tool_images: ToolImagesConfig::default(),
        }
    }
}

impl ExecutionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.tool_concurrency == 0 || self.tool_concurrency > 16 {
            anyhow::bail!(
                "execution.tool_concurrency must be between 1 and 16, got {}",
                self.tool_concurrency
            );
        }
        if self.backoff_max_ms < self.backoff_base_ms {
            anyhow::bail!(
                "execution.backoff_max_ms ({}) must not be below execution.backoff_base_ms ({})",
                self.backoff_max_ms,
                self.backoff_base_ms
            );
        }
        if let Some(policy) = &self.on_output_limit
            && (policy.message.trim().is_empty()
                || policy.message.len() > 4096
                || policy.message.chars().any(char::is_control))
        {
            anyhow::bail!(
                "execution.on_output_limit.message must be 1-4096 bytes without control characters"
            );
        }
        if let Some(policy) = &self.unknown_outcome_retry {
            policy.validate()?;
            if policy.max_retries > self.retries {
                anyhow::bail!(
                    "execution.unknown_outcome_retry.max_retries ({}) must not exceed execution.retries ({})",
                    policy.max_retries,
                    self.retries
                );
            }
        }
        self.tool_images.validate()?;
        // `budget_mode: hard` is structurally valid configuration; whether the
        // route is ledger-eligible (Paid/ExplicitlyFree sealed authority) is a
        // launch fact and is enforced at runtime start, not here.
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub execution: ExecutionConfig,
    /// Whether the sealed program declares a durable effect class
    /// (`recorded` | `sealed`) for its provider boundary. The runtime uses
    /// this only to decide whether to submit records; the daemon re-reads
    /// the declaration from the admitted capsule at publication and is the
    /// authority.
    #[serde(default)]
    pub provider_effects_recorded: bool,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    pub context_positions: HashMap<String, Vec<String>>,
    /// Execution-ready hooks. Source hook definitions are merged and compiled
    /// during bootstrap and never reach the runner.
    #[serde(skip)]
    pub hooks: Vec<ryeos_runtime::CompiledHook>,
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(default)]
    pub return_nudge: ReturnNudge,
    #[serde(default)]
    pub continuation: ContinuationConfig,
    #[serde(default)]
    pub continuation_runtime: ContinuationRuntimeConfig,
    #[serde(skip)]
    pub risk_policy: Option<crate::harness::RiskPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// The in-memory conversation message: the neutral turn, a role and an
/// ordered list of parts.
pub type ProviderMessage = ryeos_provider_contract::Message;

/// The id the runtime gives a tool call whose provider issued none. It is a
/// function of the turn and the call's position, so a replay and a recovery
/// derive the same id the live run used.
pub fn synthesised_call_id(turn: u32, call_index: usize) -> String {
    format!("call_t{turn}_{call_index}")
}

/// Constructors and readers the directive runtime uses on a message.
pub trait ProviderMessageExt: Sized {
    fn system(text: impl Into<String>) -> Self;
    fn user(text: impl Into<String>) -> Self;
    fn assistant_text(text: impl Into<String>) -> Self;
    /// An assistant message: reasoning, text, then the tool calls, in that
    /// order. Every call must carry an id by the time it is recorded.
    #[cfg(test)]
    fn assistant(
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self;
    /// A tool message holding the result text of the call named `call_id`.
    #[cfg(test)]
    fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self;
    /// A tool message holding the result text of the call named `call_id`,
    /// with the status the result guard recorded for it.
    fn tool_result_with_status(
        call_id: impl Into<String>,
        text: impl Into<String>,
        status: ToolResultStatus,
    ) -> Self;
    /// The wire role name: `system`, `user`, `assistant` or `tool`.
    fn role(&self) -> &str;
    /// Joined text of the message; for a tool message, of its result. `None`
    /// when there is no text part (an empty text part counts).
    fn text(&self) -> Option<String>;
    /// The text as a JSON string value.
    fn content_value(&self) -> Option<Value>;
    /// The tool calls the message proposes, in order.
    fn calls(&self) -> Vec<ToolCall>;
    fn has_tool_calls(&self) -> bool;
    /// The call id a tool message answers.
    fn tool_call_id(&self) -> Option<&str>;
    /// Joined text of the reasoning parts.
    fn reasoning_text(&self) -> Option<String>;
}

impl ProviderMessageExt for ProviderMessage {
    fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }

    fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    fn assistant_text(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    #[cfg(test)]
    fn assistant(
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        let mut parts = Vec::new();
        if let Some(reasoning) = reasoning {
            parts.push(Part::Reasoning(ryeos_provider_contract::ReasoningPart {
                text: Some(reasoning),
                artefact: None,
            }));
        }
        if let Some(text) = text {
            parts.push(Part::text(text));
        }
        for call in tool_calls {
            parts.push(Part::tool_call(
                call.id.unwrap_or_default(),
                call.name,
                call.arguments,
            ));
        }
        Self::new(Role::Assistant, parts)
    }

    #[cfg(test)]
    fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::tool_result_with_status(call_id, text, ToolResultStatus::Ok)
    }

    fn tool_result_with_status(
        call_id: impl Into<String>,
        text: impl Into<String>,
        status: ToolResultStatus,
    ) -> Self {
        Self::new(
            Role::Tool,
            vec![Part::tool_result(call_id, vec![Part::text(text)], status)],
        )
    }

    fn role(&self) -> &str {
        self.role.as_str()
    }

    fn text(&self) -> Option<String> {
        let mut out: Option<String> = None;
        for part in &self.parts {
            match part {
                Part::Text(text) => out.get_or_insert_with(String::new).push_str(&text.text),
                Part::ToolResult(result) => {
                    for inner in &result.parts {
                        if let Part::Text(text) = inner {
                            out.get_or_insert_with(String::new).push_str(&text.text);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn content_value(&self) -> Option<Value> {
        self.text().map(Value::String)
    }

    fn calls(&self) -> Vec<ToolCall> {
        self.tool_calls()
            .map(|call| ToolCall {
                id: Some(call.call_id.clone()),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .collect()
    }

    fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }

    fn tool_call_id(&self) -> Option<&str> {
        self.parts.iter().find_map(|part| match part {
            Part::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
    }

    fn reasoning_text(&self) -> Option<String> {
        let mut out: Option<String> = None;
        for part in &self.parts {
            if let Part::Reasoning(reasoning) = part
                && let Some(text) = &reasoning.text
            {
                out.get_or_insert_with(String::new).push_str(text);
            }
        }
        out
    }
}

/// A provider answer may not issue one tool call id twice. The turn fails
/// instead of the second use being given a synthesised id, because a result
/// is matched to its call by id and the provider's own id is evidence the
/// runtime does not rewrite.
pub fn ensure_distinct_call_ids(message: &ProviderMessage) -> anyhow::Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for call in message.tool_calls() {
        if !seen.insert(call.call_id.as_str()) {
            anyhow::bail!(
                "provider_duplicate_call_id: the provider issued tool call id {:?} more than once                  in one answer; the turn fails and no id is synthesised for the second use",
                call.call_id
            );
        }
    }
    Ok(())
}

/// Construct the one durable semantic answer for a completed provider turn.
/// Usage, request/response IDs, attempt/thread identity, and transport timing
/// are observation evidence and therefore cannot enter this value.
pub fn normalize_provider_call_answer(
    message: &ProviderMessage,
    finish_reason: Option<&str>,
) -> anyhow::Result<ryeos_provider_contract::ProviderCallAnswer> {
    ensure_distinct_call_ids(message)?;
    let answer = ryeos_provider_contract::ProviderCallAnswer {
        message: message.clone(),
        finish_reason: finish_reason.map(str::to_string),
    };
    answer.validate()?;
    Ok(answer)
}

/// A value captured for one tool call by a signed `tool_call_field` rule.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallArtefact {
    pub capture_id: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental assistant text.
    Delta(String),
    /// Incremental reasoning/thinking text. Emitted when the provider
    /// streams thinking separately (Anthropic extended thinking, Gemini
    /// thoughts, OpenAI o-series). Not all providers produce these.
    ReasoningDelta(String),
    /// One completed opaque reasoning block captured by a signed capture
    /// rule. Meaningless to this crate beyond being stored and replayed.
    ReasoningArtefact { capture_id: String, value: Value },
    /// Complete tool call ready to dispatch.
    ToolUse {
        id: Option<String>,
        name: String,
        arguments: Value,
        /// Stable parser-owned key for byte metering; never derived from a
        /// provider/model identifier and unique within one response stream.
        stream_key: String,
        /// Exact byte length of the provider argument representation used by
        /// the parser before normalization/recovery.
        argument_bytes: usize,
        /// Set when the streamed argument JSON failed to parse and was
        /// recovered as `{}` (see [`MalformedArgs`]). `None` on a clean parse.
        /// Carried so the runner can attach the corruption fact to the
        /// `tool_use` braid event instead of surfacing a silent empty
        /// invocation the operator cannot connect to the upstream fault.
        malformed_args: Option<MalformedArgs>,
        /// A value a signed capture rule collected for this call, to be
        /// stored on the call's part and replayed by declared rule.
        artefact: Option<ToolCallArtefact>,
    },
    /// Partial tool call argument JSON streamed mid-flight.
    ///
    /// Anthropic delivers tool arguments as `input_json_delta` chunks
    /// before the final `content_block_stop`. Surfacing the running
    /// accumulation here lets the runner emit progressive
    /// `cognition_out` events so the daemon (and ultimately the
    /// browser) can stream large structured outputs (e.g.
    /// `directive_return.response`) instead of waiting for the whole
    /// tool call to land at once.
    ToolUsePartial {
        id: Option<String>,
        name: String,
        stream_key: String,
        /// The new JSON fragment that just arrived (NOT the cumulative
        /// buffer — consumers get the delta and reconstruct the full
        /// state if they need it).
        delta: String,
        /// Total bytes of partial JSON accumulated so far for this
        /// tool call. Lets consumers know whether they're at the start
        /// (and may need to skip past `{"response":"`) or in the middle.
        total_len: usize,
    },
    /// Cumulative usage update from the provider. Emitted mid-stream
    /// by providers that send incremental token counts.
    Usage(UsageUpdate),
    /// Provider warning (safety, truncation, partial failure) that
    /// doesn't terminate the stream.
    Warning { code: String, message: String },
    /// Stream is finished. Terminal event — runner stops consuming.
    /// Carries the normalized finish reason and the raw provider string.
    Finish {
        reason: FinishReason,
        raw: Option<String>,
    },
}

/// Diagnostic fact recorded when a streamed tool-call's argument JSON could
/// not be parsed and was recovered as an empty object. Attached to the
/// `tool_use` braid event so an operator sees "args corrupted upstream" and can
/// correlate the empty invocation with the raw bytes by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedArgs {
    /// SHA-256 (hex) of the raw, unparseable argument bytes.
    pub sha256: String,
    /// Byte length of the raw arguments before recovery.
    pub raw_len: usize,
}

/// Normalized finish reason across all provider families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// Model produced end-of-turn.
    Stop,
    /// Model wants tools dispatched.
    ToolCalls,
    /// Hit max_tokens / output length limit.
    Length,
    /// Content filtered by provider safety.
    ContentFilter,
    /// Unmappable; check raw string.
    Other,
}

/// Cumulative token usage from the provider.
#[derive(Debug, Clone, Default)]
pub struct UsageUpdate {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_miss_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub anomalies: Vec<String>,
}

/// Normalize a provider-specific finish reason string to a canonical enum.
/// Case-insensitive: Gemini sends uppercase `"STOP"`, Anthropic sends
/// lowercase `"end_turn"`, OpenAI sends lowercase `"stop"`.
pub fn normalize_finish_reason(raw: Option<&str>) -> FinishReason {
    let lower = raw.map(|s| s.to_ascii_lowercase());
    match lower.as_deref() {
        Some("stop") | Some("end_turn") | Some("end_of_turn") => FinishReason::Stop,
        Some("tool_calls") | Some("function_call") | Some("tool_use") => FinishReason::ToolCalls,
        Some("length") | Some("max_tokens") | Some("model_length") => FinishReason::Length,
        Some("content_filter") | Some("safety") | Some("recitation") | Some("blocklist") => {
            FinishReason::ContentFilter
        }
        _ => FinishReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_assistant_message_keeps_reasoning_text_then_calls_in_order() {
        let message = ProviderMessage::assistant(
            Some("checking".to_string()),
            vec![ToolCall {
                id: Some("call_1".to_string()),
                name: "inspect".to_string(),
                arguments: serde_json::json!({"a": 1}),
            }],
            Some("thinking".to_string()),
        );
        let kinds: Vec<_> = message.parts.iter().map(|part| part.type_name()).collect();
        assert_eq!(kinds, ["reasoning", "text", "tool_call"]);
        assert_eq!(message.text().as_deref(), Some("checking"));
        assert_eq!(message.reasoning_text().as_deref(), Some("thinking"));
        assert_eq!(message.calls()[0].id.as_deref(), Some("call_1"));
        assert_eq!(message.calls().len(), 1);
        message.validate().unwrap();
    }

    #[test]
    fn a_tool_message_reads_its_call_id_and_result_text() {
        let message = ProviderMessage::tool_result("call_1", "4");
        assert_eq!(message.role(), "tool");
        assert_eq!(message.tool_call_id(), Some("call_1"));
        assert_eq!(message.text().as_deref(), Some("4"));
        message.validate().unwrap();
    }

    #[test]
    fn a_duplicate_provider_call_id_fails_with_a_clear_message() {
        let call = |name: &str| ToolCall {
            id: Some("dup".to_string()),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        };
        let message = ProviderMessage::assistant(None, vec![call("a"), call("b")], None);
        let error = ensure_distinct_call_ids(&message).unwrap_err().to_string();
        assert!(error.contains("provider_duplicate_call_id"), "{error}");
        assert!(error.contains("no id is synthesised"), "{error}");
        assert!(normalize_provider_call_answer(&message, Some("tool_calls")).is_err());
    }

    #[test]
    fn a_synthesised_call_id_is_a_function_of_turn_and_position() {
        assert_eq!(synthesised_call_id(4, 0), "call_t4_0");
        assert_eq!(synthesised_call_id(4, 2), "call_t4_2");
    }

    #[test]
    fn provider_effect_answer_excludes_observation_texture() {
        let message = ProviderMessage::assistant(
            Some("answer".to_string()),
            Vec::new(),
            Some("reasoning".to_string()),
        );
        let first = normalize_provider_call_answer(&message, Some("stop")).unwrap();
        let second = normalize_provider_call_answer(&message, Some("stop")).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
        let value = serde_json::to_value(first).unwrap();
        for observation_field in ["usage", "attempt_id", "thread_id", "response_id"] {
            assert!(value.get(observation_field).is_none());
        }
    }

    #[test]
    fn provider_effect_answer_refuses_non_assistant_or_unsafe_finish_reason() {
        let tool_message = ProviderMessage::tool_result("c1", "answer");
        assert!(normalize_provider_call_answer(&tool_message, Some("stop")).is_err());
        let message = ProviderMessage::assistant_text("answer");
        assert!(normalize_provider_call_answer(&message, Some(" stop ")).is_err());
    }

    // A config that omits the retry knobs must inherit the same sane values
    // as a fully-absent config — never 0 retries, an empty retryable-status
    // set, or 0ms backoff (a hot loop). The serde field defaults resolve
    // through the same fns as `impl Default`, so parse-path and Default agree.
    #[test]
    fn partial_config_inherits_retry_defaults_not_zero() {
        let cfg: ExecutionConfig =
            serde_yaml::from_str("category: \"ryeos-runtime\"\nretries: 5\n").unwrap();
        assert_eq!(cfg.retries, 5, "explicit value is honored");
        assert_eq!(
            cfg.retry_status_codes,
            vec![429, 500, 502, 503, 504, 529],
            "omitted status codes default to the retryable set, not empty"
        );
        assert_eq!(
            cfg.backoff_base_ms, 1000,
            "omitted backoff defaults to 1s, not a 0ms hot loop"
        );
        assert!(
            cfg.unknown_outcome_retry.is_none(),
            "an omitted unknown-outcome policy refuses such retries because a dropped stream has an uncertain provider outcome"
        );
        assert_eq!(cfg.backoff_max_ms, 60_000);
        assert_eq!(
            cfg.max_provider_output_tokens_per_turn,
            default_max_provider_output_tokens_per_turn(),
            "omitted provider output cap uses the configured default, not 0"
        );
        assert_eq!(
            cfg.max_stream_output_bytes_per_turn,
            default_max_stream_output_bytes_per_turn()
        );
        assert_eq!(
            cfg.max_provider_stream_frame_bytes,
            default_max_provider_stream_frame_bytes()
        );
    }

    #[test]
    fn empty_config_matches_struct_default() {
        let parsed: ExecutionConfig = serde_yaml::from_str("{}").unwrap();
        let default = ExecutionConfig::default();
        assert_eq!(parsed.retries, default.retries);
        assert_eq!(parsed.retry_status_codes, default.retry_status_codes);
        assert_eq!(parsed.backoff_base_ms, default.backoff_base_ms);
        assert_eq!(
            parsed.max_provider_output_tokens_per_turn,
            default.max_provider_output_tokens_per_turn
        );
        assert_eq!(
            parsed.max_stream_output_bytes_per_turn,
            default.max_stream_output_bytes_per_turn
        );
        assert_eq!(
            parsed.max_provider_stream_frame_bytes,
            default.max_provider_stream_frame_bytes
        );
        assert_eq!(parsed.unknown_outcome_retry, default.unknown_outcome_retry);
        assert_eq!(parsed.backoff_max_ms, default.backoff_max_ms);
        assert_eq!(parsed.retries, 2);
    }

    #[test]
    fn stream_output_byte_limit_is_independent_and_explicitly_disableable() {
        let disabled: ExecutionConfig = serde_yaml::from_str(
            "max_provider_output_tokens_per_turn: 10\nmax_stream_output_bytes_per_turn: 0\n",
        )
        .unwrap();
        assert_eq!(disabled.max_stream_output_bytes_per_turn, 0);

        let explicit: ExecutionConfig = serde_yaml::from_str(
            "max_provider_output_tokens_per_turn: 10\nmax_stream_output_bytes_per_turn: 123\n",
        )
        .unwrap();
        assert_eq!(explicit.max_stream_output_bytes_per_turn, 123);
    }

    #[test]
    fn removed_output_limit_key_is_rejected() {
        let error = serde_yaml::from_str::<ExecutionConfig>("max_output_tokens_per_turn: 10\n")
            .expect_err("removed key must not deserialize");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn removed_outcome_uncertain_retry_booleans_are_rejected() {
        for field in ["retry_on_timeout", "retry_mid_stream"] {
            let error = serde_yaml::from_str::<ExecutionConfig>(&format!("{field}: false\n"))
                .expect_err("removed key must not deserialize");
            assert!(error.to_string().contains("unknown field"));
        }
    }

    #[test]
    fn tool_image_limits_default_parse_and_validate() {
        let defaults: ExecutionConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(defaults.tool_images, ToolImagesConfig::default());
        let tight: ExecutionConfig = serde_yaml::from_str(
            "tool_images:\n  max_images_per_result: 1\n  max_images_per_run: 2\n",
        )
        .unwrap();
        tight.validate().unwrap();
        assert_eq!(tight.tool_images.max_images_per_result, 1);
        assert_eq!(
            tight.tool_images.max_bytes_per_run,
            default_tool_image_bytes_per_run()
        );
        let zero: ExecutionConfig =
            serde_yaml::from_str("tool_images:\n  max_images_per_run: 0\n").unwrap();
        assert!(zero.validate().is_err());
        assert!(serde_yaml::from_str::<ExecutionConfig>("tool_images:\n  unknown: 1\n").is_err());
    }

    #[test]
    fn unknown_outcome_retry_block_parses_and_validates() {
        let cfg: ExecutionConfig = serde_yaml::from_str(
            "retries: 4\nunknown_outcome_retry:\n  max_retries: 2\n  on: [timeout, mid_stream]\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let policy = cfg.unknown_outcome_retry.as_ref().unwrap();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.on.len(), 2);

        let empty: ExecutionConfig =
            serde_yaml::from_str("unknown_outcome_retry:\n  max_retries: 1\n  on: []\n").unwrap();
        assert!(empty.validate().is_err());

        let unknown_class = serde_yaml::from_str::<ExecutionConfig>(
            "unknown_outcome_retry:\n  max_retries: 1\n  on: [reset]\n",
        );
        assert!(unknown_class.is_err());

        let over: ExecutionConfig = serde_yaml::from_str(
            "retries: 1\nunknown_outcome_retry:\n  max_retries: 2\n  on: [timeout]\n",
        )
        .unwrap();
        let error = over.validate().unwrap_err();
        assert!(error.to_string().contains("must not exceed"));
    }

    #[test]
    fn output_limit_policy_parses_and_validates_its_message() {
        let cfg: ExecutionConfig = serde_yaml::from_str(
            "on_output_limit:\n  max_consecutive: 2\n  message: \"Continue from where it stopped.\"\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.on_output_limit.as_ref().unwrap().max_consecutive, 2);
        assert!(ExecutionConfig::default().on_output_limit.is_none());

        let blank: ExecutionConfig =
            serde_yaml::from_str("on_output_limit:\n  max_consecutive: 1\n  message: \"  \"\n")
                .unwrap();
        assert!(blank.validate().is_err());
        assert!(
            serde_yaml::from_str::<ExecutionConfig>("on_output_limit:\n  max_consecutive: 1\n")
                .is_err()
        );
    }

    #[test]
    fn backoff_cap_below_base_is_rejected() {
        let cfg: ExecutionConfig =
            serde_yaml::from_str("backoff_base_ms: 2000\nbackoff_max_ms: 1000\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}

#[cfg(test)]
mod tool_concurrency_tests {
    use super::ExecutionConfig;

    #[test]
    fn tool_concurrency_defaults_to_four_and_validates_range() {
        let config = ExecutionConfig::default();
        assert_eq!(config.tool_concurrency, 4);
        config.validate().unwrap();

        let serial = ExecutionConfig {
            tool_concurrency: 1,
            ..ExecutionConfig::default()
        };
        serial.validate().unwrap();
        let max = ExecutionConfig {
            tool_concurrency: 16,
            ..ExecutionConfig::default()
        };
        max.validate().unwrap();

        for invalid in [0_u32, 17, 1000] {
            let config = ExecutionConfig {
                tool_concurrency: invalid,
                ..ExecutionConfig::default()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("tool_concurrency"), "{error}");
        }

        // Unauthored config decodes to the default through serde.
        let decoded: ExecutionConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(decoded.tool_concurrency, 4);
    }
}
