//! Shared response and accounting types for provider transports.

use crate::directive::ProviderMessage;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub struct AdapterResponse {
    pub message: ProviderMessage,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<String>,
    pub generation_header_id: Option<String>,
    pub response_id: Option<String>,
    /// Effective provider-native output-token limit present in the rendered
    /// request body, after config defaults and runtime clamping.
    pub requested_output_tokens: Option<u64>,
    pub observed_output: ObservedOutput,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ObservedOutput {
    pub text_bytes: u64,
    pub reasoning_bytes: u64,
    pub tool_argument_bytes: u64,
    pub accepted_output_events: u64,
    pub live_output_events_emitted: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageSource {
    /// Parsed only through signed streaming metadata paths and semantics.
    SignedMetadata,
    /// Parsed by the configured protocol-family stream event adapter.
    ProtocolEvent,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageComparability {
    /// No declared tokenizer/modality contract proves that provider-native
    /// usage is comparable to RyeOS-observed semantic output.
    #[default]
    NotComparable,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLimitContractStatus {
    #[default]
    Unknown,
    WithinRequestedLimit,
    ReportedAboveRequestedLimit,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-reported reasoning-token dimension, when the protocol exposes
    /// it. A signed provider schema declares whether this is included in the
    /// output-token count. It remains accounting metadata and never drives
    /// local stream enforcement.
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_miss_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// The signed usage schema declares that cache reads and misses form a
    /// complete, non-overlapping partition of `input_tokens`.
    pub cache_partition_of_input: bool,
    /// Provider-reported charge for this request, when a signed streaming
    /// metadata schema declares its location.
    pub reported_cost_usd: Option<f64>,
    /// Exact JSON number token for authoritative settlement. Unlike the
    /// presentation-only `f64` above, this preserves decimal scale and digits
    /// through the callback boundary.
    #[serde(skip_serializing)]
    pub reported_cost_usd_raw: Option<String>,
    pub cost_details: Option<Value>,
    pub is_byok: Option<bool>,
    pub source: ProviderUsageSource,
    pub comparability: UsageComparability,
    pub provider_limit_contract: ProviderLimitContractStatus,
    /// Structurally invalid or contract-suspicious provider metadata. These
    /// diagnostics never participate in local stream-limit enforcement.
    pub anomalies: Vec<String>,
    /// Malformed optional billing/enrichment metadata. These do not invalidate
    /// otherwise complete token counts and are settled/persisted independently.
    pub metadata_anomalies: Vec<String>,
    /// Provider-reported spend facts that cannot be trusted for settlement
    /// (for example, a cumulative final charge that regressed). Kept separate
    /// so malformed token accounting never invalidates an otherwise
    /// trustworthy direct charge, and vice versa.
    pub spend_anomalies: Vec<String>,
    /// Well-formed accounting that contradicts another declared protocol
    /// contract (for example, output usage above the effective requested
    /// provider-native limit). These facts do not make token counts malformed.
    pub contract_anomalies: Vec<String>,
    /// Number of usage records merged into this response accounting state.
    pub snapshots_seen: u32,
}

impl TokenUsage {
    pub fn complete_token_counts(&self) -> Option<(u64, u64)> {
        Some((self.input_tokens?, self.output_tokens?))
    }

    pub fn is_valid(&self) -> bool {
        self.complete_token_counts().is_some() && self.anomalies.is_empty()
    }
}
