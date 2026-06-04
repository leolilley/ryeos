//! Shared response types for the provider adapter. The non-streaming
//! HTTP `call_provider` previously lived here; the live launch path
//! now uses `streaming::call_provider_streaming` exclusively, so this
//! module is reduced to the typed response shape both the streaming
//! function and downstream cost accounting depend on.

use crate::directive::ProviderMessage;

#[derive(Debug)]
pub struct AdapterResponse {
    pub message: ProviderMessage,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
