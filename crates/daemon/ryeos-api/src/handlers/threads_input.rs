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
use ryeos_state::objects::ThreadStatus;

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
        ThreadStatus::from_str_lossy(status),
        Some(
            ThreadStatus::Completed
                | ThreadStatus::Failed
                | ThreadStatus::Cancelled
                | ThreadStatus::Killed
                | ThreadStatus::TimedOut
                | ThreadStatus::Continued
        )
    )
}

/// Statuses a follow-up can braid a successor onto — must match the terminal
/// sources `StateStore::create_continuation` accepts. A settled turn outside
/// this set (`continued`/`cancelled`/`killed`/`timed_out`) is refused
/// synchronously rather than launched, so the caller never watches a pre-minted
/// successor id that was never persisted.
fn is_continuable(status: &str) -> bool {
    matches!(
        ThreadStatus::from_str_lossy(status),
        Some(ThreadStatus::Completed | ThreadStatus::Failed)
    )
}

/// Decide whether a follow-up can braid onto `predecessor`, or must be refused.
/// Returns the refusal notice, or `None` to proceed. Pure over the few fields
/// the decision needs, so it is unit-testable without a daemon — and so the
/// refusal runs **synchronously**, before the accepted-launch path returns a
/// pre-minted id.
///
/// Order matters: the already-continued check runs before [`is_continuable`].
/// An operator follow-up preserves a completed/failed predecessor's status, so
/// an already-followed-up turn still reads `completed` (continuable); without
/// the successor check first, a second follow-up would launch a duplicate that
/// `create_continuation`'s single-successor guard rejects async, leaving the
/// caller watching an id that was never persisted.
fn follow_up_refusal(status: &str, successor: Option<&str>, thread_id: &str) -> Option<String> {
    if !is_settled(status) {
        // Live run: runtime-command delivery is a service upgrade
        // (attention-design); refuse honestly rather than queue silently.
        return Some(format!(
            "thread {thread_id} is {status}; live delivery is not supported yet"
        ));
    }
    if let Some(successor) = successor {
        return Some(format!(
            "thread {thread_id} already continued as {successor} — follow that instead"
        ));
    }
    if !is_continuable(status) {
        return Some(format!(
            "thread {thread_id} is {status}; a follow-up can only continue a completed or failed turn"
        ));
    }
    None
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
            if let Some(notice) = follow_up_refusal(
                &detail.status,
                detail.successor_thread_id.as_deref(),
                &detail.thread_id,
            ) {
                return Ok(json!({
                    "thread_id": Value::Null,
                    "delivery": "refused",
                    "notice": notice,
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

    #[test]
    fn continuable_set_matches_create_continuation() {
        // Only completed/failed turns can be braided onto (mirrors
        // StateStore::create_continuation). A follow-up to any other settled
        // status must be refused synchronously, not launched.
        for status in ["completed", "failed"] {
            assert!(is_continuable(status), "{status} should be continuable");
        }
        for status in ["continued", "cancelled", "killed", "timed_out", "running"] {
            assert!(!is_continuable(status), "{status} must not be continuable");
        }
    }

    #[test]
    fn continuable_predecessor_without_successor_proceeds() {
        assert!(follow_up_refusal("completed", None, "T-1").is_none());
        assert!(follow_up_refusal("failed", None, "T-1").is_none());
    }

    #[test]
    fn already_continued_completed_source_is_refused_with_successor() {
        // The bug: a completed predecessor that was already followed up keeps
        // status `completed` (continuable), so it must be refused by the
        // successor check, not launched into a duplicate the async guard rejects.
        let notice = follow_up_refusal("completed", Some("T-succ"), "T-pred")
            .expect("an already-continued source must be refused");
        assert!(notice.contains("T-succ"), "notice should point at successor: {notice}");
        assert!(notice.contains("already continued"), "got: {notice}");

        // Same for a failed-then-continued predecessor.
        assert!(follow_up_refusal("failed", Some("T-succ"), "T-pred").is_some());
    }

    #[test]
    fn live_source_is_refused() {
        let notice = follow_up_refusal("running", None, "T-1").expect("live run refused");
        assert!(notice.contains("live delivery"), "got: {notice}");
    }

    #[test]
    fn settled_non_continuable_source_is_refused() {
        for status in ["cancelled", "killed", "timed_out"] {
            let notice = follow_up_refusal(status, None, "T-1")
                .unwrap_or_else(|| panic!("{status} must be refused"));
            assert!(notice.contains("can only continue"), "got: {notice}");
        }
    }
}
