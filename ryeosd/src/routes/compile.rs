use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use serde_json::Value;

use super::raw::RawRouteSpec;
use crate::dispatch_error::{RouteConfigError, RouteDispatchError};

pub struct CompiledRoute {
    pub id: String,
    pub source_file: std::path::PathBuf,
    pub path_pattern: String,
    pub methods: Vec<Method>,
    pub auth: Arc<dyn CompiledAuthVerifier>,
    pub limits: CompiledLimits,
    pub response_mode: Arc<dyn CompiledResponseMode>,
    pub raw_response: RawRouteSpec,
    pub semaphore: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, Clone)]
pub struct CompiledLimits {
    pub body_bytes_max: u64,
    pub timeout_ms: u64,
    pub concurrent_max: u32,
}

pub struct RoutePrincipal {
    pub id: String,
    pub scopes: Vec<String>,
    pub verifier_key: &'static str,
    pub verified: bool,
    /// Verifier-supplied metadata for downstream consumption by
    /// response modes. Examples populated by the `hmac` verifier:
    ///   * `delivery_id` — verifier-extracted unique id (used for
    ///     dedupe key composition and surfaced in the launch envelope)
    ///   * `header.<lowercase-name>` — value of an allow-listed
    ///     forwarded request header (one entry per matched header)
    ///
    /// `BTreeMap` rather than `HashMap` so JSON serialization order
    /// is deterministic — webhook directives often hash params for
    /// idempotency, and a stable key order is part of that contract.
    /// Scopes stay strictly authorization data; metadata never gates
    /// access. The daemon never invents vendor labels; if a route
    /// YAML wants a vendor tag, it forwards an upstream header via
    /// `forwarded_headers`.
    pub metadata: BTreeMap<String, String>,
}

impl RoutePrincipal {
    /// Convenience constructor with empty scopes + metadata.
    pub fn anonymous(id: String, verifier_key: &'static str) -> Self {
        Self {
            id,
            scopes: Vec::new(),
            verifier_key,
            verified: false,
            metadata: BTreeMap::new(),
        }
    }
}

pub struct VerifierRequestContext<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub body_raw: &'a [u8],
}

pub struct RouteDispatchContext {
    pub captures: HashMap<String, String>,
    pub request_parts: axum::http::request::Parts,
    pub body_raw: Vec<u8>,
    pub principal: RoutePrincipal,
    pub state: crate::state::AppState,
}

pub trait AuthVerifier: Send + Sync {
    fn key(&self) -> &'static str;
    fn validate_route_config(
        &self,
        route_id: &str,
        auth_config: Option<&Value>,
    ) -> Result<Arc<dyn CompiledAuthVerifier>, RouteConfigError>;
}

pub trait CompiledAuthVerifier: Send + Sync {
    fn verify(
        &self,
        route_id: &str,
        req: &VerifierRequestContext,
        state: &crate::state::AppState,
    ) -> Result<RoutePrincipal, RouteDispatchError>;
}

pub trait ResponseMode: Send + Sync {
    fn key(&self) -> &'static str;
    fn allows_zero_timeout(&self) -> bool {
        false
    }
    fn compile(
        &self,
        raw: &RawRouteSpec,
        ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError>;
}

pub struct ModeCompileContext<'a> {
    pub _phantom: std::marker::PhantomData<&'a ()>,
}

#[axum::async_trait]
pub trait CompiledResponseMode: Send + Sync {
    fn is_streaming(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn std::any::Any;
    async fn handle(
        &self,
        compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError>;
}
