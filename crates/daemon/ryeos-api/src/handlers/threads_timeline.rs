//! `threads.timeline` — a chain's events folded into timeline items by the
//! installed timeline definition.
//!
//! Authorised like `events.chain_replay`: the caller must own the chain's root
//! thread. The fold itself is the shared host in [`crate::timeline_host`]; the
//! UI endpoint serves the same response under a browser-seat authorisation.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use crate::timeline_host::{TimelineParams, timeline_response};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// The caller owns the thread, and the thread is the chain's root. Anything
/// else is indistinguishable from a missing chain.
fn authorize_chain_root(
    ctx: &HandlerContext,
    wanted_chain_root_id: &str,
    thread_chain_root_id: &str,
    requested_by: Option<&str>,
) -> Result<(), HandlerError> {
    ctx.require_owner(requested_by)?;
    if thread_chain_root_id != wanted_chain_root_id {
        return Err(HandlerError::NotFound);
    }
    Ok(())
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let params = TimelineParams::parse(params)?;
    let root = state
        .state_store
        .get_thread(&params.chain_root_id)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
        .ok_or(HandlerError::NotFound)?;
    authorize_chain_root(
        &ctx,
        &params.chain_root_id,
        &root.chain_root_id,
        root.requested_by.as_deref(),
    )?;
    timeline_response(&state, &params, &root.status)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/timeline",
    endpoint: "threads.timeline",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_test_state;

    fn ctx(fingerprint: &str) -> HandlerContext {
        HandlerContext::new(fingerprint.to_string(), Vec::new(), true)
    }

    #[test]
    fn the_owner_of_the_chain_root_is_allowed() {
        authorize_chain_root(&ctx("fp:owner"), "T-1", "T-1", Some("fp:owner")).unwrap();
    }

    #[test]
    fn a_non_owner_is_refused_as_not_found() {
        let refused = authorize_chain_root(&ctx("fp:other"), "T-1", "T-1", Some("fp:owner"));
        assert!(matches!(refused, Err(HandlerError::NotFound)));
        let unowned = authorize_chain_root(&ctx("fp:owner"), "T-1", "T-1", None);
        assert!(matches!(unowned, Err(HandlerError::NotFound)));
        let unverified = HandlerContext::new("fp:owner".to_string(), Vec::new(), false);
        let refused = authorize_chain_root(&unverified, "T-1", "T-1", Some("fp:owner"));
        assert!(matches!(refused, Err(HandlerError::NotFound)));
    }

    #[test]
    fn a_thread_that_is_not_the_chain_root_is_refused() {
        let refused = authorize_chain_root(&ctx("fp:owner"), "T-2", "T-1", Some("fp:owner"));
        assert!(matches!(refused, Err(HandlerError::NotFound)));
    }

    #[test]
    fn a_missing_definition_is_refused_clearly() {
        let (_dir, _store, state) = build_test_state();
        let params = TimelineParams::parse(serde_json::json!({"chain_root_id": "T-1"})).unwrap();
        let error = timeline_response(&state, &params, "running").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("timeline definition is not installed"),
            "{error:#}"
        );
        assert!(matches!(
            error.downcast_ref::<HandlerError>(),
            Some(HandlerError::Conflict(_))
        ));
    }
}
