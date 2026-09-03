//! Operator-authorized large-object integrity scrub.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub async fn handle(ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(
        ryeos_app::operator_external_content::scrub(state, ctx).await?,
    )?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/scrub",
    endpoint: "external-content.scrub",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/scrub"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            if !params.is_null() && params != serde_json::json!({}) {
                anyhow::bail!("external-content scrub accepts no parameters");
            }
            handle(ctx, state).await
        })
    },
};
