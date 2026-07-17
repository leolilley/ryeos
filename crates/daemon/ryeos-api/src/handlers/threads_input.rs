//! `threads.input` — deliver operator input to a directive thread chain.
//!
//! The seat's ground verb lands here. The service owns the delivery
//! decision; clients never know there are two mechanisms:
//!
//! - a `fresh` target supplies a complete execution identity and starts an
//!   independent chain;
//! - a settled completed-or-failed `thread` target inherits the predecessor's
//!   persisted execution identity and creates a successor;
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

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::live_input_queue::{
    serialized_live_input_bytes, EnqueueOutcome, MAX_LIVE_INPUT_SERIALIZED_BYTES,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{LiveInput, LiveInputIntent, ThreadStatus};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Operator text, bound whole — never tokenized here.
    pub input: String,
    /// Structurally separates a fresh execution identity from a thread whose
    /// persisted identity is authoritative.
    pub target: Target,
    /// Delivery intent for a *running* directive thread. Ignored on the
    /// launch / chained-resume paths (those land at turn boundaries by
    /// construction). Omission means cooperative `Steer`.
    #[serde(default)]
    pub intent: LiveInputIntent,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Fresh {
        item_ref: String,
        project_path: String,
        ref_bindings: BTreeMap<String, String>,
    },
    Thread {
        thread_id: String,
    },
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

fn authorize_execution_refs(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<(), HandlerError> {
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(ref_bindings)
        .map_err(dispatch_error)?;
    for (label, value) in std::iter::once(("item_ref", item_ref)).chain(
        ref_bindings
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    ) {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
            .map_err(|error| HandlerError::BadRequest(format!("invalid {label}: {error}")))?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
        state
            .authorizer
            .authorize(&ctx.scopes, &policy)
            .map_err(|_| {
                HandlerError::Forbidden(format!("missing required capability: {required}"))
            })?;
    }
    Ok(())
}

fn dispatch_error(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_string(),
        status: error.http_status().as_u16(),
        body: ryeos_executor::structured_error::StructuredErrorPayload::from(&error).to_value(),
    }
}

fn launch_spawn_error(error: crate::routes::launch::LaunchSpawnError) -> HandlerError {
    match error {
        crate::routes::launch::LaunchSpawnError::Dispatch(error) => dispatch_error(error),
        crate::routes::launch::LaunchSpawnError::InvalidRef { ref_str, reason } => {
            HandlerError::BadRequest(format!("invalid item_ref '{ref_str}': {reason}"))
        }
    }
}

fn build_and_launch_error(
    error: ryeos_executor::execution::launch::BuildAndLaunchError,
) -> HandlerError {
    use ryeos_executor::execution::launch::BuildAndLaunchError;
    match error {
        BuildAndLaunchError::MissingSecrets { item_ref, secrets } => {
            let Some(missing) = secrets.first() else {
                return HandlerError::Internal(
                    "missing-secret launch error carried no secret".to_string(),
                );
            };
            let source = missing.primary_source();
            dispatch_error(
                ryeos_executor::dispatch_error::DispatchError::RequiredSecretMissing {
                    item_ref,
                    env_var: missing.name.clone(),
                    source_kind: source.kind_for_wire().to_string(),
                    source_name: source.name_for_wire(),
                    remediation: ryeos_executor::dispatch_error::required_secret_remediation(
                        &missing.name,
                    ),
                },
            )
        }
        BuildAndLaunchError::CapabilityRejected { reason } => dispatch_error(
            ryeos_executor::dispatch_error::DispatchError::CapabilityRejected { reason },
        ),
        BuildAndLaunchError::LaunchPreparation(error) => dispatch_error(*error),
        BuildAndLaunchError::Materialization(error) => {
            HandlerError::Internal(format!("launch materialization failed: {error}"))
        }
        BuildAndLaunchError::Internal(error) => HandlerError::Internal(error.to_string()),
    }
}

fn handoff_failure(
    failure: ryeos_executor::execution::launch::LaunchHandoffFailure,
) -> HandlerError {
    HandlerError::Structured {
        code: failure.code,
        status: failure.status,
        body: failure.body,
    }
}

async fn await_dispatch_handoff(
    mut task: tokio::task::JoinHandle<Result<(), crate::routes::launch::LaunchSpawnError>>,
    ready: tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) -> Result<String, HandlerError> {
    tokio::select! {
        readiness = ready => match readiness {
            Ok(Ok(thread_id)) => Ok(thread_id),
            Ok(Err(failure)) => Err(handoff_failure(failure)),
            Err(_) => match task.await {
                Ok(Err(error)) => Err(launch_spawn_error(error)),
                Ok(Ok(())) => Err(HandlerError::Internal(
                    "launch completed without authoritative handoff".to_string(),
                )),
                Err(error) => Err(HandlerError::Internal(format!("launch task failed: {error}"))),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => Err(launch_spawn_error(error)),
            Ok(Ok(())) => Err(HandlerError::Internal(
                "launch completed without authoritative handoff".to_string(),
            )),
            Err(error) => Err(HandlerError::Internal(format!("launch task failed: {error}"))),
        },
    }
}

async fn await_operator_handoff(
    mut task: tokio::task::JoinHandle<
        Result<
            ryeos_executor::execution::launch::SuccessorLaunchOutcome,
            ryeos_executor::execution::launch::BuildAndLaunchError,
        >,
    >,
    ready: tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) -> Result<String, HandlerError> {
    tokio::select! {
        readiness = ready => match readiness {
            Ok(Ok(thread_id)) => Ok(thread_id),
            Ok(Err(failure)) => Err(handoff_failure(failure)),
            Err(_) => match task.await {
                Ok(Err(error)) => Err(build_and_launch_error(error)),
                Ok(Ok(_)) => Err(HandlerError::Internal(
                    "operator successor completed without authoritative handoff".to_string(),
                )),
                Err(error) => Err(HandlerError::Internal(format!(
                    "operator successor task failed: {error}"
                ))),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => Err(build_and_launch_error(error)),
            Ok(Ok(_)) => Err(HandlerError::Internal(
                "operator successor completed without authoritative handoff".to_string(),
            )),
            Err(error) => Err(HandlerError::Internal(format!(
                "operator successor task failed: {error}"
            ))),
        },
    }
}

fn admit_fresh_launch(
    item_ref: &crate::routes::parsed_ref::ParsedItemRef,
    ref_bindings: &BTreeMap<String, String>,
    project_path: &crate::routes::abs_path::AbsolutePathBuf,
    parameters: &Value,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<crate::routes::launch::DispatchLaunchOptions, HandlerError> {
    let preflight = crate::routes::launch::preflight_dispatch_launch(
        state,
        item_ref,
        project_path,
        parameters,
        ref_bindings,
        &ctx.fingerprint,
        &ctx.scopes,
        None,
        "inline",
        false,
        None,
        None,
    )
    .map_err(dispatch_error)?;
    if !preflight.class.persists_pre_minted_root() {
        return Err(HandlerError::BadRequest(
            "fresh threads.input target requires a launch-producing item".to_string(),
        ));
    }
    let root_admission = preflight.root_admission.ok_or_else(|| {
        HandlerError::Internal("threaded dispatch preflight returned no root admission".to_string())
    })?;
    crate::routes::launch::DispatchLaunchOptions::admitted(
        root_admission,
        project_path.as_path(),
        ref_bindings.clone(),
    )
    .map_err(|error| {
        HandlerError::Internal(format!(
            "validated fresh-launch contract rejected at dispatch boundary: {error:#}"
        ))
    })
}

fn admit_resume_launch(
    resume: &ryeos_app::launch_metadata::ResumeContext,
    state: &AppState,
) -> Result<(), HandlerError> {
    use ryeos_engine::contracts::EffectivePrincipal;

    let params =
        ryeos_executor::execution::runner::execution_params_from_resume_context(state, resume)
            .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let engine = params.provenance.request_engine().clone();
    let primary = engine
        .verify(
            &params.resolved.plan_context,
            params.resolved.resolved_item.clone(),
        )
        .map_err(|error| HandlerError::BadRequest(format!("verification failed: {error}")))?
        .resolved;
    let caller_scopes = match &params.resolved.plan_context.requested_by {
        EffectivePrincipal::Local(principal) => principal.scopes.clone(),
        EffectivePrincipal::Delegated(principal) => principal.delegated_scopes.clone(),
    };
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: params.acting_principal.clone(),
        caller_scopes,
        engine,
        plan_ctx: params.resolved.plan_context.clone(),
        requested_call: None,
    };
    ryeos_executor::dispatch::preflight_root_dispatch(
        &resume.item_ref,
        primary.canonical_ref.kind.as_str(),
        &resume.parameters,
        &resume.ref_bindings,
        None,
        None,
        &exec_ctx,
        state,
    )
    .map_err(dispatch_error)?;
    let applicability =
        ryeos_executor::dispatch::launch_contract_applicability(&resume.item_ref, &exec_ctx)
            .map_err(dispatch_error)?;
    ryeos_executor::dispatch::admit_launch_contract(
        &applicability,
        &primary,
        &resume.ref_bindings,
        params.provenance.effective_path(),
        &exec_ctx,
        state,
    )
    .map_err(dispatch_error)
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    let Request {
        input,
        target,
        intent,
    } = req;
    if input.trim().is_empty() {
        return Err(HandlerError::BadRequest("input is empty".to_string()));
    }
    let is_interrupt = intent.is_interrupt();
    let operator_input = LiveInput::new(input.clone(), intent);
    let input_bytes = serialized_live_input_bytes(&operator_input);
    if input_bytes > MAX_LIVE_INPUT_SERIALIZED_BYTES {
        return Err(HandlerError::BadRequest(format!(
            "operator input is {input_bytes} serialized bytes; maximum is {MAX_LIVE_INPUT_SERIALIZED_BYTES}"
        )));
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

    let thread_id = match target {
        Target::Fresh {
            item_ref,
            project_path,
            ref_bindings,
        } => {
            authorize_execution_refs(&item_ref, &ref_bindings, &ctx, &state)?;
            let project_path =
                crate::routes::abs_path::AbsolutePathBuf::try_new(project_path.into())
                    .map_err(|error| HandlerError::BadRequest(format!("project_path: {error}")))?;
            let parameters = json!({ "input": input });
            let parsed_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&item_ref)
                .map_err(|error| HandlerError::BadRequest(format!("item_ref: {error}")))?;
            let launch_options = admit_fresh_launch(
                &parsed_ref,
                &ref_bindings,
                &project_path,
                &parameters,
                &ctx,
                &state,
            )?;
            let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
            let launch_provenance =
                ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                    launch_options.project_path().to_path_buf(),
                    state.engine.clone(),
                );
            let (handle, ready) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
                &state,
                parsed_ref,
                parameters,
                ctx.fingerprint.clone(),
                ctx.scopes.clone(),
                thread_id.clone(),
                launch_provenance,
                launch_options,
            );
            let ready_thread_id = await_dispatch_handoff(handle, ready).await?;
            if ready_thread_id != thread_id {
                return Err(HandlerError::Internal(
                    "launch handoff returned a different thread identity".to_string(),
                ));
            }
            return Ok(json!({
                "thread_id": thread_id,
                "delivery": "launched",
                "notice": Value::Null,
            }));
        }
        Target::Thread { thread_id } => thread_id,
    };

    // Resolve the prior turn. Eligibility only — a
    // live source or a non-continuable settled status is refused; an existing
    // successor is NOT a refusal (the create-or-get below dedups by fingerprint).
    let previous = {
        let detail = state
            .state_store
            .get_thread(&thread_id)
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
                let pending = match state
                    .live_input
                    .enqueue(&detail.thread_id, operator_input.clone())
                {
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
                    EnqueueOutcome::TooLarge { bytes, max } => {
                        return Err(HandlerError::BadRequest(format!(
                            "operator input is {bytes} serialized bytes; maximum is {max}"
                        )));
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
                if is_interrupt {
                    let outcome = match detail.runtime.process_identity.as_ref() {
                        Some(identity) => ryeos_app::process::interrupt_process(identity),
                        None if detail.runtime.pgid.is_some() => {
                            ryeos_app::process::SignalResult::MissingIdentity
                        }
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
            FollowUpDecision::Continue => detail,
        }
    };

    // A settled continuation has no caller-supplied execution identity. Load the
    // predecessor's persisted ResumeContext and carry its primary ref, secondary
    // refs, project/provenance identity, runtime/executor identity, launch mode,
    // and sites without overrides.
    let prior_resume = state
        .state_store
        .get_launch_metadata(&previous.thread_id)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .and_then(|metadata| metadata.resume_context)
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "thread {} has no persisted execution identity",
                previous.thread_id
            ))
        })?;
    if prior_resume.kind != previous.kind
        || prior_resume.item_ref != previous.item_ref
        || prior_resume.launch_mode != previous.launch_mode
        || prior_resume.current_site_id != previous.current_site_id
        || prior_resume.origin_site_id != previous.origin_site_id
        || prior_resume
            .executor_ref
            .as_deref()
            .is_some_and(|executor_ref| executor_ref != previous.executor_ref)
    {
        return Err(HandlerError::Structured {
            code: "execution_identity_continuity_mismatch".to_string(),
            status: axum::http::StatusCode::CONFLICT.as_u16(),
            body: json!({
                "code": "execution_identity_continuity_mismatch",
                "error": "predecessor row and persisted execution identity disagree",
                "thread_id": previous.thread_id,
            }),
        });
    }
    authorize_execution_refs(
        &prior_resume.item_ref,
        &prior_resume.ref_bindings,
        &ctx,
        &state,
    )?;

    use ryeos_engine::contracts::{EffectivePrincipal, Principal};
    let parameters = json!({ "input": input });
    let mut resume_context = prior_resume;
    resume_context.parameters = parameters.clone();
    // Operator authority is re-derived from the verified caller; execution
    // identity itself remains inherited from the predecessor.
    resume_context.requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: ctx.fingerprint.clone(),
        scopes: ctx.scopes.clone(),
    });

    // Admission and the consumable authoritative pass both use the exact
    // persisted identity and the exact pre-minted successor ID before its row
    // exists.
    admit_resume_launch(&resume_context, &state)?;
    let candidate_id = ryeos_app::thread_lifecycle::new_thread_id();
    let prepared = ryeos_executor::execution::launch::prepare_operator_successor_launch(
        &state,
        &candidate_id,
        &resume_context,
    )
    .await
    .map_err(build_and_launch_error)?;
    let initial_audit_events = prepared
        .initial_audit_events()
        .map_err(build_and_launch_error)?;

    let successor_record = ryeos_app::state_store::NewThreadRecord {
        thread_id: candidate_id.clone(),
        chain_root_id: previous.chain_root_id.clone(),
        kind: previous.kind.clone(),
        item_ref: resume_context.item_ref.clone(),
        executor_ref: previous.executor_ref.clone(),
        launch_mode: resume_context.launch_mode.clone(),
        current_site_id: resume_context.current_site_id.clone(),
        origin_site_id: resume_context.origin_site_id.clone(),
        upstream_thread_id: Some(previous.thread_id.clone()),
        requested_by: Some(ctx.fingerprint.clone()),
        project_root: previous.project_root.as_ref().map(std::path::PathBuf::from),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    let project_identity = serde_json::to_string(&resume_context.project_context)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let fingerprint = ryeos_app::thread_lifecycle::continuation_request_fingerprint(
        &resume_context.item_ref,
        &resume_context.ref_bindings,
        &project_identity,
        &ctx.fingerprint,
        &ctx.scopes,
        &resume_context.launch_mode,
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
            prepared.launch_metadata(),
            initial_audit_events,
        )
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    use ryeos_app::thread_lifecycle::OperatorContinuation;
    match outcome {
        OperatorContinuation::Created(detail) => {
            let successor_id = detail.thread_id.clone();
            let prepared = prepared.with_persisted_birth_audit();
            let task_id = successor_id.clone();
            let st = (*state).clone();
            let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
            let task = tokio::spawn(async move {
                ryeos_executor::execution::launch::launch_prepared_operator_successor(
                    st, &task_id, prepared, &handoff,
                )
                .await
            });
            let ready_thread_id = await_operator_handoff(task, ready).await?;
            if ready_thread_id != successor_id {
                return Err(HandlerError::Internal(
                    "operator launch handoff returned a different thread identity".to_string(),
                ));
            }
            Ok(json!({
                "thread_id": detail.thread_id,
                "delivery": "launched",
                "notice": Value::Null,
                "execution": exec_facts(&detail.kind),
            }))
        }
        OperatorContinuation::Existing(detail) => {
            // Idempotent double-submit → the SAME successor id. If it was stranded
            // `created`, consume the freshly recomputed authority and require the
            // same handoff proof as a newly created successor.
            if detail.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
                let successor_id = detail.thread_id.clone();
                // The candidate authority named a speculative ID and its
                // snapshot was never committed. Re-drive from the exact durable
                // successor identity without recapturing its birth snapshot.
                drop(prepared);
                let launch_metadata = state
                    .state_store
                    .get_launch_metadata(&successor_id)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?
                    .ok_or_else(|| {
                        HandlerError::Internal(format!(
                            "operator successor {successor_id} has no persisted launch metadata"
                        ))
                    })?;
                let prepared =
                    ryeos_executor::execution::launch::prepare_existing_operator_successor_launch(
                        &state,
                        &successor_id,
                        &launch_metadata,
                    )
                    .await
                    .map_err(build_and_launch_error)?;
                let task_id = successor_id.clone();
                let st = (*state).clone();
                let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
                let task = tokio::spawn(async move {
                    ryeos_executor::execution::launch::launch_prepared_operator_successor(
                        st, &task_id, prepared, &handoff,
                    )
                    .await
                });
                let ready_thread_id = await_operator_handoff(task, ready).await?;
                if ready_thread_id != successor_id {
                    return Err(HandlerError::Internal(
                        "operator relaunch handoff returned a different thread identity"
                            .to_string(),
                    ));
                }
            } else {
                drop(prepared);
            }
            Ok(json!({
                "thread_id": detail.thread_id,
                "delivery": "launched",
                "notice": Value::Null,
                "execution": exec_facts(&detail.kind),
            }))
        }
        OperatorContinuation::Conflict(detail) => {
            drop(prepared);
            Ok(json!({
                "thread_id": detail.thread_id,
                "delivery": "refused",
                "notice": format!(
                    "thread {} already continued as {} by a different input; \
                     follow that thread or start a new chain",
                    previous.thread_id, detail.thread_id
                ),
                "execution": exec_facts(&detail.kind),
            }))
        }
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
