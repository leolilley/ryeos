//! Immutable prepared provider request (§9.2): prepare once, digest once,
//! send the exact bytes.
//!
//! Reservation and issue bind to this object's digest; transport consumes
//! its exact method, endpoint, frozen credential, headers, and body bytes
//! without rebuilding or re-resolving anything. This closes the gap where a
//! body hashed before reservation could differ from the body constructed at
//! send time. The credential VALUE stays outside every digest — only its
//! declared header name participates.

use anyhow::{Result, anyhow, bail};
#[cfg(feature = "latency-profiling")]
use std::time::Instant;

use super::streaming::{
    self, StreamingCallInput, apply_declared_output_limit, apply_declared_reasoning,
    build_request_body, declared_output_limit_from_body, inject_sampling,
};

/// Credential frozen at prepare time. The env read happens during prepare —
/// a rotation between prepare and send can never substitute a different
/// credential under an already-digested request.
pub struct PreparedCredential {
    pub header_name: String,
    pub prefix: String,
    /// Secret value; excluded from every digest and never logged.
    pub value: String,
}

/// One immutable provider request: everything transport needs, nothing it
/// may recompute.
pub struct PreparedProviderRequest {
    pub method: reqwest::Method,
    pub url: String,
    /// Non-secret header names bound into `request_digest` (the auth header
    /// NAME is included, its value excluded), sorted.
    pub header_names: Vec<String>,
    /// Behavior-bearing public headers for provider effect identity. Values
    /// are committed by digest; credential-bearing headers remain a separate
    /// name-only set so secret values can never enter durable evidence.
    #[allow(dead_code)] // consumed when effect-record v2 activates
    pub public_headers_v2:
        Vec<ryeos_state::objects::effect_record_v2::PublicHeaderCoordinateV2>,
    #[allow(dead_code)] // consumed when effect-record v2 activates
    pub credential_header_names_v2: Vec<String>,
    /// Exact serialized body bytes; transport sends these bytes verbatim.
    pub body_bytes: Vec<u8>,
    pub body_sha256: String,
    /// Effective provider-native output-token limit read back from the
    /// rendered body via the signed output-limit schema path.
    pub requested_output_tokens: Option<u64>,
    /// Credential frozen at prepare time (env read happens HERE, not at send).
    pub credential: Option<PreparedCredential>,
    /// Static non-auth headers sent with the request.
    pub headers: Vec<(String, String)>,
    /// sha256 hex over canonical JSON of
    /// `{method, url, sorted header names, body_sha256, requested_output_tokens}`.
    pub request_digest: String,
    /// Content-free request-shape telemetry captured while rendering the exact
    /// bytes above. Safe to log: lengths, counts, and a tool-schema digest only.
    pub request_metrics: PreparedRequestMetrics,
}

#[cfg(feature = "latency-profiling")]
#[derive(Clone, Debug, Default)]
pub struct PreparedRequestMetrics {
    pub prepare_duration_us: u64,
    pub body_bytes: u64,
    pub estimated_body_tokens: u64,
    pub source_message_count: u32,
    pub system_message_bytes: u64,
    pub user_message_bytes: u64,
    pub assistant_message_bytes: u64,
    pub tool_message_bytes: u64,
    pub reasoning_replay_bytes: u64,
    pub converted_messages_bytes: u64,
    pub extracted_system_prompt_bytes: u64,
    pub provider_tool_schema_bytes: u64,
    pub provider_tool_count: u32,
    pub provider_tool_schema_sha256: String,
}

#[cfg(not(feature = "latency-profiling"))]
#[derive(Clone, Debug, Default)]
pub struct PreparedRequestMetrics;

/// Build the exact provider request for one attempt. Mirrors what the
/// streaming transport used to assemble inline, but freezes every input —
/// endpoint, rendered body, effective output limit, credential — before any
/// reservation digest is taken.
pub fn prepare_provider_request(input: &StreamingCallInput<'_>) -> Result<PreparedProviderRequest> {
    #[cfg(feature = "latency-profiling")]
    let prepare_started = Instant::now();
    let provider = input.provider;
    let execution = input.execution;
    let model = input.model;

    let schemas = provider.schemas.as_ref().and_then(|s| s.messages.as_ref());
    let (converted_messages, system_prompt) =
        super::messages::convert_messages(input.messages, &schemas.cloned());

    let tool_schema = provider.schemas.as_ref().and_then(|s| s.tools.clone());
    let tools_val = super::tools::serialize_tools(input.tools, &tool_schema);
    #[cfg(feature = "latency-profiling")]
    let (
        converted_messages_bytes,
        provider_tool_schema_bytes,
        provider_tool_schema_sha256,
        system_message_bytes,
        user_message_bytes,
        assistant_message_bytes,
        tool_message_bytes,
        reasoning_replay_bytes,
    ) = {
        let converted_messages_bytes = serialized_len(&converted_messages);
        let provider_tool_schema_bytes = serialized_len(&tools_val);
        let provider_tool_schema_sha256 = serde_json::to_vec(&tools_val)
            .map(|bytes| streaming::sha256_hex(&bytes))
            .unwrap_or_else(|_| "<serialization-failed>".to_string());
        let mut system_message_bytes = 0_u64;
        let mut user_message_bytes = 0_u64;
        let mut assistant_message_bytes = 0_u64;
        let mut tool_message_bytes = 0_u64;
        let mut reasoning_replay_bytes = 0_u64;
        for message in input.messages {
            let bytes = serialized_len(message);
            match message.role.as_str() {
                "system" => system_message_bytes = system_message_bytes.saturating_add(bytes),
                "user" => user_message_bytes = user_message_bytes.saturating_add(bytes),
                "assistant" => {
                    assistant_message_bytes = assistant_message_bytes.saturating_add(bytes)
                }
                "tool" => tool_message_bytes = tool_message_bytes.saturating_add(bytes),
                _ => {}
            }
            reasoning_replay_bytes = reasoning_replay_bytes.saturating_add(
                message
                    .reasoning_content
                    .as_ref()
                    .map_or(0, |reasoning| reasoning.len() as u64),
            );
        }
        (
            converted_messages_bytes,
            provider_tool_schema_bytes,
            provider_tool_schema_sha256,
            system_message_bytes,
            user_message_bytes,
            assistant_message_bytes,
            tool_message_bytes,
            reasoning_replay_bytes,
        )
    };

    let stream_url = provider.extra.get("stream_url").and_then(|v| v.as_str());
    // Resolve {model} template in base_url (e.g. gemini profiles use
    // `{model}:streamGenerateContent`). Stream URL semantics:
    //   - None        → default to "/chat/completions"
    //   - Some("")    → base_url is the full endpoint; do not append
    //   - Some(other) → append (with leading slash if needed)
    let base_resolved = provider.base_url.replace("{model}", model);
    let url = match stream_url {
        Some("") => base_resolved,
        Some(su) => format!(
            "{}{}",
            base_resolved.trim_end_matches('/'),
            if su.starts_with('/') {
                su.to_string()
            } else {
                format!("/{}", su)
            }
        ),
        None => format!("{}/chat/completions", base_resolved.trim_end_matches('/')),
    };

    let mut body = build_request_body(
        provider,
        model,
        &converted_messages,
        system_prompt.as_deref(),
        &tools_val,
        !input.tools.is_empty(),
        (execution.max_provider_output_tokens_per_turn != 0)
            .then_some(execution.max_provider_output_tokens_per_turn),
    )?;

    // Sampling parameters — gated by provider capabilities so we never send
    // a field the upstream API will reject with a 400.
    inject_sampling(&mut body, provider.family, input.sampling);
    apply_declared_reasoning(
        &mut body,
        provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.reasoning.as_ref()),
        input.reasoning,
    )?;
    apply_declared_output_limit(
        &mut body,
        provider
            .schemas
            .as_ref()
            .and_then(|schemas| schemas.output_limit.as_ref()),
        (execution.max_provider_output_tokens_per_turn != 0)
            .then_some(execution.max_provider_output_tokens_per_turn),
    )?;
    let requested_output_tokens = declared_output_limit_from_body(provider, &body)?;

    // These bytes ARE the request. `serde_json::to_vec` here and nowhere
    // else; transport must never re-serialize `body`.
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| anyhow!("serialize prepared provider request body: {e}"))?;
    let body_len = u64::try_from(body_bytes.len())
        .map_err(|_| anyhow!("prepared provider request body length exceeds u64"))?;
    if input.provider_request_body_bytes_limit != 0
        && body_len > input.provider_request_body_bytes_limit
    {
        bail!(
            "provider_request_body_limit_exceeded: prepared request body is {body_len} bytes, exceeding the signed per-attempt limit of {} bytes; zero provider requests",
            input.provider_request_body_bytes_limit
        );
    }
    let body_sha256 = streaming::sha256_hex(&body_bytes);

    // Credential-generation model (plan §7.4, deliberate v1 property): the
    // credential VALUE is frozen here, at prepare time — before reserve and
    // issue — from the process environment the daemon injected at spawn.
    // Within one runtime process no resolver exists that could substitute a
    // newer generation after issue, so "transport resolves only the frozen
    // credential generation" holds structurally. Rotation reaches new
    // launches through a changed `credential_authority_generation` in the
    // signed provider config (a new config hash and sealed authority).
    // Residual, accepted: a revocation upstream between issue and send is
    // not locally detectable — the provider rejects the dead key and the
    // issued attempt settles at its reserved maximum, which is the same
    // conservative outcome the plan requires for a detected revocation.
    let credential = match provider.auth.env_var.as_deref() {
        Some(env_var) => {
            let value = std::env::var(env_var).map_err(|_| {
                anyhow!(
                    "provider auth env var {env_var} is not set; refusing to prepare a \
                     provider request with no credentials (typed-fail-loud)"
                )
            })?;
            Some(PreparedCredential {
                header_name: provider
                    .auth
                    .header_name
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_string()),
                prefix: provider
                    .auth
                    .prefix
                    .clone()
                    .unwrap_or_else(|| "Bearer ".to_string()),
                value,
            })
        }
        None => None,
    };

    let mut headers: Vec<(String, String)> = provider
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    headers.push(("Accept".to_string(), "text/event-stream".to_string()));
    // Raw-bytes transport must declare the content type itself (the old
    // `.json(&body)` path added it implicitly, without overriding a signed
    // provider header).
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }

    let (public_headers_v2, credential_header_names_v2) =
        ryeos_state::objects::effect_record_v2::provider_header_projection_v2(
            headers.iter().cloned(),
            credential
                .as_ref()
                .map(|credential| credential.header_name.clone()),
        )?;

    let mut header_names: Vec<String> = headers.iter().map(|(name, _)| name.clone()).collect();
    if let Some(credential) = &credential {
        header_names.push(credential.header_name.clone());
    }
    header_names.sort();

    let request_digest = ryeos_accounting::rpc::prepared_request_digest_from_parts(
        "POST",
        &url,
        &header_names,
        &body_sha256,
        requested_output_tokens,
    );
    #[cfg(feature = "latency-profiling")]
    let request_metrics = PreparedRequestMetrics {
        prepare_duration_us: u64::try_from(prepare_started.elapsed().as_micros())
            .unwrap_or(u64::MAX),
        body_bytes: body_bytes.len() as u64,
        // Deliberately labelled as an estimate. Exact provider tokenization is
        // reported later through usage accounting.
        estimated_body_tokens: (body_bytes.len() as u64).saturating_add(3) / 4,
        source_message_count: u32::try_from(input.messages.len()).unwrap_or(u32::MAX),
        system_message_bytes,
        user_message_bytes,
        assistant_message_bytes,
        tool_message_bytes,
        reasoning_replay_bytes,
        converted_messages_bytes,
        extracted_system_prompt_bytes: system_prompt.as_ref().map_or(0, |value| value.len() as u64),
        provider_tool_schema_bytes,
        provider_tool_count: u32::try_from(input.tools.len()).unwrap_or(u32::MAX),
        provider_tool_schema_sha256,
    };
    #[cfg(not(feature = "latency-profiling"))]
    let request_metrics = PreparedRequestMetrics::default();

    Ok(PreparedProviderRequest {
        method: reqwest::Method::POST,
        url,
        header_names,
        public_headers_v2,
        credential_header_names_v2,
        body_bytes,
        body_sha256,
        requested_output_tokens,
        credential,
        headers,
        request_digest,
        request_metrics,
    })
}

#[cfg(feature = "latency-profiling")]
fn serialized_len(value: &impl serde::Serialize) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::{
        ExecutionConfig, ProviderConfig, ProviderMessage, ReasoningConfig, ReasoningMode,
    };
    use ryeos_runtime::callback_client::CallbackClient;
    use ryeos_runtime::envelope::EnvelopeCallback;
    use serde_json::{Value, json};

    fn provider() -> ProviderConfig {
        serde_json::from_value(json!({
            "family": "chat_completions",
            "base_url": "https://example.invalid",
            "schemas": {
                "output_limit": {
                    "path": "max_tokens",
                    "semantics": "provider_native_output_tokens"
                },
                "reasoning": {
                    "mode": {
                        "path": "thinking.type",
                        "values": {"enabled": "on", "disabled": "off"}
                    }
                }
            },
            "body_template": {
                "model": "{model}",
                "messages": "{messages}",
                "stream": "{stream}"
            },
            "extra": {"stream_url": "/chat/completions"}
        }))
        .expect("provider fixture")
    }

    fn prepare(reasoning: Option<&ReasoningConfig>) -> PreparedProviderRequest {
        let provider = provider();
        let client = reqwest::Client::new();
        let execution = ExecutionConfig::default();
        let messages = [ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }];
        let callback_config = EnvelopeCallback {
            socket_path: std::path::PathBuf::from("/nonexistent/ryeos-callback.sock"),
            token: "unused".to_string(),
        };
        let callback = CallbackClient::new(
            &callback_config,
            "T-reasoning-request-fixture",
            "/project",
            "unused",
        );
        prepare_provider_request(&StreamingCallInput {
            client: &client,
            provider: &provider,
            provider_id: "fixture",
            matched_profile: None,
            config_hash: "0",
            execution: &execution,
            model: "fixture-model",
            messages: &messages,
            tools: &[],
            callback: &callback,
            turn: 1,
            attempt: 1,
            sampling: None,
            reasoning,
            provider_request_body_bytes_limit: 0,
            cancel_flag: None,
            interrupt_flag: None,
        })
        .expect("prepare provider request")
    }

    #[test]
    fn reasoning_policy_is_applied_before_request_bytes_and_digest_are_frozen() {
        let provider_default = prepare(None);
        let disabled = prepare(Some(&ReasoningConfig {
            mode: ReasoningMode::Disabled,
            effort: None,
        }));

        let provider_default_body: Value =
            serde_json::from_slice(&provider_default.body_bytes).expect("default request body");
        let disabled_body: Value =
            serde_json::from_slice(&disabled.body_bytes).expect("disabled request body");
        assert!(provider_default_body.get("thinking").is_none());
        assert_eq!(disabled_body["thinking"]["type"], "off");
        assert_ne!(provider_default.body_sha256, disabled.body_sha256);
        assert_ne!(provider_default.request_digest, disabled.request_digest);
    }

    #[test]
    fn prepared_v2_headers_commit_public_values_without_secret_values() {
        let prepared = prepare(None);
        let accept = prepared
            .public_headers_v2
            .iter()
            .find(|header| header.name == "accept")
            .expect("default Accept header is projected");
        assert_eq!(
            accept.value_digest,
            lillux::sha256_hex(b"text/event-stream")
        );
        assert!(prepared.credential_header_names_v2.is_empty());
    }

    #[test]
    fn signed_request_body_limit_refuses_preparation_before_transport() {
        let client = reqwest::Client::new();
        let provider = provider();
        let execution = ExecutionConfig::default();
        let messages = [ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }];
        let callback_config = EnvelopeCallback {
            socket_path: std::path::PathBuf::from("/nonexistent/ryeos-callback.sock"),
            token: "unused".to_string(),
        };
        let callback = CallbackClient::new(
            &callback_config,
            "T-request-body-limit-fixture",
            "/project",
            "unused",
        );
        let error = match prepare_provider_request(&StreamingCallInput {
            client: &client,
            provider: &provider,
            provider_id: "fixture",
            matched_profile: None,
            config_hash: "0",
            execution: &execution,
            model: "fixture-model",
            messages: &messages,
            tools: &[],
            callback: &callback,
            turn: 1,
            attempt: 1,
            sampling: None,
            reasoning: None,
            provider_request_body_bytes_limit: 1,
            cancel_flag: None,
            interrupt_flag: None,
        }) {
            Ok(_) => panic!("one-byte signed limit must refuse the prepared request"),
            Err(error) => error,
        };

        let message = format!("{error:#}");
        assert!(message.contains("provider_request_body_limit_exceeded"));
        assert!(message.contains("zero provider requests"));
    }
}
