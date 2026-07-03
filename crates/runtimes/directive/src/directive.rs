use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export types now canonically owned by the shared model-resolution
// module in ryeos-runtime. Existing `use crate::directive::*` imports
// in the directive-runtime continue to resolve these names.
#[allow(unused_imports)]
pub use ryeos_runtime::model_resolution::{
    AssistantToolCallsPlacement, MessageSchemas, ModelRoutingConfig, ModelSpec, PricingConfig,
    ProtocolFamily, ProviderConfig, ProviderProfile, SamplingConfig, SchemasConfig, StreamPaths,
    SystemMessageConfig, SystemMessageMode, TextPlacement, ToolResultConfig, ToolResultWrapMode,
    ToolSchemaConfig,
};

/// Typed runtime view of a directive's effective header *after* the
/// daemon-side composer has produced
/// `KindComposedView::ExtendsChain(...).composed`.
///
/// The runtime owns the typed shape of the fields it **consumes**
/// (`model`, `permissions`, `limits`, `outputs`, `context`, `hooks`,
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
    pub model: Option<ModelSpec>,
    #[serde(default)]
    pub limits: Option<LimitsSpec>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputSpec>>,
    #[serde(default)]
    pub context: Option<HashMap<String, Vec<String>>>,
    #[serde(default)]
    pub hooks: Option<Vec<Value>>,
    #[serde(default)]
    pub continuation: ContinuationConfig,
}

/// Allow-list of composed-view top-level keys the runtime decodes
/// into [`DirectiveHeader`].  See the type's doc comment for the
/// rationale: every other key is owned by the daemon (composer,
/// dispatcher, augmentations) and is read directly off
/// `KindComposedView` by the daemon, not by the runtime.
pub const DIRECTIVE_HEADER_RUNTIME_KEYS: &[&str] = &[
    "name",
    "extends",
    "model",
    "limits",
    "outputs",
    "context",
    "hooks",
    "continuation",
];

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
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub retry_status_codes: Vec<u16>,
    #[serde(default)]
    pub never_retry: Vec<String>,
    #[serde(default)]
    pub backoff_base_ms: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub tool_preload: bool,
    #[serde(default)]
    pub retry_on_timeout: bool,
}

fn default_timeout() -> u64 {
    300
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            category: None,
            retries: 2,
            retry_status_codes: vec![429, 500, 502, 503],
            never_retry: vec![],
            backoff_base_ms: 1000,
            timeout_seconds: default_timeout(),
            tool_preload: false,
            retry_on_timeout: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub execution: ExecutionConfig,
    pub model_routing: Option<ModelRoutingConfig>,
    pub provider: Option<ProviderConfig>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    pub context_positions: HashMap<String, Vec<String>>,
    pub hooks: Vec<ryeos_runtime::HookDefinition>,
    pub outputs: Option<Vec<OutputSpec>>,
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
