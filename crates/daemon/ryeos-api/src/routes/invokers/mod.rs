//! Compiled route invokers — one trait, many implementations.
//!
//! Every route source becomes a compiled invoker at route-table-build time:
//! - `service:` refs → `CompiledServiceInvocation` (generic wrapper)
//! - Auth verifiers → `CompiledNoneVerifier`, `CompiledRyeosSignedVerifier`, `CompiledHmacVerifier`
//! - Streaming sources → `CompiledGatewayLaunch`, `CompiledThreadsEventsStream`
//! - Launch mode → `CompiledLaunchInvocation`

pub mod chain_tail_invocation;
pub mod dispatch_invocation;
pub mod gateway_stream_invocation;
pub mod hmac_invocation;
pub mod launch_invocation;
pub mod none_invocation;
pub mod ryeos_signed_invocation;
pub mod service_invocation;
pub mod stream_helpers;
pub mod subscription_stream_invocation;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::route_error::RouteConfigError;
use crate::routes::invocation::CompiledRouteInvocation;

// ── Auth verifier factory trait ────────────────────────────────────────

/// Factory that compiles an auth verifier for a given auth key.
///
/// Built-in factories (`none`, `ryeos_signed`, `hmac`) are registered by
/// the API crate. Extension factories are registered by the composition root.
pub trait AuthVerifierFactory: Send + Sync {
    fn compile(
        &self,
        auth_config: Option<&Value>,
        route_id: &str,
    ) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError>;
}

// ── Built-in auth factories ────────────────────────────────────────────

struct NoneAuthFactory;

impl AuthVerifierFactory for NoneAuthFactory {
    fn compile(
        &self,
        _auth_config: Option<&Value>,
        _route_id: &str,
    ) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
        Ok(Arc::new(none_invocation::CompiledNoneVerifier))
    }
}

struct RyeosSignedAuthFactory;

impl AuthVerifierFactory for RyeosSignedAuthFactory {
    fn compile(
        &self,
        _auth_config: Option<&Value>,
        _route_id: &str,
    ) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
        Ok(Arc::new(
            ryeos_signed_invocation::CompiledRyeosSignedVerifier,
        ))
    }
}

struct HmacAuthFactory;

impl AuthVerifierFactory for HmacAuthFactory {
    fn compile(
        &self,
        auth_config: Option<&Value>,
        route_id: &str,
    ) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
        let config = auth_config.ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: "hmac_verifier".into(),
            reason: "auth_config is required".into(),
        })?;
        hmac_invocation::compile_hmac_verifier(route_id, config)
    }
}

// ── Auth invoker registry ──────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct AuthInvokerRegistry {
    verifiers: HashMap<String, Arc<dyn AuthVerifierFactory>>,
}

impl AuthInvokerRegistry {
    /// Registry with API built-in auth verifiers: `none`, `ryeos_signed`, `hmac`.
    pub fn with_api_builtins() -> Self {
        let mut r = Self::default();
        r.register("none", Arc::new(NoneAuthFactory));
        r.register("ryeos_signed", Arc::new(RyeosSignedAuthFactory));
        r.register("hmac", Arc::new(HmacAuthFactory));
        r
    }

    pub fn register(&mut self, name: impl Into<String>, factory: Arc<dyn AuthVerifierFactory>) {
        let name = name.into();
        if self.verifiers.contains_key(&name) {
            panic!("AuthInvokerRegistry: duplicate verifier `{name}`");
        }
        self.verifiers.insert(name, factory);
    }
}

/// Compile an auth invoker from the route's `auth` field.
///
/// The `auth` field is a short key ("none", "ryeos_signed", "hmac") or a
/// canonical ref. This function validates config and returns a compiled
/// invoker that produces `RouteInvocationResult::Principal`.
pub fn compile_auth_invoker(
    auth_key: &str,
    auth_config: Option<&Value>,
    route_id: &str,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    compile_auth_invoker_with_registry(
        auth_key,
        auth_config,
        route_id,
        &AuthInvokerRegistry::with_api_builtins(),
    )
}

pub fn compile_auth_invoker_with_registry(
    auth_key: &str,
    auth_config: Option<&Value>,
    route_id: &str,
    registry: &AuthInvokerRegistry,
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    let factory = registry.verifiers.get(auth_key);
    match factory {
        Some(f) => f.compile(auth_config, route_id),
        None => Err(RouteConfigError::UnknownVerifier {
            id: route_id.into(),
            name: auth_key.into(),
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
                authority: dispatch_invocation::DispatchAuthority::CallerPrincipal,
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
    compile_service_invoker_from(source_ref, route_id, crate::handlers::ALL)
}

/// Compile a `service:` ref using a provided descriptor set.
///
/// This is the composition-root variant: the daemon passes the full
/// composed descriptor table (API + UI), while API-only tests can pass
/// `handlers::ALL` directly.
pub fn compile_service_invoker_from(
    source_ref: &str,
    route_id: &str,
    descriptors: &[crate::registry::ServiceDescriptor],
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    let descriptor = descriptors
        .iter()
        .find(|d| d.service_ref == source_ref)
        .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!(
                "no service descriptor found for ref '{}'; available: [{}]",
                source_ref,
                descriptors
                    .iter()
                    .map(|d| d.service_ref)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

    if descriptor.availability == ryeos_executor::executor::ServiceAvailability::OfflineOnly {
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
        service_ref: source_ref.to_string(),
        endpoint: descriptor.endpoint.to_string(),
    }))
}

/// Compile an invoker from any canonical ref using a provided service
/// descriptor set.
///
/// Like `compile_canonical_ref_invoker` but the `service:` ref lookup
/// uses the provided descriptors instead of the API-only `handlers::ALL`.
pub fn compile_canonical_ref_invoker_with_descriptors(
    source_ref: &str,
    route_id: &str,
    descriptors: &[crate::registry::ServiceDescriptor],
) -> Result<Arc<dyn CompiledRouteInvocation>, RouteConfigError> {
    let parsed = crate::routes::parsed_ref::ParsedItemRef::parse(source_ref).map_err(|e| {
        RouteConfigError::InvalidSourceConfig {
            id: route_id.into(),
            src: source_ref.into(),
            reason: format!("source '{source_ref}' is not a valid canonical ref: {e}"),
        }
    })?;

    match parsed.kind() {
        "service" => compile_service_invoker_from(source_ref, route_id, descriptors),
        "tool" | "directive" | "graph" => {
            Ok(Arc::new(dispatch_invocation::CompiledDispatchInvoker {
                item_ref: source_ref.to_string(),
                authority: dispatch_invocation::DispatchAuthority::CallerPrincipal,
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
    #[should_panic(expected = "duplicate verifier")]
    fn duplicate_auth_verifier_registration_panics() {
        let mut registry = AuthInvokerRegistry::with_api_builtins();
        registry.register("none", Arc::new(NoneAuthFactory));
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
    fn compile_auth_invoker_ryeos_signed_succeeds() {
        let invoker = compile_auth_invoker("ryeos_signed", None, "r1").unwrap();
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
        let invoker = compile_canonical_ref_invoker("tool:ryeos/core/execute", "r1").unwrap();
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
    fn service_invoker_accepts_a_cap_protected_descriptor() {
        // node-sign declares node.maintenance and is available in live mode.
        // Runtime enforcement comes from the verified service item, not a
        // second descriptor copy stored on the compiled invoker.
        let invoker = compile_canonical_ref_invoker("service:node-sign", "r1").unwrap();
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

    // ── compile_canonical_ref_invoker_with_descriptors tests ─────────

    #[test]
    fn with_descriptors_accepts_known_service() {
        let invoker = compile_canonical_ref_invoker_with_descriptors(
            "service:threads/get",
            "r1",
            crate::handlers::ALL,
        )
        .unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn with_descriptors_rejects_unknown_service() {
        let result = compile_canonical_ref_invoker_with_descriptors(
            "service:nonexistent/handler",
            "r1",
            crate::handlers::ALL,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no service descriptor found"), "got: {msg}");
    }

    #[test]
    fn with_descriptors_accepts_extension_service() {
        // Verify that a non-API service compiles when its descriptor is
        // included in the provided set.
        use crate::registry::ServiceDescriptor;
        use ryeos_executor::executor::ServiceAvailability;

        fn fake_handler(
            _params: serde_json::Value,
            _ctx: ryeos_app::handler_context::HandlerContext,
            _state: std::sync::Arc<ryeos_app::state::AppState>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>,
        > {
            unreachable!("test-only descriptor")
        }

        let ext_descriptor = ServiceDescriptor {
            service_ref: "service:ui/session/current",
            endpoint: "ui.session.current",
            availability: ServiceAvailability::Both,
            required_caps: &[],
            handler: fake_handler,
        };
        let descriptors = &[ext_descriptor];
        let invoker = compile_canonical_ref_invoker_with_descriptors(
            "service:ui/session/current",
            "r1",
            descriptors,
        )
        .unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }

    #[test]
    fn with_descriptors_rejects_extension_service_when_not_provided() {
        // Same ref, empty descriptor set → must fail.
        let result =
            compile_canonical_ref_invoker_with_descriptors("service:ui/session/current", "r1", &[]);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("no service descriptor found"), "got: {msg}");
    }

    #[test]
    fn with_descriptors_accepts_tool_kind() {
        // Non-service refs work the same regardless of descriptor set.
        let invoker =
            compile_canonical_ref_invoker_with_descriptors("tool:ryeos/core/execute", "r1", &[])
                .unwrap();
        let contract = invoker.contract();
        assert!(matches!(
            contract.output,
            crate::routes::invocation::RouteInvocationOutput::Json
        ));
    }
}
