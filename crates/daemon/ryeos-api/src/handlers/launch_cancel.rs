//! `launch.cancel` — cancel an owner-bound planning admission.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_app::state_store::LaunchCancellationResolution;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub launch_id: String,
}

fn planning_outcome(launch_id: String, status: &str, outcome_code: Option<String>) -> Value {
    json!({
        "launch_id": launch_id,
        "status": status,
        "outcome_code": outcome_code,
    })
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    if !req.launch_id.starts_with("L-") || req.launch_id.len() > 96 {
        return Err(HandlerError::NotFound);
    }
    let resolution = state
        .state_store
        .cancel_launch_planning(&req.launch_id, &ctx.fingerprint)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    match resolution {
        LaunchCancellationResolution::Cancelled => Ok(planning_outcome(
            req.launch_id,
            "cancelled",
            Some("cancelled_by_requester".to_string()),
        )),
        LaunchCancellationResolution::Bound { thread_id } => {
            // `launch.cancel` is the sole public authority checked here. The
            // owner-bound planning lookup above resolves the canonical thread,
            // then this delegation reuses the thread cancellation state
            // machine without requiring a second `threads.cancel` capability.
            crate::handlers::threads_cancel::handle(
                crate::handlers::threads_cancel::Request { thread_id },
                ctx,
                state,
            )
            .await
        }
        LaunchCancellationResolution::Terminal {
            state: terminal_state,
            outcome_code,
        } => Ok(planning_outcome(
            req.launch_id,
            &terminal_state,
            outcome_code,
        )),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:launch/cancel",
    endpoint: "launch.cancel",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.launch/cancel"],
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
    fn request_rejects_caller_selected_ownership_fields() {
        assert!(serde_json::from_value::<Request>(json!({
            "launch_id": "L-opaque",
            "requested_by": "fp:other",
        }))
        .is_err());
    }

    #[test]
    fn descriptor_is_the_single_canonical_launch_cancel_contract() {
        assert_eq!(DESCRIPTOR.service_ref, "service:launch/cancel");
        assert_eq!(DESCRIPTOR.endpoint, "launch.cancel");
        assert_eq!(
            DESCRIPTOR.required_caps,
            &["ryeos.execute.service.launch/cancel"]
        );
    }

    #[test]
    fn planning_terminal_outcomes_have_one_stable_typed_shape() {
        assert_eq!(
            planning_outcome(
                "L-failed".to_string(),
                "failed",
                Some("thread_creation_failed".to_string()),
            ),
            json!({
                "launch_id": "L-failed",
                "status": "failed",
                "outcome_code": "thread_creation_failed",
            })
        );
        assert_eq!(
            planning_outcome(
                "L-expired".to_string(),
                "expired",
                Some("daemon_restarted_before_thread_bind".to_string()),
            ),
            json!({
                "launch_id": "L-expired",
                "status": "expired",
                "outcome_code": "daemon_restarted_before_thread_bind",
            })
        );
        assert_eq!(
            planning_outcome(
                "L-cancelled".to_string(),
                "cancelled",
                Some("cancelled_by_requester".to_string()),
            ),
            json!({
                "launch_id": "L-cancelled",
                "status": "cancelled",
                "outcome_code": "cancelled_by_requester",
            })
        );
    }
}
