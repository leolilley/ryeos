//! Immutable prepared provider request (§9.2): prepare once, digest once,
//! send the exact bytes.
//!
//! Reservation and issue bind to this object's digest; transport consumes
//! its exact method, endpoint, frozen credential, headers, and body bytes
//! without rebuilding or re-resolving anything. This closes the gap where a
//! body hashed before reservation could differ from the body constructed at
//! send time. The credential VALUE stays outside every digest — only its
//! declared header name participates.

use anyhow::{anyhow, Result};

use super::streaming::{
    self, apply_declared_output_limit, build_request_body, declared_output_limit_from_body,
    inject_sampling, StreamingCallInput,
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
}

/// Build the exact provider request for one attempt. Mirrors what the
/// streaming transport used to assemble inline, but freezes every input —
/// endpoint, rendered body, effective output limit, credential — before any
/// reservation digest is taken.
pub fn prepare_provider_request(input: &StreamingCallInput<'_>) -> Result<PreparedProviderRequest> {
    let provider = input.provider;
    let execution = input.execution;
    let model = input.model;

    let schemas = provider.schemas.as_ref().and_then(|s| s.messages.as_ref());
    let (converted_messages, system_prompt) =
        super::messages::convert_messages(input.messages, &schemas.cloned());

    let tool_schema = provider.schemas.as_ref().and_then(|s| s.tools.clone());
    let tools_val = super::tools::serialize_tools(input.tools, &tool_schema);

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

    let mut header_names: Vec<String> = headers.iter().map(|(name, _)| name.clone()).collect();
    if let Some(credential) = &credential {
        header_names.push(credential.header_name.clone());
    }
    header_names.sort();

    let digest_input = serde_json::json!({
        "method": "POST",
        "url": &url,
        "header_names": &header_names,
        "body_sha256": &body_sha256,
        "requested_output_tokens": requested_output_tokens,
    });
    let canonical = lillux::cas::canonical_json(&digest_input)
        .map_err(|e| anyhow!("canonicalize prepared-request digest input: {e}"))?;
    let request_digest = streaming::sha256_hex(canonical.as_bytes());

    Ok(PreparedProviderRequest {
        method: reqwest::Method::POST,
        url,
        header_names,
        body_bytes,
        body_sha256,
        requested_output_tokens,
        credential,
        headers,
        request_digest,
    })
}
