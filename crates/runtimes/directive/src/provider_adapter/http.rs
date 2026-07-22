//! Shared response types for the provider adapter. The non-streaming
//! HTTP `call_provider` previously lived here; the live launch path
//! now uses `streaming::call_provider_streaming` exclusively, so this
//! module is reduced to the typed response shape both the streaming
//! function and downstream cost accounting depend on.

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
    /// Provider-reported completion-token subset spent on reasoning, when the
    /// protocol exposes it. This remains accounting metadata and never drives
    /// local stream enforcement.
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported charge for this request, when a signed streaming
    /// metadata schema declares its location.
    pub reported_cost_usd: Option<f64>,
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
