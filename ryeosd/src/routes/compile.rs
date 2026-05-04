use std::collections::HashMap;
use std::sync::Arc;

use axum::http::Method;

use super::invocation::{CompiledRouteInvocation, RoutePrincipal};
use super::raw::RawRouteSpec;
use crate::dispatch_error::RouteConfigError;

pub struct CompiledRoute {
    pub id: String,
    pub source_file: std::path::PathBuf,
    pub path_pattern: String,
    pub methods: Vec<Method>,
    pub auth_invoker: Arc<dyn CompiledRouteInvocation>,
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

pub struct RouteDispatchContext {
    pub captures: HashMap<String, String>,
    pub request_parts: axum::http::request::Parts,
    pub body_raw: Vec<u8>,
    pub principal: RoutePrincipal,
    pub state: crate::state::AppState,
}

pub trait ResponseMode: Send + Sync {
    fn key(&self) -> &'static str;
    fn allows_zero_timeout(&self) -> bool {
        false
    }
    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError>;
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
    ) -> Result<axum::response::Response, crate::dispatch_error::RouteDispatchError>;
}
