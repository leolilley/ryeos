//! Browser-session adapter for the signed chain-tail source.
//!
//! Native `EventSource` cannot attach RyeOS request-signature headers. This
//! UI-owned source accepts the already-verified browser session, restores the
//! session's authenticated user principal for the chain ownership check, and
//! emits every upstream envelope as an unnamed SSE message so the browser does
//! not need a brittle allowlist of event names.

use std::sync::Arc;

use ryeos_api::route_error::{RouteConfigError, RouteDispatchError};
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use ryeos_api::routes::invokers::chain_tail_invocation::CompiledChainTailInvocation;
use ryeos_api::routes::response_modes::event_stream_mode::{
    validate_and_extract_path_capture, EventStreamStrategy, StreamSourceCompiler,
};
use ryeos_app::route_raw::RawRouteSpec;
use serde_json::Value;
use tokio_stream::StreamExt;

const SOURCE: &str = "browser_chain_tail";
const REQUIRED_AUTH: &str = "browser_session";

pub struct BrowserChainTailSourceFactory;

impl StreamSourceCompiler for BrowserChainTailSourceFactory {
    fn compile(&self, raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
        if raw.auth != REQUIRED_AUTH {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw.id.clone(),
                src: SOURCE.into(),
                required: REQUIRED_AUTH.into(),
                got: raw.auth.clone(),
            });
        }
        let source_config = &raw.response.source_config;
        let chain_root = source_config
            .get("chain_root_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: SOURCE.into(),
                reason: "missing 'chain_root_id' in source_config".into(),
            })?;
        let capture_name = validate_and_extract_path_capture(
            chain_root,
            SOURCE,
            "chain_root_id",
            &raw.id,
            &raw.path,
        )?;
        let keep_alive_secs = source_config
            .get("keep_alive_secs")
            .and_then(Value::as_u64)
            .unwrap_or(15);
        if keep_alive_secs == 0 {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: SOURCE.into(),
                reason: "keep_alive_secs must be > 0".into(),
            });
        }
        let invoker: Arc<dyn CompiledRouteInvocation> =
            Arc::new(CompiledBrowserChainTailInvocation { keep_alive_secs });
        Ok(EventStreamStrategy::PathCaptureInput {
            invoker,
            input_field: "chain_root_id".into(),
            capture_name,
        })
    }
}

struct CompiledBrowserChainTailInvocation {
    keep_alive_secs: u64,
}

static CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Required,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledBrowserChainTailInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &CONTRACT
    }

    async fn invoke(
        &self,
        mut ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let principal = ctx
            .principal
            .as_mut()
            .ok_or(RouteDispatchError::Unauthorized)?;
        let user_principal_id = principal
            .metadata
            .get("user_principal_id")
            .cloned()
            .unwrap_or_else(|| principal.id.clone());
        principal.id = user_principal_id;

        let upstream = CompiledChainTailInvocation {
            keep_alive_secs: self.keep_alive_secs,
        };
        let RouteInvocationResult::Stream(stream) = upstream.invoke(ctx).await? else {
            return Err(RouteDispatchError::Internal(
                "chain-tail adapter received a non-stream result".into(),
            ));
        };
        let keep_alive_secs = stream.keep_alive_secs;
        let events = stream.events.map(|item| {
            item.map(|mut envelope| {
                envelope.payload = serde_json::json!({
                    "event_type": envelope.event_type,
                    "payload": envelope.payload,
                });
                envelope.event_type = "message".to_string();
                envelope
            })
        });
        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: Box::pin(events),
            keep_alive_secs,
        }))
    }
}
