//! `threads.input` — deliver operator input to a directive thread chain.
//!
//! The seat's ground verb lands here. The service owns the delivery
//! decision; clients never know there are two mechanisms:
//!
//! - no `thread` / settled completed-or-failed thread → launch the profile
//!   directive as a chained-resume turn (`previous_thread_id`); each turn is a
//!   new thread in the chain — continuity is the chain, not an entity;
//! - running directive thread → fold the operator input into the in-flight
//!   loop as a new `cognition_in` (steer at the next turn boundary, or after a
//!   forced interrupt) via the live-input queue; the runtime drains and
//!   persists it through `runtime.poll_input`;
//! - otherwise (not-yet-running, or a settled non-continuable status) →
//!   structured refusal.
//!
//! Submit result contract (input-design.md):
//! `{ thread_id?, delivery: launched|submitted|refused, notice? }`.
//! A live fold returns `submitted` with the SAME `thread_id` (no new thread).

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::live_input_queue::EnqueueOutcome;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{LiveInput, LiveInputIntent, ThreadStatus};

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
    /// Delivery intent for a *running* directive thread. Ignored on the
    /// launch / chained-resume paths (those land at turn boundaries by
    /// construction). Omitted by older clients → cooperative `Steer`.
    #[serde(default)]
    pub intent: LiveInputIntent,
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

/// The operator-facing notice for an accepted live steer, reporting the current
/// staged depth (the input queued behind any not-yet-folded ones). `pending` is
/// the queue depth AFTER this input was enqueued, so it is always ≥ 1.
fn staged_notice(pending: usize) -> String {
    format!("Input queued ({pending} staged).")
}

/// Where a follow-up to an existing thread lands, decided purely from status.
#[derive(Debug, PartialEq, Eq)]
enum FollowUpDecision {
    /// Running thread → fold the input into its in-flight loop (live).
    Live,
    /// Settled completed/failed turn → braid a successor (chained-resume).
    Continue,
    /// Not deliverable; carries the operator-facing notice.
    Refuse(String),
}

/// Classify a follow-up by the predecessor's STATUS only (kind eligibility is
/// decided separately, since it needs daemon policy). Pure, so it is
/// unit-testable without a daemon and runs synchronously.
///
/// This decides delivery TARGET, not idempotency: it does NOT refuse merely
/// because a successor already exists — an idempotent double-submit is deduped
/// downstream by fingerprint in `create_or_get_operator_continuation` (same
/// input → same successor; different input → conflict).
fn classify_follow_up(status: &str, thread_id: &str) -> FollowUpDecision {
    if ThreadStatus::from_str_lossy(status) == Some(ThreadStatus::Running) {
        // A running thread folds operator input as a new cognition_in — steered
        // at the next turn boundary, or after a forced interrupt.
        return FollowUpDecision::Live;
    }
    if !is_settled(status) {
        // created / accepted / pending: no live loop to steer yet, and no
        // settled turn to braid onto. Refuse honestly; the operator retries
        // once it is running or has finished.
        return FollowUpDecision::Refuse(format!(
            "thread {thread_id} is {status}; operator input needs a running thread \
             or a completed/failed turn to continue"
        ));
    }
    if !is_continuable(status) {
        return FollowUpDecision::Refuse(format!(
            "thread {thread_id} is {status}; a follow-up can only continue a completed or failed turn"
        ));
    }
    FollowUpDecision::Continue
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

    // Full daemon-authored execution facts for a kind — every arm returns both
    // so the client gates the operator-input affordance on
    // `supports_operator_followup`, not on `supports_continuation` alone.
    let exec_facts = |kind: &str| {
        json!({
            "supports_continuation": state.threads.supports_continuation_for_kind(kind),
            "supports_operator_followup": state.threads.supports_operator_followup_for_kind(kind),
        })
    };

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

            // Kind eligibility, enforced at the API boundary, applies to BOTH
            // live delivery and settled continuation: a kind that cannot
            // chain-fold (e.g. graph), or that self-continues by machine only,
            // has nowhere to put operator input. Refuse cleanly with
            // authoritative execution facts rather than letting the lifecycle
            // guard surface a 500. (Decided before status so a stale/third-party
            // client that skipped the client-side gate gets a structured
            // refusal regardless of whether the thread is live or settled.)
            if !state.threads.supports_continuation_for_kind(&detail.kind) {
                return Ok(json!({
                    "thread_id": Value::Null,
                    "delivery": "refused",
                    "notice": format!(
                        "thread {} is kind '{}', which does not support continuation",
                        detail.thread_id, detail.kind
                    ),
                    "execution": exec_facts(&detail.kind),
                }));
            }
            if !state
                .threads
                .supports_operator_followup_for_kind(&detail.kind)
            {
                return Ok(json!({
                    "thread_id": Value::Null,
                    "delivery": "refused",
                    "notice": format!(
                        "thread {} is kind '{}', which self-continues by machine only \
                         and does not accept operator follow-up",
                        detail.thread_id, detail.kind
                    ),
                    "execution": exec_facts(&detail.kind),
                }));
            }

            match classify_follow_up(&detail.status, &detail.thread_id) {
                // Running, eligible directive: fold operator input into the
                // in-flight loop. Enqueue first (so the input is present when
                // the runtime next polls), then — for an interrupt — nudge the
                // runtime to cut its current cognition.
                FollowUpDecision::Live => {
                    let input = LiveInput::new(req.input.clone(), req.intent);
                    let pending = match state.live_input.enqueue(&detail.thread_id, input) {
                        EnqueueOutcome::Accepted { pending } => pending,
                        EnqueueOutcome::Full { pending } => {
                            return Ok(json!({
                                "thread_id": Value::Null,
                                "delivery": "refused",
                                "notice": format!(
                                    "thread {} already has {} operator input(s) pending; \
                                     wait for them to fold",
                                    detail.thread_id, pending
                                ),
                                "execution": exec_facts(&detail.kind),
                            }));
                        }
                        EnqueueOutcome::Closed => {
                            // Finalized between the status read and the enqueue.
                            // The live window closed; resubmitting takes the
                            // settled continuation path.
                            return Ok(json!({
                                "thread_id": Value::Null,
                                "delivery": "refused",
                                "notice": format!(
                                    "thread {} finalized before the input could be delivered; \
                                     resubmit to continue",
                                    detail.thread_id
                                ),
                                "execution": exec_facts(&detail.kind),
                            }));
                        }
                    };

                    // Default notice reports the staged depth so the operator can
                    // see input piling up before the runtime folds it. An
                    // interrupt that failed to signal overrides it with the
                    // degrade message below.
                    let mut notice = json!(staged_notice(pending));
                    // Interrupt: cut the in-flight cognition. The input is
                    // already enqueued, so on any signal failure we degrade to a
                    // cooperative steer (it still folds at the next boundary).
                    if req.intent.is_interrupt() {
                        let outcome = match detail.runtime.pgid {
                            Some(pgid) => ryeos_app::process::interrupt_process_group(pgid),
                            None => ryeos_app::process::SignalResult::MissingPgid,
                        };
                        if outcome != ryeos_app::process::SignalResult::Delivered {
                            notice = json!(format!(
                                "interrupt not delivered ({}); steering instead — \
                                 input folds at the next turn boundary",
                                outcome.as_str()
                            ));
                            tracing::warn!(
                                thread_id = %detail.thread_id,
                                signal = %outcome.as_str(),
                                "live interrupt signal not delivered; degraded to steer"
                            );
                        }
                    }

                    return Ok(json!({
                        "thread_id": detail.thread_id,
                        "delivery": "submitted",
                        "notice": notice,
                        "pending": pending,
                        "execution": exec_facts(&detail.kind),
                    }));
                }
                FollowUpDecision::Refuse(notice) => {
                    return Ok(json!({
                        "thread_id": Value::Null,
                        "delivery": "refused",
                        "notice": notice,
                        "execution": exec_facts(&detail.kind),
                    }));
                }
                FollowUpDecision::Continue => Some(detail),
            }
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
    //
    // No `execution` key here: the thread is created ASYNCHRONOUSLY, so its kind
    // (hence continuation authority) is not yet known synchronously. The client
    // treats the absence as "unknown yet" — NOT `false` — and fetches the
    // enriched `threads.get`/`threads.list` once the thread materializes before
    // ratcheting `route.thread`.
    let Some(previous) = previous else {
        let parsed_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&directive)
            .map_err(|e| HandlerError::BadRequest(format!("directive ref: {e}")))?;
        let preflight = crate::routes::launch::preflight_dispatch_launch(
            &state,
            &parsed_ref,
            &project_path,
            &parameters,
            &ctx.fingerprint,
            &ctx.scopes,
            None,
            "inline",
            false,
            None,
            None,
        )
        .map_err(|error| {
            HandlerError::BadRequest(format!("root launch admission failed: {error}"))
        })?;
        if !preflight.class.persists_pre_minted_root() {
            return Err(HandlerError::BadRequest(
                "thread input requires execution that persists a pre-minted thread root"
                    .to_string(),
            ));
        }
        let root_admission = preflight.root_admission.ok_or_else(|| {
            HandlerError::Internal(
                "threaded dispatch preflight returned no root admission".to_string(),
            )
        })?;
        let launch_options = crate::routes::launch::DispatchLaunchOptions::admitted(root_admission)
            .map_err(|error| HandlerError::Internal(error.to_string()))?;
        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        let _handle = crate::routes::launch::spawn_dispatch_launch(
            &state,
            parsed_ref,
            parameters,
            ctx.fingerprint.clone(),
            ctx.scopes.clone(),
            thread_id.clone(),
            launch_options,
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
        project_root: previous.project_root.as_ref().map(std::path::PathBuf::from),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    // Inherit the predecessor's runtime identity so the successor reconstructs
    // the same launch without a per-item executor_id (delegate kinds carry none).
    // `executor_ref` comes straight off the predecessor row; `runtime_ref` from
    // its captured launch metadata, when present.
    let prior_runtime_ref = state
        .state_store
        .get_launch_metadata(&previous.thread_id)
        .ok()
        .flatten()
        .and_then(|m| m.resume_context)
        .and_then(|rc| rc.runtime_ref);
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
        // Operator follow-up launches against the live project tree.
        original_pushed_head_ref: None,
        state_root: None,
        current_site_id: previous.current_site_id.clone(),
        origin_site_id: previous.origin_site_id.clone(),
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: ctx.fingerprint.clone(),
            scopes: ctx.scopes.clone(),
        }),
        execution_hints: Default::default(),
        effective_caps: Vec::new(),
        executor_ref: Some(previous.executor_ref.clone()),
        runtime_ref: prior_runtime_ref,
    };
    let project_path_str = project_path.as_path().to_str().ok_or_else(|| {
        HandlerError::BadRequest("canonical project_path is not valid UTF-8".to_string())
    })?;
    let fingerprint = ryeos_app::thread_lifecycle::continuation_request_fingerprint(
        &directive,
        project_path_str,
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
                "execution": exec_facts(&detail.kind),
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
                "execution": exec_facts(&detail.kind),
            }))
        }
        OperatorContinuation::Conflict(detail) => Ok(json!({
            "thread_id": detail.thread_id,
            "delivery": "refused",
            "notice": format!(
                "thread {} already continued as {} by a different input; \
                 follow that thread or start a new chain",
                previous.thread_id, detail.thread_id
            ),
            "execution": exec_facts(&detail.kind),
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
    fn staged_notice_reports_depth() {
        assert_eq!(staged_notice(1), "Input queued (1 staged).");
        assert_eq!(staged_notice(2), "Input queued (2 staged).");
    }

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
    fn continuable_predecessor_continues() {
        // Completed/failed sources braid a successor. An already-continued
        // source is NOT pre-refused here — the create-or-get dedups by fingerprint.
        assert_eq!(
            classify_follow_up("completed", "T-1"),
            FollowUpDecision::Continue
        );
        assert_eq!(
            classify_follow_up("failed", "T-1"),
            FollowUpDecision::Continue
        );
    }

    #[test]
    fn running_source_is_live() {
        // A running thread is no longer refused — it folds operator input live.
        assert_eq!(classify_follow_up("running", "T-1"), FollowUpDecision::Live);
    }

    #[test]
    fn not_yet_running_source_is_refused() {
        // created / accepted / pending have no live loop and no settled turn.
        for status in ["created", "accepted", "pending"] {
            match classify_follow_up(status, "T-1") {
                FollowUpDecision::Refuse(notice) => {
                    assert!(notice.contains("needs a running thread"), "got: {notice}");
                }
                other => panic!("{status} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn settled_non_continuable_source_is_refused() {
        for status in ["cancelled", "killed", "timed_out", "continued"] {
            match classify_follow_up(status, "T-1") {
                FollowUpDecision::Refuse(notice) => {
                    assert!(notice.contains("can only continue"), "got: {notice}");
                }
                other => panic!("{status} must be refused, got {other:?}"),
            }
        }
    }
}
