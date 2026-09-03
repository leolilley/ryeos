use std::collections::HashMap;

use ryeos_directive_definition::{ContinuationConfig, OutputSpec, ReturnNudge};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Provider-facing lifecycle tool intercepted by the directive runtime.
///
/// This is not a dispatchable RyeOS item. Keep its wire name centralized so
/// tool exposure, lifecycle detection, event recording, and replay cannot
/// silently diverge.
pub(crate) const DIRECTIVE_RETURN_TOOL: &str = "directive_return";

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
            tool_concurrency: default_tool_concurrency(),
            retry_on_timeout: false,
            retry_mid_stream: default_retry_mid_stream(),
            accounting: AttemptAccountingConfig::default(),
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

/// Construct the one durable semantic answer for a completed provider turn.
/// Usage, request/response IDs, attempt/thread identity, and transport timing
/// are observation evidence and therefore cannot enter this value.
pub fn normalize_provider_call_answer(
    message: &ProviderMessage,
    finish_reason: Option<&str>,
) -> anyhow::Result<ryeos_provider_contract::ProviderCallAnswer> {
    let recorded_message = serde_json::from_value(serde_json::to_value(message)?)
        .map_err(|error| anyhow::anyhow!("provider message is not record-safe: {error}"))?;
    let answer = ryeos_provider_contract::ProviderCallAnswer {
        message: recorded_message,
        finish_reason: finish_reason.map(str::to_string),
    };
    answer.validate()?;
    Ok(answer)
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
    fn provider_effect_answer_excludes_observation_texture() {
        let message = ProviderMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::json!("answer")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("reasoning".to_string()),
        };
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
        let mut message = ProviderMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::json!("answer")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        };
        message.role = "tool".to_string();
        assert!(normalize_provider_call_answer(&message, Some("stop")).is_err());
        message.role = "assistant".to_string();
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
