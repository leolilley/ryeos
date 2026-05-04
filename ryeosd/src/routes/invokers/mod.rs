//! Compiled route invokers — one trait, many implementations.
//!
//! Every route source becomes a compiled invoker at route-table-build time:
//! - `service:` refs → `CompiledServiceInvocation` (generic wrapper)
//! - Auth verifiers → `CompiledNoneVerifier`, `CompiledRyeSignedVerifier`, `CompiledHmacVerifier`
//! - Streaming sources → `CompiledGatewayLaunch`, `CompiledThreadsEventsStream`
//! - Launch mode → `CompiledLaunchInvocation`

pub mod dispatch_invocation;
pub mod gateway_stream_invocation;
pub mod hmac_invocation;
pub mod launch_invocation;
pub mod none_invocation;
pub mod rye_signed_invocation;
pub mod service_invocation;
pub mod stream_helpers;
pub mod subscription_stream_invocation;

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

/// Compile an invoker from any canonical ref.
///
/// Dispatches on the ref's kind:
/// - `service:` → `CompiledServiceInvocation` (in-process handler)
/// - `tool:` / `directive:` / `graph:` → `CompiledDispatchInvoker` (engine dispatch)
pub fn compile_canonical_ref_invoker(
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

    match parsed.kind() {
        "service" => compile_service_invoker_inner(source_ref, route_id, &parsed),
        "tool" | "directive" | "graph" => {
            Ok(Arc::new(dispatch_invocation::CompiledDispatchInvoker {
                item_ref: source_ref.to_string(),
            }))
        }
        other => Err(RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!(
                "unsupported source kind '{}' in '{}'; expected service/tool/directive/graph",
                other, source_ref
            ),
        }),
    }
}

/// Inner: compile a `service:` ref into a `CompiledServiceInvocation`.
fn compile_service_invoker_inner(
    source_ref: &str,
    route_id: &str,
    _parsed: &crate::routes::parsed_ref::ParsedItemRef,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
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
        required_caps: descriptor
            .required_caps
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }))
}

/// Backward-compatible alias — route code that calls `compile_service_invoker`
/// still works, but prefer `compile_canonical_ref_invoker` for new code.
pub fn compile_service_invoker(
    source_ref: &str,
    route_id: &str,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    compile_canonical_ref_invoker(source_ref, route_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_auth_invoker_unknown_verifier_rejected() {
        let result = compile_auth_invoker("unknown_key", None, "r1");
        match result {
            Err(RouteConfigError::UnknownVerifier { id, name }) => {
                assert_eq!(id, "r1");
                assert_eq!(name, "unknown_key");
            }
            Err(other) => panic!("expected UnknownVerifier, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn compile_auth_invoker_none_succeeds() {
        let invoker = compile_auth_invoker("none", None, "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Principal
        ));
    }

    #[test]
    fn compile_auth_invoker_rye_signed_succeeds() {
        let invoker = compile_auth_invoker("rye_signed", None, "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Principal
        ));
    }

    #[test]
    fn compile_auth_invoker_hmac_requires_config() {
        let result = compile_auth_invoker("hmac", None, "r1");
        match result {
            Err(RouteConfigError::InvalidSourceConfig { id, src, reason }) => {
                assert_eq!(id, "r1");
                assert_eq!(src, "hmac_verifier");
                assert!(reason.contains("auth_config is required"), "got: {reason}");
            }
            Err(other) => panic!("expected InvalidSourceConfig, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ── compile_canonical_ref_invoker tests ──────────────────────────────

    #[test]
    fn canonical_ref_compiler_accepts_service() {
        let invoker = compile_canonical_ref_invoker("service:threads/get", "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn canonical_ref_compiler_accepts_tool() {
        let invoker = compile_canonical_ref_invoker("tool:rye/core/execute", "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn canonical_ref_compiler_accepts_directive() {
        let invoker = compile_canonical_ref_invoker("directive:my/agent", "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn canonical_ref_compiler_accepts_graph() {
        let invoker = compile_canonical_ref_invoker("graph:workflows/approval", "r1").unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn canonical_ref_compiler_rejects_unknown_kind() {
        let result = compile_canonical_ref_invoker("fictional:item/path", "r1");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("unsupported source kind"), "got: {msg}");
    }

    #[test]
    fn service_invoker_extracts_real_caps() {
        // node-sign declares required_caps: &["node.maintenance"] and is ServiceAvailability::Both.
        let invoker = compile_canonical_ref_invoker("service:node-sign", "r1").unwrap();
        // The invoker should be a CompiledServiceInvocation with node.maintenance caps.
        // We can't directly inspect the caps (they're inside an Arc<dyn>),
        // but we can verify it compiled successfully for a cap-protected service.
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn service_invoker_rejects_unknown_service() {
        let result = compile_canonical_ref_invoker("service:nonexistent/handler", "r1");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no service descriptor found"), "got: {msg}");
    }

    #[test]
    fn backward_compat_alias_still_works() {
        // compile_service_invoker is a thin wrapper now.
        let invoker = compile_service_invoker("service:system/status", "r1").unwrap();
        assert!(matches!(
            invoker.contract().output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }
}
