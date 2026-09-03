//! Exact owner-bound status for one accepted launch coordinate.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub launch_id: String,
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    if !ryeos_app::state_store::is_canonical_launch_id(&req.launch_id) {
        return Err(HandlerError::NotFound);
    }
    let status = state
        .state_store
        .launch_planning_status(&req.launch_id, &ctx.fingerprint)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    Ok(json!(status))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:launch/status",
    endpoint: "launch.status",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_caller_selected_owner() {
        assert!(
            serde_json::from_value::<Request>(json!({
                "launch_id": "L-0123456789abcdef0123456789abcdef",
                "requested_by": "fp:other",
            }))
            .is_err()
        );
    }

    #[test]
    fn descriptor_is_the_single_canonical_launch_status_contract() {
        assert_eq!(DESCRIPTOR.service_ref, "service:launch/status");
        assert_eq!(DESCRIPTOR.endpoint, "launch.status");
        assert!(DESCRIPTOR.required_caps.is_empty());
    }
}
