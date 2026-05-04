//! Compiled route invokers — one trait, many implementations.
//!
//! Every route source becomes a compiled invoker at route-table-build time:
//! - `service:` refs → `CompiledServiceInvocation` (generic wrapper)
//! - Auth verifiers → `CompiledNoneVerifier`, `CompiledRyeSignedVerifier`, `CompiledHmacVerifier`
//! - Streaming sources → `CompiledGatewayLaunch`, `CompiledThreadsEventsStream`
//! - Launch mode → `CompiledLaunchInvocation`

pub mod hmac_invocation;
pub mod none_invocation;
pub mod rye_signed_invocation;
pub mod service_invocation;

use std::sync::Arc;

use serde_json::Value;

use crate::dispatch_error::RouteConfigError;
use crate::routes::invocation::CompiledRouteInvocation;

/// Compile an auth invoker from the route's `auth` field.
///
/// The `auth` field is a short key ("none", "rye_signed", "hmac") or a
/// canonical ref. This function validates config and returns a compiled
/// invoker that produces `RouteInvocationResult::Principal`.
pub fn compile_auth_invoker(
    auth_key: &str,
    auth_config: Option<&Value>,
    route_id: &str,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    match auth_key {
        "none" => Ok(Arc::new(none_invocation::CompiledNoneVerifier)),
        "rye_signed" => Ok(Arc::new(rye_signed_invocation::CompiledRyeSignedVerifier)),
        "hmac" => {
            let config = auth_config.ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: route_id.into(),
                src: "hmac_verifier".into(),
                reason: "auth_config is required".into(),
            })?;
            hmac_invocation::compile_hmac_verifier(route_id, config)
        }
        other => Err(RouteConfigError::UnknownVerifier {
            id: route_id.into(),
            name: other.into(),
        }),
    }
}

/// Compile a service invoker from a `service:` canonical ref.
///
/// Validates the ref, looks up the service descriptor, and returns
/// a generic `CompiledServiceInvocation`.
pub fn compile_service_invoker(
    source_ref: &str,
    route_id: &str,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    let parsed = crate::routes::parsed_ref::ParsedItemRef::parse(source_ref).map_err(|e| {
        RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!("source '{source_ref}' is not a valid canonical ref: {e}"),
        }
    })?;

    if parsed.kind() != "service" {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!(
                "source must be 'service:' kind, got '{}' kind in '{}'",
                parsed.kind(),
                source_ref
            ),
        });
    }

    let descriptor = crate::service_handlers::ALL
        .iter()
        .find(|d| d.service_ref == source_ref)
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!(
                "no service descriptor found for ref '{}'; available: [{}]",
                source_ref,
                crate::service_handlers::ALL
                    .iter()
                    .map(|d| d.service_ref)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

    if descriptor.availability
        == crate::service_executor::ServiceAvailability::OfflineOnly
    {
        return Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!(
                "service '{}' is OfflineOnly (only available when daemon is down)",
                source_ref
            ),
        });
    }

    Ok(Arc::new(service_invocation::CompiledServiceInvocation {
        endpoint: descriptor.endpoint.to_string(),
        required_caps: Vec::new(),
    }))
}
