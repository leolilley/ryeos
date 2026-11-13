//! The node's live view of what still holds external capacity or a credential
//! profile, for the local operator and `ryeos node doctor`.

use std::sync::Arc;

use serde::Deserialize;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {}

async fn status(
    _req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<serde_json::Value, HandlerError> {
    // Node-wide: it names every operator's held profiles and capacity, so
    // only this node's own configured operator may read it.
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)
        .map_err(|_| HandlerError::Forbidden("local configured operator required".into()))?;
    let status = ryeos_app::external_execution_status::read(&state.state_store)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    serde_json::to_value(status).map_err(|error| HandlerError::Internal(error.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/status",
    endpoint: "external-execution.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-execution/status"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            status(req, ctx, state).await.map_err(Into::into)
        })
    },
};
