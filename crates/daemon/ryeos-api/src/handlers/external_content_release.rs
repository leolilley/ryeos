//! Operator-authorized external-content binding release.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_app::operator_external_content::ReleaseRequest;

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::release(state, ctx, req).await?,
    )?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/release",
    endpoint: "external-content.release",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/release"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
