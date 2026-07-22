use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export directive-domain types from the directive-owned shared crate.
// The launch preparer and runtime consume this one strict schema.
#[allow(unused_imports)]
pub use ryeos_directive_core::{
    AssistantToolCallsPlacement, MessageSchemas, ModelRoutingConfig, ModelSpec, OutputLimitConfig,
    OutputLimitSemantics, PricingConfig, ProtocolFamily, ProviderAccountingConfig, ProviderConfig,
    ProviderProfile, ReportedCostUnit, SamplingConfig, SchemasConfig, StreamErrorConfig,
    StreamMetadataConfig, StreamPaths, StreamUsageConfig, StreamingConfig, StreamingMode,
    SystemMessageConfig, SystemMessageMode, TextPlacement, ToolResultConfig, ToolResultWrapMode,
    ToolSchemaConfig, UsageAggregation,
};

/// Typed runtime view of a directive's effective header *after* the
/// daemon-side composer has produced
/// `KindComposedView::ExtendsChain(...).composed`.
///
/// The runtime owns the typed shape of the fields it **consumes**
/// (`permissions`, `limits`, `outputs`, `context`, `hooks`,
/// `extends`, `name`).  `deny_unknown_fields` is mandatory — relaxing
/// it once already masked a routing-config regression by accepting
/// fields the runtime quietly ignored.
///
/// The composer ships extra frontmatter keys (`body`, `category`,
/// `description`, `inputs`, `required_secrets`, …) that are owned by
/// the engine / dispatcher (e.g. `required_secrets` is read on the
/// daemon side by `dispatch.rs::vault_bindings`).  Those fields MUST
/// be stripped *before* deserialising into `DirectiveHeader` —
/// `bootstrap.rs::parse_effective_header` does the projection — so
/// the typed shape stays narrow and `deny_unknown_fields` keeps
/// catching real drift instead of being defeated by every new
/// composer-emitted key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveHeader {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub limits: Option<LimitsSpec>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(default)]
    pub context: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub hooks: Option<Vec<ryeos_runtime::HookDefinition>>,
    #[serde(default)]
    pub continuation: ContinuationConfig,
    /// Opt-in corrective turn: when the directive declares `outputs` and a
    /// run is about to settle without a successful `directive_return`, grant
    /// ONE extra turn before settling with empty outputs. Default off — a
    /// directive that does not opt in settles exactly as its final turn left
    /// it. `true` uses the built-in message (which names the declared
    /// outputs); a string is the nudge stimulus verbatim — the prompt is the
    /// directive author's to word. Untagged so `return_nudge: true` and
    /// `return_nudge: "<text>"` both parse from the header.
    #[serde(default)]
    pub return_nudge: ReturnNudge,
}

/// Allow-list of composed-view top-level keys the runtime decodes
/// into [`DirectiveHeader`].  See the type's doc comment for the
/// rationale: every other key is owned by the daemon (composer,
/// dispatcher, augmentations) and is read directly off
/// `KindComposedView` by the daemon, not by the runtime.
pub const DIRECTIVE_HEADER_RUNTIME_KEYS: &[&str] = &[
    "name",
    "extends",
    "limits",
    "outputs",
    "context",
    "hooks",
    "continuation",
    "return_nudge",
];

/// The `return_nudge` header value: off, on with the built-in message, or on
/// with an author-worded stimulus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ReturnNudge {
    Flag(bool),
    Message(String),
}

impl Default for ReturnNudge {
    fn default() -> Self {
        ReturnNudge::Flag(false)
    }
}

impl ReturnNudge {
    pub fn enabled(&self) -> bool {
        match self {
            ReturnNudge::Flag(on) => *on,
            ReturnNudge::Message(_) => true,
        }
    }

    /// The corrective stimulus to inject: the author's wording when the
    /// header carries a non-empty string, else the built-in message naming
    /// the declared outputs.
    pub fn message(&self, declared_outputs: &[String]) -> String {
        if let ReturnNudge::Message(text) = self {
            if !text.trim().is_empty() {
                return text.clone();
            }
        }
        format!(
            "This directive declares structured outputs ({}) that have not been \
             emitted. Call the `directive_return` tool now with every declared \
             output. This is the final turn.",
            declared_outputs.join(", ")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSpec {
    #[serde(default)]
    pub turns: Option<u32>,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub spend_usd: Option<f64>,
    #[serde(default)]
    pub spawns: Option<u32>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
}

/// What a directive does at the context-window continuation boundary.
///
/// `false` or absent → **stop**: finalize immediately with the current state
/// (the last assistant content). The runtime grants no extra turn and enforces
/// no declared outputs at the boundary — emitting outputs before the wall (via
/// `directive_return`) is the directive's own job. This is the default: a
/// directive that does not opt in stops when its live context reaches the
/// threshold. (This resolves the limits-design §9.2/§9.3 open questions in the
/// landed shape: there is no auto-granted final turn, so neither "final-turn
/// overrun reserve" nor "final turn without `directive_return`" can arise — the
/// context-threshold ratio is itself the pre-wall reserve.)
///
/// An object → **self-continue**: the same directive resumes across segments
/// carrying `carry_turns` recent turns. Opt-in. (`true` = enabled with
/// defaults.) The successor fork fires at the live-context threshold — a
/// per-segment reserve below the hard context limit — never at the wall.
/// Untagged so `continuation: false` and `continuation: {…}` both parse from
/// the header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ContinuationConfig {
    Flag(bool),
    Enabled(ContinuationEnabled),
}

impl Default for ContinuationConfig {
    fn default() -> Self {
        ContinuationConfig::Flag(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContinuationEnabled {
    /// Declared *intent* for how many recent complete turns carry into the
    /// successor. `None` = use the resolved default. The default and the cap are
    /// NOT defined here — they live with every other limit in the runtime limits
    /// config (`.ai/config/directive-runtime/limits.yaml` via the executor's
    /// `LimitsConfig`) and are resolved/clamped at launch, exactly like a header
    /// `limits:` value. Keeping the numbers out of this file avoids scattering
    /// limit config across the tree.
    #[serde(default)]
    pub carry_turns: Option<u32>,
}

impl ContinuationConfig {
    pub fn enabled(&self) -> bool {
        match self {
            ContinuationConfig::Flag(b) => *b,
            ContinuationConfig::Enabled(_) => true,
        }
    }

    /// The directive's DECLARED `carry_turns` intent, if any. Resolved against
    /// [`ContinuationRuntimeConfig`] (default + cap) at launch.
    pub fn declared_carry_turns(&self) -> Option<u32> {
        match self {
            ContinuationConfig::Enabled(e) => e.carry_turns,
            ContinuationConfig::Flag(_) => None,
        }
    }
}

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
pub struct OutputSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
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
    #[serde(default)]
    pub retry_on_timeout: bool,
    /// Retry a stream that dies MID-READ (chunk timeout, reset, dropped
    /// connection) under the same `retries` budget. The request is idempotent
    /// (same message array); deltas persisted before the cut stay in the braid
    /// as the record of the abandoned partial, delimited by the
    /// `provider_retry` event. On by default — a dropped stream is the same
    /// transient transport class as a pre-stream connect failure, and without
    /// this every long generation is one hiccup away from a dead thread. Set
    /// `false` to fail a directive on the first mid-stream cut.
    #[serde(default = "default_retry_mid_stream")]
    pub retry_mid_stream: bool,
    /// Execution-owned provider-attempt accounting policy. Wire parsing stays
    /// in the signed provider schema; budget/failure behavior belongs here.
    #[serde(default)]
    pub accounting: AttemptAccountingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttemptAccountingConfig {
    #[serde(default)]
    pub failure_policy: AccountingFailurePolicy,
    #[serde(default)]
    pub budget_mode: AccountingBudgetMode,
    #[serde(default)]
    pub reservation: AttemptReservationConfig,
}

impl Default for AttemptAccountingConfig {
    fn default() -> Self {
        Self {
            failure_policy: AccountingFailurePolicy::Auto,
            budget_mode: AccountingBudgetMode::Settled,
            reservation: AttemptReservationConfig::default(),
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingBudgetMode {
    #[default]
    Settled,
    Hard,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttemptReservationConfig {
    #[serde(default)]
    pub max_tokens_per_attempt: Option<u64>,
    #[serde(default)]
    pub max_cost_per_attempt_usd: Option<f64>,
}

fn default_retries() -> u32 {
    2
}

fn default_retry_status_codes() -> Vec<u16> {
    vec![429, 500, 502, 503]
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

fn default_retry_mid_stream() -> bool {
    true
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
            retry_on_timeout: false,
            retry_mid_stream: default_retry_mid_stream(),
            accounting: AttemptAccountingConfig::default(),
        }
    }
}

impl ExecutionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(tokens) = self.accounting.reservation.max_tokens_per_attempt {
            if tokens == 0 {
                anyhow::bail!("accounting.reservation.max_tokens_per_attempt must be positive");
            }
        }
        if let Some(cost) = self.accounting.reservation.max_cost_per_attempt_usd {
            if !cost.is_finite() || cost <= 0.0 {
                anyhow::bail!(
                    "accounting.reservation.max_cost_per_attempt_usd must be finite and positive"
                );
            }
        }
        if self.accounting.budget_mode == AccountingBudgetMode::Hard {
            anyhow::bail!(
                "accounting.budget_mode=hard requires the durable reservation/reconciliation backend, which is not enabled by this runtime build"
            );
        }
        if self.accounting.budget_mode == AccountingBudgetMode::Settled
            && (self.accounting.reservation.max_tokens_per_attempt.is_some()
                || self
                    .accounting
                    .reservation
                    .max_cost_per_attempt_usd
                    .is_some())
        {
            anyhow::bail!(
                "accounting.reservation ceilings are only meaningful with accounting.budget_mode=hard"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub execution: ExecutionConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMessage {
    pub role: String,
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Provider-specific hidden reasoning that must be replayed with an
    /// assistant tool-call message for OpenAI-compatible reasoning models
    /// such as DeepSeek thinking-mode endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(clippy::upper_case_acronyms)] // Usage is a domain term, not an acronym leak
pub enum StreamEvent {
    /// Incremental assistant text.
    Delta(String),
    /// Incremental reasoning/thinking text. Emitted when the provider
    /// streams thinking separately (Anthropic extended thinking, Gemini
    /// thoughts, OpenAI o-series). Not all providers produce these.
    #[allow(dead_code)] // Emitted by parser once reasoning extraction is wired
    ReasoningDelta(String),
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
    #[allow(dead_code)] // Emitted by parser once usage extraction is wired
    Usage(UsageUpdate),
    /// Provider warning (safety, truncation, partial failure) that
    /// doesn't terminate the stream.
    #[allow(dead_code)] // Emitted by parser once warning extraction is wired
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
    fn return_nudge_parses_flag_and_custom_message_forms() {
        // Absent → off; `true` → on with the built-in message (which names
        // the declared outputs); a string → on with the author's wording.
        let off: DirectiveHeader = serde_yaml::from_str("name: t").unwrap();
        assert!(!off.return_nudge.enabled());

        let flag: DirectiveHeader = serde_yaml::from_str("return_nudge: true").unwrap();
        assert!(flag.return_nudge.enabled());
        let outs = vec!["model_body".to_string(), "notes".to_string()];
        let built_in = flag.return_nudge.message(&outs);
        assert!(built_in.contains("model_body, notes"), "{built_in}");
        assert!(built_in.contains("directive_return"), "{built_in}");

        let custom: DirectiveHeader =
            serde_yaml::from_str("return_nudge: \"Call directive_return with the model.\"")
                .unwrap();
        assert!(custom.return_nudge.enabled());
        assert_eq!(
            custom.return_nudge.message(&outs),
            "Call directive_return with the model."
        );

        // A blank string is still an opt-in — it falls back to the built-in
        // message rather than injecting an empty stimulus.
        let blank: DirectiveHeader = serde_yaml::from_str("return_nudge: \"  \"").unwrap();
        assert!(blank.return_nudge.enabled());
        assert!(blank
            .return_nudge
            .message(&outs)
            .contains("directive_return"));
    }

    #[test]
    fn directive_header_hooks_use_the_typed_scalar_condition_grammar() {
        let header: DirectiveHeader = serde_yaml::from_str(
            r#"
hooks:
  - id: selected
    event: after_step
    condition: "turn >= 2"
    action: {item_id: "tool:test/hook"}
"#,
        )
        .unwrap();
        let hooks = header.hooks.unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(matches!(
            &hooks[0].condition,
            ryeos_runtime::ExpressionCondition::Expression(source) if source == "turn >= 2"
        ));

        let error = serde_yaml::from_str::<DirectiveHeader>(
            r#"
hooks:
  - id: legacy
    event: after_step
    condition: {path: turn, op: gte, value: 2}
    action: {item_id: "tool:test/hook"}
"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("structured path/op/value conditions are not supported"),
            "expected the structured condition to be rejected, got: {error}"
        );
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
            vec![429, 500, 502, 503],
            "omitted status codes default to the retryable set, not empty"
        );
        assert_eq!(
            cfg.backoff_base_ms, 1000,
            "omitted backoff defaults to 1s, not a 0ms hot loop"
        );
        assert!(
            cfg.retry_mid_stream,
            "omitted retry_mid_stream defaults ON — a dropped stream is transient"
        );
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
        assert_eq!(parsed.retry_mid_stream, default.retry_mid_stream);
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
}
