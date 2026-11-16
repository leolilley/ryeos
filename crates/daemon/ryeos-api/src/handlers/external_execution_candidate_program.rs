//! Operator-authorized, read-only emission of the admitted external candidate
//! program and its runtime content qualification binding.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::operator_external_content::qualification_testimony::RemoteContentTestimonyRefusal;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_app::operator_external_content::candidate_program::CandidateProgramRequest;

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let response = ryeos_app::operator_external_content::candidate_program::admit(state, ctx, req)
        .await
        .map_err(remote_content_refusal)?;
    Ok(serde_json::to_value(response)?)
}

/// A remote content testimony this node refuses to use for its own activation
/// is a conflict the operator can resolve, so it reaches the caller as a 409
/// carrying its stable code and fix rather than as an internal error.
fn remote_content_refusal(error: anyhow::Error) -> anyhow::Error {
    let Some(refusal) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RemoteContentTestimonyRefusal>())
    else {
        return error;
    };
    HandlerError::Structured {
        code: refusal.code().to_owned(),
        status: 409,
        body: serde_json::json!({
            "code": refusal.code(),
            "error": refusal.to_string(),
            "fix": refusal.fix(),
        }),
    }
    .into()
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/admit-candidate-program",
    endpoint: "external-execution.admit-candidate-program",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-execution/admit-candidate-program"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_content_refusal_is_a_conflict_with_its_code_and_fix() {
        let refusal = RemoteContentTestimonyRefusal::NoLocalActivation {
            activation_ref: "config:fixtures/runtime-activation".into(),
        };
        let error = remote_content_refusal(
            anyhow::Error::new(refusal.clone()).context("candidate program admission"),
        );
        let Some(HandlerError::Structured { code, status, body }) =
            crate::handler_error::extract_handler_error(&error)
        else {
            panic!("a typed refusal must become a structured handler error");
        };
        assert_eq!(code, "remote_content_no_local_activation");
        assert_eq!(status, 409);
        assert_eq!(body["fix"], refusal.fix());
        assert!(
            body["fix"]
                .as_str()
                .unwrap()
                .contains("ryeos external-content activate config:fixtures/runtime-activation")
        );
        // Any other error passes through untouched.
        let other = remote_content_refusal(anyhow::anyhow!("unrelated"));
        assert!(crate::handler_error::extract_handler_error(&other).is_none());
    }
}
