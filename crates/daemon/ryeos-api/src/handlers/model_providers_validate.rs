//! Provider-native credential validation driven by verified provider setup metadata.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::EffectiveItemRequest;
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub provider_id: String,
    #[serde(default)]
    pub model: Option<String>,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    if req.provider_id.is_empty()
        || req.provider_id.len() > 128
        || !req
            .provider_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
    {
        return Err(HandlerError::BadRequest("invalid provider_id".to_string()));
    }
    let item_ref = CanonicalRef::parse(&format!(
        "config:ryeos-runtime/model-providers/{}",
        req.provider_id
    ))
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let effective = state
        .engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: None,
            project_root: None,
        })
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let provider: ryeos_directive_core::ProviderConfig =
        serde_json::from_value(effective.composed_value)
            .map_err(|error| HandlerError::Internal(error.to_string()))?;
    provider
        .validate(&format!(" for '{}'", req.provider_id))
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let setup = provider
        .setup_projection(&req.provider_id)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let validation = setup.validation.ok_or_else(|| {
        HandlerError::BadRequest(format!(
            "provider '{}' does not declare a validation operation",
            req.provider_id
        ))
    })?;
    if validation.r#ref != DESCRIPTOR.service_ref {
        return Err(HandlerError::BadRequest(format!(
            "provider '{}' declares unsupported validation operation '{}'",
            req.provider_id, validation.r#ref
        )));
    }
    if let Some(model) = req.model.as_deref() {
        if !setup.models.iter().any(|declared| declared.name == model) {
            return Err(HandlerError::BadRequest(format!(
                "provider '{}' does not declare model '{}' for setup",
                req.provider_id, model
            )));
        }
    }
    let runtime_provider = req
        .model
        .as_deref()
        .map(|model| provider.resolve_for_model(model))
        .unwrap_or_else(|| provider.clone());
    runtime_provider
        .validate(&format!(" for '{}' validation", req.provider_id))
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let credential = match setup.credential {
        Some(credential) => {
            let mut secrets = state
                .vault
                .read_all(&ctx.fingerprint)
                .map_err(|error| HandlerError::Internal(error.to_string()))?;
            let selected = match secrets.remove(&credential.secret_name) {
                Some(selected) => selected,
                None => {
                    for other in secrets.values_mut() {
                        other.zeroize();
                    }
                    return Err(HandlerError::BadRequest(format!(
                        "provider '{}' credential '{}' is not configured",
                        req.provider_id, credential.secret_name
                    )));
                }
            };
            for other in secrets.values_mut() {
                other.zeroize();
            }
            Some(Zeroizing::new(selected))
        }
        None => None,
    };
    let model = req.model.as_deref().unwrap_or_default();
    let url = validation.url.replace("{model}", model);
    let parsed_url = url::Url::parse(&url).map_err(|error| {
        HandlerError::Internal(format!("verified validation URL is invalid: {error}"))
    })?;
    let pinned_resolution = resolve_validation_target(&parsed_url, credential.is_some()).await?;
    let mut client_builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(validation.timeout_seconds.min(30)))
        .timeout(Duration::from_secs(validation.timeout_seconds.min(60)))
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, address)) = pinned_resolution {
        client_builder = client_builder.resolve(&host, address);
    }
    let client = client_builder
        .build()
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let mut request = client.get(&url);
    for (name, value) in &runtime_provider.headers {
        request = request.header(name, value);
    }
    if let (Some(credential), Some(header_name)) = (
        credential.as_ref().map(|value| value.as_str()),
        runtime_provider.auth.header_name.as_deref(),
    ) {
        let authorization = Zeroizing::new(format!(
            "{}{}",
            runtime_provider.auth.prefix.as_deref().unwrap_or_default(),
            credential
        ));
        request = request.header(header_name, authorization.as_str());
    }
    let mut response = request.send().await.map_err(|error| {
        let detail = redact(
            &error.to_string(),
            credential.as_ref().map(|value| value.as_str()),
        );
        HandlerError::BadRequest(format!(
            "provider '{}' validation request failed: {}",
            req.provider_id,
            detail.as_str()
        ))
    })?;
    let status = response.status();
    if !status.is_success() {
        let mut body = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            HandlerError::BadRequest(format!(
                "provider '{}' validation error body failed: {error}",
                req.provider_id
            ))
        })? {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err(HandlerError::BadRequest(format!(
                    "provider '{}' validation returned HTTP {} with an oversized error body",
                    req.provider_id, status
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let rendered = std::str::from_utf8(body.as_slice()).unwrap_or("[non-UTF-8 response body]");
        let sanitized = Zeroizing::new(
            rendered
                .chars()
                .map(|character| {
                    if character.is_control() {
                        '�'
                    } else {
                        character
                    }
                })
                .collect::<String>(),
        );
        let detail = redact(
            sanitized.as_str(),
            credential.as_ref().map(|value| value.as_str()),
        );
        return Err(HandlerError::BadRequest(format!(
            "provider '{}' validation returned HTTP {}: {}",
            req.provider_id,
            status,
            detail.as_str()
        )));
    }
    Ok(serde_json::json!({
        "provider_id": req.provider_id,
        "connected": true,
        "status": status.as_u16(),
        "may_incur_cost": validation.may_incur_cost,
    }))
}

async fn resolve_validation_target(
    url: &url::Url,
    sends_credential: bool,
) -> HandlerResult<Option<(String, SocketAddr)>> {
    let host = url
        .host_str()
        .ok_or_else(|| HandlerError::Internal("verified validation URL has no host".to_string()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| HandlerError::Internal("verified validation URL has no port".to_string()))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        validate_target_ip(ip, sends_credential)?;
        return Ok(None);
    }
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| {
            HandlerError::BadRequest(format!("provider validation DNS lookup failed: {error}"))
        })?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(HandlerError::BadRequest(
            "provider validation DNS lookup returned no addresses".to_string(),
        ));
    }
    for address in &addresses {
        validate_target_ip(address.ip(), sends_credential)?;
    }
    Ok(Some((host.to_string(), addresses[0])))
}

fn validate_target_ip(ip: IpAddr, sends_credential: bool) -> HandlerResult<()> {
    let loopback = ip.is_loopback();
    let unsafe_network = match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8
        }
    };
    let credential_to_loopback = sends_credential && loopback;
    let unsafe_non_loopback = unsafe_network && !loopback;
    if credential_to_loopback || unsafe_non_loopback {
        return Err(HandlerError::BadRequest(
            "provider validation target resolves to a disallowed network".to_string(),
        ));
    }
    Ok(())
}

fn redact(value: &str, secret: Option<&str>) -> Zeroizing<String> {
    Zeroizing::new(match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => value.replace(secret, "[REDACTED]"),
        None => value.to_string(),
    })
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:model-providers/validate",
    endpoint: "model-providers.validate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.model-providers/validate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_errors_redact_exact_secret_values() {
        let rendered = redact(
            "authentication failed for secret-example-value",
            Some("secret-example-value"),
        );
        assert_eq!(rendered.as_str(), "authentication failed for [REDACTED]");
        assert!(!rendered.contains("secret-example-value"));
    }

    #[test]
    fn validation_network_policy_allows_only_credentialless_loopback() {
        assert!(validate_target_ip("127.0.0.1".parse().unwrap(), false).is_ok());
        assert!(validate_target_ip("127.0.0.1".parse().unwrap(), true).is_err());
        assert!(validate_target_ip("10.0.0.1".parse().unwrap(), false).is_err());
        assert!(validate_target_ip("169.254.1.1".parse().unwrap(), false).is_err());
    }
}
