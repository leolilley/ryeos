use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use serde_json::Value;

use super::raw::RawRouteSpec;
use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::streaming_sources::StreamingSourceRegistry;

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
    pub streaming_sources: &'a StreamingSourceRegistry,
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
