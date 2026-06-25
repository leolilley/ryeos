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
/// Returns the refusal notice, or `None` to proceed. Pure over the few fields the
/// decision needs, so it is unit-testable without a daemon and runs synchronously.
///
/// This decides ELIGIBILITY only (live source / non-continuable status). It does
/// NOT refuse merely because a successor already exists: an idempotent
/// double-submit is deduped downstream by fingerprint in
/// `create_or_get_operator_continuation` (same input → same successor; different
/// input → conflict), which returns the real successor id either way.
fn follow_up_refusal(status: &str, thread_id: &str) -> Option<String> {
    if !is_settled(status) {
        // Live run: runtime-command delivery is a service upgrade
        // (attention-design); refuse honestly rather than queue silently.
        return Some(format!(
            "thread {thread_id} is {status}; live delivery is not supported yet"
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

    // Resolve the prior turn, if the route carries one. Eligibility only — a
    // live source or a non-continuable settled status is refused; an existing
    // successor is NOT a refusal (the create-or-get below dedups by fingerprint).
    let previous = match &req.thread {
        None => None,
        Some(thread_id) => {
            let detail = state
                .state_store
                .get_thread(thread_id)
                .map_err(|e| HandlerError::Internal(e.to_string()))?
                .ok_or(HandlerError::NotFound)?;
            ctx.require_owner(detail.requested_by.as_deref())?;
            if let Some(notice) = follow_up_refusal(&detail.status, &detail.thread_id) {
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

    let project_path = req
        .project_path
        .clone()
        .ok_or_else(|| HandlerError::BadRequest("project_path is required".to_string()))?;
    let project_path = crate::routes::abs_path::AbsolutePathBuf::try_new(project_path.into())
        .map_err(|e| HandlerError::BadRequest(format!("project_path: {e}")))?;
    let parameters = json!({ "input": req.input });

    // Fresh chain (no prior turn): the existing accepted-launch path — spawn the
    // dispatch, drop the handle, return immediately.
    let Some(previous) = previous else {
        let parsed_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&directive)
            .map_err(|e| HandlerError::BadRequest(format!("directive ref: {e}")))?;
        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        let _handle = crate::routes::launch::spawn_dispatch_launch(
            &state,
            parsed_ref,
            project_path,
            parameters,
            ctx.fingerprint.clone(),
            ctx.scopes.clone(),
            thread_id.clone(),
            crate::routes::launch::DispatchLaunchOptions::default(),
        );
        return Ok(json!({
            "thread_id": thread_id,
            "delivery": "launched",
            "notice": Value::Null,
        }));
    };

    // Operator follow-up: SYNCHRONOUSLY create-or-get the canonical successor so
    // the returned id is always real (never a phantom pre-minted id), then launch
    // the pre-created row. The successor inherits the source's execution identity
    // (kind / executor / launch_mode / sites); the launcher re-resolves the item
    // fresh, so a changed directive ref still resolves correctly.
    use ryeos_engine::contracts::{EffectivePrincipal, Principal, ProjectContext};
    let candidate_id = ryeos_app::thread_lifecycle::new_thread_id();
    let successor_record = ryeos_app::state_store::NewThreadRecord {
        thread_id: candidate_id.clone(),
        chain_root_id: previous.chain_root_id.clone(),
        kind: previous.kind.clone(),
        item_ref: directive.clone(),
        executor_ref: previous.executor_ref.clone(),
        launch_mode: previous.launch_mode.clone(),
        current_site_id: previous.current_site_id.clone(),
        origin_site_id: previous.origin_site_id.clone(),
        upstream_thread_id: Some(previous.thread_id.clone()),
        requested_by: Some(ctx.fingerprint.clone()),
        usage_subject: None,
        usage_subject_asserted_by: None,
    };
    // Operator launch context, seeded atomically with the edge so the row is
    // relaunchable the instant it exists. Caps left empty: the operator launch
    // re-derives them fresh (it is an explicit action, not a pinned relaunch).
    let resume_context = ryeos_app::launch_metadata::ResumeContext {
        kind: previous.kind.clone(),
        item_ref: directive.clone(),
        launch_mode: previous.launch_mode.clone(),
        parameters: parameters.clone(),
        project_context: ProjectContext::LocalPath {
            path: project_path.as_path().to_path_buf(),
        },
        original_snapshot_hash: None,
        current_site_id: previous.current_site_id.clone(),
        origin_site_id: previous.origin_site_id.clone(),
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        execution_hints: Default::default(),
        effective_caps: Vec::new(),
    };
    let project_path_str = project_path.as_path().to_string_lossy();
    let fingerprint = ryeos_app::thread_lifecycle::continuation_request_fingerprint(
        &directive,
        project_path_str.as_ref(),
        &ctx.fingerprint,
        &ctx.scopes,
        &previous.launch_mode,
        &previous.thread_id,
        &parameters,
    );

    let outcome = state
        .threads
        .create_or_get_operator_continuation(
            &successor_record,
            &previous.thread_id,
            Some(ryeos_state::queries::ContinuationReasonMarker::OperatorFollowUp.as_str()),
            &fingerprint,
            &resume_context,
        )
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Fire-and-forget the claim-guarded operator launch (a duplicate launch is a
    // benign no-op via the claim).
    let launch_async = |successor_id: String| {
        let st = (*state).clone();
        tokio::spawn(async move {
            if let Err(err) =
                ryeos_executor::execution::launch::launch_operator_successor(st, &successor_id)
                    .await
            {
                tracing::error!(
                    successor_id = %successor_id,
                    error = %err,
                    "operator follow-up launch failed"
                );
            }
        });
    };

    use ryeos_app::thread_lifecycle::OperatorContinuation;
    match outcome {
        OperatorContinuation::Created(detail) => {
            launch_async(detail.thread_id.clone());
            Ok(json!({
                "thread_id": detail.thread_id,
                "delivery": "launched",
                "notice": Value::Null,
            }))
        }
        OperatorContinuation::Existing(detail) => {
            // Idempotent double-submit → the SAME successor id. If it was stranded
            // `created` (crash before the original launch), ensure-launch.
            if detail.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
                launch_async(detail.thread_id.clone());
            }
            Ok(json!({
                "thread_id": detail.thread_id,
                "delivery": "launched",
                "notice": Value::Null,
            }))
        }
        OperatorContinuation::Conflict {
            existing_successor_id,
        } => Ok(json!({
            "thread_id": existing_successor_id,
            "delivery": "refused",
            "notice": format!(
                "thread {} already continued as {existing_successor_id} by a different input; \
                 follow that thread or start a new chain",
                previous.thread_id
            ),
        })),
    }
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
    fn continuable_predecessor_proceeds() {
        // Eligibility passes for completed/failed sources. An already-continued
        // source is NOT pre-refused here — the create-or-get dedups by fingerprint.
        assert!(follow_up_refusal("completed", "T-1").is_none());
        assert!(follow_up_refusal("failed", "T-1").is_none());
    }

    #[test]
    fn live_source_is_refused() {
        let notice = follow_up_refusal("running", "T-1").expect("live run refused");
        assert!(notice.contains("live delivery"), "got: {notice}");
    }

    #[test]
    fn settled_non_continuable_source_is_refused() {
        for status in ["cancelled", "killed", "timed_out"] {
            let notice = follow_up_refusal(status, "T-1")
                .unwrap_or_else(|| panic!("{status} must be refused"));
            assert!(notice.contains("can only continue"), "got: {notice}");
        }
    }
}
