use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ModeCompileContext, ResponseMode, RouteDispatchContext,
};
use crate::routes::raw::RawRouteSpec;

pub struct PlaceholderMode;

impl ResponseMode for PlaceholderMode {
    fn key(&self) -> &'static str {
        "placeholder"
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
        _ctx: &ModeCompileContext,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        Ok(Arc::new(CompiledPlaceholder {
            route_id: raw.id.clone(),
        }))
    }
}

pub struct CompiledPlaceholder {
    route_id: String,
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledPlaceholder {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        _ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let body = serde_json::json!({
            "error": "response mode not yet implemented",
            "route_id": self.route_id,
        });
        Ok((
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(body),
        )
            .into_response())
    }
}
