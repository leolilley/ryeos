//! `threads.input` — deliver operator input to a directive thread chain.
//!
//! The seat's ground verb lands here. The service owns the delivery
//! decision; clients never know there are two mechanisms:
//!
//! - no `thread` / settled thread → launch the profile directive as a
//!   chained-resume turn (`previous_thread_id`); each turn is a new
//!   thread in the chain — continuity is the chain, not an entity;
//! - live thread → structured refusal in v1 (the runtime-command
//!   delivery path is the attention-design upgrade).
//!
//! Submit result contract (input-design.md):
//! `{ thread_id?, delivery: launched|submitted|refused, notice? }`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Operator text, bound whole — never tokenized here.
    pub input: String,
    /// Chain head to continue from (the route's ratchet value).
    #[serde(default)]
    pub thread: Option<String>,
    /// Directive profile for a fresh chain. Optional on follow-ups: the
    /// prior thread's item_ref is the continuity default.
    #[serde(default)]
    pub directive: Option<String>,
    /// Project root for resolution.
    #[serde(default)]
    pub project_path: Option<String>,
}

fn is_settled(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    if req.input.trim().is_empty() {
        return Err(HandlerError::BadRequest("input is empty".to_string()));
    }

    // Resolve the prior turn, if the route carries one.
    let previous = match &req.thread {
        None => None,
        Some(thread_id) => {
            let detail = state
                .state_store
                .get_thread(thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?
                .ok_or(HandlerError::NotFound)?;
            ctx.require_owner(detail.requested_by.as_deref())?;
            if !is_settled(&detail.status) {
                // Live run: runtime-command delivery is a service upgrade
                // (attention-design); refuse honestly rather than queue
                // silently.
                return Ok(json!({
                    "thread_id": Value::Null,
                    "delivery": "refused",
                    "notice": format!(
                        "thread {} is {}; live delivery is not supported yet",
                        detail.thread_id, detail.status
                    ),
                }));
            }
            Some(detail)
        }
    };

    // Directive: explicit param wins; otherwise continuity default — the
    // prior turn's item_ref. The client never names kinds; dispatch
    // decides whether the ref is root-executable.
    let directive = req
        .directive
        .clone()
        .or_else(|| previous.as_ref().map(|detail| detail.item_ref.clone()))
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "no directive profile: provide `directive` or a `thread` to continue".to_string(),
            )
        })?;

    let parsed_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&directive)
        .map_err(|e| HandlerError::BadRequest(format!("directive ref: {e}")))?;
    let project_path = req
        .project_path
        .clone()
        .ok_or_else(|| HandlerError::BadRequest("project_path is required".to_string()))?;
    let project_path = crate::routes::abs_path::AbsolutePathBuf::try_new(project_path.into())
        .map_err(|e| HandlerError::BadRequest(format!("project_path: {e}")))?;

    let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let parameters = json!({ "input": req.input });
    // Accepted-launch pattern: spawn the dispatch, drop the handle, return
    // immediately — the braid is the place to watch it.
    let _handle = crate::routes::launch::spawn_dispatch_launch(
        &state,
        parsed_ref,
        project_path,
        parameters,
        ctx.fingerprint.clone(),
        ctx.scopes.clone(),
        thread_id.clone(),
        crate::routes::launch::DispatchLaunchOptions {
            previous_thread_id: previous.as_ref().map(|detail| detail.thread_id.clone()),
            ..Default::default()
        },
    );

    Ok(json!({
        "thread_id": thread_id,
        "delivery": "launched",
        "notice": Value::Null,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/input",
    endpoint: "threads.input",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.threads/input"],
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
    fn settled_statuses_match_state_store_vocabulary() {
        for status in [
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert!(is_settled(status), "{status} should be settled");
        }
        for status in ["running", "accepted", "pending"] {
            assert!(!is_settled(status), "{status} should be live");
        }
    }
}
