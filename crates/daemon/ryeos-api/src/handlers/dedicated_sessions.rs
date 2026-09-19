//! Meaning-blind owner-authorized access to one attached exclusive session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Result;
use base64::Engine as _;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::dedicated_session_service::{
    CandidatePublicationAuthority, candidate_publication_operation_id,
    candidate_publication_reservation_payload, candidate_publication_result_payload,
    verify_candidate_publication_reservation, verify_candidate_publication_result,
};
use ryeos_app::persistent_session::ExclusiveRetirementOutcome;
use ryeos_app::state::AppState;
use ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT;
use ryeos_executor::executor::ServiceAvailability;

const MAX_HANDOFF_TERMINAL_ATTESTATION_BYTES: usize = 512 * 1024;

fn disposition_operation_lock(placement_thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("candidate disposition lock poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(placement_thread_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(placement_thread_id.to_owned(), Arc::downgrade(&lock));
    lock
}

fn approval_delivery_lock(placement_thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("approval delivery lock map poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(placement_thread_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(placement_thread_id.to_owned(), Arc::downgrade(&lock));
    lock
}

struct CandidateEvaluationLaunchGuard {
    state: AppState,
    reserved_thread_id: String,
    armed: bool,
}

impl Drop for CandidateEvaluationLaunchGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self
                .state
                .state_store
                .settle_launch_planning_admission_exit(&self.reserved_thread_id)
        {
            tracing::error!(
                thread_id = %self.reserved_thread_id,
                error = %error,
                "failed to settle candidate-evaluator launch admission"
            );
        }
    }
}

fn exact_reaped_source_worker_authority(
    source_placement_thread_id: &str,
    retained_boot_epoch: Option<u64>,
    worker_placement_thread_id: &str,
    worker_boot_epoch: u64,
    worker_state: ryeos_app::runtime_db::WorkerProcessState,
    cleanup_state: &str,
) -> bool {
    retained_boot_epoch.is_some_and(|retained| retained == worker_boot_epoch)
        && worker_placement_thread_id == source_placement_thread_id
        && worker_state == ryeos_app::runtime_db::WorkerProcessState::Dead
        && cleanup_state == "reaped"
}

fn find_authoritative_approval_delivery_settlement(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    approval: &ryeos_app::runtime_db::DedicatedSessionApprovalRecord,
    reservation_token: &str,
    decision_digest: &str,
) -> Result<Option<String>, HandlerError> {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "approval_id":approval.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_settled",
    }))
    .map_err(internal)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_approval.delivery_settled",
        &operation_id,
    )
    .map_err(internal)?;
    if fact.count > 1 {
        return Err(internal(
            "approval settlement operation is duplicated in the root chain",
        ));
    }
    let Some(payload) = fact.payload else {
        return if fact.count == 0 {
            Ok(None)
        } else {
            Err(internal(
                "approval settlement testimony exceeds the bounded replay cache payload",
            ))
        };
    };
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload.get("approval_id").and_then(Value::as_str)
            == Some(approval.approval_id.as_str())
        && payload.get("worker_boot_epoch").and_then(Value::as_u64)
            == Some(approval.worker_boot_epoch)
        && payload.get("request_digest").and_then(Value::as_str)
            == Some(approval.request_digest.as_str())
        && payload.get("reservation_token").and_then(Value::as_str) == Some(reservation_token)
        && payload.get("decision_digest").and_then(Value::as_str) == Some(decision_digest);
    if !exact {
        return Err(internal(
            "approval settlement operation id is bound to contradictory root testimony",
        ));
    }
    payload
        .get("delivery_digest")
        .and_then(Value::as_str)
        .filter(|digest| lillux::valid_hash(digest))
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| internal("approval settlement fact has no valid delivery digest"))
}

fn owned_session(
    state: &AppState,
    ctx: &HandlerContext,
    chain_root_id: &str,
) -> Result<ryeos_app::state_store::DedicatedSessionRecord, HandlerError> {
    // Initial hosted execution is deliberately a single configured-operator
    // trust domain. Enforce that predicate before lookup so discovery and
    // timing do not turn owner rows into an accidental multi-tenant boundary.
    ryeos_app::operator_authority::require_admitted_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;
    let placement_thread_id = state
        .state_store
        .current_chain_placement_thread_id(chain_root_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    let session = state
        .state_store
        .dedicated_session(&placement_thread_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    if session.chain_root_id != chain_root_id {
        return Err(internal(
            "hosted execution projection contradicts its authoritative chain",
        ));
    }
    ctx.require_owner(Some(&session.owner_principal))?;
    Ok(session)
}

fn owned_session_placement(
    state: &AppState,
    ctx: &HandlerContext,
    chain_root_id: &str,
    placement_thread_id: &str,
) -> Result<ryeos_app::state_store::DedicatedSessionRecord, HandlerError> {
    ryeos_app::operator_authority::require_admitted_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;
    let thread = state
        .state_store
        .get_thread(placement_thread_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    let session = state
        .state_store
        .dedicated_session(placement_thread_id)
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    if thread.chain_root_id != chain_root_id
        || session.chain_root_id != chain_root_id
        || session.placement_thread_id != placement_thread_id
    {
        return Err(HandlerError::NotFound);
    }
    ctx.require_owner(Some(&session.owner_principal))?;
    Ok(session)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    chain_root_id: String,
}

async fn status(
    req: StatusRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;
    let session = state
        .state_store
        .current_chain_placement_thread_id(&req.chain_root_id)
        .map_err(internal)?
        .map(|placement_thread_id| {
            state
                .state_store
                .dedicated_session(&placement_thread_id)
                .map_err(internal)
        })
        .transpose()?
        .flatten();
    if let Some(session) = &session {
        if session.chain_root_id != req.chain_root_id {
            return Err(internal(
                "hosted execution projection contradicts its authoritative chain",
            ));
        }
        ctx.require_owner(Some(&session.owner_principal))?;
    }
    let handoff = state
        .state_store
        .with_state_db(|db| latest_handoff_job_for_chain(db, &req.chain_root_id))
        .map_err(internal)?;
    if let Some((_job, operation)) = &handoff {
        ctx.require_owner(Some(&operation.owner_principal))?;
    }
    if session.is_none() && handoff.is_none() {
        return Err(HandlerError::NotFound);
    }

    // Preserve the established attached-session projection shape. During the
    // post-cut/pre-attachment interval there is deliberately no session row;
    // expose that absence alongside the durable handoff operation instead of
    // returning a false 404 or synthesizing session state.
    let mut result = match session {
        Some(session) => serde_json::to_value(session).map_err(internal)?,
        None => json!({
            "chain_root_id":req.chain_root_id,
            "session_projection":Value::Null,
        }),
    };
    result["handoff"] = match handoff {
        Some((job, operation)) => {
            handoff_status_value(Some(&state), job, operation).map_err(internal)?
        }
        None => Value::Null,
    };
    Ok(result)
}

async fn candidate_result(
    req: ryeos_app::hosted_candidate_result::HostedCandidateResultRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::identity::validate_forwarding_origin_assertion(
        Some(&req.source_site_id),
        ctx.authorized_key_class,
        ctx.authenticated_origin_site_id.as_deref(),
    )
    .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted candidate placement changed while result testimony was reserved".into(),
        ));
    }
    let expected_root_status = candidate_result_root_status(
        &session.candidate_disposition,
        &session.state,
        session.terminal_reason.as_deref(),
        session.publication_result.as_deref(),
    )?;
    // The interactive root is still waiting for owner disposition. A bounded
    // worker root is already completed: inspect its immutable testimony, never
    // reopen it or append a synthetic qualification/publication event.
    let _root_operation = if expected_root_status == ryeos_state::objects::ThreadStatus::Running {
        Some(
            ryeos_app::hosted_operation::begin_hosted_root_operation_async(
                &state.state_store,
                &session.placement_thread_id,
            )
            .await
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        )
    } else {
        None
    };
    if !session.candidate_required {
        return Err(HandlerError::BadRequest(
            "hosted execution has no retained terminal candidate".into(),
        ));
    }
    let candidate = session
        .candidate_snapshot_hash
        .clone()
        .ok_or_else(|| internal("frozen hosted execution has no candidate"))?;
    let validation = session
        .candidate_validation_hash
        .clone()
        .ok_or_else(|| internal("frozen hosted execution has no validation identity"))?;
    let (thread, last_event, chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("hosted candidate authoritative placement disappeared"))?;
    let last_event_hash = last_event
        .and_then(|event| event.event_hash)
        .ok_or_else(|| internal("hosted candidate placement has no authoritative last event"))?;
    let signed_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &session.chain_root_id))
        .map_err(internal)?
        .ok_or_else(|| internal("hosted candidate chain has no signed current head"))?;
    if signed_head.signer != state.identity.fingerprint()
        || signed_head.target_hash != chain_head_hash
        || thread.thread_id != session.placement_thread_id
        || thread.chain_root_id != session.chain_root_id
        || thread.status != expected_root_status
        || thread.current_site_id != state.threads.site_id()
        || thread.origin_site_id != req.source_site_id
        || thread.requested_by.as_deref() != Some(session.owner_principal.as_str())
    {
        return Err(internal(
            "hosted candidate placement contradicts its authoritative chain head",
        ));
    }
    let admitted_launch_capsule_hash = candidate_result_launch_capsule(&state, &session, &thread)?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity,
        display_path: Some(target_project_path),
        base_snapshot_hash,
        realization:
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication,
            },
        ..
    } = &thread.project_authority
    else {
        return Err(HandlerError::BadRequest(
            "hosted candidate has no retained pinned project authority".into(),
        ));
    };
    let target_project_path = target_project_path
        .to_str()
        .ok_or_else(|| internal("hosted candidate target project path is not UTF-8"))?;
    let expected_identity = ryeos_app::launch_metadata::StableProjectIdentity::from_path(
        std::path::Path::new(target_project_path),
        state.threads.site_id(),
    )
    .map_err(internal)?;
    let publication_authority_matches = match (&session.candidate_disposition, terminal_publication)
    {
        (
            ryeos_app::runtime_db::DedicatedCandidateDisposition::OwnerDecision,
            ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                principal_key,
                project_hash,
                expected_hash,
            },
        ) => {
            principal_key.as_str()
                == ryeos_state::refs::principal_storage_key(&session.owner_principal)
                    .map_err(internal)?
                && project_hash == &lillux::sha256_hex(target_project_path.as_bytes())
                && expected_hash == base_snapshot_hash
        }
        (
            ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview,
            ryeos_state::objects::PinnedTerminalPublication::RetainResult,
        ) => true,
        _ => false,
    };
    if stable_project_identity != &expected_identity.normalized_logical_key
        || !publication_authority_matches
    {
        return Err(internal(
            "hosted candidate project destination contradicts admitted authority",
        ));
    }

    let completion_fence = session.completion_fence.clone().ok_or_else(|| {
        HandlerError::BadRequest("hosted candidate has no reserved exact command completion".into())
    })?;
    let command = ryeos_app::dedicated_session_service::command_observation(
        &state,
        &session.placement_thread_id,
        completion_fence.command_sequence,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let observed_fence = command
        .get("completion_fence")
        .cloned()
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "hosted candidate owner-route command has no completed turn".into(),
            )
        })
        .and_then(|value| {
            serde_json::from_value::<
                ryeos_app::dedicated_session_service::HostedCommandCompletionFence,
            >(value)
            .map_err(|error| internal(format!("decode hosted completion fence: {error}")))
        })?;
    if observed_fence != completion_fence {
        return Err(internal(
            "hosted candidate completion differs from its exact reservation",
        ));
    }
    let command_response_digest = command
        .get("response_digest")
        .and_then(Value::as_str)
        .filter(|digest| {
            lillux::valid_hash(digest) && !digest.bytes().any(|byte| byte.is_ascii_uppercase())
        })
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "hosted candidate owner-route command has no settled response digest".into(),
            )
        })?
        .to_owned();
    state
        .state_store
        .require_dedicated_session_route_frontier(
            &session.placement_thread_id,
            completion_fence.command_sequence,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

    let capture_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_capture_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate,
    }))
    .map_err(internal)?;
    let capture = exact_root_fact_payload(
        &state,
        &session,
        "hosted_candidate.captured",
        &capture_operation_id,
    )?;
    if capture.get("schema").and_then(Value::as_u64) != Some(1)
        || capture.get("origin").and_then(Value::as_str) != Some("filesystem_verified")
        || capture.get("chain_root_id").and_then(Value::as_str)
            != Some(session.chain_root_id.as_str())
        || capture.get("placement_thread_id").and_then(Value::as_str)
            != Some(session.placement_thread_id.as_str())
        || capture.get("workspace_id").and_then(Value::as_str)
            != Some(session.workspace_id.as_str())
        || capture
            .get("candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(candidate.as_str())
        || capture.get("base_snapshot_hash").and_then(Value::as_str)
            != Some(base_snapshot_hash.as_str())
        || capture.get("admitted_capsule_hash").and_then(Value::as_str)
            != Some(session.admitted_capsule_hash.as_str())
    {
        return Err(internal(
            "hosted candidate capture testimony contradicts current authority",
        ));
    }

    if expected_root_status == ryeos_state::objects::ThreadStatus::Completed {
        verify_completed_bounded_candidate_result(
            &state,
            &session,
            &thread,
            base_snapshot_hash,
            &candidate,
            &completion_fence,
        )?;
    }
    let (verified_base, closure_evidence) =
        frozen_candidate_closure_evidence(&state, &session, &candidate, &validation)?;
    if verified_base != *base_snapshot_hash {
        return Err(internal(
            "hosted candidate base changed during result verification",
        ));
    }
    let closure_validation: ryeos_app::hosted_candidate_result::HostedCandidateClosureValidation =
        serde_json::from_value(closure_evidence).map_err(internal)?;
    let closure_validation_hash = closure_validation.content_hash().map_err(internal)?;

    let evidence = ryeos_app::hosted_candidate_result::HostedCandidateResultEvidence {
        schema: ryeos_app::hosted_candidate_result::HOSTED_CANDIDATE_RESULT_SCHEMA.to_owned(),
        owner_principal: session.owner_principal,
        source_site_id: req.source_site_id.clone(),
        target_site_id: state.threads.site_id().to_owned(),
        chain_root_id: session.chain_root_id,
        placement_thread_id: session.placement_thread_id,
        chain_head_hash,
        last_event_hash,
        admitted_launch_capsule_hash,
        admitted_session_capsule_hash: session.admitted_capsule_hash,
        stable_project_identity: stable_project_identity.clone(),
        target_project_path: target_project_path.to_owned(),
        base_snapshot_hash: base_snapshot_hash.clone(),
        candidate_snapshot_hash: candidate,
        candidate_validation_hash: validation,
        completion_fence,
        command_response_digest,
        candidate_capture_operation_id: capture_operation_id,
        closure_validation,
        closure_validation_hash,
        candidate_state: ryeos_app::hosted_candidate_result::HostedCandidateState::Frozen,
        disposition: ryeos_app::hosted_candidate_result::HostedCandidateDisposition::Retained,
    };
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let response =
        ryeos_app::hosted_candidate_result::HostedCandidateResultResponse::new(evidence, &signer)
            .map_err(internal)?;
    response
        .validate_against(
            &req,
            &ctx.fingerprint,
            state.threads.site_id(),
            state.identity.verifying_key(),
        )
        .map_err(internal)?;
    serde_json::to_value(response).map_err(internal)
}

fn candidate_result_root_status(
    disposition: &ryeos_app::runtime_db::DedicatedCandidateDisposition,
    session_state: &str,
    terminal_reason: Option<&str>,
    publication_result: Option<&str>,
) -> Result<ryeos_state::objects::ThreadStatus, HandlerError> {
    use ryeos_app::runtime_db::DedicatedCandidateDisposition;
    use ryeos_state::objects::ThreadStatus;
    if terminal_reason == Some("completed") {
        match disposition {
            DedicatedCandidateDisposition::OwnerDecision
                if matches!(session_state, "frozen" | "publish_ready")
                    && publication_result == Some("retained") =>
            {
                return Ok(ThreadStatus::Running);
            }
            DedicatedCandidateDisposition::RetainedForReview
                if matches!(session_state, "terminal" | "publish_ready")
                    && publication_result == Some("retained_for_review") =>
            {
                return Ok(ThreadStatus::Completed);
            }
            _ => {}
        }
    }
    Err(HandlerError::BadRequest(
        "hosted execution has no frozen retained completed candidate".into(),
    ))
}

fn candidate_result_launch_capsule(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    thread: &ryeos_state::objects::ThreadSnapshot,
) -> Result<String, HandlerError> {
    let launch_hash = thread
        .admitted_launch_capsule_hash
        .as_deref()
        .ok_or_else(|| internal("hosted candidate root has no admitted launch capsule"))?;
    let launch = state
        .state_store
        .admitted_launch_capsule(&thread.thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("hosted candidate root lost its admitted launch capsule"))?;
    if launch.content_hash().map_err(internal)? != launch_hash
        || !launch
            .admitted_persistent_session_capsules()
            .map_err(internal)?
            .values()
            .any(|hash| hash == &session.admitted_capsule_hash)
    {
        return Err(internal(
            "hosted candidate session capsule is not sealed by its launch",
        ));
    }
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &launch.execution_closure
    else {
        return Err(internal("hosted candidate has no managed worker launch"));
    };
    let expected_mode = match session.candidate_disposition {
        ryeos_app::runtime_db::DedicatedCandidateDisposition::OwnerDecision => "session",
        ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview => "bounded_turn",
    };
    if prepared_runtime_launch
        .pointer("/runtime_data/worker_execution/mode/kind")
        .and_then(Value::as_str)
        != Some(expected_mode)
    {
        return Err(internal(
            "hosted candidate disposition contradicts its sealed worker mode",
        ));
    }
    let cas_read = state.acquire_cas_read().map_err(internal)?;
    let value = cas_read
        .cas()
        .get_object(&session.admitted_capsule_hash)
        .map_err(internal)?
        .ok_or_else(|| internal("hosted candidate session capsule disappeared"))?;
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)
            .map_err(internal)?;
    if capsule.content_hash().map_err(internal)? != session.admitted_capsule_hash {
        return Err(internal("hosted candidate session capsule hash changed"));
    }
    Ok(launch_hash.to_owned())
}

fn verify_completed_bounded_candidate_result(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    thread: &ryeos_state::objects::ThreadSnapshot,
    base: &str,
    candidate: &str,
    completion_fence: &ryeos_app::dedicated_session_service::HostedCommandCompletionFence,
) -> Result<(), HandlerError> {
    let outcome = session
        .bounded_outcome
        .as_ref()
        .ok_or_else(|| internal("completed bounded candidate has no durable outcome"))?;
    if outcome.kind != ryeos_runtime::callback::DedicatedSessionBoundedOutcomeKind::Completed
        || outcome.dimension.is_some()
        || outcome.approval.is_some()
    {
        return Err(internal(
            "bounded candidate has no successful bounded outcome",
        ));
    }
    require_bounded_candidate_result_terminal(thread, candidate)?;
    let terminal = thread
        .result
        .as_ref()
        .and_then(|value| value.get("session"))
        .ok_or_else(|| internal("bounded candidate root lost its canonical terminal session"))?;
    let operation_id = terminal
        .get("bounded_outcome_testimony_operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("bounded candidate root has no terminal outcome testimony"))?;
    let testimony = exact_root_fact_payload(
        state,
        session,
        "hosted_bounded_execution.terminal_outcome",
        operation_id,
    )?;
    verify_candidate_result_terminal_fact_digest(&testimony, operation_id)?;
    if testimony.get("schema").and_then(Value::as_str) != Some("ryeos.bounded_worker_outcome.v1")
        || testimony.get("chain_root_id").and_then(Value::as_str)
            != Some(session.chain_root_id.as_str())
        || testimony.get("placement_thread_id").and_then(Value::as_str)
            != Some(session.placement_thread_id.as_str())
        || testimony
            .get("admitted_capsule_hash")
            .and_then(Value::as_str)
            != Some(session.admitted_capsule_hash.as_str())
        || testimony.get("workspace")
            != Some(&json!({
                "workspace_id":session.workspace_id,
                "base_snapshot_hash":base,
                "candidate_snapshot_hash":candidate,
            }))
        || testimony.get("outcome") != Some(&serde_json::to_value(outcome).map_err(internal)?)
        || testimony.get("terminal_reason").and_then(Value::as_str) != Some("completed")
        || testimony.get("completion_fence")
            != Some(&serde_json::to_value(completion_fence).map_err(internal)?)
        || testimony.get("execution").is_none()
        || terminal.get("bounded_execution") != testimony.get("execution")
        || terminal.get("bounded_outcome") != testimony.get("outcome")
        || terminal.get("completion_fence") != testimony.get("completion_fence")
        || terminal
            .get("candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(candidate)
        || terminal.get("publication_result").and_then(Value::as_str) != Some("retained_for_review")
    {
        return Err(internal(
            "bounded candidate contradicts its exact terminal outcome testimony",
        ));
    }
    Ok(())
}

/// Use the lifecycle owner's typed terminal classification, as retained-result
/// import and product qualification do. Outcome labels belong to the admitted
/// execution (for example `exit:0` for a subprocess), not to this generic
/// candidate authority. Completion does not imply evaluator acceptance.
fn candidate_execution_succeeded(thread: &ryeos_state::objects::ThreadSnapshot) -> bool {
    thread.status == ryeos_state::objects::ThreadStatus::Completed && thread.error.is_none()
}

fn require_bounded_candidate_result_terminal(
    thread: &ryeos_state::objects::ThreadSnapshot,
    candidate: &str,
) -> Result<(), HandlerError> {
    if !candidate_execution_succeeded(thread)
        || thread.result_project_snapshot_hash.as_deref() != Some(candidate)
    {
        return Err(internal(
            "bounded candidate has no authoritative successful terminal result",
        ));
    }
    Ok(())
}

fn verify_candidate_result_terminal_fact_digest(
    testimony: &Value,
    operation_id: &str,
) -> Result<(), HandlerError> {
    let mut payload = testimony.clone();
    let recorded = payload
        .as_object_mut()
        .and_then(|object| object.remove("operation_id"));
    if recorded.as_ref().and_then(Value::as_str) != Some(operation_id)
        || ryeos_state::objects::canonical_value_digest(&payload).map_err(internal)? != operation_id
    {
        return Err(internal(
            "bounded candidate terminal testimony digest changed",
        ));
    }
    Ok(())
}

fn exact_root_fact_payload(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    event_type: &str,
    operation_id: &str,
) -> Result<Value, HandlerError> {
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        event_type,
        operation_id,
    )
    .map_err(internal)?;
    if fact.count != 1 {
        return Err(internal(format!(
            "hosted candidate root fact `{event_type}` is absent or duplicated"
        )));
    }
    fact.payload.ok_or_else(|| {
        internal(format!(
            "hosted candidate root fact `{event_type}` has no replayable payload"
        ))
    })
}

fn handoff_status_value(
    state: Option<&AppState>,
    job: ryeos_state::SyncJobRecord,
    operation: ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
) -> Result<Value> {
    if operation.role == ryeos_app::worker_handoff::WorkerHandoffJobRole::Source {
        validate_source_handoff_job_coordinates(&job, &operation)?;
    }
    let (progress, terminal_result) = match job.state {
        ryeos_state::SyncJobState::Completed => {
            let result = match operation.role {
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Source => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("source handoff status requires chain authority")
                    })?;
                    match validate_source_handoff_terminal(state, &job, &operation)? {
                        ValidatedSourceHandoffTerminal::Completed(receipt) => {
                            serde_json::to_value(receipt)?
                        }
                        ValidatedSourceHandoffTerminal::TargetCompleted(completion) => {
                            serde_json::to_value(completion)?
                        }
                        _ => anyhow::bail!(
                            "completed source handoff validated as a non-success terminal"
                        ),
                    }
                }
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Target => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("target handoff status requires terminal authority")
                    })?;
                    if job.phase == "target_completed_before_attachment" {
                        let permanent = state
                            .state_store
                            .worker_handoff_terminal_completion(&operation.operation_id)?
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "target-completed handoff has no signed completion receipt"
                                )
                            })?;
                        if permanent.target_operation != operation
                            || job.heads.as_slice()
                                != [permanent.completion.target_chain_head_hash.as_str()]
                        {
                            anyhow::bail!(
                                "target-completed handoff contradicts its signed completion receipt"
                            );
                        }
                        let branch_hash = state
                            .state_store
                            .worker_handoff_target_branch_hash(&operation.operation_id)?
                            .ok_or_else(|| {
                                anyhow::anyhow!("target-completed handoff branch head is absent")
                            })?;
                        if !job.roots.iter().any(|root| root == &branch_hash) {
                            anyhow::bail!(
                                "target-completed handoff did not retain its signed receipt"
                            );
                        }
                        let authority = state.state_store.pinned_state_authority()?;
                        let guard = authority.acquire_shared_guard()?;
                        let cas = authority.cas_store()?;
                        ryeos_state::sync::verify_chain_closure_anchored_pinned(
                            &cas,
                            &operation.chain_root_id,
                            &permanent.completion.target_chain_head_hash,
                            &permanent.request.target_chain_head_hash,
                        )?;
                        validate_source_handoff_terminal_successor(
                            &cas,
                            &operation,
                            &permanent.completion.target_chain_head_hash,
                            &permanent.completion.terminal_status,
                        )?;
                        drop(guard);
                        drop(authority);
                        serde_json::to_value(permanent.completion)?
                    } else {
                        let result = job.result.clone().ok_or_else(|| {
                            anyhow::anyhow!("completed handoff has no adoption receipt")
                        })?;
                        let receipt: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
                            serde_json::from_value(result.clone())?;
                        if receipt.operation_id != operation.operation_id
                            || receipt.chain_root_id != operation.chain_root_id
                            || receipt.placement_thread_id
                                != operation.successor_placement_thread_id
                            || receipt.delivery != "attached"
                        {
                            anyhow::bail!(
                                "completed handoff receipt contradicts its durable operation"
                            );
                        }
                        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
                            "completed handoff target head",
                            &receipt.target_chain_head_hash,
                        )?;
                        let permanent = state
                            .state_store
                            .worker_handoff_adoption_receipt(&operation.operation_id)?
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "completed target handoff has no signed adoption receipt"
                                )
                            })?;
                        if permanent.target_operation != operation || permanent.response != receipt
                        {
                            anyhow::bail!(
                                "completed target handoff job contradicts its signed adoption receipt"
                            );
                        }
                        result
                    }
                }
            };
            (None, Some(result))
        }
        ryeos_state::SyncJobState::Cancelled => {
            let result = match operation.role {
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Source => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("source handoff status requires chain authority")
                    })?;
                    let ValidatedSourceHandoffTerminal::Cancelled(receipt) =
                        validate_source_handoff_terminal(state, &job, &operation)?
                    else {
                        anyhow::bail!("cancelled source handoff validated as another terminal");
                    };
                    serde_json::to_value(receipt)?
                }
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Target => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("target handoff status requires terminal authority")
                    })?;
                    let result = job
                        .result
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("cancelled handoff has no abort receipt"))?;
                    let receipt: ryeos_app::worker_handoff::WorkerPlacementAbortResponse =
                        serde_json::from_value(result.clone())?;
                    if receipt.operation_id != operation.operation_id
                        || receipt.chain_root_id != operation.chain_root_id
                        || !matches!(
                            receipt.disposition.as_str(),
                            "reservation_released" | "target_absent"
                        )
                    {
                        anyhow::bail!(
                            "cancelled handoff receipt contradicts its durable operation"
                        );
                    }
                    let fence = state
                        .state_store
                        .worker_handoff_abort_fence(&operation.operation_id)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "cancelled target handoff has no signed terminal abort receipt"
                            )
                        })?;
                    if fence.target_operation != operation
                        || fence.terminal_disposition.as_deref()
                            != Some(receipt.disposition.as_str())
                        || job.heads.as_slice() != [fence.abort_chain_head_hash.as_str()]
                    {
                        anyhow::bail!(
                            "cancelled target handoff job contradicts its signed abort receipt"
                        );
                    }
                    result
                }
            };
            (None, Some(result))
        }
        ryeos_state::SyncJobState::Failed => {
            if job.phase != "target_terminal_before_attachment" {
                anyhow::bail!("failed handoff job has another terminal phase");
            }
            let failure = match operation.role {
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Source => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("source handoff status requires chain authority")
                    })?;
                    let ValidatedSourceHandoffTerminal::Failed(failure) =
                        validate_source_handoff_terminal(state, &job, &operation)?
                    else {
                        anyhow::bail!("failed source handoff validated as another terminal");
                    };
                    failure
                }
                ryeos_app::worker_handoff::WorkerHandoffJobRole::Target => {
                    let state = state.ok_or_else(|| {
                        anyhow::anyhow!("target handoff status requires terminal authority")
                    })?;
                    let permanent = state
                        .state_store
                        .worker_handoff_terminal_failure(&operation.operation_id)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "failed target handoff has no signed terminal failure receipt"
                            )
                        })?;
                    if permanent.target_operation != operation
                        || job.heads.as_slice()
                            != [permanent.failure.target_chain_head_hash.as_str()]
                    {
                        anyhow::bail!(
                            "failed target handoff job contradicts its signed failure receipt"
                        );
                    }
                    let branch_hash = state
                        .state_store
                        .worker_handoff_target_branch_hash(&operation.operation_id)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("failed target handoff branch head is absent")
                        })?;
                    if !job.roots.iter().any(|root| root == &branch_hash) {
                        anyhow::bail!("failed target handoff did not retain its signed receipt");
                    }
                    let authority = state.state_store.pinned_state_authority()?;
                    let guard = authority.acquire_shared_guard()?;
                    let cas = authority.cas_store()?;
                    ryeos_state::sync::verify_chain_closure_anchored_pinned(
                        &cas,
                        &operation.chain_root_id,
                        &permanent.failure.target_chain_head_hash,
                        &permanent.request.target_chain_head_hash,
                    )?;
                    validate_source_handoff_terminal_successor(
                        &cas,
                        &operation,
                        &permanent.failure.target_chain_head_hash,
                        &permanent.failure.terminal_status,
                    )?;
                    drop(guard);
                    drop(authority);
                    permanent.failure
                }
            };
            (None, Some(serde_json::to_value(failure)?))
        }
        _ => {
            let progress = job
                .result
                .clone()
                .map(ryeos_app::worker_handoff::WorkerSessionHandoffProgress::from_value)
                .transpose()?;
            if let Some(progress) = &progress
                && progress.operation_id != operation.operation_id
            {
                anyhow::bail!("handoff progress belongs to another operation");
            }
            (progress, None)
        }
    };
    let recovery_required = matches!(
        job.state,
        ryeos_state::SyncJobState::Planned
            | ryeos_state::SyncJobState::Running
            | ryeos_state::SyncJobState::Retryable
    );
    let operator_action = match job.state {
        ryeos_state::SyncJobState::Retryable => "retry_exact_operation",
        ryeos_state::SyncJobState::Failed => "inspect_terminal_failure",
        _ => "none",
    };
    Ok(json!({
        "schema":"ryeos.worker_handoff_status.v1",
        "operation_id":operation.operation_id,
        "role":operation.role,
        "source_placement_thread_id":operation.source_placement_thread_id,
        "successor_placement_thread_id":operation.successor_placement_thread_id,
        "source_site_id":operation.source_site_id,
        "target_site_id":operation.target_site_id,
        "state":job.state.as_str(),
        "phase":job.phase,
        "durable_progress":progress,
        "terminal_result":terminal_result,
        "attempt_count":job.attempt_count,
        "max_attempts":job.max_attempts,
        "last_error":job.last_error,
        "recovery_required":recovery_required,
        "operator_action":operator_action,
        "terminal_disposition":match job.state {
            ryeos_state::SyncJobState::Completed => Some("completed"),
            ryeos_state::SyncJobState::Cancelled => Some("aborted"),
            ryeos_state::SyncJobState::Failed => Some("failed"),
            _ => None,
        },
    }))
}

const HANDOFF_STATUS_SCAN_PAGE: usize = 128;

/// Find the newest durable handoff operation for one stable chain without a
/// global-history cap. Rows are narrowed by the existing operation-type index
/// and traversed through immutable reverse keyset coordinates.
fn latest_handoff_job_for_chain(
    db: &ryeos_state::StateDb,
    chain_root_id: &str,
) -> Result<
    Option<(
        ryeos_state::SyncJobRecord,
        ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    )>,
> {
    let mut before: Option<(String, String)> = None;
    loop {
        let page = db.list_sync_jobs_by_operation_type_before(
            ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION,
            before
                .as_ref()
                .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
            HANDOFF_STATUS_SCAN_PAGE,
        )?;
        let Some(last) = page.last() else {
            return Ok(None);
        };
        let next_before = (last.created_at.clone(), last.job_id.clone());
        for job in page {
            let operation =
                ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
                    job.operation.clone(),
                )?;
            if operation.chain_root_id == chain_root_id {
                return Ok(Some((job, operation)));
            }
        }
        before = Some(next_before);
    }
}

fn source_handoff_job_for_request(
    db: &ryeos_state::StateDb,
    req: &HandoffRequest,
) -> Result<
    Option<(
        ryeos_state::SyncJobRecord,
        ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    )>,
> {
    let mut matched = None;
    let mut before: Option<(String, String)> = None;
    loop {
        let page = db.list_sync_jobs_by_operation_type_before(
            ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION,
            before
                .as_ref()
                .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
            HANDOFF_STATUS_SCAN_PAGE,
        )?;
        let Some(last) = page.last() else {
            return Ok(matched);
        };
        let next_before = (last.created_at.clone(), last.job_id.clone());
        for job in page {
            let operation =
                ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
                    job.operation.clone(),
                )?;
            if operation.role == ryeos_app::worker_handoff::WorkerHandoffJobRole::Source {
                validate_source_handoff_job_coordinates(&job, &operation)?;
            }
            if operation.role != ryeos_app::worker_handoff::WorkerHandoffJobRole::Source
                || operation.chain_root_id != req.chain_root_id
                || operation.peer_remote_name != req.remote
                || operation.preflight_id != req.preflight_id
                || operation.target_credential_profile_id != req.target_credential_profile_id
                || format!("cas:{}", operation.checkpoint_manifest_hash) != req.manifest_ref
            {
                continue;
            }
            if matched.is_some() {
                anyhow::bail!("multiple durable handoffs match one exact operator request");
            }
            matched = Some((job, operation));
        }
        before = Some(next_before);
    }
}

fn validate_source_handoff_job_coordinates(
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
) -> Result<()> {
    operation.validate()?;
    let expected_job_id = format!("worker-handoff-source:{}", operation.operation_id);
    if operation.role != ryeos_app::worker_handoff::WorkerHandoffJobRole::Source
        || job.job_id != expected_job_id
        || job.operation_type != ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
        || job.peer.as_deref() != Some(operation.peer_remote_name.as_str())
        || job.operation != operation.to_value()?
        || job.max_attempts != ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS
        || !job.attempt_count_is_valid()
    {
        anyhow::bail!("source handoff job is not the canonical operation coordinate");
    }
    Ok(())
}

fn any_rooted_target_handoff_terminal_attestation(
    cas: &lillux::CasStore,
    job: &ryeos_state::SyncJobRecord,
) -> Result<Option<ryeos_state::objects::Attestation>> {
    let mut terminal = None;
    for root in &job.roots {
        let Some(value) = cas.get_object(root)? else {
            continue;
        };
        let policy = value.get("policy").and_then(Value::as_str);
        if !matches!(
            policy,
            Some(ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY)
                | Some(ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_POLICY)
                | Some(ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_COMPLETION_POLICY)
                | Some(ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_POLICY)
        ) {
            continue;
        }
        if terminal.is_some() {
            anyhow::bail!("source handoff roots retain multiple target terminal attestations");
        }
        if ryeos_state::objects::canonical_value_digest(&value)? != *root {
            anyhow::bail!("source handoff target terminal attestation changed its CAS digest");
        }
        let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
        terminal = Some(attestation);
    }
    Ok(terminal)
}

fn rooted_target_handoff_terminal_attestation(
    cas: &lillux::CasStore,
    job: &ryeos_state::SyncJobRecord,
    target: &crate::remote::config::RemoteConfig,
    expected_policy: &str,
    expected_claim: &str,
) -> Result<ryeos_state::objects::Attestation> {
    let attestation =
        any_rooted_target_handoff_terminal_attestation(cas, job)?.ok_or_else(|| {
            anyhow::anyhow!("source handoff has no rooted target terminal attestation")
        })?;
    if attestation.policy != expected_policy
        || attestation.claim != expected_claim
        || attestation.issuer != target.principal_id
        || attestation.expires_at.is_some()
    {
        anyhow::bail!("source handoff retained another target terminal authority");
    }
    attestation.verify_with_key(&target.pinned_signing_key()?)?;
    Ok(attestation)
}

fn retained_preflight_target_signer_fingerprint(
    cas: &lillux::CasStore,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    target: &crate::remote::config::RemoteConfig,
) -> Result<String> {
    let value = cas
        .get_object(&operation.preflight_attestation_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!("source handoff retained preflight attestation is absent")
        })?;
    if ryeos_state::objects::canonical_value_digest(&value)? != operation.preflight_attestation_hash
    {
        anyhow::bail!("source handoff retained preflight attestation changed digest");
    }
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    let target_key = target.pinned_signing_key()?;
    let target_fingerprint = lillux::crypto::fingerprint(&target_key);
    if attestation.issuer != target.principal_id
        || attestation.issuer_fingerprint()? != target_fingerprint
        || attestation.expires_at.is_some()
    {
        anyhow::bail!("source handoff target signer changed since its retained preflight");
    }
    attestation.verify_with_key(&target_key)?;
    let evidence = ryeos_app::worker_handoff::WorkerPlacementPreflightEvidence::from_attestation(
        &attestation,
    )?;
    if evidence.preflight_id != operation.preflight_id
        || evidence.owner_principal != operation.owner_principal
        || evidence.chain_root_id != operation.chain_root_id
        || evidence.origin_site_id != operation.origin_site_id
        || evidence.source_site_id != operation.source_site_id
        || evidence.target_site_id != operation.target_site_id
        || evidence.source_placement_thread_id != operation.source_placement_thread_id
        || evidence.successor_placement_thread_id != operation.successor_placement_thread_id
        || evidence.source_chain_head_hash != operation.source_chain_head_hash
        || evidence.source_last_event_hash != operation.source_last_event_hash
        || evidence.target_project_path != operation.target_project_path
        || evidence.project_route_digest != operation.project_route_digest
        || evidence.target_credential_profile_id != operation.target_credential_profile_id
        || evidence.follow_delivery_reservation_attestation_hash
            != operation.follow_delivery_reservation_attestation_hash
    {
        anyhow::bail!("source handoff operation differs from its retained signed preflight");
    }
    Ok(target_fingerprint)
}

enum ValidatedSourceHandoffTerminal {
    Completed(ryeos_app::worker_handoff::WorkerPlacementAdoptResponse),
    TargetCompleted(ryeos_app::worker_handoff::WorkerPlacementCompletionResponse),
    Cancelled(ryeos_app::worker_handoff::WorkerPlacementAbortResponse),
    Failed(ryeos_app::worker_handoff::WorkerPlacementFailureResponse),
}

/// Settle an active source job from target-signed testimony that is already
/// rooted in its local CAS closure. This path deliberately creates no retry
/// attempt and performs no network contact: once the exact terminal receipt
/// is durable locally, attempt budget and target availability are no longer
/// authority for completing the source projection.
fn fold_rooted_source_handoff_terminal(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
) -> Result<Option<ValidatedSourceHandoffTerminal>> {
    validate_source_handoff_job_coordinates(job, operation)?;
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Completed
            | ryeos_state::SyncJobState::Failed
            | ryeos_state::SyncJobState::Cancelled
    ) {
        return validate_source_handoff_terminal(state, job, operation).map(Some);
    }

    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let terminal = any_rooted_target_handoff_terminal_attestation(&authority.cas_store()?, job)?;
    drop(guard);
    drop(authority);
    let Some(terminal) = terminal else {
        return Ok(None);
    };

    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(std::path::Path::new(&operation.source_project_path)),
    )?;
    let target =
        crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)?;
    if target.config.site_id != operation.target_site_id {
        anyhow::bail!("rooted source terminal configured target site changed");
    }
    let target_key = target.config.pinned_signing_key()?;

    let mut terminal_job = job.clone();
    terminal_job.last_error = None;
    match terminal.policy.as_str() {
        ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY => {
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffAdoptionReceiptEvidence::from_attestation(
                    &terminal,
                    &target_key,
                )?;
            terminal_job.state = ryeos_state::SyncJobState::Completed;
            terminal_job.phase = "completed".to_owned();
            terminal_job.result = Some(serde_json::to_value(receipt.response)?);
        }
        ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_POLICY => {
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffAbortFenceEvidence::from_attestation(
                    &terminal,
                    &target_key,
                )?;
            let disposition = receipt.terminal_disposition.ok_or_else(|| {
                anyhow::anyhow!("rooted target abort fence is not terminal testimony")
            })?;
            terminal_job.state = ryeos_state::SyncJobState::Cancelled;
            terminal_job.phase = "aborted".to_owned();
            terminal_job.result = Some(serde_json::to_value(
                ryeos_app::worker_handoff::WorkerPlacementAbortResponse {
                    operation_id: operation.operation_id.clone(),
                    chain_root_id: operation.chain_root_id.clone(),
                    disposition,
                },
            )?);
        }
        ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_POLICY => {
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffTerminalFailureEvidence::from_attestation(
                    &terminal,
                    &target_key,
                )?;
            terminal_job.state = ryeos_state::SyncJobState::Failed;
            terminal_job.phase = "target_terminal_before_attachment".to_owned();
            terminal_job.heads = vec![receipt.failure.target_chain_head_hash.clone()];
            terminal_job.last_error = Some(format!(
                "target placement terminalized {} before worker attachment",
                receipt.failure.terminal_status
            ));
            terminal_job.result = Some(serde_json::to_value(receipt.failure)?);
        }
        ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_COMPLETION_POLICY => {
            let receipt = ryeos_app::worker_handoff::WorkerHandoffTerminalCompletionEvidence::from_attestation(
                &terminal,
                &target_key,
            )?;
            terminal_job.state = ryeos_state::SyncJobState::Completed;
            terminal_job.phase = "target_completed_before_attachment".to_owned();
            terminal_job.heads = vec![receipt.completion.target_chain_head_hash.clone()];
            terminal_job.result = Some(serde_json::to_value(receipt.completion)?);
        }
        _ => anyhow::bail!("source handoff retained an unknown terminal receipt policy"),
    }

    // Validate the prospective terminal row against the complete signed
    // chain/receipt braid before allowing the mutable operational row to
    // reflect it.
    let prospective = validate_source_handoff_terminal(state, &terminal_job, operation)?;
    state.state_store.with_state_db(|db| {
        let current = db
            .get_sync_job(&job.job_id)?
            .ok_or_else(|| anyhow::anyhow!("source handoff job disappeared during receipt fold"))?;
        if current != *job {
            anyhow::bail!("source handoff job changed during terminal receipt fold");
        }
        db.update_sync_job(
            &job.job_id,
            &ryeos_state::SyncJobUpdate {
                state: terminal_job.state,
                phase: terminal_job.phase.clone(),
                roots: None,
                heads: Some(terminal_job.heads.clone()),
                uploaded_hashes: terminal_job.uploaded_hashes.clone(),
                fetched_hashes: terminal_job.fetched_hashes.clone(),
                last_error: terminal_job.last_error.clone(),
                result: terminal_job.result.clone(),
            },
        )
    })?;
    Ok(Some(prospective))
}

fn validate_source_handoff_terminal(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
) -> Result<ValidatedSourceHandoffTerminal> {
    validate_source_handoff_job_coordinates(job, operation)?;
    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(std::path::Path::new(&operation.source_project_path)),
    )?;
    let target =
        crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)?;
    if target.config.site_id != operation.target_site_id {
        anyhow::bail!("terminal source handoff configured target site changed");
    }
    let [terminal_head] = job.heads.as_slice() else {
        anyhow::bail!("terminal source handoff job must retain one exact chain head");
    };
    let current_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .ok_or_else(|| anyhow::anyhow!("terminal source handoff chain head is absent"))?;
    if current_head.signer != state.identity.fingerprint() {
        anyhow::bail!("terminal source handoff chain head is not locally signed");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &cas,
        &operation.chain_root_id,
        terminal_head,
        &current_head.target_hash,
    )?;

    let terminal = match job.state {
        ryeos_state::SyncJobState::Completed => {
            if job.phase == "target_completed_before_attachment" {
                let completion: ryeos_app::worker_handoff::WorkerPlacementCompletionResponse =
                    serde_json::from_value(job.result.clone().ok_or_else(|| {
                        anyhow::anyhow!("target-completed source handoff has no result")
                    })?)?;
                if completion.operation_id != operation.operation_id
                    || completion.chain_root_id != operation.chain_root_id
                    || completion.placement_thread_id != operation.successor_placement_thread_id
                    || completion.target_chain_head_hash != *terminal_head
                    || completion.terminal_status != "completed"
                {
                    anyhow::bail!(
                        "target-completed source handoff contradicts its operation or head"
                    );
                }
                let continuation = validate_source_handoff_continuation_head(
                    &cas,
                    operation,
                    &current_head.target_hash,
                )?;
                let current_target_fingerprint =
                    lillux::crypto::fingerprint(&target.config.pinned_signing_key()?);
                if continuation.target_node_signer_fingerprint != current_target_fingerprint {
                    anyhow::bail!("target-completed handoff signer changed since writer transfer");
                }
                validate_source_handoff_terminal_successor(
                    &cas,
                    operation,
                    terminal_head,
                    &completion.terminal_status,
                )?;
                let attestation = rooted_target_handoff_terminal_attestation(
                    &cas,
                    job,
                    &target.config,
                    ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_COMPLETION_POLICY,
                    ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_COMPLETION_CLAIM,
                )?;
                if attestation.issuer_fingerprint()? != continuation.target_node_signer_fingerprint
                {
                    anyhow::bail!(
                        "target completion receipt signer differs from the signed continuation"
                    );
                }
                let receipt = ryeos_app::worker_handoff::WorkerHandoffTerminalCompletionEvidence::from_attestation(
                    &attestation,
                    &target.config.pinned_signing_key()?,
                )?;
                receipt
                    .target_operation
                    .validate_target_projection_of(operation)?;
                let expected_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
                    operation_id: operation.operation_id.clone(),
                    chain_root_id: operation.chain_root_id.clone(),
                    target_chain_head_hash: current_head.target_hash.clone(),
                    placement_attestation_hash: continuation.target_placement_attestation_hash,
                    writer_grant_hash: continuation.chain_writer_grant_hash,
                };
                if receipt.request != expected_request || receipt.completion != completion {
                    anyhow::bail!(
                        "target-completed handoff contradicts its signed terminal receipt"
                    );
                }
                ValidatedSourceHandoffTerminal::TargetCompleted(completion)
            } else {
                if job.phase != "completed" {
                    anyhow::bail!("completed source handoff job has another terminal phase");
                }
                let adopted: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
                    serde_json::from_value(job.result.clone().ok_or_else(|| {
                        anyhow::anyhow!("completed source handoff has no result")
                    })?)?;
                if adopted.operation_id != operation.operation_id
                    || adopted.chain_root_id != operation.chain_root_id
                    || adopted.placement_thread_id != operation.successor_placement_thread_id
                    || adopted.target_chain_head_hash != *terminal_head
                    || adopted.delivery != "attached"
                {
                    anyhow::bail!(
                        "completed source handoff result contradicts its exact operation or head"
                    );
                }
                let continuation =
                    validate_source_handoff_continuation_head(&cas, operation, terminal_head)?;
                let current_target_fingerprint =
                    lillux::crypto::fingerprint(&target.config.pinned_signing_key()?);
                if continuation.target_node_signer_fingerprint != current_target_fingerprint {
                    anyhow::bail!(
                        "completed source handoff target signer changed since writer transfer"
                    );
                }
                let attestation = rooted_target_handoff_terminal_attestation(
                    &cas,
                    job,
                    &target.config,
                    ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY,
                    ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_CLAIM,
                )?;
                if attestation.issuer_fingerprint()? != continuation.target_node_signer_fingerprint
                {
                    anyhow::bail!(
                        "target adoption receipt signer differs from the signed continuation"
                    );
                }
                let receipt =
                ryeos_app::worker_handoff::WorkerHandoffAdoptionReceiptEvidence::from_attestation(
                    &attestation,
                    &target.config.pinned_signing_key()?,
                )?;
                receipt
                    .target_operation
                    .validate_target_projection_of(operation)?;
                let expected_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
                    operation_id: operation.operation_id.clone(),
                    chain_root_id: operation.chain_root_id.clone(),
                    target_chain_head_hash: terminal_head.clone(),
                    placement_attestation_hash: continuation.target_placement_attestation_hash,
                    writer_grant_hash: continuation.chain_writer_grant_hash,
                };
                if receipt.request != expected_request || receipt.response != adopted {
                    anyhow::bail!(
                        "completed source handoff contradicts its target-signed terminal receipt"
                    );
                }
                ValidatedSourceHandoffTerminal::Completed(adopted)
            }
        }
        ryeos_state::SyncJobState::Cancelled => {
            if job.phase != "aborted" {
                anyhow::bail!("cancelled source handoff job has another terminal phase");
            }
            let aborted: ryeos_app::worker_handoff::WorkerPlacementAbortResponse =
                serde_json::from_value(
                    job.result
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("cancelled source handoff has no result"))?,
                )?;
            let request = ryeos_app::worker_handoff::WorkerPlacementAbortRequest {
                operation: operation.clone(),
                abort_chain_head_hash: terminal_head.clone(),
            };
            aborted.validate_against(&request)?;
            ryeos_app::worker_handoff::validate_handoff_abort_authority(
                &cas,
                operation,
                terminal_head,
            )?;
            let preflight_target_fingerprint =
                retained_preflight_target_signer_fingerprint(&cas, operation, &target.config)?;
            let attestation = rooted_target_handoff_terminal_attestation(
                &cas,
                job,
                &target.config,
                ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_POLICY,
                ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_CLAIM,
            )?;
            if attestation.issuer_fingerprint()? != preflight_target_fingerprint {
                anyhow::bail!("target abort receipt signer differs from the retained preflight");
            }
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffAbortFenceEvidence::from_attestation(
                    &attestation,
                    &target.config.pinned_signing_key()?,
                )?;
            receipt
                .target_operation
                .validate_target_projection_of(operation)?;
            if receipt.abort_chain_head_hash != *terminal_head
                || receipt.terminal_disposition.as_deref() != Some(aborted.disposition.as_str())
            {
                anyhow::bail!(
                    "cancelled source handoff contradicts its target-signed terminal receipt"
                );
            }
            ValidatedSourceHandoffTerminal::Cancelled(aborted)
        }
        ryeos_state::SyncJobState::Failed => {
            if job.phase != "target_terminal_before_attachment" {
                anyhow::bail!("failed source handoff job has another terminal phase");
            }
            let failed: ryeos_app::worker_handoff::WorkerPlacementFailureResponse =
                serde_json::from_value(
                    job.result
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("failed source handoff has no result"))?,
                )?;
            if failed.operation_id != operation.operation_id
                || failed.chain_root_id != operation.chain_root_id
                || failed.placement_thread_id != operation.successor_placement_thread_id
                || failed.target_chain_head_hash != *terminal_head
            {
                anyhow::bail!(
                    "failed source handoff result contradicts its exact operation or head"
                );
            }
            let continuation = validate_source_handoff_continuation_head(
                &cas,
                operation,
                &current_head.target_hash,
            )?;
            let current_target_fingerprint =
                lillux::crypto::fingerprint(&target.config.pinned_signing_key()?);
            if continuation.target_node_signer_fingerprint != current_target_fingerprint {
                anyhow::bail!("failed source handoff target signer changed since writer transfer");
            }
            validate_source_handoff_terminal_successor(
                &cas,
                operation,
                terminal_head,
                &failed.terminal_status,
            )?;
            let attestation = rooted_target_handoff_terminal_attestation(
                &cas,
                job,
                &target.config,
                ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_POLICY,
                ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_CLAIM,
            )?;
            if attestation.issuer_fingerprint()? != continuation.target_node_signer_fingerprint {
                anyhow::bail!("target failure receipt signer differs from the signed continuation");
            }
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffTerminalFailureEvidence::from_attestation(
                    &attestation,
                    &target.config.pinned_signing_key()?,
                )?;
            receipt
                .target_operation
                .validate_target_projection_of(operation)?;
            let expected_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
                operation_id: operation.operation_id.clone(),
                chain_root_id: operation.chain_root_id.clone(),
                target_chain_head_hash: current_head.target_hash.clone(),
                placement_attestation_hash: continuation.target_placement_attestation_hash,
                writer_grant_hash: continuation.chain_writer_grant_hash,
            };
            if receipt.request != expected_request || receipt.failure != failed {
                anyhow::bail!(
                    "failed source handoff contradicts its target-signed terminal receipt"
                );
            }
            ValidatedSourceHandoffTerminal::Failed(failed)
        }
        _ => anyhow::bail!("source handoff job is not terminal"),
    };
    drop(guard);
    Ok(terminal)
}

fn validate_source_handoff_terminal_successor(
    cas: &lillux::CasStore,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    head_hash: &str,
    terminal_status: &str,
) -> Result<()> {
    use ryeos_state::objects::{ChainState, ThreadEvent, ThreadSnapshot, ThreadStatus};

    let status = ThreadStatus::from_str_lossy(terminal_status)
        .filter(|status| status.is_terminal() && *status != ThreadStatus::Continued)
        .ok_or_else(|| anyhow::anyhow!("target terminal receipt names an unsupported status"))?;
    let head: ChainState = serde_json::from_value(
        cas.get_object(head_hash)?
            .ok_or_else(|| anyhow::anyhow!("target terminal chain head is absent"))?,
    )?;
    head.validate()?;
    if head.chain_root_id != operation.chain_root_id {
        anyhow::bail!("target terminal chain belongs to another execution");
    }
    let successor = head
        .threads
        .get(&operation.successor_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("target terminal chain lost its successor placement"))?;
    if successor.status != status {
        anyhow::bail!("target terminal chain status contradicts its signed receipt");
    }
    let snapshot: ThreadSnapshot = serde_json::from_value(
        cas.get_object(&successor.snapshot_hash)?
            .ok_or_else(|| anyhow::anyhow!("target terminal successor snapshot is absent"))?,
    )?;
    snapshot.validate()?;
    if snapshot.thread_id != operation.successor_placement_thread_id
        || snapshot.chain_root_id != operation.chain_root_id
        || snapshot.upstream_thread_id.as_deref()
            != Some(operation.source_placement_thread_id.as_str())
        || snapshot.current_site_id != operation.target_site_id
        || snapshot.status != status
        || snapshot.finished_at.is_none()
        || snapshot.last_event_hash != successor.last_event_hash
    {
        anyhow::bail!("target terminal successor snapshot contradicts its signed receipt");
    }
    let event_hash = successor
        .last_event_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("target successor has no terminal event"))?;
    let event: ThreadEvent = serde_json::from_value(
        cas.get_object(event_hash)?
            .ok_or_else(|| anyhow::anyhow!("target terminal event is absent"))?,
    )?;
    event.validate()?;
    let expected_event_type = match status {
        ThreadStatus::Completed => ryeos_state::event_types::THREAD_COMPLETED,
        ThreadStatus::Failed => ryeos_state::event_types::THREAD_FAILED,
        ThreadStatus::Cancelled => ryeos_state::event_types::THREAD_CANCELLED,
        ThreadStatus::Killed => ryeos_state::event_types::THREAD_KILLED,
        ThreadStatus::TimedOut => ryeos_state::event_types::THREAD_TIMED_OUT,
        _ => anyhow::bail!("target receipt carries an unsupported terminal status"),
    };
    if event.chain_root_id != operation.chain_root_id
        || event.thread_id != operation.successor_placement_thread_id
        || event.event_type != expected_event_type
        || event.chain_seq != snapshot.last_chain_seq
        || event.thread_seq != snapshot.last_thread_seq
    {
        anyhow::bail!("target terminal event contradicts its signed receipt");
    }
    Ok(())
}

fn validate_source_handoff_continuation_head(
    cas: &lillux::CasStore,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    head_hash: &str,
) -> Result<ryeos_state::objects::RemoteContinuationAuthority> {
    use ryeos_state::objects::{
        ChainState, RemoteContinuationAuthority, ThreadEvent, ThreadSnapshot, ThreadStatus,
    };

    let head: ChainState = serde_json::from_value(
        cas.get_object(head_hash)?
            .ok_or_else(|| anyhow::anyhow!("source handoff continuation head is absent"))?,
    )?;
    head.validate()?;
    if head.chain_root_id != operation.chain_root_id
        || head.prev_chain_state_hash.as_deref() != Some(operation.source_chain_head_hash.as_str())
    {
        anyhow::bail!("source handoff continuation is not the immediate source-head successor");
    }
    let source = head
        .threads
        .get(&operation.source_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("source handoff continuation lost its source placement"))?;
    if source.status != ThreadStatus::Continued {
        anyhow::bail!("source handoff continuation did not terminalize its source placement");
    }
    let continuation_hash = source
        .last_event_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("source handoff continuation has no edge"))?;
    let continuation: ThreadEvent = serde_json::from_value(
        cas.get_object(continuation_hash)?
            .ok_or_else(|| anyhow::anyhow!("source handoff continuation edge is absent"))?,
    )?;
    continuation.validate()?;
    if continuation.event_type != "thread_continued"
        || continuation.chain_root_id != operation.chain_root_id
        || continuation.thread_id != operation.source_placement_thread_id
        || continuation.prev_thread_event_hash.as_deref()
            != Some(operation.source_last_event_hash.as_str())
        || continuation
            .payload
            .get("successor_thread_id")
            .and_then(Value::as_str)
            != Some(operation.successor_placement_thread_id.as_str())
        || continuation.payload.get("reason").and_then(Value::as_str) != Some("remote_adoption")
    {
        anyhow::bail!("source handoff continuation edge differs from its operation");
    }
    let remote: RemoteContinuationAuthority = serde_json::from_value(
        continuation
            .payload
            .get("remote_adoption")
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("source handoff continuation has no remote authority")
            })?,
    )?;
    remote.validate()?;
    if remote.operation_id != operation.operation_id
        || remote.preflight_id != operation.preflight_id
        || remote.preflight_attestation_hash != operation.preflight_attestation_hash
        || remote.follow_delivery_reservation_attestation_hash
            != operation.follow_delivery_reservation_attestation_hash
        || remote.source_chain_head_hash != operation.source_chain_head_hash
        || remote.source_last_event_hash != operation.source_last_event_hash
        || remote.checkpoint_manifest_hash != operation.checkpoint_manifest_hash
        || remote.source_site_id != operation.source_site_id
        || remote.target_site_id != operation.target_site_id
        || remote.successor_thread_id != operation.successor_placement_thread_id
    {
        anyhow::bail!("source handoff continuation authority differs from its operation");
    }
    let successor = head
        .threads
        .get(&operation.successor_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("source handoff continuation lost its successor"))?;
    let successor_snapshot: ThreadSnapshot = serde_json::from_value(
        cas.get_object(&successor.snapshot_hash)?
            .ok_or_else(|| anyhow::anyhow!("source handoff successor snapshot is absent"))?,
    )?;
    successor_snapshot.validate()?;
    if successor_snapshot.thread_id != operation.successor_placement_thread_id
        || successor_snapshot.chain_root_id != operation.chain_root_id
        || successor_snapshot.upstream_thread_id.as_deref()
            != Some(operation.source_placement_thread_id.as_str())
        || successor_snapshot.origin_site_id != operation.origin_site_id
        || successor_snapshot.current_site_id != operation.target_site_id
        || successor_snapshot.requested_by.as_deref() != Some(operation.owner_principal.as_str())
        || successor_snapshot.admitted_launch_capsule_hash.as_deref()
            != Some(remote.target_launch_capsule_hash.as_str())
    {
        anyhow::bail!("source handoff successor snapshot differs from its signed continuation");
    }
    Ok(remote)
}

fn load_persistent_session_capsule(
    state: &AppState,
    capsule_hash: &str,
) -> Result<ryeos_state::objects::AdmittedPersistentSessionCapsule> {
    if !lillux::valid_hash(capsule_hash) {
        anyhow::bail!("persistent-session capsule hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(capsule_hash)?
        .ok_or_else(|| anyhow::anyhow!("persistent-session capsule is absent from CAS"))?;
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != capsule_hash {
        anyhow::bail!("persistent-session capsule content hash changed");
    }
    Ok(capsule)
}

fn load_worker_checkpoint_predecessor(
    state: &AppState,
    chain_root_id: &str,
) -> Result<Option<(String, String)>> {
    let Some((_event_hash, anchor)) = state
        .state_store
        .latest_verified_chain_state_anchor(chain_root_id)?
    else {
        return Ok(None);
    };
    let ryeos_state::objects::StateAnchorSubject::Execution {
        chain_root_id: anchor_chain,
        ..
    } = &anchor.subject
    else {
        anyhow::bail!("worker execution chain contains a non-execution state anchor");
    };
    if anchor_chain != chain_root_id {
        anyhow::bail!("state-anchor execution subject escaped its chain");
    }
    let manifest_hash = anchor
        .payload
        .manifest_ref
        .strip_prefix("cas:")
        .ok_or_else(|| anyhow::anyhow!("state-anchor manifest ref is not canonical"))?
        .to_owned();
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let manifest_value = cas
        .get_object(&manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("state-anchor manifest is absent from CAS"))?;
    let manifest = ryeos_state::objects::StateManifest::from_current_value(manifest_value)?;
    if manifest.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
        || manifest.publisher_chain_root_id != chain_root_id
    {
        anyhow::bail!("latest execution state anchor is not a worker-session checkpoint");
    }
    let restore_bytes = cas
        .get_blob(&manifest.restore.blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("worker-session restore document is absent from CAS"))?;
    if lillux::sha256_hex(&restore_bytes) != manifest.restore.blob_hash {
        anyhow::bail!("worker-session restore document digest mismatch");
    }
    let restore_value: Value = serde_json::from_slice(&restore_bytes)?;
    let restore = ryeos_state::objects::WorkerSessionRestore::from_current_value(restore_value)?;
    if restore.source_position.chain_root_id != chain_root_id {
        anyhow::bail!("worker-session restore belongs to another chain");
    }
    Ok(Some((
        manifest_hash,
        restore.portable_state.incoming_tree_hash,
    )))
}

struct LoadedWorkerCheckpoint {
    manifest_hash: String,
    restore: ryeos_state::objects::WorkerSessionRestore,
    tree: ryeos_state::objects::PortableStateTree,
}

fn load_worker_checkpoint(
    state: &AppState,
    chain_root_id: &str,
    manifest_ref: &str,
) -> Result<LoadedWorkerCheckpoint> {
    let manifest_hash = manifest_ref
        .strip_prefix("cas:")
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| anyhow::anyhow!("checkpoint manifest_ref must be cas:<sha256>"))?
        .to_owned();
    let Some((_event_hash, latest_anchor)) = state
        .state_store
        .latest_verified_chain_state_anchor(chain_root_id)?
    else {
        anyhow::bail!("worker execution chain has no checkpoint anchor");
    };
    if latest_anchor.payload.manifest_ref != manifest_ref {
        anyhow::bail!("checkpoint is not the authoritative latest chain anchor");
    }
    let ryeos_state::objects::StateAnchorSubject::Execution {
        chain_root_id: anchor_chain,
        placement_thread_id: anchor_placement,
        exact_program_hash: anchor_program,
        launch_capsule_hash: anchor_capsule,
        source_chain_seq,
        source_event_hash,
        ..
    } = &latest_anchor.subject
    else {
        anyhow::bail!("worker checkpoint anchor has a non-execution subject");
    };
    if anchor_chain != chain_root_id {
        anyhow::bail!("worker checkpoint anchor belongs to another chain");
    }

    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let manifest_value = cas
        .get_object(&manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("worker checkpoint manifest is absent from CAS"))?;
    if lillux::sha256_hex(lillux::canonical_json(&manifest_value)?.as_bytes()) != manifest_hash {
        anyhow::bail!("worker checkpoint manifest content hash changed");
    }
    let manifest = ryeos_state::objects::StateManifest::from_current_value(manifest_value)?;
    if manifest.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
        || manifest.publisher_chain_root_id != chain_root_id
        || manifest.publisher_thread_id != *anchor_placement
    {
        anyhow::bail!("worker checkpoint manifest contradicts its anchor subject");
    }
    let restore_bytes = cas
        .get_blob(&manifest.restore.blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("worker checkpoint restore document is absent"))?;
    if lillux::sha256_hex(&restore_bytes) != manifest.restore.blob_hash {
        anyhow::bail!("worker checkpoint restore document digest mismatch");
    }
    let restore_value: Value = serde_json::from_slice(&restore_bytes)?;
    if lillux::canonical_json(&restore_value)?.as_bytes() != restore_bytes {
        anyhow::bail!("worker checkpoint restore document is not canonical JSON");
    }
    let restore = ryeos_state::objects::WorkerSessionRestore::from_current_value(restore_value)?;
    if restore.source_position.chain_root_id != chain_root_id
        || restore.source_position.placement_thread_id != *anchor_placement
        || restore.source_position.chain_seq != *source_chain_seq
        || restore.source_position.event_hash != *source_event_hash
        || restore.outer_exact_program_hash != *anchor_program
        || restore.source_launch_capsule_hash != *anchor_capsule
    {
        anyhow::bail!("worker checkpoint restore document contradicts its anchor position");
    }
    let tree_entry = manifest
        .objects
        .iter()
        .find(|object| object.name == restore.portable_state.attachment_name)
        .ok_or_else(|| anyhow::anyhow!("worker checkpoint has no portable-state attachment"))?;
    if tree_entry.media_type != ryeos_state::objects::PORTABLE_STATE_TREE_MEDIA_TYPE
        || tree_entry.blob_hash != restore.portable_state.incoming_tree_hash
    {
        anyhow::bail!("worker checkpoint portable-state attachment identity changed");
    }
    let tree_bytes = cas
        .get_blob(&tree_entry.blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("worker checkpoint portable-state attachment is absent"))?;
    if lillux::sha256_hex(&tree_bytes) != tree_entry.blob_hash
        || u64::try_from(tree_bytes.len())? != tree_entry.size_bytes
    {
        anyhow::bail!("worker checkpoint portable-state attachment digest changed");
    }
    let tree = ryeos_state::objects::PortableStateTree::from_canonical_bytes(
        &tree_bytes,
        &restore.portable_state.selector_contract,
        &restore.upstream_session_id,
    )?;
    Ok(LoadedWorkerCheckpoint {
        manifest_hash,
        restore,
        tree,
    })
}

fn load_worker_checkpoint_predecessor_tree(
    state: &AppState,
    restore: &ryeos_state::objects::WorkerSessionRestore,
) -> Result<Option<ryeos_state::objects::PortableStateTree>> {
    let (Some(manifest_hash), Some(tree_hash)) = (
        restore
            .portable_state
            .expected_predecessor_manifest_hash
            .as_deref(),
        restore
            .portable_state
            .expected_predecessor_tree_hash
            .as_deref(),
    ) else {
        return Ok(None);
    };
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let manifest_value = cas
        .get_object(manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("predecessor checkpoint manifest is absent from CAS"))?;
    if lillux::sha256_hex(lillux::canonical_json(&manifest_value)?.as_bytes()) != manifest_hash {
        anyhow::bail!("predecessor checkpoint manifest content hash changed");
    }
    let manifest = ryeos_state::objects::StateManifest::from_current_value(manifest_value)?;
    if manifest.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
        || manifest.publisher_chain_root_id != restore.source_position.chain_root_id
    {
        anyhow::bail!("predecessor checkpoint manifest belongs to another contract or chain");
    }
    let attachment = manifest
        .objects
        .iter()
        .find(|object| object.name == restore.portable_state.attachment_name)
        .ok_or_else(|| {
            anyhow::anyhow!("predecessor checkpoint has no portable-state attachment")
        })?;
    if attachment.media_type != ryeos_state::objects::PORTABLE_STATE_TREE_MEDIA_TYPE
        || attachment.blob_hash != tree_hash
    {
        anyhow::bail!("predecessor portable-state attachment identity changed");
    }
    let bytes = cas
        .get_blob(tree_hash)?
        .ok_or_else(|| anyhow::anyhow!("predecessor portable-state tree is absent from CAS"))?;
    if lillux::sha256_hex(&bytes) != tree_hash
        || u64::try_from(bytes.len())? != attachment.size_bytes
    {
        anyhow::bail!("predecessor portable-state tree digest changed");
    }
    Ok(Some(
        ryeos_state::objects::PortableStateTree::from_canonical_bytes(
            &bytes,
            &restore.portable_state.selector_contract,
            &restore.upstream_session_id,
        )?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRequest {
    chain_root_id: String,
}

async fn checkpoint(
    req: CheckpointRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let _disposition = disposition_operation_lock(&placement_thread_id)
        .lock_owned()
        .await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "worker placement changed while checkpoint was reserved".into(),
        ));
    }
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &session.placement_thread_id,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let _profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &session.credential_profile_id,
    )
    .await
    .map_err(internal)?;

    let session = state
        .state_store
        .dedicated_session(&placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker session disappeared during checkpoint"))?;
    if session.state != "frozen"
        || session.current_turn_id.is_some()
        || session.send_boundary != "settled"
    {
        return Err(HandlerError::BadRequest(
            "checkpoint requires a frozen session with no unsettled contact".into(),
        ));
    }
    let worker_instance_id = session
        .worker_instance_id
        .as_deref()
        .ok_or_else(|| internal("frozen session has no retained worker identity"))?;
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| internal("frozen session has no retained worker epoch"))?;
    let worker = state
        .state_store
        .worker_process(worker_instance_id)
        .map_err(internal)?
        .ok_or_else(|| internal("frozen session worker record disappeared"))?;
    if worker.placement_thread_id != placement_thread_id
        || worker.boot_epoch != worker_boot_epoch
        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
        || worker.cleanup_state != "reaped"
    {
        return Err(HandlerError::BadRequest(
            "checkpoint requires proof that the exact worker process was reaped".into(),
        ));
    }
    let profile = state
        .state_store
        .credential_profile(&session.credential_profile_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker credential profile disappeared"))?;
    if profile.owner_principal != session.owner_principal
        || profile.state != "active"
        || profile.credential_generation != session.credential_generation
        || profile.lock_owner.is_some()
    {
        return Err(HandlerError::BadRequest(
            "checkpoint credential profile is not active, unlocked, and generation-exact".into(),
        ));
    }
    let sanitized_account = profile
        .sanitized_account
        .as_ref()
        .ok_or_else(|| internal("active credential profile has no sanitized account"))?;
    let upstream_session_id = session.remote_thread_id.as_deref().ok_or_else(|| {
        HandlerError::BadRequest("worker has no resumable upstream session".into())
    })?;

    let inner_capsule = load_persistent_session_capsule(&state, &session.admitted_capsule_hash)
        .map_err(internal)?;
    let structured = inner_capsule
        .structured_session_profile
        .as_ref()
        .ok_or_else(|| internal("worker capsule has no structured-session contract"))?;
    let portable_contract = structured
        .portable_state_contract()
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("worker profile is not portable".into()))?;
    let credential_contract = structured
        .credential_subject_contract()
        .map_err(internal)?
        .ok_or_else(|| {
            HandlerError::BadRequest("worker profile has no credential subject".into())
        })?;
    let credential_subject_digest = credential_contract
        .derive_subject_digest(sanitized_account)
        .map_err(internal)?;
    let credential_subject_contract_digest =
        credential_contract.contract_digest().map_err(internal)?;

    let settlement_digest = state
        .state_store
        .dedicated_session_checkpoint_settlement_digest(&placement_thread_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if let Some(accounting) = &state.accounting {
        if accounting
            .nonterminal_reservations()
            .map_err(internal)?
            .iter()
            .any(|(_, thread_id, _, _)| thread_id == &placement_thread_id)
        {
            return Err(HandlerError::BadRequest(
                "checkpoint has an unsettled provider attempt".into(),
            ));
        }
        if accounting
            .unpublished_outbox_for_thread(&placement_thread_id)
            .map_err(internal)?
            != 0
        {
            return Err(HandlerError::BadRequest(
                "checkpoint provider-attempt testimony is not durably published".into(),
            ));
        }
    }

    let (source_snapshot, source_event, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("authoritative worker placement disappeared"))?;
    let source_event = source_event.ok_or_else(|| internal("worker placement has no event"))?;
    let source_event_hash = source_event
        .event_hash
        .clone()
        .ok_or_else(|| internal("authoritative worker event has no CAS hash"))?;
    let launch_capsule_hash = source_snapshot
        .admitted_launch_capsule_hash
        .clone()
        .ok_or_else(|| internal("worker placement has no launch capsule"))?;
    let outer_capsule = state
        .state_store
        .admitted_launch_capsule(&placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker launch capsule disappeared"))?;
    if outer_capsule.content_hash().map_err(internal)? != launch_capsule_hash {
        return Err(internal("worker launch capsule content hash changed"));
    }
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &outer_capsule.execution_closure
    else {
        return Err(internal("worker placement is not a managed runtime"));
    };
    let prepared: ryeos_executor::execution::launch_preparation::PreparedRuntimeLaunch =
        serde_json::from_value(prepared_runtime_launch.clone()).map_err(internal)?;
    let mut dependencies = BTreeMap::new();
    for (name, capsule_hash) in prepared.admitted_sessions {
        let capsule = load_persistent_session_capsule(&state, &capsule_hash).map_err(internal)?;
        dependencies.insert(
            name,
            ryeos_state::objects::WorkerSessionDependencyRestore {
                exact_program_hash: capsule.exact_program_hash,
                source_capsule_hash: capsule_hash,
            },
        );
    }
    if !dependencies
        .values()
        .any(|dependency| dependency.source_capsule_hash == session.admitted_capsule_hash)
    {
        return Err(internal(
            "worker session capsule is absent from the outer admitted dependency set",
        ));
    }

    let tree = ryeos_app::private_artifact_home::capture_portable_state(
        &state.config.runtime_state_dir(),
        &profile.home_id,
        &portable_contract,
        upstream_session_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let tree_bytes = tree.canonical_bytes().map_err(internal)?;
    let tree_hash = lillux::sha256_hex(&tree_bytes);
    let selector_contract_digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(&portable_contract).map_err(internal)?)
            .map_err(internal)?
            .as_bytes(),
    );
    let predecessor =
        load_worker_checkpoint_predecessor(&state, &session.chain_root_id).map_err(internal)?;
    let restore = ryeos_state::objects::WorkerSessionRestore {
        schema: ryeos_state::objects::WORKER_SESSION_RESTORE_SCHEMA,
        kind: ryeos_state::objects::WORKER_SESSION_RESTORE_KIND.to_owned(),
        contract: ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT.to_owned(),
        outer_exact_program_hash: outer_capsule.exact_program_hash.clone(),
        persistent_dependencies: dependencies,
        upstream_session_id: upstream_session_id.to_owned(),
        source_position: ryeos_state::objects::WorkerSessionCheckpointPosition {
            chain_root_id: session.chain_root_id.clone(),
            placement_thread_id: placement_thread_id.clone(),
            chain_seq: u64::try_from(source_event.chain_seq).map_err(internal)?,
            event_hash: source_event_hash.clone(),
        },
        source_project_authority: source_snapshot.project_authority.clone(),
        project_candidate_snapshot_hash: session.candidate_snapshot_hash.clone(),
        portable_state: ryeos_state::objects::WorkerSessionPortableStateRestore {
            selector_contract: portable_contract,
            selector_contract_digest,
            attachment_name: "portable_state_tree".to_owned(),
            incoming_tree_hash: tree_hash.clone(),
            expected_predecessor_manifest_hash: predecessor
                .as_ref()
                .map(|(manifest, _)| manifest.clone()),
            expected_predecessor_tree_hash: predecessor.as_ref().map(|(_, tree)| tree.clone()),
        },
        pending_contact_settlement_digest: settlement_digest,
        credential_subject_contract_digest,
        credential_subject_digest: credential_subject_digest.clone(),
        source_site_id: source_snapshot.current_site_id.clone(),
        source_launch_capsule_hash: launch_capsule_hash.clone(),
    };
    let restore_value = restore.to_value().map_err(internal)?;
    let restore_digest = format!(
        "sha256:{}",
        lillux::sha256_hex(
            lillux::canonical_json(&restore_value)
                .map_err(internal)?
                .as_bytes()
        )
    );
    let publication = state
        .state_store
        .publish_state_anchor(&ryeos_app::state_store::StateAnchorPublishParams {
            thread_id: placement_thread_id.clone(),
            subject: ryeos_state::objects::StateAnchorSubject::Execution {
                chain_root_id: session.chain_root_id.clone(),
                placement_thread_id: placement_thread_id.clone(),
                item_ref: source_snapshot.item_ref.clone(),
                exact_program_hash: outer_capsule.exact_program_hash,
                launch_capsule_hash,
                source_chain_seq: u64::try_from(source_event.chain_seq).map_err(internal)?,
                source_event_hash,
            },
            contract: ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT.to_owned(),
            restore: restore_value,
            restore_digest,
            objects: vec![ryeos_app::state_store::StateManifestInput {
                name: "portable_state_tree".to_owned(),
                media_type: ryeos_state::objects::PORTABLE_STATE_TREE_MEDIA_TYPE.to_owned(),
                digest: format!("sha256:{tree_hash}"),
                content_base64: base64::engine::general_purpose::STANDARD.encode(tree_bytes),
            }],
            anchor: ryeos_app::state_store::StateAnchorDraft {
                label: "worker_session_checkpoint".to_owned(),
                runtime: json!({
                    "kind":"worker_session",
                    "restore_contract":ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT,
                }),
                metadata: json!({
                    "portable_state_tree_hash":tree_hash,
                    "credential_subject_digest":credential_subject_digest,
                }),
            },
        })
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "manifest_ref":publication.manifest_ref,
        "state_digest":publication.state_digest,
        "state_anchor_event_hash":publication.event.event_hash,
        "portable_state_tree_hash":tree_hash,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    chain_root_id: String,
    manifest_ref: String,
}

async fn await_worker_resume_handoff(
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
            Ok(Err(failure)) => Err(HandlerError::Structured {
                code: failure.code,
                status: failure.status,
                body: failure.body,
            }),
            Err(_) => match task.await {
                Ok(Err(error)) => Err(internal(error)),
                Ok(Ok(_)) => Err(internal("worker successor completed without launch handoff")),
                Err(error) => Err(internal(error)),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => Err(internal(error)),
            Ok(Ok(_)) => Err(internal("worker successor completed without launch handoff")),
            Err(error) => Err(internal(error)),
        },
    }
}

async fn resume(
    req: ResumeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let source_thread_id = initial.placement_thread_id.clone();
    let _disposition = disposition_operation_lock(&source_thread_id)
        .lock_owned()
        .await;
    let source = owned_session(&state, &ctx, &req.chain_root_id)?;
    if source.placement_thread_id != source_thread_id {
        return Err(HandlerError::BadRequest(
            "worker placement changed while restore was reserved".into(),
        ));
    }
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &source_thread_id,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &source.credential_profile_id,
    )
    .await
    .map_err(internal)?;
    let source = state
        .state_store
        .dedicated_session(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker restore source disappeared"))?;
    if source.state != "frozen"
        || source.current_turn_id.is_some()
        || source.send_boundary != "settled"
    {
        return Err(HandlerError::BadRequest(
            "restore requires the exact frozen checkpoint source".into(),
        ));
    }
    let checkpoint = load_worker_checkpoint(&state, &source.chain_root_id, &req.manifest_ref)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if checkpoint.restore.project_candidate_snapshot_hash != source.candidate_snapshot_hash {
        return Err(HandlerError::BadRequest(
            "checkpoint project candidate differs from the frozen source".into(),
        ));
    }
    let (source_snapshot, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(&source.chain_root_id, &source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("authoritative restore source disappeared"))?;
    if source_snapshot.status != ryeos_state::objects::ThreadStatus::Running
        || source_snapshot.project_authority != checkpoint.restore.source_project_authority
        || source_snapshot.admitted_launch_capsule_hash.as_deref()
            != Some(checkpoint.restore.source_launch_capsule_hash.as_str())
    {
        return Err(HandlerError::BadRequest(
            "checkpoint source authority differs from the current placement".into(),
        ));
    }
    if let Some(candidate) = checkpoint
        .restore
        .project_candidate_snapshot_hash
        .as_deref()
    {
        state
            .state_store
            .verify_project_snapshot_closure(candidate)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    }
    let profile = state
        .state_store
        .credential_profile(&source.credential_profile_id)
        .map_err(internal)?
        .ok_or_else(|| internal("restore credential profile disappeared"))?;
    if profile.owner_principal != source.owner_principal
        || profile.state != "active"
        || profile.credential_generation != source.credential_generation
        || profile.lock_owner.is_some()
    {
        return Err(HandlerError::BadRequest(
            "restore credential profile is not active, unlocked, and generation-exact".into(),
        ));
    }
    let inner_capsule =
        load_persistent_session_capsule(&state, &source.admitted_capsule_hash).map_err(internal)?;
    let structured = inner_capsule
        .structured_session_profile
        .as_ref()
        .ok_or_else(|| internal("restore worker has no structured-session contract"))?;
    let portable_contract = structured
        .portable_state_contract()
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("worker profile is not portable".into()))?;
    if portable_contract != checkpoint.restore.portable_state.selector_contract {
        return Err(HandlerError::BadRequest(
            "portable-state selector contract changed since checkpoint".into(),
        ));
    }
    let credential_contract = structured
        .credential_subject_contract()
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("worker has no credential subject".into()))?;
    let sanitized_account = profile
        .sanitized_account
        .as_ref()
        .ok_or_else(|| internal("active credential profile has no sanitized account"))?;
    if credential_contract.contract_digest().map_err(internal)?
        != checkpoint.restore.credential_subject_contract_digest
        || credential_contract
            .derive_subject_digest(sanitized_account)
            .map_err(internal)?
            != checkpoint.restore.credential_subject_digest
    {
        return Err(HandlerError::BadRequest(
            "local credential does not represent the checkpoint workload account".into(),
        ));
    }
    let predecessor_tree = load_worker_checkpoint_predecessor_tree(&state, &checkpoint.restore)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let portable_install = ryeos_app::private_artifact_home::install_portable_state_conditionally(
        &state.config.runtime_state_dir(),
        &profile.home_id,
        &portable_contract,
        &checkpoint.restore.upstream_session_id,
        predecessor_tree.as_ref(),
        &checkpoint.tree,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let settlement = state
        .state_store
        .dedicated_session_checkpoint_settlement_digest(&source_thread_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if settlement != checkpoint.restore.pending_contact_settlement_digest {
        return Err(HandlerError::BadRequest(
            "worker contact frontier changed after checkpoint".into(),
        ));
    }

    let outer_capsule = state
        .state_store
        .admitted_launch_capsule(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("restore source launch capsule disappeared"))?;
    if outer_capsule.exact_program_hash != checkpoint.restore.outer_exact_program_hash {
        return Err(HandlerError::BadRequest(
            "worker exact program changed since checkpoint".into(),
        ));
    }
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &outer_capsule.execution_closure
    else {
        return Err(internal("restore source is not a managed runtime"));
    };
    let source_prepared: ryeos_executor::execution::launch_preparation::PreparedRuntimeLaunch =
        serde_json::from_value(prepared_runtime_launch.clone()).map_err(internal)?;
    if source_prepared.admitted_sessions.len() != checkpoint.restore.persistent_dependencies.len() {
        return Err(HandlerError::BadRequest(
            "worker dependency set changed since checkpoint".into(),
        ));
    }
    for (name, capsule_hash) in &source_prepared.admitted_sessions {
        let expected = checkpoint
            .restore
            .persistent_dependencies
            .get(name)
            .ok_or_else(|| HandlerError::BadRequest("worker dependency set changed".into()))?;
        let capsule = load_persistent_session_capsule(&state, capsule_hash).map_err(internal)?;
        if &expected.source_capsule_hash != capsule_hash
            || expected.exact_program_hash != capsule.exact_program_hash
        {
            return Err(HandlerError::BadRequest(
                "worker dependency identity changed since checkpoint".into(),
            ));
        }
    }

    let source_metadata = state
        .state_store
        .get_launch_metadata(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("restore source has no launch metadata"))?;
    let mut resume_context = source_metadata
        .resume_context
        .clone()
        .ok_or_else(|| internal("restore source has no resume context"))?;
    if let Some(candidate) = checkpoint
        .restore
        .project_candidate_snapshot_hash
        .as_deref()
    {
        resume_context.project_authority = resume_context
            .project_authority
            .transition_operational_generation(
                ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                    result_snapshot_hash: candidate,
                },
            )
            .map_err(internal)?;
        resume_context.original_snapshot_hash = Some(candidate.to_owned());
        resume_context.original_pushed_head_ref = None;
    }
    let successor_id = ryeos_app::thread_lifecycle::new_thread_id();
    let prepared =
        ryeos_executor::execution::launch::prepare_externally_restored_machine_successor_launch(
            &state,
            &successor_id,
            &resume_context,
            &source_thread_id,
        )
        .await
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let mut initial_events = prepared
        .initial_audit_events()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.worker_session.restore_operation.v1",
        "chain_root_id":source.chain_root_id,
        "source_thread_id":source_thread_id,
        "successor_thread_id":successor_id,
        "manifest_hash":checkpoint.manifest_hash,
    }))
    .map_err(internal)?;
    initial_events.push(ryeos_app::state_store::NewEventRecord {
        event_type: "worker_session.restored".to_owned(),
        storage_class: "indexed".to_owned(),
        payload: json!({
            "schema":1,
            "operation_id":operation_id,
            "chain_root_id":source.chain_root_id,
            "source_placement_thread_id":source_thread_id,
            "successor_placement_thread_id":successor_id,
            "manifest_hash":checkpoint.manifest_hash,
            "portable_state_tree_hash":checkpoint.restore.portable_state.incoming_tree_hash,
            "portable_state_install":match portable_install {
                ryeos_app::private_artifact_home::PortableStateInstallOutcome::AlreadyCurrent => "already_current",
                ryeos_app::private_artifact_home::PortableStateInstallOutcome::Advanced => "advanced",
            },
            "credential_subject_digest":checkpoint.restore.credential_subject_digest,
            "source_site_id":checkpoint.restore.source_site_id,
            "target_site_id":source_snapshot.current_site_id,
        }),
    });
    let continuation = state
        .threads
        .request_continuation_with_project_generation(
            &ryeos_app::thread_lifecycle::ThreadContinuationParams {
                thread_id: source_thread_id.clone(),
                reason: Some("worker_session_restore".to_owned()),
                completion: ryeos_runtime::TerminalCompletion {
                    status: ryeos_runtime::ThreadTerminalStatus::Continued,
                    outcome_code: Some("continued".to_owned()),
                    result: None,
                    error: None,
                    cost: None,
                    outputs: json!({}),
                    warnings: Vec::new(),
                },
            },
            &successor_id,
            &resume_context,
            prepared.launch_metadata(),
            checkpoint
                .restore
                .project_candidate_snapshot_hash
                .as_deref(),
            initial_events,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let prepared = prepared.with_persisted_birth_audit();
    drop(profile_operation);
    drop(root_operation);

    let task_state = (*state).clone();
    let task_id = successor_id.clone();
    let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
    let task = tokio::spawn(async move {
        ryeos_executor::execution::launch::launch_prepared_machine_successor_with_handoff(
            task_state, &task_id, prepared, &handoff,
        )
        .await
    });
    let ready_thread_id = await_worker_resume_handoff(task, ready).await?;
    if ready_thread_id != successor_id {
        return Err(internal(
            "worker restore handoff returned another successor",
        ));
    }
    Ok(json!({
        "chain_root_id":continuation.chain_root_id,
        "source_placement_thread_id":continuation.source_thread_id,
        "placement_thread_id":continuation.successor_thread_id,
        "manifest_ref":req.manifest_ref,
        "delivery":"launched",
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffPreflightRequest {
    chain_root_id: String,
    remote: String,
    target_credential_profile_id: String,
}

async fn handoff_preflight(
    req: HandoffPreflightRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let source_thread_id = initial.placement_thread_id.clone();
    let _disposition = disposition_operation_lock(&source_thread_id)
        .lock_owned()
        .await;
    let source = owned_session(&state, &ctx, &req.chain_root_id)?;
    if source.placement_thread_id != source_thread_id
        || matches!(
            source.state.as_str(),
            "terminal" | "published" | "discarded"
        )
    {
        return Err(HandlerError::BadRequest(
            "worker placement changed or terminalized during handoff preflight".into(),
        ));
    }
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &source_thread_id,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let _profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &source.credential_profile_id,
    )
    .await
    .map_err(internal)?;
    let source = state
        .state_store
        .dedicated_session(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker preflight source disappeared"))?;
    if source.state != "frozen"
        || source.current_turn_id.is_some()
        || source.send_boundary != "settled"
    {
        return Err(HandlerError::BadRequest(
            "handoff preflight requires an exact frozen, settled checkpoint source".into(),
        ));
    }
    let worker_instance_id = source
        .worker_instance_id
        .as_deref()
        .ok_or_else(|| internal("handoff preflight source has no retained worker identity"))?;
    let worker_boot_epoch = source
        .worker_boot_epoch
        .ok_or_else(|| internal("handoff preflight source has no retained worker epoch"))?;
    let worker = state
        .state_store
        .worker_process(worker_instance_id)
        .map_err(internal)?
        .ok_or_else(|| internal("handoff preflight source worker projection disappeared"))?;
    if worker.placement_thread_id != source_thread_id
        || worker.boot_epoch != worker_boot_epoch
        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
        || worker.cleanup_state != "reaped"
    {
        return Err(HandlerError::BadRequest(
            "handoff preflight requires proof that the exact source worker was reaped".into(),
        ));
    }
    let checkpoint_manifest_hash =
        load_worker_checkpoint_predecessor(&state, &source.chain_root_id)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?
            .map(|(manifest_hash, _)| manifest_hash)
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "handoff preflight requires an authoritative source checkpoint".into(),
                )
            })?;
    let checkpoint = load_worker_checkpoint(
        &state,
        &source.chain_root_id,
        &format!("cas:{checkpoint_manifest_hash}"),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if checkpoint.restore.source_position.placement_thread_id != source_thread_id
        || checkpoint.restore.project_candidate_snapshot_hash != source.candidate_snapshot_hash
    {
        return Err(HandlerError::BadRequest(
            "handoff preflight checkpoint differs from the frozen source placement".into(),
        ));
    }
    let upstream_session_id = source.remote_thread_id.clone().ok_or_else(|| {
        HandlerError::BadRequest("worker has no resumable upstream session".into())
    })?;
    let profile = state
        .state_store
        .credential_profile(&source.credential_profile_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker credential profile disappeared"))?;
    if profile.owner_principal != source.owner_principal
        || profile.state != "active"
        || profile.credential_generation != source.credential_generation
    {
        return Err(HandlerError::BadRequest(
            "source credential profile is not active and generation-exact".into(),
        ));
    }
    let sanitized_account = profile
        .sanitized_account
        .as_ref()
        .ok_or_else(|| internal("active source credential profile has no sanitized account"))?;
    let inner_capsule =
        load_persistent_session_capsule(&state, &source.admitted_capsule_hash).map_err(internal)?;
    let structured = inner_capsule
        .structured_session_profile
        .as_ref()
        .ok_or_else(|| internal("worker capsule has no structured-session contract"))?;
    structured
        .portable_state_contract()
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("worker profile is not portable".into()))?;
    let credential_contract = structured
        .credential_subject_contract()
        .map_err(internal)?
        .ok_or_else(|| {
            HandlerError::BadRequest("worker profile has no credential subject".into())
        })?;
    let credential_subject_contract_digest =
        credential_contract.contract_digest().map_err(internal)?;
    let credential_subject_digest = credential_contract
        .derive_subject_digest(sanitized_account)
        .map_err(internal)?;

    let (source_snapshot, source_event, source_chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(&source.chain_root_id, &source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("authoritative preflight source disappeared"))?;
    let source_event_hash = source_event
        .and_then(|event| event.event_hash)
        .ok_or_else(|| internal("preflight source has no authoritative last event"))?;
    let source_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &source.chain_root_id))
        .map_err(internal)?
        .ok_or_else(|| internal("preflight source has no signed chain head"))?;
    if source_head.signer != state.identity.fingerprint() {
        return Err(internal("preflight source head is not owned by this node"));
    }
    if source_head.target_hash != source_chain_head_hash {
        return Err(HandlerError::BadRequest(
            "worker placement changed during handoff preflight".into(),
        ));
    }
    let source_launch_capsule_hash = source_snapshot
        .admitted_launch_capsule_hash
        .clone()
        .ok_or_else(|| internal("preflight source has no launch capsule"))?;
    let source_metadata = state
        .state_store
        .get_launch_metadata(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("preflight source has no launch metadata"))?;
    source_metadata.validate().map_err(internal)?;
    if source_metadata.cancellation_mode.is_some() {
        return Err(HandlerError::BadRequest(
            "portable worker handoff v1 has no rooted cancellation-policy transfer".into(),
        ));
    }
    let source_capsule = source_metadata
        .admitted_launch_capsule()
        .map_err(internal)?
        .ok_or_else(|| internal("preflight source metadata has no launch capsule"))?;
    if source_capsule.content_hash().map_err(internal)? != source_launch_capsule_hash {
        return Err(internal("preflight source launch capsule changed"));
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &source_capsule,
        )
        .map_err(internal)?;
    source_capsule
        .validate_durable_handoff_eligibility()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let source_credential_profile_id = sealed
        .validate_worker_handoff_source(
            &source.owner_principal,
            state.threads.site_id(),
            &source_snapshot.origin_site_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if source_credential_profile_id != source.credential_profile_id {
        return Err(HandlerError::BadRequest(
            "source launch capsule contradicts its hosted session authority".into(),
        ));
    }
    let source_project_path = source_capsule
        .project_authority
        .project_root_projection()
        .map(PathBuf::from)
        .ok_or_else(|| {
            HandlerError::BadRequest("preflight source has no stable project endpoint".into())
        })?;
    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(&source_project_path),
    )
    .map_err(internal)?;
    let loaded_remote = crate::remote::config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let binding =
        crate::remote::config::resolve_loaded_project_binding(&loaded_remote, &source_project_path)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if binding.sync_scope != crate::remote::config::ProjectSyncScope::FullProject {
        return Err(HandlerError::BadRequest(
            "worker handoff preflight requires a configured full_project route".into(),
        ));
    }
    let target_site_id = loaded_remote.config.site_id.clone();
    if target_site_id == state.threads.site_id() {
        return Err(HandlerError::BadRequest(
            "handoff preflight target is the current site".into(),
        ));
    }
    let project_route_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.worker_project_route.v1",
        "remote":loaded_remote.config.name,
        "source_site_id":state.threads.site_id(),
        "target_site_id":target_site_id,
        "source_project_path":binding.local_project_path,
        "target_project_path":binding.remote_project_path,
        "sync_scope":"full_project",
    }))
    .map_err(internal)?;
    let follow_delivery_reservation_attestation_hash = state
        .state_store
        .prepare_remote_follow_delivery_reservation(
            &source.chain_root_id,
            &source.owner_principal,
            state.threads.site_id(),
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?
        .map(|(hash, _attestation)| hash);
    let preflight_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.worker_session_handoff_preflight_operation.v2",
        "owner_principal":source.owner_principal,
        "chain_root_id":source.chain_root_id,
        "origin_site_id":source_snapshot.origin_site_id,
        "source_site_id":state.threads.site_id(),
        "target_site_id":target_site_id,
        "source_placement_thread_id":source_thread_id,
        "source_chain_head_hash":source_head.target_hash,
        "source_last_event_hash":source_event_hash,
        "source_launch_capsule_hash":source_launch_capsule_hash,
        "project_route_digest":project_route_digest,
        "target_project_path":binding.remote_project_path,
        "target_credential_profile_id":req.target_credential_profile_id,
        "upstream_session_id":upstream_session_id,
        "credential_subject_contract_digest":credential_subject_contract_digest,
        "credential_subject_digest":credential_subject_digest,
        "follow_delivery_reservation_attestation_hash":follow_delivery_reservation_attestation_hash,
    }))
    .map_err(internal)?;
    let successor_placement_thread_id = format!("T-handoff-{}", &preflight_id[..32]);
    let request = ryeos_app::worker_handoff::WorkerPlacementPreflightRequest {
        preflight_id: preflight_id.clone(),
        owner_principal: source.owner_principal.clone(),
        chain_root_id: source.chain_root_id.clone(),
        origin_site_id: source_snapshot.origin_site_id.clone(),
        source_site_id: state.threads.site_id().to_owned(),
        target_site_id: target_site_id.clone(),
        source_placement_thread_id: source_thread_id.clone(),
        successor_placement_thread_id: successor_placement_thread_id.clone(),
        source_chain_head_hash: source_head.target_hash.clone(),
        source_last_event_hash: source_event_hash,
        source_launch_capsule_hash,
        target_project_path: binding.remote_project_path.clone(),
        project_route_digest,
        target_credential_profile_id: req.target_credential_profile_id.clone(),
        upstream_session_id,
        credential_subject_contract_digest,
        credential_subject_digest,
        follow_delivery_reservation_attestation_hash,
    };
    request.validate().map_err(internal)?;
    let target_client =
        crate::remote::client::RemoteClient::from_remote_cfg(&state, &loaded_remote.config);
    let target_value = target_client
        .execute_service_result_with_total_timeout(
            ryeos_app::worker_handoff::WORKER_PLACEMENT_PREFLIGHT_SERVICE,
            &BTreeMap::new(),
            None,
            &serde_json::to_value(&request).map_err(internal)?,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "target placement preflight")
        })?;
    let response: ryeos_app::worker_handoff::WorkerPlacementPreflightResponse =
        serde_json::from_value(target_value)
            .map_err(|error| internal(format!("decode target preflight response: {error}")))?;
    let target_key = loaded_remote
        .config
        .pinned_signing_key()
        .map_err(internal)?;
    response
        .validate_against(&request, &target_key)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let target_node_client =
        crate::remote::client::RemoteClient::from_remote_cfg(&state, &loaded_remote.config);
    let target_closure_options =
        crate::remote::client::NodeAdmittedObjectsClosureRequestOptions::for_node(
            &state,
            crate::remote::client::ObjectsClosureRequestOptions {
                allow_incomplete: false,
                allow_untransported_large_objects: true,
                ..Default::default()
            },
        )
        .map_err(internal)?;
    let reservation_entry =
        if let Some(hash) = &request.follow_delivery_reservation_attestation_hash {
            let authority = state
                .state_store
                .pinned_state_authority()
                .map_err(internal)?;
            let guard = authority.acquire_shared_guard().map_err(internal)?;
            authority.ensure_guard(&guard).map_err(internal)?;
            let value = authority
                .cas_store()
                .map_err(internal)?
                .get_object(hash)
                .map_err(internal)?
                .ok_or_else(|| internal("local follow delivery reservation disappeared"))?;
            let bytes = lillux::canonical_json(&value)
                .map_err(internal)?
                .into_bytes();
            if lillux::sha256_hex(&bytes) != *hash {
                return Err(internal("local follow delivery reservation changed digest"));
            }
            Some(ryeos_state::sync::SyncEntry {
                hash: hash.clone(),
                is_blob: false,
                data: bytes,
            })
        } else {
            None
        };
    let target_fetch_options = match reservation_entry.as_ref() {
        Some(entry) => target_closure_options
            .reserving_supplemental_entry(entry)
            .map_err(internal)?,
        None => target_closure_options.clone(),
    };
    let target_closure = target_node_client
        .objects_closure_get_with_total_timeout(
            &[response.preflight_attestation_hash.clone()],
            target_fetch_options,
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "fetch target preflight receipt")
        })?;
    crate::remote::import::require_local_large_object_dependencies(
        &state,
        &target_closure.closure.large_object_hashes,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let mut payload = crate::remote::import::closure_response_to_export_payload(
        &format!("worker-preflight:{preflight_id}"),
        &response.preflight_attestation_hash,
        &target_closure.entries,
    )
    .map_err(internal)?;
    if let Some(entry) = reservation_entry {
        crate::remote::import::append_admitted_supplemental_entry(
            &mut payload,
            entry,
            target_closure_options.limits(),
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    }
    let operation = ryeos_app::worker_handoff::WorkerPlacementPreflightJobOperation::from_request(
        ryeos_app::worker_handoff::WorkerHandoffJobRole::Source,
        req.remote.clone(),
        &request,
    )
    .map_err(internal)?;
    let job_id = source_preflight_job_id(&preflight_id);
    let (_, existing_job) =
        state
            .state_store
            .stage_sync_payload_and_create_job(
                &payload,
                &ryeos_state::sync::ImportAttribution {
                    source_principal: Some(loaded_remote.config.principal_id.clone()),
                    source_peer: Some(req.remote.clone()),
                    job_id: Some(job_id.clone()),
                },
                &ryeos_state::NewSyncJob {
                    job_id: job_id.clone(),
                    operation_type:
                        ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION
                            .to_owned(),
                    operation: operation.to_value().map_err(internal)?,
                    peer: Some(req.remote),
                    roots: {
                        let mut roots = vec![response.preflight_attestation_hash.clone()];
                        if let Some(hash) = &request.follow_delivery_reservation_attestation_hash {
                            roots.push(hash.clone());
                        }
                        roots
                    },
                    heads: vec![request.source_chain_head_hash.clone()],
                    max_attempts: 4,
                },
            )
            .map_err(internal)?;
    if existing_job.state == ryeos_state::SyncJobState::Completed {
        let retained: ryeos_app::worker_handoff::WorkerPlacementPreflightResponse =
            serde_json::from_value(
                existing_job
                    .result
                    .ok_or_else(|| internal("completed source preflight has no result"))?,
            )
            .map_err(internal)?;
        if retained != response {
            return Err(internal(
                "completed source preflight differs from the target's idempotent receipt",
            ));
        }
    } else {
        let attempt_id = begin_worker_handoff_attempt(
            &state,
            &job_id,
            "preflight_complete",
            "source-handoff-preflight",
        )
        .map_err(internal)?;
        settle_worker_handoff_attempt(
            &state,
            &job_id,
            &attempt_id,
            ryeos_state::SyncJobAttemptState::Completed,
            ryeos_state::SyncJobState::Completed,
            "preflight_complete",
            None,
            Some(serde_json::to_value(&response).map_err(internal)?),
        )
        .map_err(internal)?;
    }
    Ok(json!({
        "preflight_id":preflight_id,
        "preflight_attestation_hash":response.preflight_attestation_hash,
        "chain_root_id":source.chain_root_id,
        "source_placement_thread_id":source_thread_id,
        "successor_placement_thread_id":successor_placement_thread_id,
        "target_site_id":target_site_id,
        "target_project_head_hash":response.evidence.target_project_head_hash,
        "target_credential_generation":response.evidence.target_credential_generation,
        "status":"eligible",
    }))
}

fn source_preflight_job_id(preflight_id: &str) -> String {
    format!("worker-handoff-preflight-source:{preflight_id}")
}

#[cfg(any(test, feature = "handoff-test-support"))]
fn reach_source_handoff_phase_cut(
    state: &AppState,
    boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
) -> anyhow::Result<()> {
    if let Some(gate) = state
        .extensions
        .get::<ryeos_app::worker_handoff::test_support::HandoffPhaseGate>()
    {
        gate.reach(boundary, operation)?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequest {
    chain_root_id: String,
    manifest_ref: String,
    remote: String,
    target_credential_profile_id: String,
    preflight_id: String,
}

async fn handoff(
    req: HandoffRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    #[cfg(any(test, feature = "handoff-test-support"))]
    let qualification_started = lillux::time::MonotonicTimer::start();
    if let Some(recovered) = resume_committed_handoff(&req, &ctx, &state).await? {
        return Ok(recovered);
    }
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let source_thread_id = initial.placement_thread_id.clone();
    let _disposition = disposition_operation_lock(&source_thread_id)
        .lock_owned()
        .await;
    let source = owned_session(&state, &ctx, &req.chain_root_id)?;
    if source.placement_thread_id != source_thread_id {
        return Err(HandlerError::BadRequest(
            "worker placement changed while handoff was reserved".into(),
        ));
    }
    let mut root_terminalization =
        ryeos_app::hosted_operation::begin_hosted_root_terminalization_async(
            &state.state_store,
            &source_thread_id,
        )
        .await
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let _profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &source.credential_profile_id,
    )
    .await
    .map_err(internal)?;
    let source = state
        .state_store
        .dedicated_session(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("worker handoff source disappeared"))?;
    if source.state != "frozen"
        || source.current_turn_id.is_some()
        || source.send_boundary != "settled"
    {
        return Err(HandlerError::BadRequest(
            "handoff requires an exact frozen, settled checkpoint source".into(),
        ));
    }
    let worker_instance_id = source
        .worker_instance_id
        .as_deref()
        .ok_or_else(|| internal("handoff source has no retained worker identity"))?;
    let worker = state
        .state_store
        .worker_process(worker_instance_id)
        .map_err(internal)?
        .ok_or_else(|| internal("handoff source worker projection disappeared"))?;
    if !exact_reaped_source_worker_authority(
        &source_thread_id,
        source.worker_boot_epoch,
        &worker.placement_thread_id,
        worker.boot_epoch,
        worker.state,
        &worker.cleanup_state,
    ) {
        return Err(HandlerError::BadRequest(
            "handoff requires proof that the exact source worker was reaped".into(),
        ));
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    let checkpoint_started = lillux::time::MonotonicTimer::start();
    let checkpoint = load_worker_checkpoint(&state, &source.chain_root_id, &req.manifest_ref)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let checkpoint_load_ms = checkpoint_started.elapsed_millis();
    if checkpoint.restore.project_candidate_snapshot_hash != source.candidate_snapshot_hash {
        return Err(HandlerError::BadRequest(
            "handoff checkpoint candidate differs from the frozen source".into(),
        ));
    }
    let (source_snapshot, source_event, source_chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(&source.chain_root_id, &source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("authoritative handoff source disappeared"))?;
    let source_event_hash = source_event
        .and_then(|event| event.event_hash)
        .ok_or_else(|| internal("handoff source has no authoritative last event"))?;
    let launch_capsule_hash = source_snapshot
        .admitted_launch_capsule_hash
        .as_deref()
        .ok_or_else(|| internal("handoff source has no launch capsule"))?
        .to_owned();
    let source_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &source.chain_root_id))
        .map_err(internal)?
        .ok_or_else(|| internal("handoff source has no signed chain head"))?;
    if source_head.signer != state.identity.fingerprint() {
        return Err(internal("handoff source head is not owned by this node"));
    }
    if source_head.target_hash != source_chain_head_hash {
        return Err(HandlerError::BadRequest(
            "worker placement changed during handoff".into(),
        ));
    }
    let source_metadata = state
        .state_store
        .get_launch_metadata(&source_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("handoff source has no launch metadata"))?;
    source_metadata.validate().map_err(internal)?;
    if source_metadata.cancellation_mode.is_some() {
        return Err(HandlerError::BadRequest(
            "portable worker handoff v1 has no rooted cancellation-policy transfer".into(),
        ));
    }
    let source_capsule = source_metadata
        .admitted_launch_capsule()
        .map_err(internal)?
        .ok_or_else(|| internal("handoff source metadata has no launch capsule"))?;
    if source_capsule.content_hash().map_err(internal)? != launch_capsule_hash {
        return Err(internal("handoff source launch capsule changed"));
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &source_capsule,
        )
        .map_err(internal)?;
    source_capsule
        .validate_durable_handoff_eligibility()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let source_credential_profile_id = sealed
        .validate_worker_handoff_source(
            &source.owner_principal,
            state.threads.site_id(),
            &source_snapshot.origin_site_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if source_credential_profile_id != source.credential_profile_id {
        return Err(HandlerError::BadRequest(
            "source launch capsule contradicts its hosted session authority".into(),
        ));
    }
    let source_project_path = source_capsule
        .project_authority
        .project_root_projection()
        .map(PathBuf::from)
        .ok_or_else(|| {
            HandlerError::BadRequest("handoff source has no stable project endpoint".into())
        })?;
    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(&source_project_path),
    )
    .map_err(internal)?;
    let loaded_remote = crate::remote::config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let binding =
        crate::remote::config::resolve_loaded_project_binding(&loaded_remote, &source_project_path)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if binding.sync_scope != crate::remote::config::ProjectSyncScope::FullProject {
        return Err(HandlerError::BadRequest(
            "worker handoff requires a configured full_project route".into(),
        ));
    }
    let target_site_id = loaded_remote.config.site_id.clone();
    if target_site_id == state.threads.site_id() {
        return Err(HandlerError::BadRequest(
            "cross-site handoff target is the current site".into(),
        ));
    }
    let route_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.worker_project_route.v1",
        "remote":loaded_remote.config.name,
        "source_site_id":state.threads.site_id(),
        "target_site_id":target_site_id,
        "source_project_path":binding.local_project_path,
        "target_project_path":binding.remote_project_path,
        "sync_scope":"full_project",
    }))
    .map_err(internal)?;
    let preflight_job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&source_preflight_job_id(&req.preflight_id)))
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("handoff preflight does not exist".into()))?;
    if preflight_job.state != ryeos_state::SyncJobState::Completed {
        return Err(HandlerError::BadRequest(
            "handoff preflight has not completed".into(),
        ));
    }
    let preflight_operation =
        ryeos_app::worker_handoff::WorkerPlacementPreflightJobOperation::from_value(
            preflight_job.operation,
        )
        .map_err(internal)?;
    let preflight_response: ryeos_app::worker_handoff::WorkerPlacementPreflightResponse =
        serde_json::from_value(
            preflight_job
                .result
                .ok_or_else(|| internal("completed handoff preflight has no result"))?,
        )
        .map_err(internal)?;
    let target_key = loaded_remote
        .config
        .pinned_signing_key()
        .map_err(internal)?;
    preflight_response
        .preflight_attestation
        .verify_with_key(&target_key)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let preflight_evidence =
        ryeos_app::worker_handoff::WorkerPlacementPreflightEvidence::from_attestation(
            &preflight_response.preflight_attestation,
        )
        .map_err(internal)?;
    if preflight_operation.role != ryeos_app::worker_handoff::WorkerHandoffJobRole::Source
        || preflight_operation.preflight_id != req.preflight_id
        || preflight_operation.owner_principal != source.owner_principal
        || preflight_operation.chain_root_id != source.chain_root_id
        || preflight_operation.source_site_id != state.threads.site_id()
        || preflight_operation.target_site_id != target_site_id
        || preflight_operation.source_placement_thread_id != source_thread_id
        || preflight_operation.source_chain_head_hash != source_head.target_hash
        || preflight_operation.source_last_event_hash != source_event_hash
        || preflight_operation.target_project_path != binding.remote_project_path
        || preflight_operation.project_route_digest != route_digest
        || preflight_operation.target_credential_profile_id != req.target_credential_profile_id
        || preflight_operation.follow_delivery_reservation_attestation_hash
            != preflight_evidence.follow_delivery_reservation_attestation_hash
        || preflight_operation.peer_remote_name != req.remote
        || preflight_response.preflight_attestation_hash
            != ryeos_state::objects::canonical_value_digest(
                &preflight_response.preflight_attestation.to_value(),
            )
            .map_err(internal)?
        || preflight_evidence.preflight_id != req.preflight_id
        || preflight_evidence.source_chain_head_hash != source_head.target_hash
        || preflight_evidence.source_last_event_hash != source_event_hash
        || preflight_evidence.source_launch_capsule_hash != launch_capsule_hash
        || preflight_evidence.target_project_path != binding.remote_project_path
        || preflight_evidence.project_route_digest != route_digest
        || preflight_evidence.target_credential_profile_id != req.target_credential_profile_id
    {
        return Err(HandlerError::BadRequest(
            "handoff request differs from its completed target preflight".into(),
        ));
    }
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(internal)?;
    let guard = authority.acquire_shared_guard().map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let closure_verification_started = lillux::time::MonotonicTimer::start();
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store().map_err(internal)?,
        &source.chain_root_id,
        &source_head.target_hash,
        &preflight_evidence.source_chain_head_hash,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    drop(guard);
    #[cfg(any(test, feature = "handoff-test-support"))]
    let closure_verification_ms = closure_verification_started.elapsed_millis();
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.worker_session_handoff_operation.v1",
        "preflight_id":req.preflight_id,
        "preflight_attestation_hash":preflight_response.preflight_attestation_hash,
        "owner_principal":source.owner_principal,
        "chain_root_id":source.chain_root_id,
        "source_site_id":state.threads.site_id(),
        "target_site_id":target_site_id,
        "source_placement_thread_id":source_thread_id,
        "source_chain_head_hash":source_head.target_hash,
        "source_last_event_hash":source_event_hash,
        "checkpoint_manifest_hash":checkpoint.manifest_hash,
        "project_route_digest":route_digest,
        "target_credential_profile_id":req.target_credential_profile_id,
        "follow_delivery_reservation_attestation_hash":preflight_evidence.follow_delivery_reservation_attestation_hash.clone(),
    }))
    .map_err(internal)?;
    let successor_thread_id = preflight_evidence.successor_placement_thread_id.clone();
    let project_candidate_snapshot_hash = checkpoint
        .restore
        .project_candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| {
            HandlerError::BadRequest("remote handoff checkpoint has no project candidate".into())
        })?;
    let source_accounting_frontier = match (
        state.accounting.as_ref(),
        source_metadata.accounting_scope.as_ref(),
    ) {
        (Some(accounting), Some(scope)) => Some(
            accounting
                .handoff_frontier(
                    &operation_id,
                    &source_thread_id,
                    &source.chain_root_id,
                    scope,
                )
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        ),
        (None, Some(_)) => {
            return Err(internal(
                "accounted handoff source has no available accounting ledger",
            ));
        }
        (_, None) => None,
    };
    let prepared_transfer = ryeos_app::worker_handoff::prepare_placement_transfer_manifest(
        &operation_id,
        &source.owner_principal,
        &source.chain_root_id,
        &source_snapshot.origin_site_id,
        state.threads.site_id(),
        &target_site_id,
        &source_thread_id,
        &successor_thread_id,
        &source_head.target_hash,
        &source_event_hash,
        &checkpoint.manifest_hash,
        project_candidate_snapshot_hash,
        &launch_capsule_hash,
    )
    .map_err(internal)?;
    let transfer_manifest_hash = prepared_transfer.object_hash().map_err(internal)?;
    let operation = ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation::new(
        ryeos_app::worker_handoff::WorkerHandoffJobRole::Source,
        operation_id.clone(),
        req.preflight_id.clone(),
        preflight_response.preflight_attestation_hash.clone(),
        source.owner_principal.clone(),
        source.chain_root_id.clone(),
        source_snapshot.origin_site_id.clone(),
        state.threads.site_id().to_owned(),
        target_site_id.clone(),
        source_thread_id.clone(),
        successor_thread_id.clone(),
        source_head.target_hash.clone(),
        source_event_hash.clone(),
        checkpoint.manifest_hash.clone(),
        transfer_manifest_hash.clone(),
        req.remote.clone(),
        binding.local_project_path.display().to_string(),
        binding.remote_project_path.clone(),
        route_digest.clone(),
        req.target_credential_profile_id.clone(),
        preflight_evidence
            .follow_delivery_reservation_attestation_hash
            .clone(),
    )
    .map_err(internal)?;
    let job_id = format!("worker-handoff-source:{operation_id}");
    let exported_progress =
        ryeos_app::worker_handoff::WorkerSessionHandoffProgress::source_exported(
            operation_id.clone(),
        )
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforeExportPublication,
        &operation,
    )
    .map_err(internal)?;
    let retained_source_job = state
        .state_store
        .create_worker_handoff_source_job(
            &prepared_transfer,
            &ryeos_state::NewSyncJob {
                job_id: job_id.clone(),
                operation_type: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                    .to_owned(),
                operation: operation.to_value().map_err(internal)?,
                peer: Some(req.remote.clone()),
                roots: {
                    let mut roots = vec![
                        transfer_manifest_hash.clone(),
                        preflight_response.preflight_attestation_hash.clone(),
                    ];
                    if let Some(hash) =
                        &preflight_evidence.follow_delivery_reservation_attestation_hash
                    {
                        roots.push(hash.clone());
                    }
                    roots
                },
                heads: vec![source_head.target_hash.clone()],
                max_attempts: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            },
            &exported_progress,
        )
        .map_err(internal)?;
    match retained_source_job.state {
        ryeos_state::SyncJobState::Completed => {
            return match validate_source_handoff_terminal(&state, &retained_source_job, &operation)
                .map_err(internal)?
            {
                ValidatedSourceHandoffTerminal::Completed(adopted) => {
                    handoff_response(&operation, &adopted)
                }
                ValidatedSourceHandoffTerminal::TargetCompleted(completion) => {
                    target_completed_handoff_response(&operation, &completion)
                }
                _ => Err(internal(
                    "completed source handoff validated as a non-success terminal",
                )),
            };
        }
        ryeos_state::SyncJobState::Cancelled => {
            let ValidatedSourceHandoffTerminal::Cancelled(_receipt) =
                validate_source_handoff_terminal(&state, &retained_source_job, &operation)
                    .map_err(internal)?
            else {
                return Err(internal(
                    "cancelled source handoff validated as another terminal",
                ));
            };
            return Err(HandlerError::Conflict(
                "worker handoff was durably aborted".into(),
            ));
        }
        ryeos_state::SyncJobState::Failed => {
            let ValidatedSourceHandoffTerminal::Failed(failure) =
                validate_source_handoff_terminal(&state, &retained_source_job, &operation)
                    .map_err(internal)?
            else {
                return Err(internal(
                    "failed source handoff validated as another terminal",
                ));
            };
            return Err(target_terminal_failure_conflict(&failure));
        }
        ryeos_state::SyncJobState::Planned
        | ryeos_state::SyncJobState::Running
        | ryeos_state::SyncJobState::Retryable => {
            let retained_progress = retained_source_job
                .result
                .map(ryeos_app::worker_handoff::WorkerSessionHandoffProgress::from_value)
                .transpose()
                .map_err(internal)?
                .ok_or_else(|| internal("active source handoff has no durable progress"))?;
            if retained_progress.operation_id != operation.operation_id {
                return Err(internal(
                    "active source handoff progress belongs to another operation",
                ));
            }
            if retained_progress.phase
                == ryeos_app::worker_handoff::WorkerHandoffPhase::AbortAuthorized
            {
                return Err(HandlerError::Conflict(
                    "worker handoff abort authority is already durable".into(),
                ));
            }
        }
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceExportPublished,
        &operation,
    )
    .map_err(internal)?;

    let prepare_request = ryeos_app::worker_handoff::WorkerPlacementPrepareRequest {
        preflight_id: req.preflight_id.clone(),
        preflight_attestation_hash: preflight_response.preflight_attestation_hash.clone(),
        operation_id: operation_id.clone(),
        chain_root_id: source.chain_root_id.clone(),
        source_site_id: state.threads.site_id().to_owned(),
        target_site_id: target_site_id.clone(),
        source_chain_head_hash: source_head.target_hash.clone(),
        transfer_manifest_hash: transfer_manifest_hash.clone(),
        target_project_path: binding.remote_project_path.clone(),
        project_route_digest: route_digest,
        target_credential_profile_id: req.target_credential_profile_id.clone(),
        follow_delivery_reservation_attestation_hash: preflight_evidence
            .follow_delivery_reservation_attestation_hash
            .clone(),
        source_accounting_frontier: source_accounting_frontier.clone(),
    };
    let target_client =
        crate::remote::client::RemoteClient::from_remote_cfg(&state, &loaded_remote.config);
    let prepare_attempt =
        begin_worker_handoff_attempt(&state, &job_id, "target_prepare", "source-handoff")?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let target_prepare_started = lillux::time::MonotonicTimer::start();
    let prepared_result: Result<
        (
            ryeos_app::worker_handoff::WorkerPlacementPrepareResponse,
            Option<Value>,
        ),
        HandlerError,
    > = async {
        let target_value = target_client
            .execute_service_result_with_total_timeout(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_PREPARE_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&prepare_request).map_err(internal)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(error, "target placement preparation")
            })?;
        let (prepared, target_qualification_measurements): (
            ryeos_app::worker_handoff::WorkerPlacementPrepareResponse,
            Option<Value>,
        ) = decode_worker_handoff_service_response(target_value)
            .map_err(|error| internal(format!("decode target placement response: {error}")))?;
        prepared
            .validate_against(&prepare_request)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

        let target_node_client =
            crate::remote::client::RemoteClient::from_remote_cfg(&state, &loaded_remote.config);
        let target_roots = vec![
            prepared.placement_attestation_hash.clone(),
            prepared.target_runtime_seed_hash.clone(),
        ];
        let target_closure = target_node_client
            .objects_closure_get_with_total_timeout(
                &target_roots,
                crate::remote::client::NodeAdmittedObjectsClosureRequestOptions::for_node(
                    &state,
                    crate::remote::client::ObjectsClosureRequestOptions {
                        allow_incomplete: false,
                        allow_untransported_large_objects: true,
                        ..Default::default()
                    },
                )
                .map_err(internal)?,
                WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(
                    error,
                    "fetch target placement closure",
                )
            })?;
        crate::remote::import::require_local_large_object_dependencies(
            &state,
            &target_closure.closure.large_object_hashes,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        let target_payload = crate::remote::import::closure_response_to_export_payload(
            &format!("worker-placement:{operation_id}"),
            &prepared.placement_attestation_hash,
            &target_closure.entries,
        )
        .map_err(internal)?;
        let progress = ryeos_app::worker_handoff::WorkerSessionHandoffProgress {
            schema: "ryeos.worker_session_handoff_progress.v1".to_owned(),
            operation_id: operation_id.clone(),
            phase: ryeos_app::worker_handoff::WorkerHandoffPhase::TargetPrepared,
            placement_attestation_hash: Some(prepared.placement_attestation_hash.clone()),
            target_runtime_seed_hash: Some(prepared.target_runtime_seed_hash.clone()),
            writer_grant_hash: None,
            target_chain_head_hash: None,
            credential_reservation_id: Some(prepared.credential_reservation.reservation_id.clone()),
            abort_chain_head_hash: None,
        };
        #[cfg(any(test, feature = "handoff-test-support"))]
        reach_source_handoff_phase_cut(
            &state,
            ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforePreparedEvidenceProjection,
            &operation,
        )
        .map_err(internal)?;
        state
            .state_store
            .stage_sync_payload_for_existing_job(
                &target_payload,
                &ryeos_state::sync::ImportAttribution {
                    source_principal: Some(loaded_remote.config.principal_id.clone()),
                    source_peer: Some(req.remote.clone()),
                    job_id: Some(job_id.clone()),
                },
                &job_id,
                progress.phase.as_str(),
                &target_roots,
                Some(progress.to_value().map_err(internal)?),
            )
            .map_err(internal)?;
        #[cfg(any(test, feature = "handoff-test-support"))]
        reach_source_handoff_phase_cut(
            &state,
            ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourcePreparedEvidenceProjected,
            &operation,
        )
        .map_err(internal)?;
        verify_target_placement_attestation(&state, &loaded_remote.config, &prepared)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        Ok((prepared, target_qualification_measurements))
    }
    .await;
    let (prepared, _target_prepare_qualification_measurements) = match prepared_result {
        Ok((prepared, qualification_measurements)) => {
            settle_worker_handoff_attempt(
                &state,
                &job_id,
                &prepare_attempt,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Running,
                "target_prepared",
                None,
                None,
            )?;
            (prepared, qualification_measurements)
        }
        Err(error) => {
            let detail = bounded_handoff_recovery_error(&error.to_string());
            settle_worker_handoff_attempt(
                &state,
                &job_id,
                &prepare_attempt,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                "target_prepare_failed",
                Some(detail),
                None,
            )?;
            return Err(error);
        }
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    let target_prepare_ms = target_prepare_started.elapsed_millis();

    let target_authority = &prepared.placement.project_rebind.target_authority;
    target_authority.validate().map_err(internal)?;
    let successor_project_root = target_authority
        .project_root_projection()
        .map(PathBuf::from);
    let successor_project_snapshot_hash = target_authority
        .operational_snapshot_projection()
        .map(ToOwned::to_owned);
    let source_accounting_transfer = match (
        state.accounting.as_ref(),
        source_metadata.accounting_scope.as_ref(),
    ) {
        (Some(accounting), Some(_scope)) => {
            let target_scope = prepared
                .placement
                .accounting
                .target_scope
                .as_ref()
                .ok_or_else(|| internal("accounted target placement has no target scope"))?;
            Some(
                accounting
                    .export_handoff_allowance(
                        &operation_id,
                        &source_thread_id,
                        &source.chain_root_id,
                        source_accounting_frontier.as_ref().ok_or_else(|| {
                            internal("accounted handoff lost its source proposal")
                        })?,
                        target_scope,
                    )
                    .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
            )
        }
        (_, None) => None,
        (None, Some(_)) => return Err(internal("source accounting ledger disappeared")),
    };
    let successor = ryeos_app::state_store::NewThreadRecord {
        thread_id: successor_thread_id.clone(),
        chain_root_id: source.chain_root_id.clone(),
        kind: source_snapshot.kind_name.clone(),
        item_ref: source_snapshot.item_ref.clone(),
        executor_ref: source_snapshot.executor_ref.clone(),
        launch_mode: source_snapshot.launch_mode.clone(),
        current_site_id: target_site_id.clone(),
        origin_site_id: source_snapshot.origin_site_id.clone(),
        upstream_thread_id: Some(source_thread_id.clone()),
        requested_by: Some(source.owner_principal.clone()),
        project_root: successor_project_root,
        project_authority: target_authority.clone(),
        base_project_snapshot_hash: successor_project_snapshot_hash,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforeWriterCut,
        &operation,
    )
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let source_publication_started = lillux::time::MonotonicTimer::start();
    let publication = state
        .state_store
        .create_remote_adoption_successor(
            &successor,
            &source_thread_id,
            &source.chain_root_id,
            &ryeos_app::state_store::RemoteAdoptionContinuationAuthority {
                placement_attestation_hash: prepared.placement_attestation_hash.clone(),
                placement: prepared.placement.clone(),
                source_accounting_transfer,
                target_node_verifying_key: loaded_remote
                    .config
                    .pinned_signing_key()
                    .map_err(internal)?,
            },
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    root_terminalization.commit();
    if source_metadata.accounting_scope.is_some() {
        state
            .accounting
            .as_ref()
            .ok_or_else(|| internal("source accounting ledger disappeared"))?
            .confirm_handoff_source_publication(&source_thread_id)
            .map_err(internal)?;
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    let source_publication_ms = source_publication_started.elapsed_millis();
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceWriterCutPublished,
        &operation,
    )
    .map_err(internal)?;
    let target_chain_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &source.chain_root_id))
        .map_err(internal)?
        .ok_or_else(|| internal("remote continuation committed without a chain head"))?;
    let committed_progress = ryeos_app::worker_handoff::WorkerSessionHandoffProgress {
        schema: "ryeos.worker_session_handoff_progress.v1".to_owned(),
        operation_id: operation_id.clone(),
        phase: ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted,
        placement_attestation_hash: Some(prepared.placement_attestation_hash.clone()),
        target_runtime_seed_hash: Some(prepared.target_runtime_seed_hash.clone()),
        writer_grant_hash: Some(publication.writer_grant_hash.clone()),
        target_chain_head_hash: Some(target_chain_head.target_hash.clone()),
        credential_reservation_id: Some(prepared.credential_reservation.reservation_id.clone()),
        abort_chain_head_hash: None,
    };
    state
        .state_store
        .with_state_db(|db| {
            db.update_sync_job(
                &job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Running,
                    phase: committed_progress.phase.as_str().to_owned(),
                    roots: None,
                    heads: Some(vec![target_chain_head.target_hash.clone()]),
                    uploaded_hashes: vec![target_chain_head.target_hash.clone()],
                    fetched_hashes: Vec::new(),
                    last_error: None,
                    result: Some(committed_progress.to_value()?),
                },
            )
        })
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceCommitProjected,
        &operation,
    )
    .map_err(internal)?;
    let adopt_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
        operation_id: operation_id.clone(),
        chain_root_id: source.chain_root_id.clone(),
        target_chain_head_hash: target_chain_head.target_hash.clone(),
        placement_attestation_hash: prepared.placement_attestation_hash.clone(),
        writer_grant_hash: publication.writer_grant_hash,
    };
    let adopt_attempt =
        begin_worker_handoff_attempt(&state, &job_id, "target_adopt", "source-handoff")?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let target_adoption_started = lillux::time::MonotonicTimer::start();
    let adopted_result: Result<(ValidatedTargetAdoptionOutcome, Option<Value>), HandlerError> =
        async {
            let adopted_value = target_client
                .execute_service_result_with_total_timeout(
                    ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
                    &BTreeMap::new(),
                    None,
                    &serde_json::to_value(&adopt_request).map_err(internal)?,
                    &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                        ryeos_app::execution_policy::ExecutionResponse::Wait,
                    ),
                    WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
                )
                .await
                .map_err(|error| {
                    crate::remote::client::map_remote_call_error(error, "target placement adoption")
                })?;
            let (adopted_result, target_qualification_measurements): (
                ryeos_app::worker_handoff::WorkerPlacementAdoptResult,
                Option<Value>,
            ) = decode_worker_handoff_service_response(adopted_value)
                .map_err(|error| internal(format!("decode target adoption response: {error}")))?;
            let outcome = validate_and_retain_target_adoption_result(
                &state,
                &target_client,
                &loaded_remote.config,
                &job_id,
                &operation,
                &adopt_request,
                adopted_result,
                committed_progress.to_value().map_err(internal)?,
            )
            .await?;
            Ok((outcome, target_qualification_measurements))
        }
        .await;
    let (response, _target_adoption_qualification_measurements) = match adopted_result {
        Ok((outcome, qualification_measurements)) => {
            #[cfg(any(test, feature = "handoff-test-support"))]
            reach_source_handoff_phase_cut(
                &state,
                ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforeCompletion,
                &operation,
            )
            .map_err(internal)?;
            match outcome {
                ValidatedTargetAdoptionOutcome::Attached(adopted) => {
                    settle_worker_handoff_attempt(
                        &state,
                        &job_id,
                        &adopt_attempt,
                        ryeos_state::SyncJobAttemptState::Completed,
                        ryeos_state::SyncJobState::Completed,
                        "completed",
                        None,
                        Some(serde_json::to_value(&adopted).map_err(internal)?),
                    )?;
                    #[cfg(any(test, feature = "handoff-test-support"))]
                    reach_source_handoff_phase_cut(
                        &state,
                        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceCompletedBeforeResponse,
                        &operation,
                    )
                    .map_err(internal)?;
                    (
                        handoff_response(&operation, &adopted)?,
                        qualification_measurements,
                    )
                }
                ValidatedTargetAdoptionOutcome::CompletedBeforeAttachment(completion) => {
                    settle_worker_handoff_attempt_with_heads(
                        &state,
                        &job_id,
                        &adopt_attempt,
                        ryeos_state::SyncJobAttemptState::Completed,
                        ryeos_state::SyncJobState::Completed,
                        "target_completed_before_attachment",
                        None,
                        Some(serde_json::to_value(&completion).map_err(internal)?),
                        Some(vec![completion.target_chain_head_hash.clone()]),
                    )?;
                    #[cfg(any(test, feature = "handoff-test-support"))]
                    reach_source_handoff_phase_cut(
                        &state,
                        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceCompletedBeforeResponse,
                        &operation,
                    )
                    .map_err(internal)?;
                    (
                        target_completed_handoff_response(&operation, &completion)?,
                        qualification_measurements,
                    )
                }
                ValidatedTargetAdoptionOutcome::FailedBeforeAttachment(failure) => {
                    settle_worker_handoff_attempt_with_heads(
                        &state,
                        &job_id,
                        &adopt_attempt,
                        ryeos_state::SyncJobAttemptState::Completed,
                        ryeos_state::SyncJobState::Failed,
                        "target_terminal_before_attachment",
                        Some(target_terminal_failure_message(&failure)),
                        Some(serde_json::to_value(&failure).map_err(internal)?),
                        Some(vec![failure.target_chain_head_hash.clone()]),
                    )?;
                    return Err(target_terminal_failure_conflict(&failure));
                }
            }
        }
        Err(error) => {
            let detail = bounded_handoff_recovery_error(&error.to_string());
            settle_worker_handoff_attempt(
                &state,
                &job_id,
                &adopt_attempt,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                "target_adopt_failed",
                Some(detail),
                Some(committed_progress.to_value().map_err(internal)?),
            )?;
            return Err(error);
        }
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    let target_adoption_ms = target_adoption_started.elapsed_millis();
    #[cfg(any(test, feature = "handoff-test-support"))]
    let response = {
        let mut response = response;
        response["qualification_measurements"] = json!({
            "schema":"ryeos.worker_handoff_stage_measurements.v1",
            "checkpoint_load_ms":checkpoint_load_ms,
            "closure_verification_ms":closure_verification_ms,
            "target_prepare_ms":target_prepare_ms,
            "source_publication_ms":source_publication_ms,
            "target_adoption_ms":target_adoption_ms,
            "event_replay_ms":_target_adoption_qualification_measurements
                .as_ref()
                .and_then(|value| value.get("event_replay_ms"))
                .and_then(Value::as_u64),
            "project_materialization_ms":_target_prepare_qualification_measurements
                .as_ref()
                .and_then(|value| value.get("project_materialization_ms"))
                .and_then(Value::as_u64),
            "worker_attach_recovery_ms":_target_adoption_qualification_measurements
                .as_ref()
                .and_then(|value| value.get("worker_attach_recovery_ms"))
                .and_then(Value::as_u64),
            "total_handoff_ms":qualification_started.elapsed_millis(),
        });
        response
    };
    Ok(response)
}

async fn resume_committed_handoff(
    req: &HandoffRequest,
    ctx: &HandlerContext,
    state: &Arc<AppState>,
) -> Result<Option<Value>, HandlerError> {
    ryeos_app::operator_authority::require_admitted_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;

    // Startup recovery and an operator retry are two callers of the same
    // durable source operation. Once an exact operation exists, serialize
    // both through the source placement's existing disposition authority and
    // re-read every durable fact after acquiring it. Without this cut, a
    // retry can observe a retryable job immediately before recovery reserves
    // its next attempt, then fail internally while trying to reserve a second
    // running attempt for the same operation.
    loop {
        let source_placement_thread_id = match state
            .state_store
            .with_state_db(|db| source_handoff_job_for_request(db, req))
            .map_err(internal)?
        {
            Some((_, operation)) => operation.source_placement_thread_id,
            None => {
                let Some(current_placement_thread_id) = state
                    .state_store
                    .current_chain_placement_thread_id(&req.chain_root_id)
                    .map_err(internal)?
                else {
                    return Ok(None);
                };
                current_placement_thread_id
            }
        };
        let disposition = disposition_operation_lock(&source_placement_thread_id)
            .lock_owned()
            .await;

        // The initial lookup is only a lock-key probe. An operation can be
        // published, or the chain can cross its writer cut, before this guard
        // is acquired. Prove that the guard still names the source placement
        // of the exact request (or the unchanged pre-operation placement)
        // before allowing the helper to inspect or contact the target.
        let retained = state
            .state_store
            .with_state_db(|db| source_handoff_job_for_request(db, req))
            .map_err(internal)?;
        let guarded_coordinate_matches = match retained {
            Some((_, operation)) => {
                operation.source_placement_thread_id == source_placement_thread_id
            }
            None => {
                state
                    .state_store
                    .current_chain_placement_thread_id(&req.chain_root_id)
                    .map_err(internal)?
                    .as_deref()
                    == Some(source_placement_thread_id.as_str())
            }
        };
        if !guarded_coordinate_matches {
            drop(disposition);
            continue;
        }
        return resume_committed_handoff_locked(req, ctx, state).await;
    }
}

async fn resume_committed_handoff_locked(
    req: &HandoffRequest,
    ctx: &HandlerContext,
    state: &Arc<AppState>,
) -> Result<Option<Value>, HandlerError> {
    if let Some((job, operation)) = state
        .state_store
        .with_state_db(|db| source_handoff_job_for_request(db, req))
        .map_err(internal)?
    {
        ctx.require_owner(Some(&operation.owner_principal))?;
        if let Some(terminal) =
            fold_rooted_source_handoff_terminal(state, &job, &operation).map_err(internal)?
        {
            return match terminal {
                ValidatedSourceHandoffTerminal::Completed(adopted) => {
                    Ok(Some(handoff_response(&operation, &adopted)?))
                }
                ValidatedSourceHandoffTerminal::TargetCompleted(completion) => Ok(Some(
                    target_completed_handoff_response(&operation, &completion)?,
                )),
                ValidatedSourceHandoffTerminal::Cancelled(aborted) => {
                    Err(HandlerError::Conflict(format!(
                        "worker handoff {} is durably aborted ({})",
                        operation.operation_id, aborted.disposition
                    )))
                }
                ValidatedSourceHandoffTerminal::Failed(failure) => {
                    Err(target_terminal_failure_conflict(&failure))
                }
            };
        }
        match job.state {
            ryeos_state::SyncJobState::Completed => {
                return match validate_source_handoff_terminal(state, &job, &operation)
                    .map_err(internal)?
                {
                    ValidatedSourceHandoffTerminal::Completed(adopted) => {
                        Ok(Some(handoff_response(&operation, &adopted)?))
                    }
                    ValidatedSourceHandoffTerminal::TargetCompleted(completion) => Ok(Some(
                        target_completed_handoff_response(&operation, &completion)?,
                    )),
                    _ => Err(internal(
                        "completed source handoff validated as a non-success terminal",
                    )),
                };
            }
            ryeos_state::SyncJobState::Cancelled => {
                let ValidatedSourceHandoffTerminal::Cancelled(aborted) =
                    validate_source_handoff_terminal(state, &job, &operation).map_err(internal)?
                else {
                    return Err(internal(
                        "cancelled source handoff validated as another terminal",
                    ));
                };
                return Err(HandlerError::Conflict(format!(
                    "worker handoff {} is durably aborted ({})",
                    operation.operation_id, aborted.disposition
                )));
            }
            ryeos_state::SyncJobState::Failed => {
                let ValidatedSourceHandoffTerminal::Failed(failure) =
                    validate_source_handoff_terminal(state, &job, &operation).map_err(internal)?
                else {
                    return Err(internal(
                        "failed source handoff validated as another terminal",
                    ));
                };
                return Err(target_terminal_failure_conflict(&failure));
            }
            ryeos_state::SyncJobState::Planned
            | ryeos_state::SyncJobState::Running
            | ryeos_state::SyncJobState::Retryable => {}
        }
    }
    let Some(placement_thread_id) = state
        .state_store
        .current_chain_placement_thread_id(&req.chain_root_id)
        .map_err(internal)?
    else {
        return Ok(None);
    };
    if state
        .state_store
        .dedicated_session(&placement_thread_id)
        .map_err(internal)?
        .is_some()
    {
        return Ok(None);
    }
    let Some(remote_authority) = state
        .state_store
        .remote_continuation_authority(&req.chain_root_id, &placement_thread_id)
        .map_err(internal)?
    else {
        return Ok(None);
    };
    let job_id = format!("worker-handoff-source:{}", remote_authority.operation_id);
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))
        .map_err(internal)?
        .ok_or_else(|| internal("committed remote continuation has no source handoff job"))?;
    let operation = ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
        job.operation.clone(),
    )
    .map_err(internal)?;
    validate_source_handoff_job_coordinates(&job, &operation).map_err(internal)?;
    if operation.role != ryeos_app::worker_handoff::WorkerHandoffJobRole::Source
        || operation.operation_id != remote_authority.operation_id
        || operation.preflight_id != remote_authority.preflight_id
        || operation.preflight_attestation_hash != remote_authority.preflight_attestation_hash
        || operation.follow_delivery_reservation_attestation_hash
            != remote_authority.follow_delivery_reservation_attestation_hash
        || operation.chain_root_id != req.chain_root_id
        || operation.successor_placement_thread_id != placement_thread_id
        || operation.peer_remote_name != req.remote
        || operation.preflight_id != req.preflight_id
        || operation.target_credential_profile_id != req.target_credential_profile_id
        || format!("cas:{}", operation.checkpoint_manifest_hash) != req.manifest_ref
    {
        return Err(HandlerError::BadRequest(
            "handoff retry differs from the committed source operation".into(),
        ));
    }
    ctx.require_owner(Some(&operation.owner_principal))?;
    if let Some(terminal) =
        fold_rooted_source_handoff_terminal(state, &job, &operation).map_err(internal)?
    {
        return match terminal {
            ValidatedSourceHandoffTerminal::Completed(adopted) => {
                Ok(Some(handoff_response(&operation, &adopted)?))
            }
            ValidatedSourceHandoffTerminal::TargetCompleted(completion) => Ok(Some(
                target_completed_handoff_response(&operation, &completion)?,
            )),
            ValidatedSourceHandoffTerminal::Cancelled(aborted) => {
                Err(HandlerError::Conflict(format!(
                    "worker handoff {} is durably aborted ({})",
                    operation.operation_id, aborted.disposition
                )))
            }
            ValidatedSourceHandoffTerminal::Failed(failure) => {
                Err(target_terminal_failure_conflict(&failure))
            }
        };
    }
    let progress = job
        .result
        .map(ryeos_app::worker_handoff::WorkerSessionHandoffProgress::from_value)
        .transpose()
        .map_err(internal)?
        .ok_or_else(|| internal("committed handoff job has no durable progress"))?;
    if progress.phase.source_is_only_authorized_writer()
        || progress.operation_id != operation.operation_id
        || progress.placement_attestation_hash.as_deref()
            != Some(remote_authority.target_placement_attestation_hash.as_str())
        || progress.writer_grant_hash.as_deref()
            != Some(remote_authority.chain_writer_grant_hash.as_str())
    {
        return Err(internal(
            "committed remote continuation contradicts source handoff progress",
        ));
    }
    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(std::path::Path::new(&operation.source_project_path)),
    )
    .map_err(internal)?;
    let loaded_remote =
        crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if loaded_remote.config.site_id != operation.target_site_id {
        return Err(HandlerError::BadRequest(
            "configured target site changed after source commit".into(),
        ));
    }
    let target_client =
        crate::remote::client::RemoteClient::from_remote_cfg(state, &loaded_remote.config);
    let adopt_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        target_chain_head_hash: progress
            .target_chain_head_hash
            .clone()
            .ok_or_else(|| internal("committed handoff progress has no target chain head"))?,
        placement_attestation_hash: progress
            .placement_attestation_hash
            .clone()
            .ok_or_else(|| internal("committed handoff progress has no target placement"))?,
        writer_grant_hash: progress
            .writer_grant_hash
            .clone()
            .ok_or_else(|| internal("committed handoff progress has no writer grant"))?,
    };
    let adopt_attempt =
        begin_worker_handoff_attempt(state, &job_id, "target_adopt_retry", "source-handoff")?;
    let adopted_result: Result<ValidatedTargetAdoptionOutcome, HandlerError> = async {
        let value = target_client
            .execute_service_result_with_total_timeout(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&adopt_request).map_err(internal)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(
                    error,
                    "retry target placement adoption",
                )
            })?;
        let (result, _qualification_measurements): (
            ryeos_app::worker_handoff::WorkerPlacementAdoptResult,
            Option<Value>,
        ) = decode_worker_handoff_service_response(value).map_err(internal)?;
        validate_and_retain_target_adoption_result(
            state,
            &target_client,
            &loaded_remote.config,
            &job_id,
            &operation,
            &adopt_request,
            result,
            progress.to_value().map_err(internal)?,
        )
        .await
    }
    .await;
    let success_response = match adopted_result {
        Ok(ValidatedTargetAdoptionOutcome::Attached(adopted)) => {
            settle_worker_handoff_attempt(
                state,
                &job_id,
                &adopt_attempt,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Completed,
                "completed",
                None,
                Some(serde_json::to_value(&adopted).map_err(internal)?),
            )?;
            handoff_response(&operation, &adopted)?
        }
        Ok(ValidatedTargetAdoptionOutcome::CompletedBeforeAttachment(completion)) => {
            settle_worker_handoff_attempt_with_heads(
                state,
                &job_id,
                &adopt_attempt,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Completed,
                "target_completed_before_attachment",
                None,
                Some(serde_json::to_value(&completion).map_err(internal)?),
                Some(vec![completion.target_chain_head_hash.clone()]),
            )?;
            target_completed_handoff_response(&operation, &completion)?
        }
        Ok(ValidatedTargetAdoptionOutcome::FailedBeforeAttachment(failure)) => {
            settle_worker_handoff_attempt_with_heads(
                state,
                &job_id,
                &adopt_attempt,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Failed,
                "target_terminal_before_attachment",
                Some(target_terminal_failure_message(&failure)),
                Some(serde_json::to_value(&failure).map_err(internal)?),
                Some(vec![failure.target_chain_head_hash.clone()]),
            )?;
            return Err(target_terminal_failure_conflict(&failure));
        }
        Err(error) => {
            let detail = bounded_handoff_recovery_error(&error.to_string());
            settle_worker_handoff_attempt(
                state,
                &job_id,
                &adopt_attempt,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                "target_adopt_failed",
                Some(detail),
                Some(progress.to_value().map_err(internal)?),
            )?;
            return Err(error);
        }
    };
    Ok(Some(success_response))
}

fn take_handoff_qualification_measurements(value: &mut Value) -> Option<Value> {
    #[cfg(any(test, feature = "handoff-test-support"))]
    {
        value
            .as_object_mut()
            .and_then(|object| object.remove("qualification_measurements"))
    }
    #[cfg(not(any(test, feature = "handoff-test-support")))]
    {
        let _ = value;
        None
    }
}

async fn fetch_target_handoff_terminal_attestation(
    target_client: &crate::remote::client::RemoteClient,
    target: &crate::remote::config::RemoteConfig,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    terminal_attestation_hash: &str,
) -> Result<
    (
        ryeos_state::objects::Attestation,
        ryeos_state::sync::ExportPayload,
    ),
    HandlerError,
> {
    fail_handoff_terminal_fetch_once_for_qualification()?;
    ryeos_state::objects::thread_snapshot::validate_canonical_hash(
        "worker handoff terminal attestation",
        terminal_attestation_hash,
    )
    .map_err(internal)?;
    let response = target_client
        .objects_get_with_response_limit_and_total_timeout(
            &[terminal_attestation_hash.to_owned()],
            &[],
            MAX_HANDOFF_TERMINAL_ATTESTATION_BYTES,
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(
                error,
                "fetch target handoff terminal attestation",
            )
        })?;
    let [entry] = response.entries.as_slice() else {
        return Err(internal(
            "target handoff terminal attestation fetch returned another cardinality",
        ));
    };
    if entry.hash != terminal_attestation_hash || entry.kind != "object" {
        return Err(internal(
            "target handoff terminal attestation is absent or has another CAS kind",
        ));
    }
    let payload = crate::remote::import::closure_response_to_export_payload(
        &operation.chain_root_id,
        terminal_attestation_hash,
        &response.entries,
    )
    .map_err(internal)?;
    let attestation = ryeos_state::objects::Attestation::from_value(
        entry
            .value
            .as_ref()
            .ok_or_else(|| internal("target handoff terminal attestation has no object value"))?,
    )
    .map_err(internal)?;
    attestation
        .verify_with_key(&target.pinned_signing_key().map_err(internal)?)
        .map_err(internal)?;
    if attestation.issuer != target.principal_id || attestation.expires_at.is_some() {
        return Err(internal(
            "target handoff terminal attestation changed its configured signer or retention",
        ));
    }
    Ok((attestation, payload))
}

async fn fetch_target_handoff_terminal_chain_closure(
    state: &AppState,
    target_client: &crate::remote::client::RemoteClient,
    target: &crate::remote::config::RemoteConfig,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    terminal_attestation_hash: &str,
    terminal_chain_head_hash: &str,
) -> Result<
    (
        ryeos_state::objects::Attestation,
        ryeos_state::sync::ExportPayload,
    ),
    HandlerError,
> {
    fail_handoff_terminal_fetch_once_for_qualification()?;
    for (label, hash) in [
        (
            "worker handoff terminal attestation",
            terminal_attestation_hash,
        ),
        (
            "worker handoff terminal chain head",
            terminal_chain_head_hash,
        ),
    ] {
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(label, hash)
            .map_err(internal)?;
    }
    let roots = vec![
        terminal_attestation_hash.to_owned(),
        terminal_chain_head_hash.to_owned(),
    ];
    let response = target_client
        .objects_closure_get_with_total_timeout(
            &roots,
            crate::remote::client::NodeAdmittedObjectsClosureRequestOptions::for_node(
                state,
                crate::remote::client::ObjectsClosureRequestOptions {
                    allow_incomplete: false,
                    allow_untransported_large_objects: true,
                    ..Default::default()
                },
            )
            .map_err(internal)?,
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(
                error,
                "fetch target handoff terminal chain closure",
            )
        })?;
    crate::remote::import::require_local_large_object_dependencies(
        state,
        &response.closure.large_object_hashes,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let entry = response
        .entries
        .iter()
        .find(|entry| entry.hash == terminal_attestation_hash && entry.kind == "object")
        .ok_or_else(|| internal("target handoff terminal attestation is absent"))?;
    let attestation = ryeos_state::objects::Attestation::from_value(
        entry
            .value
            .as_ref()
            .ok_or_else(|| internal("target handoff terminal attestation has no object value"))?,
    )
    .map_err(internal)?;
    attestation
        .verify_with_key(&target.pinned_signing_key().map_err(internal)?)
        .map_err(internal)?;
    if attestation.issuer != target.principal_id || attestation.expires_at.is_some() {
        return Err(internal(
            "target handoff terminal attestation changed its configured signer or retention",
        ));
    }
    let payload = crate::remote::import::closure_response_to_export_payload(
        &operation.chain_root_id,
        terminal_chain_head_hash,
        &response.entries,
    )
    .map_err(internal)?;
    Ok((attestation, payload))
}

#[cfg(any(test, feature = "handoff-test-support"))]
fn fail_handoff_terminal_fetch_once_for_qualification() -> Result<(), HandlerError> {
    static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if std::env::var_os("RYEOS_TEST_HANDOFF_FAIL_TERMINAL_FETCH_ONCE").is_some()
        && !FAILED.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(internal(
            "qualification injected one target terminal fetch failure",
        ));
    }
    Ok(())
}

#[cfg(not(any(test, feature = "handoff-test-support")))]
fn fail_handoff_terminal_fetch_once_for_qualification() -> Result<(), HandlerError> {
    Ok(())
}

enum ValidatedTargetAdoptionOutcome {
    Attached(ryeos_app::worker_handoff::WorkerPlacementAdoptResponse),
    CompletedBeforeAttachment(ryeos_app::worker_handoff::WorkerPlacementCompletionResponse),
    FailedBeforeAttachment(ryeos_app::worker_handoff::WorkerPlacementFailureResponse),
}

#[allow(clippy::too_many_arguments)]
async fn validate_and_retain_target_adoption_result(
    state: &AppState,
    target_client: &crate::remote::client::RemoteClient,
    target: &crate::remote::config::RemoteConfig,
    job_id: &str,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    request: &ryeos_app::worker_handoff::WorkerPlacementAdoptRequest,
    result: ryeos_app::worker_handoff::WorkerPlacementAdoptResult,
    durable_progress: Value,
) -> Result<ValidatedTargetAdoptionOutcome, HandlerError> {
    result.validate().map_err(internal)?;
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(internal)?;
    let guard = authority.acquire_shared_guard().map_err(internal)?;
    let continuation = validate_source_handoff_continuation_head(
        &authority.cas_store().map_err(internal)?,
        operation,
        &request.target_chain_head_hash,
    )
    .map_err(internal)?;
    let target_fingerprint =
        lillux::crypto::fingerprint(&target.pinned_signing_key().map_err(internal)?);
    if continuation.target_node_signer_fingerprint != target_fingerprint {
        return Err(internal(
            "target adoption signer changed since the signed writer transfer",
        ));
    }
    drop(guard);
    drop(authority);
    match result {
        ryeos_app::worker_handoff::WorkerPlacementAdoptResult::Attached {
            response,
            terminal_attestation_hash,
        } => {
            let (attestation, payload) = fetch_target_handoff_terminal_attestation(
                target_client,
                target,
                operation,
                &terminal_attestation_hash,
            )
            .await?;
            if attestation.issuer_fingerprint().map_err(internal)? != target_fingerprint {
                return Err(internal(
                    "target adoption attestation signer differs from the signed writer transfer",
                ));
            }
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffAdoptionReceiptEvidence::from_attestation(
                    &attestation,
                    &target.pinned_signing_key().map_err(internal)?,
                )
                .map_err(internal)?;
            receipt
                .target_operation
                .validate_target_projection_of(operation)
                .map_err(internal)?;
            if receipt.request != *request || receipt.response != response {
                return Err(internal(
                    "target adoption result contradicts its signed terminal attestation",
                ));
            }
            state
                .state_store
                .stage_sync_payload_for_existing_job(
                    &payload,
                    &ryeos_state::sync::ImportAttribution {
                        source_principal: Some(target.principal_id.clone()),
                        source_peer: Some(target.name.clone()),
                        job_id: Some(job_id.to_owned()),
                    },
                    job_id,
                    ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted.as_str(),
                    &[terminal_attestation_hash],
                    Some(durable_progress),
                )
                .map_err(internal)?;
            Ok(ValidatedTargetAdoptionOutcome::Attached(response))
        }
        ryeos_app::worker_handoff::WorkerPlacementAdoptResult::FailedBeforeAttachment {
            failure,
            terminal_attestation_hash,
        } => {
            if failure.operation_id != operation.operation_id
                || failure.chain_root_id != operation.chain_root_id
                || failure.placement_thread_id != operation.successor_placement_thread_id
                || failure.target_chain_head_hash == request.target_chain_head_hash
            {
                return Err(internal(
                    "target terminal failure changed its handoff coordinates",
                ));
            }
            let (attestation, payload) = fetch_target_handoff_terminal_chain_closure(
                state,
                target_client,
                target,
                operation,
                &terminal_attestation_hash,
                &failure.target_chain_head_hash,
            )
            .await?;
            if attestation.issuer_fingerprint().map_err(internal)? != target_fingerprint {
                return Err(internal(
                    "target failure attestation signer differs from the signed writer transfer",
                ));
            }
            let receipt =
                ryeos_app::worker_handoff::WorkerHandoffTerminalFailureEvidence::from_attestation(
                    &attestation,
                    &target.pinned_signing_key().map_err(internal)?,
                )
                .map_err(internal)?;
            receipt
                .target_operation
                .validate_target_projection_of(operation)
                .map_err(internal)?;
            if receipt.request != *request || receipt.failure != failure {
                return Err(internal(
                    "target failure result contradicts its signed terminal attestation",
                ));
            }
            let failure_head = failure.target_chain_head_hash.clone();
            state
                .state_store
                .stage_verified_sync_payload_for_existing_job(
                    &payload,
                    &ryeos_state::sync::ImportAttribution {
                        source_principal: Some(target.principal_id.clone()),
                        source_peer: Some(target.name.clone()),
                        job_id: Some(job_id.to_owned()),
                    },
                    job_id,
                    ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted.as_str(),
                    &[terminal_attestation_hash, failure_head.clone()],
                    Some(durable_progress),
                    |cas| {
                        ryeos_state::sync::verify_chain_closure_anchored_pinned(
                            cas,
                            &operation.chain_root_id,
                            &failure_head,
                            &request.target_chain_head_hash,
                        )?;
                        validate_source_handoff_terminal_successor(
                            cas,
                            operation,
                            &failure_head,
                            &failure.terminal_status,
                        )
                    },
                )
                .map_err(internal)?;
            Ok(ValidatedTargetAdoptionOutcome::FailedBeforeAttachment(
                failure,
            ))
        }
        ryeos_app::worker_handoff::WorkerPlacementAdoptResult::CompletedBeforeAttachment {
            completion,
            terminal_attestation_hash,
        } => {
            if completion.operation_id != operation.operation_id
                || completion.chain_root_id != operation.chain_root_id
                || completion.placement_thread_id != operation.successor_placement_thread_id
                || completion.target_chain_head_hash == request.target_chain_head_hash
                || completion.terminal_status != "completed"
            {
                return Err(internal(
                    "target terminal completion changed its handoff coordinates",
                ));
            }
            let (attestation, payload) = fetch_target_handoff_terminal_chain_closure(
                state,
                target_client,
                target,
                operation,
                &terminal_attestation_hash,
                &completion.target_chain_head_hash,
            )
            .await?;
            if attestation.issuer_fingerprint().map_err(internal)? != target_fingerprint {
                return Err(internal(
                    "target completion attestation signer differs from the signed writer transfer",
                ));
            }
            let receipt = ryeos_app::worker_handoff::WorkerHandoffTerminalCompletionEvidence::from_attestation(
                &attestation,
                &target.pinned_signing_key().map_err(internal)?,
            )
            .map_err(internal)?;
            receipt
                .target_operation
                .validate_target_projection_of(operation)
                .map_err(internal)?;
            if receipt.request != *request || receipt.completion != completion {
                return Err(internal(
                    "target completion result contradicts its signed terminal attestation",
                ));
            }
            let completion_head = completion.target_chain_head_hash.clone();
            state
                .state_store
                .stage_verified_sync_payload_for_existing_job(
                    &payload,
                    &ryeos_state::sync::ImportAttribution {
                        source_principal: Some(target.principal_id.clone()),
                        source_peer: Some(target.name.clone()),
                        job_id: Some(job_id.to_owned()),
                    },
                    job_id,
                    ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted.as_str(),
                    &[terminal_attestation_hash, completion_head.clone()],
                    Some(durable_progress),
                    |cas| {
                        ryeos_state::sync::verify_chain_closure_anchored_pinned(
                            cas,
                            &operation.chain_root_id,
                            &completion_head,
                            &request.target_chain_head_hash,
                        )?;
                        validate_source_handoff_terminal_successor(
                            cas,
                            operation,
                            &completion_head,
                            &completion.terminal_status,
                        )
                    },
                )
                .map_err(internal)?;
            Ok(ValidatedTargetAdoptionOutcome::CompletedBeforeAttachment(
                completion,
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn validate_and_retain_target_abort_result(
    state: &AppState,
    target_client: &crate::remote::client::RemoteClient,
    target: &crate::remote::config::RemoteConfig,
    job_id: &str,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    request: &ryeos_app::worker_handoff::WorkerPlacementAbortRequest,
    result: ryeos_app::worker_handoff::WorkerPlacementAbortResult,
    durable_progress: Value,
) -> Result<ryeos_app::worker_handoff::WorkerPlacementAbortResponse, HandlerError> {
    result.validate_against(request).map_err(internal)?;
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(internal)?;
    let guard = authority.acquire_shared_guard().map_err(internal)?;
    let preflight_target_fingerprint = retained_preflight_target_signer_fingerprint(
        &authority.cas_store().map_err(internal)?,
        operation,
        target,
    )
    .map_err(internal)?;
    drop(guard);
    drop(authority);
    let (attestation, payload) = fetch_target_handoff_terminal_attestation(
        target_client,
        target,
        operation,
        &result.terminal_attestation_hash,
    )
    .await?;
    if attestation.issuer_fingerprint().map_err(internal)? != preflight_target_fingerprint {
        return Err(internal(
            "target abort attestation signer differs from the retained preflight",
        ));
    }
    let receipt = ryeos_app::worker_handoff::WorkerHandoffAbortFenceEvidence::from_attestation(
        &attestation,
        &target.pinned_signing_key().map_err(internal)?,
    )
    .map_err(internal)?;
    receipt
        .target_operation
        .validate_target_projection_of(operation)
        .map_err(internal)?;
    if receipt.abort_chain_head_hash != request.abort_chain_head_hash
        || receipt.terminal_disposition.as_deref() != Some(result.response.disposition.as_str())
    {
        return Err(internal(
            "target abort result contradicts its signed terminal attestation",
        ));
    }
    state
        .state_store
        .stage_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(target.principal_id.clone()),
                source_peer: Some(target.name.clone()),
                job_id: Some(job_id.to_owned()),
            },
            job_id,
            ryeos_app::worker_handoff::WorkerHandoffPhase::AbortAuthorized.as_str(),
            &[result.terminal_attestation_hash],
            Some(durable_progress),
        )
        .map_err(internal)?;
    Ok(result.response)
}

/// Decode the closed, typed worker-handoff response after separating the
/// unsigned timing evidence emitted only by qualification builds. Keeping this
/// at the remote service boundary prevents live retry and startup recovery from
/// acquiring different wire contracts; durable job results always contain
/// only the typed response.
fn decode_worker_handoff_service_response<T: DeserializeOwned>(
    mut value: Value,
) -> serde_json::Result<(T, Option<Value>)> {
    let qualification_measurements = take_handoff_qualification_measurements(&mut value);
    serde_json::from_value(value).map(|response| (response, qualification_measurements))
}

fn handoff_response(
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    adopted: &ryeos_app::worker_handoff::WorkerPlacementAdoptResponse,
) -> Result<Value, HandlerError> {
    Ok(json!({
        "operation_id":operation.operation_id,
        "chain_root_id":operation.chain_root_id,
        "source_placement_thread_id":operation.source_placement_thread_id,
        "placement_thread_id":operation.successor_placement_thread_id,
        "current_site_id":operation.target_site_id,
        "target_chain_head_hash":adopted.target_chain_head_hash,
        "delivery":adopted.delivery,
    }))
}

fn target_completed_handoff_response(
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    completion: &ryeos_app::worker_handoff::WorkerPlacementCompletionResponse,
) -> Result<Value, HandlerError> {
    Ok(json!({
        "operation_id":operation.operation_id,
        "chain_root_id":operation.chain_root_id,
        "source_placement_thread_id":operation.source_placement_thread_id,
        "placement_thread_id":operation.successor_placement_thread_id,
        "current_site_id":operation.target_site_id,
        "target_chain_head_hash":completion.target_chain_head_hash,
        "delivery":"completed_on_target",
        "terminal_status":completion.terminal_status,
    }))
}

/// Recover every durable source-handoff commit state. Before source-ledger
/// export, recovery may publish a signed abort. After the anchored export it
/// must finish the exact writer cut; after that it redrives target adoption.
pub async fn recover_durable_source_handoffs(state: &AppState) -> Result<usize> {
    let mut recovered = 0usize;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type_after(
                ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION,
                after
                    .as_ref()
                    .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
                64,
            )
        })?;
        let Some(last) = jobs.last() else {
            break;
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            let operation =
                match ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
                    job.operation.clone(),
                ) {
                    Ok(operation)
                        if operation.role
                            == ryeos_app::worker_handoff::WorkerHandoffJobRole::Source =>
                    {
                        operation
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::error!(job_id = %job.job_id, error = %error, "invalid source worker handoff job retained for operator inspection");
                        continue;
                    }
                };
            let _operation_guard =
                disposition_operation_lock(&operation.source_placement_thread_id)
                    .lock_owned()
                    .await;
            let latest_job = state
                .state_store
                .with_state_db(|db| db.get_sync_job(&job.job_id))?
                .ok_or_else(|| anyhow::anyhow!("source worker handoff job disappeared"))?;
            if matches!(
                latest_job.state,
                ryeos_state::SyncJobState::Completed
                    | ryeos_state::SyncJobState::Failed
                    | ryeos_state::SyncJobState::Cancelled
            ) {
                continue;
            }
            if latest_job.operation != job.operation {
                anyhow::bail!("source worker handoff job operation changed during recovery");
            }
            if fold_rooted_source_handoff_terminal(state, &latest_job, &operation)?.is_some() {
                recovered += 1;
                continue;
            }
            let mut progress = latest_job
                .result
                .clone()
                .map(ryeos_app::worker_handoff::WorkerSessionHandoffProgress::from_value)
                .transpose()?
                .unwrap_or(
                    ryeos_app::worker_handoff::WorkerSessionHandoffProgress::planned(
                        operation.operation_id.clone(),
                    )?,
                );
            let current_placement = state
                .state_store
                .current_chain_placement_thread_id(&operation.chain_root_id)?;
            if progress.phase.source_is_only_authorized_writer()
                && current_placement.as_deref()
                    == Some(operation.successor_placement_thread_id.as_str())
            {
                let remote = state
                    .state_store
                    .remote_continuation_authority(
                        &operation.chain_root_id,
                        &operation.successor_placement_thread_id,
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "authoritative successor has no remote-continuation authority"
                        )
                    })?;
                let head = state
                    .state_store
                    .with_state_db(|db| {
                        db.read_generic_head_ref("chains", &operation.chain_root_id)
                    })?
                    .ok_or_else(|| anyhow::anyhow!("source handoff chain head disappeared"))?;
                if remote.operation_id != operation.operation_id
                    || remote.preflight_id != operation.preflight_id
                    || remote.preflight_attestation_hash != operation.preflight_attestation_hash
                    || remote.follow_delivery_reservation_attestation_hash
                        != operation.follow_delivery_reservation_attestation_hash
                    || remote.source_chain_head_hash != operation.source_chain_head_hash
                    || remote.source_last_event_hash != operation.source_last_event_hash
                    || remote.checkpoint_manifest_hash != operation.checkpoint_manifest_hash
                    || remote.successor_thread_id != operation.successor_placement_thread_id
                    || remote.source_site_id != operation.source_site_id
                    || remote.target_site_id != operation.target_site_id
                    || progress.placement_attestation_hash.as_deref()
                        != Some(remote.target_placement_attestation_hash.as_str())
                    || progress.target_runtime_seed_hash.as_deref()
                        != Some(remote.target_runtime_seed_hash.as_str())
                    || progress.credential_reservation_id.is_none()
                    || head.signer != state.identity.fingerprint()
                {
                    anyhow::bail!(
                        "source handoff crash recovery contradicts signed continuation authority"
                    );
                }
                progress.phase = ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted;
                progress.writer_grant_hash = Some(remote.chain_writer_grant_hash);
                progress.target_chain_head_hash = Some(head.target_hash.clone());
                progress.validate()?;
                state.state_store.with_state_db(|db| {
                    db.update_sync_job(
                        &job.job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Running,
                            phase: progress.phase.as_str().to_owned(),
                            roots: None,
                            heads: Some(vec![head.target_hash]),
                            uploaded_hashes: Vec::new(),
                            fetched_hashes: Vec::new(),
                            last_error: None,
                            result: Some(progress.to_value()?),
                        },
                    )
                })?;
            }
            if progress.phase.source_is_only_authorized_writer() {
                let allowance_was_exported = state
                    .accounting
                    .as_ref()
                    .map(|ledger| ledger.handoff_allowance_exported(&operation.operation_id))
                    .transpose()?
                    .unwrap_or(false);
                if allowance_was_exported {
                    recover_exported_source_writer_cut(
                        state,
                        &latest_job,
                        &operation,
                        &mut progress,
                    )
                    .await?;
                    recovered += 1;
                    continue;
                }
                if recover_pre_cut_source_handoff_abort(state, &latest_job, &operation, &progress)
                    .await?
                {
                    recovered += 1;
                }
                continue;
            }
            let (Some(placement_hash), Some(writer_hash), Some(target_head_hash)) = (
                progress.placement_attestation_hash.clone(),
                progress.writer_grant_hash.clone(),
                progress.target_chain_head_hash.clone(),
            ) else {
                continue;
            };
            let current = state
                .state_store
                .current_chain_placement_thread_id(&operation.chain_root_id)?;
            if current.as_deref() != Some(operation.successor_placement_thread_id.as_str()) {
                tracing::error!(
                    job_id = %job.job_id,
                    "source worker handoff recovery found another authoritative chain placement"
                );
                continue;
            }
            let Some(remote) = state.state_store.remote_continuation_authority(
                &operation.chain_root_id,
                &operation.successor_placement_thread_id,
            )?
            else {
                tracing::error!(job_id = %job.job_id, "source handoff successor has no remote authority");
                continue;
            };
            let head = state
                .state_store
                .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
                .ok_or_else(|| anyhow::anyhow!("source handoff chain head disappeared"))?;
            if remote.operation_id != operation.operation_id
                || remote.preflight_id != operation.preflight_id
                || remote.preflight_attestation_hash != operation.preflight_attestation_hash
                || remote.follow_delivery_reservation_attestation_hash
                    != operation.follow_delivery_reservation_attestation_hash
                || remote.target_placement_attestation_hash != placement_hash
                || remote.chain_writer_grant_hash != writer_hash
                || remote.successor_thread_id != operation.successor_placement_thread_id
                || head.target_hash != target_head_hash
                || head.signer != state.identity.fingerprint()
            {
                tracing::error!(job_id = %job.job_id, "source handoff durable progress contradicts signed chain authority");
                continue;
            }
            let report = crate::remote::config::load_remotes_layered_report(
                &state.config.app_root,
                Some(std::path::Path::new(&operation.source_project_path)),
            )?;
            let loaded_remote = crate::remote::config::get_loaded_remote(
                &report.remotes,
                &operation.peer_remote_name,
            )?;
            if loaded_remote.config.site_id != operation.target_site_id {
                tracing::error!(job_id = %job.job_id, "source handoff configured target site changed");
                continue;
            }
            let client =
                crate::remote::client::RemoteClient::from_remote_cfg(state, &loaded_remote.config);
            let request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
                operation_id: operation.operation_id.clone(),
                chain_root_id: operation.chain_root_id.clone(),
                target_chain_head_hash: target_head_hash,
                placement_attestation_hash: placement_hash,
                writer_grant_hash: writer_hash,
            };
            let attempt_id = begin_worker_handoff_attempt(
                state,
                &job.job_id,
                "target_adopt_recovery",
                "source-handoff-recovery",
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let outcome: Result<ValidatedTargetAdoptionOutcome> = async {
                let value = client
                    .execute_service_result_with_total_timeout(
                        ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
                        &BTreeMap::new(),
                        None,
                        &serde_json::to_value(&request)?,
                        &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                            ryeos_app::execution_policy::ExecutionResponse::Wait,
                        ),
                        WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let (terminal_result, _qualification_measurements): (
                    ryeos_app::worker_handoff::WorkerPlacementAdoptResult,
                    Option<Value>,
                ) = decode_worker_handoff_service_response(value)?;
                validate_and_retain_target_adoption_result(
                    state,
                    &client,
                    &loaded_remote.config,
                    &job.job_id,
                    &operation,
                    &request,
                    terminal_result,
                    progress.to_value()?,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
            }
            .await;
            match outcome {
                Ok(outcome) => {
                    match outcome {
                        ValidatedTargetAdoptionOutcome::Attached(adopted) => {
                            settle_worker_handoff_attempt(
                                state,
                                &job.job_id,
                                &attempt_id,
                                ryeos_state::SyncJobAttemptState::Completed,
                                ryeos_state::SyncJobState::Completed,
                                "completed",
                                None,
                                Some(serde_json::to_value(&adopted)?),
                            )
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        }
                        ValidatedTargetAdoptionOutcome::CompletedBeforeAttachment(completion) => {
                            settle_worker_handoff_attempt_with_heads(
                                state,
                                &job.job_id,
                                &attempt_id,
                                ryeos_state::SyncJobAttemptState::Completed,
                                ryeos_state::SyncJobState::Completed,
                                "target_completed_before_attachment",
                                None,
                                Some(serde_json::to_value(&completion)?),
                                Some(vec![completion.target_chain_head_hash]),
                            )
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        }
                        ValidatedTargetAdoptionOutcome::FailedBeforeAttachment(failure) => {
                            settle_worker_handoff_attempt_with_heads(
                                state,
                                &job.job_id,
                                &attempt_id,
                                ryeos_state::SyncJobAttemptState::Completed,
                                ryeos_state::SyncJobState::Failed,
                                "target_terminal_before_attachment",
                                Some(target_terminal_failure_message(&failure)),
                                Some(serde_json::to_value(&failure)?),
                                Some(vec![failure.target_chain_head_hash]),
                            )
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        }
                    }
                    recovered += 1;
                }
                Err(error) => {
                    let latest = state
                        .state_store
                        .with_state_db(|db| db.get_sync_job(&job.job_id))?
                        .ok_or_else(|| anyhow::anyhow!("source handoff job disappeared"))?;
                    if matches!(
                        latest.state,
                        ryeos_state::SyncJobState::Completed
                            | ryeos_state::SyncJobState::Failed
                            | ryeos_state::SyncJobState::Cancelled
                    ) {
                        continue;
                    }
                    settle_worker_handoff_attempt(
                        state,
                        &job.job_id,
                        &attempt_id,
                        ryeos_state::SyncJobAttemptState::Failed,
                        ryeos_state::SyncJobState::Retryable,
                        &latest.phase,
                        Some(bounded_handoff_recovery_error(&error.to_string())),
                        latest.result,
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
            }
        }
        after = Some(next);
    }
    Ok(recovered)
}

async fn recover_exported_source_writer_cut(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    progress: &mut ryeos_app::worker_handoff::WorkerSessionHandoffProgress,
) -> Result<()> {
    let ledger = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("exported source handoff lost its accounting ledger"))?;
    let transfer = ledger
        .handoff_allowance_transfer(&operation.operation_id)?
        .ok_or_else(|| anyhow::anyhow!("exported source handoff lost its ledger receipt"))?;
    let placement_hash = progress
        .placement_attestation_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("exported source handoff has no target placement"))?;
    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(std::path::Path::new(&operation.source_project_path)),
    )?;
    let loaded_remote =
        crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)?;
    if loaded_remote.config.site_id != operation.target_site_id {
        anyhow::bail!("exported source handoff target configuration changed");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let value = authority
        .cas_store()?
        .get_object(placement_hash)?
        .ok_or_else(|| anyhow::anyhow!("exported source placement attestation is absent"))?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    attestation.verify_with_key(&loaded_remote.config.pinned_signing_key()?)?;
    let placement = ryeos_app::worker_handoff::WorkerPlacementAdmissionEvidence::from_attestation(
        &attestation,
    )?;
    drop(guard);
    if placement.operation_id != operation.operation_id
        || placement.source_placement_thread_id != operation.source_placement_thread_id
        || placement.successor_placement_thread_id != operation.successor_placement_thread_id
        || placement.chain_root_id != operation.chain_root_id
        || placement.target_site_id != operation.target_site_id
        || placement.accounting.target_scope.as_ref() != Some(&transfer.target_scope)
        || placement.accounting.target_cap_usd_nanos != transfer.target_cap_usd_nanos
        || placement.accounting.target_directive_cap_usd_nanos
            != transfer.target_directive_cap_usd_nanos
    {
        anyhow::bail!("exported source allowance contradicts its target placement");
    }
    let session = state
        .state_store
        .dedicated_session(&operation.source_placement_thread_id)?
        .ok_or_else(|| anyhow::anyhow!("exported source session disappeared"))?;
    let _profile = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &session.credential_profile_id,
    )
    .await?;
    let mut root = ryeos_app::hosted_operation::begin_hosted_root_handoff_recovery_async(
        &state.state_store,
        &operation.source_placement_thread_id,
        &operation.operation_id,
    )
    .await?;
    let (source_snapshot, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &operation.chain_root_id,
            &operation.source_placement_thread_id,
        )?
        .ok_or_else(|| anyhow::anyhow!("exported source snapshot disappeared"))?;
    let target_authority = &placement.project_rebind.target_authority;
    let successor = ryeos_app::state_store::NewThreadRecord {
        thread_id: operation.successor_placement_thread_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        kind: source_snapshot.kind_name.clone(),
        item_ref: source_snapshot.item_ref.clone(),
        executor_ref: source_snapshot.executor_ref.clone(),
        launch_mode: source_snapshot.launch_mode.clone(),
        current_site_id: operation.target_site_id.clone(),
        origin_site_id: source_snapshot.origin_site_id.clone(),
        upstream_thread_id: Some(operation.source_placement_thread_id.clone()),
        requested_by: Some(operation.owner_principal.clone()),
        project_root: target_authority
            .project_root_projection()
            .map(std::path::PathBuf::from),
        project_authority: target_authority.clone(),
        base_project_snapshot_hash: target_authority
            .operational_snapshot_projection()
            .map(ToOwned::to_owned),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    let publication = state.state_store.create_remote_adoption_successor(
        &successor,
        &operation.source_placement_thread_id,
        &operation.chain_root_id,
        &ryeos_app::state_store::RemoteAdoptionContinuationAuthority {
            placement_attestation_hash: placement_hash.to_owned(),
            placement,
            source_accounting_transfer: Some(transfer),
            target_node_verifying_key: loaded_remote.config.pinned_signing_key()?,
        },
    )?;
    root.commit();
    let head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .ok_or_else(|| anyhow::anyhow!("recovered source writer cut produced no head"))?;
    progress.phase = ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted;
    progress.writer_grant_hash = Some(publication.writer_grant_hash);
    progress.target_chain_head_hash = Some(head.target_hash.clone());
    progress.validate()?;
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            &job.job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Running,
                phase: progress.phase.as_str().to_owned(),
                roots: None,
                heads: Some(vec![head.target_hash]),
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(progress.to_value()?),
            },
        )
    })?;
    Ok(())
}

async fn recover_pre_cut_source_handoff_abort(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    progress: &ryeos_app::worker_handoff::WorkerSessionHandoffProgress,
) -> Result<bool> {
    if let Some(ledger) = state.accounting.as_ref()
        && ledger.handoff_allowance_exported(&operation.operation_id)?
    {
        anyhow::bail!(
            "source allowance export is already irreversible; resume the exact writer cut instead of aborting"
        );
    }
    let current_placement = state
        .state_store
        .current_chain_placement_thread_id(&operation.chain_root_id)?;
    if current_placement.as_deref() != Some(operation.source_placement_thread_id.as_str()) {
        tracing::error!(
            job_id = %job.job_id,
            "pre-cut worker handoff recovery found another authoritative placement"
        );
        return Ok(false);
    }
    let _root_disposition = ryeos_app::hosted_operation::begin_hosted_root_handoff_recovery_async(
        &state.state_store,
        &operation.source_placement_thread_id,
        &operation.operation_id,
    )
    .await?;
    let current_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .ok_or_else(|| anyhow::anyhow!("pre-cut handoff source head disappeared"))?;
    if current_head.signer != state.identity.fingerprint() {
        anyhow::bail!("pre-cut handoff source head is not locally owned");
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforeAbortPublication,
        operation,
    )?;
    let abort_head_hash =
        if progress.phase == ryeos_app::worker_handoff::WorkerHandoffPhase::AbortAuthorized {
            let expected = progress
                .abort_chain_head_hash
                .clone()
                .ok_or_else(|| anyhow::anyhow!("abort-authorized handoff has no abort head"))?;
            if current_head.target_hash != expected {
                anyhow::bail!("source handoff advanced after its abort authority");
            }
            expected
        } else if current_head.target_hash == operation.source_chain_head_hash {
            let (_, event, observed_head_hash) = state
                .state_store
                .get_authoritative_thread_snapshot_with_last_event(
                    &operation.chain_root_id,
                    &operation.source_placement_thread_id,
                )?
                .ok_or_else(|| anyhow::anyhow!("pre-cut handoff source disappeared"))?;
            if observed_head_hash != operation.source_chain_head_hash {
                anyhow::bail!("pre-cut handoff source head changed before abort");
            }
            if event.and_then(|event| event.event_hash).as_deref()
                != Some(operation.source_last_event_hash.as_str())
            {
                anyhow::bail!("pre-cut handoff source event changed before abort");
            }
            ryeos_app::authoritative_root_fact::append_once(
                state,
                &operation.source_placement_thread_id,
                "worker_session.handoff_aborted",
                &operation.operation_id,
                serde_json::json!({
                    "schema":"ryeos.worker_session_handoff_abort.v1",
                    "operation_id":operation.operation_id,
                    "chain_root_id":operation.chain_root_id,
                    "source_placement_thread_id":operation.source_placement_thread_id,
                    "source_site_id":operation.source_site_id,
                    "target_site_id":operation.target_site_id,
                    "source_chain_head_hash":operation.source_chain_head_hash,
                    "source_last_event_hash":operation.source_last_event_hash,
                }),
            )?;
            state
                .state_store
                .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
                .ok_or_else(|| anyhow::anyhow!("source abort append produced no chain head"))?
                .target_hash
        } else {
            // Cover the crash gap after append and before job-progress update. The
            // exact current head must itself be the immediate abort successor.
            current_head.target_hash.clone()
        };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceAbortPublished,
        operation,
    )?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_app::worker_handoff::validate_handoff_abort_authority(
        &authority.cas_store()?,
        operation,
        &abort_head_hash,
    )?;
    drop(guard);
    let mut abort_progress = progress.clone();
    abort_progress.phase = ryeos_app::worker_handoff::WorkerHandoffPhase::AbortAuthorized;
    abort_progress.abort_chain_head_hash = Some(abort_head_hash.clone());
    abort_progress.validate()?;
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            &job.job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Running,
                phase: abort_progress.phase.as_str().to_owned(),
                roots: None,
                heads: Some(vec![abort_head_hash.clone()]),
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(abort_progress.to_value()?),
            },
        )
    })?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_source_handoff_phase_cut(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceAbortProjected,
        operation,
    )?;

    let report = crate::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(std::path::Path::new(&operation.source_project_path)),
    )?;
    let loaded_remote =
        crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)?;
    if loaded_remote.config.site_id != operation.target_site_id {
        anyhow::bail!("source handoff configured target site changed before abort");
    }
    let client = crate::remote::client::RemoteClient::from_remote_cfg(state, &loaded_remote.config);
    let request = ryeos_app::worker_handoff::WorkerPlacementAbortRequest {
        operation: operation.clone(),
        abort_chain_head_hash: abort_head_hash.clone(),
    };
    let attempt_id = begin_worker_handoff_attempt(
        state,
        &job.job_id,
        "target_abort",
        "source-handoff-recovery",
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let abort_result: Result<ryeos_app::worker_handoff::WorkerPlacementAbortResponse> = async {
        let value = client
            .execute_service_result_with_total_timeout(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_ABORT_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&request)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let terminal_result: ryeos_app::worker_handoff::WorkerPlacementAbortResult =
            serde_json::from_value(value)?;
        validate_and_retain_target_abort_result(
            state,
            &client,
            &loaded_remote.config,
            &job.job_id,
            operation,
            &request,
            terminal_result,
            abort_progress.to_value()?,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
    .await;
    match abort_result {
        Ok(response) => {
            #[cfg(any(test, feature = "handoff-test-support"))]
            reach_source_handoff_phase_cut(
                state,
                ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::SourceBeforeCompletion,
                operation,
            )?;
            settle_worker_handoff_attempt_with_heads(
                state,
                &job.job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Cancelled,
                ryeos_state::SyncJobState::Cancelled,
                "aborted",
                None,
                Some(serde_json::to_value(response)?),
                Some(vec![abort_head_hash.clone()]),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(true)
        }
        Err(error) => {
            settle_worker_handoff_attempt(
                state,
                &job.job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                abort_progress.phase.as_str(),
                Some(bounded_handoff_recovery_error(&error.to_string())),
                Some(abort_progress.to_value()?),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(false)
        }
    }
}

fn bounded_handoff_recovery_error(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    while output.len() > 2_048 {
        output.pop();
    }
    let trimmed = output.trim();
    if trimmed.is_empty() {
        "worker handoff recovery failed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn target_terminal_failure_message(
    failure: &ryeos_app::worker_handoff::WorkerPlacementFailureResponse,
) -> String {
    bounded_handoff_recovery_error(&format!(
        "target placement {} terminalized {} before worker attachment",
        failure.placement_thread_id, failure.terminal_status
    ))
}

fn target_terminal_failure_conflict(
    failure: &ryeos_app::worker_handoff::WorkerPlacementFailureResponse,
) -> HandlerError {
    HandlerError::Conflict(format!(
        "worker handoff {} terminalized {} on the target before worker attachment",
        failure.operation_id, failure.terminal_status
    ))
}

fn begin_worker_handoff_attempt(
    state: &AppState,
    job_id: &str,
    phase: &str,
    worker_id: &str,
) -> Result<String, HandlerError> {
    let attempt_id = format!("worker-handoff-attempt:{}", uuid::Uuid::new_v4());
    state
        .state_store
        .with_state_db(|db| {
            db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
                attempt_id: attempt_id.clone(),
                job_id: job_id.to_owned(),
                worker_id: Some(worker_id.to_owned()),
                phase: phase.to_owned(),
            })?;
            Ok(())
        })
        .map_err(internal)?;
    Ok(attempt_id)
}

fn settle_worker_handoff_attempt(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
) -> Result<(), HandlerError> {
    settle_worker_handoff_attempt_with_heads(
        state,
        job_id,
        attempt_id,
        attempt_state,
        job_state,
        phase,
        error,
        result,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn settle_worker_handoff_attempt_with_heads(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
    heads: Option<Vec<String>>,
) -> Result<(), HandlerError> {
    state
        .state_store
        .with_state_db(|db| {
            let latest = db
                .get_sync_job(job_id)?
                .ok_or_else(|| anyhow::anyhow!("worker handoff job disappeared"))?;
            db.finish_sync_job_attempt_and_update_job(
                attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: attempt_state,
                    phase: phase.to_owned(),
                    error: error.clone(),
                    result: result.clone(),
                },
                job_id,
                &ryeos_state::SyncJobUpdate {
                    state: job_state,
                    phase: phase.to_owned(),
                    roots: None,
                    heads,
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: latest.fetched_hashes,
                    last_error: error,
                    result: result.or(latest.result),
                },
            )
        })
        .map_err(internal)
}

fn verify_target_placement_attestation(
    state: &AppState,
    remote: &crate::remote::config::RemoteConfig,
    prepared: &ryeos_app::worker_handoff::WorkerPlacementPrepareResponse,
) -> Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let value = authority
        .cas_store()?
        .get_object(&prepared.placement_attestation_hash)?
        .ok_or_else(|| anyhow::anyhow!("target placement attestation is absent"))?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    attestation.verify_with_key(&remote.pinned_signing_key()?)?;
    if attestation.issuer != remote.principal_id
        || attestation.is_expired_at(&lillux::time::iso8601_now())?
        || ryeos_app::worker_handoff::WorkerPlacementAdmissionEvidence::from_attestation(
            &attestation,
        )? != prepared.placement
    {
        anyhow::bail!("target placement attestation is not the exact current remote evidence");
    }
    drop(guard);
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    chain_root_id: String,
    idempotency_key: String,
    route_id: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandObservationRequest {
    chain_root_id: String,
    placement_thread_id: String,
    command_sequence: u64,
}

async fn command_observation(
    req: CommandObservationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session =
        owned_session_placement(&state, &ctx, &req.chain_root_id, &req.placement_thread_id)?;
    ryeos_app::dedicated_session_service::command_observation(
        &state,
        &session.placement_thread_id,
        req.command_sequence,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

async fn command(
    req: CommandRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.state == "recovering" {
        return Err(HandlerError::BadRequest(
            "recovering worker executions accept only the runtime-owned reattach route".into(),
        ));
    }
    if req.route_id.is_empty()
        || req.route_id.len() > 256
        || req.route_id.chars().any(char::is_control)
    {
        return Err(HandlerError::BadRequest(
            "worker execution route id is not canonical and bounded".into(),
        ));
    }
    let mut result = ryeos_app::dedicated_session_service::execute_command(
        &state,
        &session.placement_thread_id,
        &req.idempotency_key,
        "route",
        json!({"route_id":req.route_id,"payload":req.payload}),
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let object = result
        .as_object_mut()
        .ok_or_else(|| internal("worker command result is not an object"))?;
    object.insert(
        "chain_root_id".to_owned(),
        Value::String(session.chain_root_id),
    );
    object.insert(
        "placement_thread_id".to_owned(),
        Value::String(session.placement_thread_id),
    );
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListRequest {
    chain_root_id: String,
}

async fn approvals(
    req: ApprovalListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.placement_thread_id)
        .map_err(internal)?
        .into_iter()
        .filter(|approval| approval.state == "pending")
        .map(|approval| {
            Ok(json!({
                "chain_root_id":session.chain_root_id.clone(),
                "placement_thread_id":approval.placement_thread_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "operation_class":approval.operation_class,
                "requested_authority":ryeos_app::dedicated_session_service::public_approval_authority(&approval.requested_authority),
                "state":approval.state,
                "expires_at_ms":approval.expires_at_ms,
                "created_at_ms":approval.created_at_ms,
            }))
        })
        .collect::<Result<Vec<_>, HandlerError>>()?;
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "approvals": approvals,
    }))
}

fn repair_approval_outbox_for_session(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<(), HandlerError> {
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.placement_thread_id)
        .map_err(internal)?;
    for approval in approvals {
        if !matches!(
            approval.state.as_str(),
            "decision_reserved" | "delivery_contacting" | "delivery_unknown"
        ) {
            continue;
        }
        let token = approval
            .reservation_token
            .as_deref()
            .ok_or_else(|| internal("reserved approval has no reservation token"))?;
        let decision_digest = approval
            .decision_digest
            .as_deref()
            .ok_or_else(|| internal("reserved approval has no decision digest"))?;
        let decision = approval
            .decision
            .as_ref()
            .ok_or_else(|| internal("reserved approval has no retained decision"))?;
        if ryeos_state::objects::canonical_value_digest(decision).map_err(internal)?
            != decision_digest
        {
            return Err(internal("reserved approval decision digest mismatch"));
        }
        if decision.get("reservation_token").and_then(Value::as_str) != Some(token) {
            return Err(internal(
                "reserved approval token differs from retained decision",
            ));
        }
        let semantic_decision = decision
            .get("decision")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "accept" | "decline"))
            .ok_or_else(|| internal("reserved approval has no valid semantic decision"))?;
        if find_authoritative_approval_delivery_settlement(
            state,
            session,
            &approval,
            token,
            decision_digest,
        )?
        .is_some()
        {
            state
                .state_store
                .settle_recovered_dedicated_approval_delivery(
                    &session.placement_thread_id,
                    &approval.approval_id,
                    approval.worker_boot_epoch,
                    token,
                    decision_digest,
                )
                .map_err(internal)?;
            continue;
        }
        let root = state
            .state_store
            .get_thread(&session.placement_thread_id)
            .map_err(internal)?
            .ok_or_else(|| internal("hosted execution root thread disappeared"))?;
        if ryeos_app::state_store::is_terminal_status(&root.status) {
            match approval.state.as_str() {
                "decision_reserved" => state
                    .state_store
                    .reconcile_dedicated_approval_stale_epoch(
                        &session.placement_thread_id,
                        &approval.approval_id,
                        approval.worker_boot_epoch,
                    )
                    .map_err(internal)?,
                "delivery_contacting" => state
                    .state_store
                    .reconcile_dedicated_approval_delivery_unknown(
                        &session.placement_thread_id,
                        &approval.approval_id,
                        approval.worker_boot_epoch,
                    )
                    .map_err(internal)?,
                "delivery_unknown" => {}
                other => {
                    return Err(internal(format!(
                        "approval outbox has invalid terminal state `{other}`"
                    )));
                }
            }
            tracing::warn!(
                placement_thread_id = %session.placement_thread_id,
                approval_id = %approval.approval_id,
                "terminal hosted root cannot accept missing historical approval outbox facts"
            );
            continue;
        }
        let decision_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_decision_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "approval_id":approval.approval_id,
            "reservation_token":token,
            "stage":"decision_reserved",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.decision_reserved",
            &decision_operation_id,
            json!({
                "schema":1,
                "origin":"owner_authorized",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "decision_digest":decision_digest,
                "decision_principal":approval.decision_principal,
                "decision":semantic_decision,
            }),
        )?;
        // No worker contact was possible yet. Retain the reservation until
        // worker fencing classifies it as a stale old-epoch decision; calling
        // this delivery-unknown would overstate the evidence boundary.
        if approval.state == "decision_reserved" {
            continue;
        }
        if matches!(
            approval.state.as_str(),
            "delivery_contacting" | "delivery_unknown"
        ) {
            let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "reservation_token":token,
                "stage":"delivery_contacting",
            }))
            .map_err(internal)?;
            append_root_fact_once(
                &state,
                &session,
                "hosted_approval.delivery_contacting",
                &contacting_operation_id,
                json!({
                    "schema":1,
                    "origin":"daemon_reserved_io",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":session.placement_thread_id,
                    "approval_id":approval.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "request_digest":approval.request_digest,
                    "reservation_token":token,
                    "decision_digest":decision_digest,
                }),
            )?;
        }
        if approval.state != "delivery_unknown" {
            state
                .state_store
                .reconcile_dedicated_approval_delivery_unknown(
                    &session.placement_thread_id,
                    &approval.approval_id,
                    approval.worker_boot_epoch,
                )
                .map_err(internal)?;
        }
        let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_delivery_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "approval_id":approval.approval_id,
            "reservation_token":token,
            "stage":"delivery_unknown",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.delivery_unknown",
            &unknown_operation_id,
            json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "approval_id":approval.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":approval.request_digest,
                "reservation_token":token,
                "decision_digest":decision_digest,
            }),
        )?;
    }
    Ok(())
}

/// Repair approval outbox evidence during daemon startup, before public
/// service traffic can race the delivery state machine. Listing approvals is
/// deliberately read-only and never invokes this reconciler.
pub async fn reconcile_approval_outboxes(state: Arc<AppState>) -> anyhow::Result<()> {
    for placement_thread_id in state.state_store.dedicated_approval_outbox_session_ids()? {
        let _delivery_guard = approval_delivery_lock(&placement_thread_id)
            .lock_owned()
            .await;
        let Some(session) = state.state_store.dedicated_session(&placement_thread_id)? else {
            anyhow::bail!("approval outbox references a missing dedicated session");
        };
        let root_operation =
            ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable_async(
                &state.state_store,
                &session.placement_thread_id,
            )
            .await?;
        let _credential_operation = if root_operation.is_some() {
            Some(
                ryeos_app::hosted_operation::acquire_credential_profile_operation(
                    &session.credential_profile_id,
                )
                .await?,
            )
        } else {
            None
        };
        repair_approval_outbox_for_session(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidatePublicationRecovery {
    Published,
    NotPublished,
    Unknown,
}

fn classify_candidate_publication_recovery(
    current: Option<&str>,
    base: &str,
    candidate: &str,
) -> CandidatePublicationRecovery {
    if current == Some(candidate) {
        CandidatePublicationRecovery::Published
    } else if current == Some(base) {
        CandidatePublicationRecovery::NotPublished
    } else {
        CandidatePublicationRecovery::Unknown
    }
}

/// Reconcile the only irreversible candidate-publication crash boundary. The
/// project HEAD is read back under retained project authority before the
/// controller projection is settled. After an authorized possible contact, only the
/// admitted base proves that this operation did not publish. The candidate
/// proves success; a missing or different HEAD is irreducibly ambiguous and is
/// terminalized as unknown rather than reopening an unsafe retry.
pub async fn reconcile_candidate_publications(state: Arc<AppState>) -> anyhow::Result<()> {
    // Qualification journals the exact evaluator adoption before contacting
    // its separate recorded-service root. Startup either observes that exact
    // append-only fact and settles it, or rolls back a complete-replay-proven
    // absence; it never discovers a replacement root through global scans.
    for session in state
        .state_store
        .dedicated_sessions_in_state("qualifying")?
    {
        let _operation_guard = disposition_operation_lock(&session.placement_thread_id)
            .lock_owned()
            .await;
        reconcile_candidate_qualification(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
    }
    for session in state
        .state_store
        .dedicated_sessions_in_state("publishing")?
    {
        let _operation_guard = disposition_operation_lock(&session.placement_thread_id)
            .lock_owned()
            .await;
        let authority = candidate_publication_authority(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let source_candidate = authority.source_candidate_snapshot_hash.as_str();
        let candidate = authority.candidate_snapshot_hash.as_str();
        let base_snapshot_hash = authority.base_snapshot_hash.as_str();
        let principal_key = authority.principal_key.as_str();
        let project_hash = authority.project_hash.as_str();
        let publication_root_id = session
            .candidate_disposition_root_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no publication root"))?;
        let publication_operation_id = session
            .candidate_disposition_operation_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no publication operation"))?;
        verify_candidate_publication_reservation(
            &state,
            &session,
            &authority,
            publication_root_id,
            publication_operation_id,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let published_fact = verify_candidate_publication_result(
            &state,
            &session,
            &authority,
            publication_root_id,
            publication_operation_id,
            "hosted_candidate.published",
            "published",
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let not_applied_fact = verify_candidate_publication_result(
            &state,
            &session,
            &authority,
            publication_root_id,
            publication_operation_id,
            "hosted_candidate.publication_not_applied",
            "not_applied",
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let unknown_fact = verify_candidate_publication_result(
            &state,
            &session,
            &authority,
            publication_root_id,
            publication_operation_id,
            "hosted_candidate.publication_unknown",
            "unknown",
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if usize::from(published_fact) + usize::from(not_applied_fact) + usize::from(unknown_fact)
            > 1
        {
            anyhow::bail!("candidate publication root contains contradictory terminal facts");
        }
        let pinned = state
            .state_store
            .with_state_db(|db| db.pinned_authority())?;
        let _cas_guard = pinned.acquire_shared_guard()?;
        let cas = pinned.cas_store()?;
        if !super::project_apply_snapshot::snapshot_history_contains(
            &cas,
            candidate,
            base_snapshot_hash,
        )? {
            anyhow::bail!("publishing candidate no longer proves descent from its admitted base");
        }
        // A rooted terminal result records the classification made at the
        // irreversible contact boundary. It remains authoritative even if a
        // later independent publication advances HEAD again. Only an
        // interrupted operation with no result fact is classified from the
        // signed current HEAD and its admitted history.
        if published_fact {
            state.state_store.settle_dedicated_candidate_publication(
                &session.placement_thread_id,
                source_candidate,
                publication_root_id,
                publication_operation_id,
                &format!("published:{candidate}"),
            )?;
        } else if not_applied_fact {
            state
                .state_store
                .fail_dedicated_candidate_disposition(&session.placement_thread_id, "publishing")?;
        } else if unknown_fact {
            state.state_store.settle_dedicated_candidate_publication(
                &session.placement_thread_id,
                source_candidate,
                publication_root_id,
                publication_operation_id,
                &format!("publication_unknown:{candidate}"),
            )?;
        } else {
            // The reservation proves that HEAD may already have been
            // contacted. Keep the exact publication root appendable while we
            // classify that boundary, then make the recovered classification
            // durable before changing the mutable session projection. A
            // terminal root without a result fact cannot safely be inferred
            // from whatever HEAD happens to contain later.
            let _publication_operation =
                ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable_async(
                    &state.state_store,
                    publication_root_id,
                )
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "candidate publication root is terminal without a rooted result"
                    )
                })?;
            let current = state
                .state_store
                .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))?;
            let recovery = classify_candidate_publication_recovery(
                current.as_deref(),
                base_snapshot_hash,
                candidate,
            );
            let (event_type, outcome) = match recovery {
                CandidatePublicationRecovery::Published => {
                    ("hosted_candidate.published", "published")
                }
                CandidatePublicationRecovery::NotPublished => {
                    ("hosted_candidate.publication_not_applied", "not_applied")
                }
                CandidatePublicationRecovery::Unknown => {
                    ("hosted_candidate.publication_unknown", "unknown")
                }
            };
            ryeos_app::authoritative_root_fact::append_once(
                &state,
                publication_root_id,
                event_type,
                publication_operation_id,
                candidate_publication_result_payload(
                    &session,
                    &authority,
                    publication_root_id,
                    publication_operation_id,
                    outcome,
                    true,
                ),
            )?;
            if !verify_candidate_publication_result(
                &state,
                &session,
                &authority,
                publication_root_id,
                publication_operation_id,
                event_type,
                outcome,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            {
                anyhow::bail!("candidate publication recovery result was not rooted");
            }
            match recovery {
                CandidatePublicationRecovery::Published => {
                    state.state_store.settle_dedicated_candidate_publication(
                        &session.placement_thread_id,
                        source_candidate,
                        publication_root_id,
                        publication_operation_id,
                        &format!("published:{candidate}"),
                    )?;
                }
                CandidatePublicationRecovery::NotPublished => {
                    state.state_store.fail_dedicated_candidate_disposition(
                        &session.placement_thread_id,
                        "publishing",
                    )?;
                }
                CandidatePublicationRecovery::Unknown => {
                    state.state_store.settle_dedicated_candidate_publication(
                        &session.placement_thread_id,
                        source_candidate,
                        publication_root_id,
                        publication_operation_id,
                        &format!("publication_unknown:{candidate}"),
                    )?;
                }
            }
        }
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
    }
    // Discard has no external side effect. Its append-only owner fact is the
    // commit boundary, so an interrupted projection settlement is completed
    // deterministically on startup instead of leaving `discarding` stranded.
    for session in state
        .state_store
        .dedicated_sessions_in_state("discarding")?
    {
        let _operation_guard = disposition_operation_lock(&session.placement_thread_id)
            .lock_owned()
            .await;
        reconcile_candidate_discard(&state, &session)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolveRequest {
    chain_root_id: String,
    approval_id: String,
    request_digest: String,
    accept: bool,
}

async fn resolve_approval(
    req: ApprovalResolveRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial_session = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial_session.placement_thread_id.clone();
    let _delivery_guard = approval_delivery_lock(&placement_thread_id)
        .lock_owned()
        .await;
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &placement_thread_id,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let _credential_guard = ryeos_app::hosted_operation::acquire_credential_profile_causal_contact(
        &initial_session.credential_profile_id,
        &placement_thread_id,
    )
    .await
    .map_err(internal)?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while approval delivery was reserved".into(),
        ));
    }
    let approval = state
        .state_store
        .pending_dedicated_session_approvals(&placement_thread_id)
        .map_err(internal)?
        .into_iter()
        .find(|approval| approval.approval_id == req.approval_id && approval.state == "pending")
        .ok_or(HandlerError::NotFound)?;
    if session.worker_boot_epoch != Some(approval.worker_boot_epoch) {
        return Err(HandlerError::BadRequest(
            "approval belongs to a stale worker epoch".into(),
        ));
    }
    if approval.request_digest != req.request_digest {
        return Err(HandlerError::BadRequest(
            "approval request digest does not match pending authority".into(),
        ));
    }
    if req.accept
        && approval
            .requested_authority
            .get("accept_allowed")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(HandlerError::BadRequest(
            "the admitted structured-session contract does not allow accepting this authority delta".into(),
        ));
    }
    let request_id = approval
        .requested_authority
        .get("request_id")
        .cloned()
        .ok_or_else(|| internal("pending approval has no upstream request id"))?;
    let adapter_decision = if req.accept { "accept" } else { "decline" };
    let reservation_token = ryeos_app::thread_lifecycle::new_thread_id();
    let decision = json!({
        "kind":"approval_decision",
        "request_id":request_id,
        "request_digest":req.request_digest,
        "decision":adapter_decision,
        "reservation_token":reservation_token,
    });
    let decision_digest =
        ryeos_state::objects::canonical_value_digest(&decision).map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_session_approval_decision(
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &req.request_digest,
            &ctx.fingerprint,
            &decision,
            &decision_digest,
            &reservation_token,
        )
        .map_err(internal)?;
    let decision_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_decision_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"decision_reserved",
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.decision_reserved",
        &decision_operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "decision_digest":decision_digest,
            "decision_principal":ctx.fingerprint,
            "decision":if req.accept { "accept" } else { "decline" },
        }),
    )?;
    let contacting_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_contacting",
    }))
    .map_err(internal)?;
    // The root chain receives the exact possible-delivery boundary before
    // SQLite advances to `delivery_contacting`. A crash between these steps
    // leaves a retryable reservation whose repeated fact append is
    // idempotent; advancing SQLite first could strand delivery-unknown state
    // after the root had become terminal, with no authoritative testimony.
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.delivery_contacting",
        &contacting_operation_id,
        json!({
            "schema":1,
            "origin":"daemon_reserved_io",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "reservation_token":reservation_token,
            "decision_digest":decision_digest,
        }),
    )?;
    state
        .state_store
        .mark_dedicated_approval_delivery_contacting(
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &reservation_token,
            &decision_digest,
        )
        .map_err(internal)?;
    let registry = Arc::clone(&state.persistent_sessions);
    let delivery_placement_thread_id = placement_thread_id.clone();
    let delivery = tokio::task::spawn_blocking(move || {
        registry.execute_exclusive_control(&delivery_placement_thread_id, decision)
    })
    .await
    .map_err(internal)?;
    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(error) => {
            state
                .state_store
                .mark_dedicated_approval_delivery_unknown(
                    &placement_thread_id,
                    &req.approval_id,
                    approval.worker_boot_epoch,
                    &reservation_token,
                    &decision_digest,
                )
                .map_err(internal)?;
            let cleanup_state = state
                .persistent_sessions
                .take_exclusive_failure_cleanup_state_or_unproved(&placement_thread_id)
                .map_err(internal)?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| internal("approval contact failure has no worker identity"))?;
            state
                .state_store
                .fence_abandoned_worker_process(
                    worker_instance_id,
                    &placement_thread_id,
                    approval.worker_boot_epoch,
                    cleanup_state,
                )
                .map_err(internal)?;
            let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
                "schema":"ryeos.hosted_approval_delivery_fact.v1",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":placement_thread_id,
                "approval_id":req.approval_id,
                "reservation_token":reservation_token,
                "stage":"delivery_unknown",
            }))
            .map_err(internal)?;
            append_root_fact_once(
                &state,
                &session,
                "hosted_approval.delivery_unknown",
                &unknown_operation_id,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":placement_thread_id,
                    "approval_id":req.approval_id,
                    "worker_boot_epoch":approval.worker_boot_epoch,
                    "request_digest":req.request_digest,
                    "reservation_token":reservation_token,
                    "decision_digest":decision_digest,
                }),
            )?;
            return Err(HandlerError::BadRequest(format!(
                "approval delivery outcome is unknown and will not be retried: {error}"
            )));
        }
    };
    let exact_expiry = delivery.as_object().is_some_and(|object| {
        object.len() == 4
            && object.get("resolved").and_then(Value::as_bool) == Some(false)
            && object.get("outcome").and_then(Value::as_str) == Some("expired")
            && object.get("request_id") == Some(&request_id)
            && object.get("request_digest").and_then(Value::as_str)
                == Some(req.request_digest.as_str())
    });
    if exact_expiry {
        let projected = ryeos_app::dedicated_session_service::wait_for_exact_approval_state(
            &state,
            &placement_thread_id,
            &req.approval_id,
            approval.worker_boot_epoch,
            &req.request_digest,
            &reservation_token,
            &decision_digest,
            "expired",
            lillux::time::Duration::from_secs(30),
        )
        .await
        .map_err(internal)?;
        if projected {
            return Err(HandlerError::BadRequest(
                "approval expired before the owner decision reached the worker".into(),
            ));
        }
        let cleanup_state = match state
            .persistent_sessions
            .retire_exclusive(&placement_thread_id)
            .map_err(internal)?
        {
            ExclusiveRetirementOutcome::Reaped => "reaped",
            ExclusiveRetirementOutcome::Unproved => "unproved",
            ExclusiveRetirementOutcome::Reserved | ExclusiveRetirementOutcome::Absent => {
                return Err(internal(
                    "expiry result without its authoritative observation lost worker ownership",
                ));
            }
        };
        state
            .state_store
            .mark_dedicated_approval_delivery_unknown(
                &placement_thread_id,
                &req.approval_id,
                approval.worker_boot_epoch,
                &reservation_token,
                &decision_digest,
            )
            .map_err(internal)?;
        let worker_instance_id = session
            .worker_instance_id
            .as_deref()
            .ok_or_else(|| internal("approval expiry contradiction has no worker identity"))?;
        state
            .state_store
            .fence_abandoned_worker_process(
                worker_instance_id,
                &placement_thread_id,
                approval.worker_boot_epoch,
                cleanup_state,
            )
            .map_err(internal)?;
        let unknown_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_approval_delivery_fact.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "reservation_token":reservation_token,
            "stage":"delivery_unknown",
        }))
        .map_err(internal)?;
        append_root_fact_once(
            &state,
            &session,
            "hosted_approval.delivery_unknown",
            &unknown_operation_id,
            json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":placement_thread_id,
                "approval_id":req.approval_id,
                "worker_boot_epoch":approval.worker_boot_epoch,
                "request_digest":req.request_digest,
                "reservation_token":reservation_token,
                "decision_digest":decision_digest,
            }),
        )?;
        return Err(HandlerError::BadRequest(
            "worker reported approval expiry without durable expiry testimony; the worker was retired and delivery remains unknown".into(),
        ));
    }
    let settled_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_delivery_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":placement_thread_id,
        "approval_id":req.approval_id,
        "reservation_token":reservation_token,
        "stage":"delivery_settled",
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_approval.delivery_settled",
        &settled_operation_id,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "approval_id":req.approval_id,
            "worker_boot_epoch":approval.worker_boot_epoch,
            "request_digest":req.request_digest,
            "reservation_token":reservation_token,
            "decision_digest":decision_digest,
            "delivery_digest":ryeos_state::objects::canonical_value_digest(&delivery).map_err(internal)?,
        }),
    )?;
    if find_authoritative_approval_delivery_settlement(
        &state,
        &session,
        &approval,
        &reservation_token,
        &decision_digest,
    )?
    .is_none()
    {
        return Err(internal(
            "approval settlement fact disappeared after authoritative append",
        ));
    }
    if let Err(error) = state.state_store.settle_dedicated_approval_delivery(
        &placement_thread_id,
        &req.approval_id,
        approval.worker_boot_epoch,
        &reservation_token,
        &decision_digest,
    ) {
        // The authoritative root already proves a correlated response. Do not
        // downgrade that evidence to unknown merely because its rebuildable
        // projection failed to advance.
        return Err(internal(error));
    }
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "decision": if req.accept { "accepted" } else { "denied" },
        "delivery_state": "settled",
        "delivery": delivery,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminateRequest {
    chain_root_id: String,
    reason: String,
    #[serde(default)]
    completion: Option<ryeos_app::dedicated_session_service::HostedCommandCompletionFence>,
}

async fn terminate(
    req: TerminateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    if !matches!(req.reason.as_str(), "completed" | "cancelled") {
        return Err(HandlerError::BadRequest(
            "terminal reason must be completed or cancelled".into(),
        ));
    }
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    ryeos_app::dedicated_session_service::terminate_session(
        &state,
        &session.placement_thread_id,
        &req.reason,
        req.completion.as_ref(),
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    chain_root_id: String,
    expected_previous_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateCandidateRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartCandidateEvaluationRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
    evaluator_item_ref: String,
    #[serde(default)]
    evaluator_parameters: Value,
    #[serde(default)]
    evaluator_ref_bindings: BTreeMap<String, String>,
    /// Required when the evaluated generation was produced by a separate
    /// candidate-integration operation. Direct worker candidates omit it.
    #[serde(default)]
    integration: Option<CandidateIntegrationCoordinate>,
    launch_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateIntegrationCoordinate {
    chain_root_id: String,
    terminal_thread_id: String,
    capsule_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartCandidateIntegrationRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
    accepted_evaluation_hash: String,
    authoring_wrapper_item_ref: String,
    #[serde(default)]
    authoring_parameters: Value,
    #[serde(default)]
    authoring_ref_bindings: BTreeMap<String, String>,
    launch_id: String,
}

struct CandidateOperationLaunchRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
    item_ref: String,
    parameters: Value,
    ref_bindings: BTreeMap<String, String>,
    launch_id: String,
    pre_reserved_thread_id: Option<String>,
    purpose: ryeos_app::thread_lifecycle::CandidateOperationPurpose,
    integration: Option<CandidateIntegrationCoordinate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifyCandidateRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
    candidate_validation_hash: String,
    evaluator_chain_root_id: String,
    evaluator_terminal_thread_id: String,
    evaluator_capsule_hash: String,
    evaluator_item_ref: String,
    evaluator_definition_digest: String,
    evaluator_parameters_digest: String,
    #[serde(default)]
    integration: Option<CandidateIntegrationCoordinate>,
    /// One pre-minted launch coordinate reserved by accepted source-candidate
    /// testimony for its sole integration attempt. Final integrated-candidate
    /// qualification omits it.
    #[serde(default)]
    integration_launch_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateEvaluationResult {
    schema_version: u32,
    candidate_snapshot_hash: String,
    base_snapshot_hash: String,
    accepted: bool,
    evidence: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateQualificationReservation {
    schema: String,
    previous_candidate_evaluation_hash: Option<String>,
    candidate_evaluation_hash: String,
    candidate_evaluation: Value,
    qualification_root_id: String,
    qualification_operation_id: String,
}

fn authorize_candidate_operation_refs(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<(), HandlerError> {
    let mut refs = Vec::with_capacity(ref_bindings.len() + 1);
    refs.push(item_ref);
    refs.extend(ref_bindings.values().map(String::as_str));
    for item_ref in refs {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
            .map_err(|error| HandlerError::BadRequest(format!("invalid evaluator ref: {error}")))?;
        let capability = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        state
            .authorizer
            .authorize(
                &ctx.scopes,
                &ryeos_runtime::authorizer::AuthorizationPolicy::require(&capability),
            )
            .map_err(|_| {
                HandlerError::Forbidden(format!(
                    "candidate operation requires capability `{capability}`"
                ))
            })?;
    }
    Ok(())
}

fn candidate_evaluation_source_ready(
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_snapshot_hash: &str,
    candidate_validation_hash: &str,
) -> bool {
    let owner_candidate = matches!(session.state.as_str(), "frozen" | "publish_ready")
        && session.publication_result.as_deref() == Some("retained");
    let bounded_candidate = matches!(session.state.as_str(), "terminal" | "publish_ready")
        && session.candidate_disposition
            == ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        && session.publication_result.as_deref() == Some("retained_for_review")
        && session.terminal_reason.as_deref() == Some("completed")
        && session.completion_fence.is_some();
    (owner_candidate || bounded_candidate)
        && session.candidate_snapshot_hash.as_deref() == Some(candidate_snapshot_hash)
        && session.candidate_validation_hash.as_deref() == Some(candidate_validation_hash)
}

async fn start_candidate_evaluation(
    req: StartCandidateEvaluationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    start_candidate_operation(
        CandidateOperationLaunchRequest {
            chain_root_id: req.chain_root_id,
            candidate_snapshot_hash: req.candidate_snapshot_hash,
            candidate_validation_hash: req.candidate_validation_hash,
            item_ref: req.evaluator_item_ref,
            parameters: req.evaluator_parameters,
            ref_bindings: req.evaluator_ref_bindings,
            launch_id: req.launch_id,
            pre_reserved_thread_id: None,
            purpose: ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate,
            integration: req.integration,
        },
        ctx,
        state,
    )
    .await
}

async fn start_candidate_integration(
    req: StartCandidateIntegrationRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate integration was reserved".into(),
        ));
    }
    let evaluation = session.candidate_evaluation.as_ref().ok_or_else(|| {
        HandlerError::BadRequest(
            "candidate integration requires accepted independent evaluator testimony".into(),
        )
    })?;
    if session.state != "publish_ready"
        || session.candidate_evaluation_hash.as_deref()
            != Some(req.accepted_evaluation_hash.as_str())
        || ryeos_state::objects::canonical_value_digest(evaluation).map_err(internal)?
            != req.accepted_evaluation_hash
        || evaluation
            .pointer("/candidate/candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(req.candidate_snapshot_hash.as_str())
        || evaluation
            .pointer("/result/accepted")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(HandlerError::BadRequest(
            "candidate integration identity differs from its accepted evaluator testimony".into(),
        ));
    }
    verify_candidate_evaluation_testimony(
        &state,
        &session,
        &req.accepted_evaluation_hash,
        evaluation,
    )?;
    if evaluation
        .pointer("/qualification/reserved_integration_launch_id")
        .and_then(Value::as_str)
        != Some(req.launch_id.as_str())
    {
        return Err(HandlerError::BadRequest(
            "candidate integration launch differs from its one reserved coordinate".into(),
        ));
    }
    let reserved_thread_id = evaluation
        .pointer("/qualification/reserved_integration_thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "candidate integration qualification has no pre-minted thread coordinate".into(),
            )
        })?
        .to_owned();
    start_candidate_operation(
        CandidateOperationLaunchRequest {
            chain_root_id: req.chain_root_id,
            candidate_snapshot_hash: req.candidate_snapshot_hash,
            candidate_validation_hash: req.candidate_validation_hash,
            item_ref: req.authoring_wrapper_item_ref,
            parameters: req.authoring_parameters,
            ref_bindings: req.authoring_ref_bindings,
            launch_id: req.launch_id,
            pre_reserved_thread_id: Some(reserved_thread_id),
            purpose: ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate {
                accepted_evaluation_hash: req.accepted_evaluation_hash,
            },
            integration: None,
        },
        ctx,
        state,
    )
    .await
}

async fn start_candidate_operation(
    req: CandidateOperationLaunchRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    authorize_candidate_operation_refs(&req.item_ref, &req.ref_bindings, &ctx, &state)?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    let verified_source = match &req.purpose {
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
            verified_candidate_source(
                &state,
                &session,
                &req.candidate_snapshot_hash,
                &req.candidate_validation_hash,
                req.integration.as_ref(),
            )?
        }
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate {
            accepted_evaluation_hash,
        } => {
            if req.integration.is_some()
                || !candidate_evaluation_source_ready(
                    &session,
                    &req.candidate_snapshot_hash,
                    &req.candidate_validation_hash,
                )
                || session.candidate_evaluation_hash.as_deref()
                    != Some(accepted_evaluation_hash.as_str())
            {
                return Err(HandlerError::BadRequest(
                    "candidate integration requires the exact directly retained accepted candidate"
                        .into(),
                ));
            }
            let (base_snapshot_hash, closure_evidence) = frozen_candidate_closure_evidence(
                &state,
                &session,
                &req.candidate_snapshot_hash,
                &req.candidate_validation_hash,
            )?;
            VerifiedCandidateSource {
                base_snapshot_hash,
                closure_evidence,
                integration_evidence: None,
                integration_operation_hash: None,
            }
        }
    };
    let base_snapshot_hash = verified_source.base_snapshot_hash.clone();
    let evaluation_authority = ryeos_app::thread_lifecycle::CandidateEvaluationAuthority {
        schema_version: ryeos_app::thread_lifecycle::CandidateEvaluationAuthority::SCHEMA_VERSION,
        source_chain_root_id: session.chain_root_id.clone(),
        source_placement_thread_id: session.placement_thread_id.clone(),
        owner_principal: session.owner_principal.clone(),
        base_snapshot_hash: base_snapshot_hash.clone(),
        candidate_snapshot_hash: req.candidate_snapshot_hash.clone(),
        candidate_validation_hash: req.candidate_validation_hash.clone(),
        integration_operation_hash: verified_source.integration_operation_hash.clone(),
        integration_launch_id: matches!(
            &req.purpose,
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
        )
        .then(|| req.launch_id.clone()),
        purpose: req.purpose.clone(),
    };
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated candidate source disappeared"))?;
    let (stable_project_identity, original_project_path, source_base_snapshot_hash) = match &source
        .project_authority
    {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path: Some(display_path),
            base_snapshot_hash,
            ..
        } => (
            stable_project_identity.clone(),
            display_path.clone(),
            base_snapshot_hash.clone(),
        ),
        _ => {
            return Err(HandlerError::BadRequest(
                "candidate evaluator requires a pinned source with stable project identity".into(),
            ));
        }
    };
    if source.current_site_id != state.threads.site_id()
        || source_base_snapshot_hash != base_snapshot_hash
        || source.requested_by.as_deref() != Some(session.owner_principal.as_str())
    {
        return Err(HandlerError::BadRequest(
            "candidate evaluator source authority is not local and exact".into(),
        ));
    }

    let integration_head_destination = if matches!(
        &req.purpose,
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
    ) {
        let project_path = original_project_path
            .to_str()
            .ok_or_else(|| internal("candidate project path is not UTF-8"))?;
        let principal_key = ryeos_state::refs::principal_storage_key(&session.owner_principal)
            .map_err(internal)?
            .to_owned();
        let project_hash = ryeos_state::refs::deployed_project_key(project_path);
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
            .map_err(internal)?;
        if current.as_deref() != Some(base_snapshot_hash.as_str()) {
            return Err(HandlerError::BadRequest(format!(
                "candidate integration base is stale: expected HEAD {}, current HEAD is {:?}",
                base_snapshot_hash, current
            )));
        }
        Some((principal_key, project_hash))
    } else {
        None
    };

    let reserved_thread_id = if let Some(reserved_thread_id) = &req.pre_reserved_thread_id {
        let planning = state
            .state_store
            .launch_planning_record_for_owner(&req.launch_id, &session.owner_principal)
            .map_err(internal)?
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "candidate integration lost its qualified launch reservation".into(),
                )
            })?;
        if planning.reserved_thread_id != *reserved_thread_id {
            return Err(HandlerError::BadRequest(
                "candidate integration launch reservation changed its pre-minted thread".into(),
            ));
        }
        match planning.state.as_str() {
            "qualified" => {
                state
                    .state_store
                    .activate_qualified_launch_planning(
                        &req.launch_id,
                        reserved_thread_id,
                        &session.owner_principal,
                    )
                    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
                reserved_thread_id.clone()
            }
            "planning" => reserved_thread_id.clone(),
            "bound" if planning.bound_thread_id.as_deref() == Some(reserved_thread_id.as_str()) => {
                let capsule = state
                    .state_store
                    .admitted_launch_capsule(reserved_thread_id)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        internal("bound candidate integration has no admitted capsule")
                    })?;
                let sealed = ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule)
                    .map_err(internal)?;
                if sealed.candidate_evaluation_authority() != Some(&evaluation_authority) {
                    return Err(internal(
                        "bound candidate integration capsule contradicts its qualified operation",
                    ));
                }
                return Ok(json!({
                    "launch_id":req.launch_id,
                    "thread_id":reserved_thread_id,
                    "chain_root_id":reserved_thread_id,
                    "candidate_operation":evaluation_authority,
                    "idempotent":true,
                }));
            }
            _ => {
                return Err(HandlerError::BadRequest(
                    "candidate integration launch reservation is no longer startable".into(),
                ));
            }
        }
    } else {
        let reserved_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        state
            .state_store
            .reserve_launch_planning_with_id(
                &req.launch_id,
                &reserved_thread_id,
                &session.owner_principal,
            )
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        reserved_thread_id
    };
    let mut launch_guard = CandidateEvaluationLaunchGuard {
        state: (*state).clone(),
        reserved_thread_id: reserved_thread_id.clone(),
        armed: true,
    };

    let base_checkout_id = format!("candidate-operation-base-{reserved_thread_id}");
    let candidate_checkout_id = format!("candidate-operation-view-{reserved_thread_id}");
    let base = crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
        crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
            state: (*state).clone(),
            source: ryeos_executor::execution::project_source::ProjectSource::Snapshot {
                hash: base_snapshot_hash.clone(),
            },
            project_path: original_project_path.clone(),
            principal_id: session.owner_principal.clone(),
            checkout_id: base_checkout_id,
            pinned_realization: Some(
                ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly,
            ),
            normalization:
                crate::routes::response_modes::execute_mode::ProjectRootNormalization::Preserve,
            launch_timings: None,
        },
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let candidate =
        crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
            crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
                state: (*state).clone(),
                source: ryeos_executor::execution::project_source::ProjectSource::Snapshot {
                    hash: req.candidate_snapshot_hash.clone(),
                },
                project_path: original_project_path.clone(),
                principal_id: session.owner_principal.clone(),
                checkout_id: candidate_checkout_id,
                pinned_realization: Some(match &req.purpose {
                    ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
                        ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly
                    }
                    ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. } => {
                        ryeos_executor::execution::project_source::PinnedContextRealization::Cow
                    }
                }),
                normalization: crate::routes::response_modes::execute_mode::ProjectRootNormalization::Preserve,
                launch_timings: None,
            },
        )
        .await
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

    let mut effective_caps = ctx.scopes.clone();
    // Candidate operations never receive ambient live-filesystem publication
    // authority. Integration authoring is separately capsule-scoped to the
    // private COW workspace by runtime.author_item.
    effective_caps.retain(|capability| {
        capability != ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
    });
    effective_caps.sort();
    effective_caps.dedup();
    let base_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
        stable_project_identity.clone(),
        Some(original_project_path.clone()),
        base_snapshot_hash.clone(),
        ryeos_state::objects::PinnedProjectRealization::ReadOnly,
        ryeos_state::objects::EnvironmentAuthority::None,
        effective_caps.clone(),
    )
    .and_then(|authority| {
        authority.with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
    })
    .map_err(internal)?;
    let base_lifeline = base
        .temp_dir
        .clone()
        .ok_or_else(|| internal("candidate evaluator base lost its materialization lease"))?;
    let base_materialization = base
        .pinned_materialization
        .clone()
        .ok_or_else(|| internal("candidate evaluator base has no materialization proof"))?;
    let base_provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
        original_project_path.clone(),
        base.request_engine.clone(),
        base_lifeline,
        base_materialization,
        base_authority,
    )
    .map_err(internal)?;
    let current_site_id = state.threads.site_id().to_string();
    let origin_site_id = source.origin_site_id.clone();
    let requested_by =
        ryeos_engine::contracts::EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
            fingerprint: session.owner_principal.clone(),
            scopes: effective_caps.clone(),
        });
    let base_plan_context = ryeos_engine::contracts::PlanContext {
        requested_by: requested_by.clone(),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: base.effective_path.clone(),
        },
        subject_resolution_authority: base_provenance.subject_resolution_authority(),
        current_site_id: current_site_id.clone(),
        origin_site_id: origin_site_id.clone(),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let base_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &base.request_engine,
        &base_plan_context,
        &base_provenance,
    )
    .map_err(internal)?;
    let evaluation_scope = Arc::new(
        ryeos_app::thread_lifecycle::CandidateEvaluationExecutionScope::admit(
            evaluation_authority.clone(),
            base_plan_context,
            base_binding,
        )
        .map_err(internal)?,
    );
    let candidate_realization = match &req.purpose {
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
            ryeos_state::objects::PinnedProjectRealization::ReadOnly
        }
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. } => {
            let (principal_key, project_hash) = integration_head_destination
                .as_ref()
                .ok_or_else(|| internal("candidate integration lost its HEAD destination"))?;
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication:
                    ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                        principal_key: principal_key.clone(),
                        project_hash: project_hash.clone(),
                        expected_hash: base_snapshot_hash.clone(),
                    },
            }
        }
    };
    let mut candidate_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
        stable_project_identity,
        Some(original_project_path.clone()),
        match &req.purpose {
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
                req.candidate_snapshot_hash.clone()
            }
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. } => {
                base_snapshot_hash.clone()
            }
        },
        candidate_realization,
        ryeos_state::objects::EnvironmentAuthority::None,
        effective_caps.clone(),
    )
    .and_then(|authority| {
        authority.with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
    })
    .map_err(internal)?;
    if matches!(
        &req.purpose,
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
    ) {
        candidate_authority = candidate_authority
            .transition_operational_generation(
                ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                    result_snapshot_hash: &req.candidate_snapshot_hash,
                },
            )
            .map_err(internal)?;
    }
    let candidate_lifeline = candidate
        .temp_dir
        .clone()
        .ok_or_else(|| internal("candidate evaluator view lost its materialization lease"))?;
    let candidate_materialization = candidate
        .pinned_materialization
        .clone()
        .ok_or_else(|| internal("candidate evaluator view has no materialization proof"))?;
    let candidate_provenance =
        ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
            original_project_path,
            base.request_engine.clone(),
            candidate_lifeline,
            candidate_materialization,
            candidate_authority,
        )
        .and_then(|provenance| provenance.with_candidate_evaluation_scope(evaluation_scope))
        .map_err(internal)?;
    let parsed_item_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&req.item_ref)
        .map_err(HandlerError::BadRequest)?;
    let preflight = crate::routes::launch::preflight_dispatch_launch_off_thread(
        crate::routes::launch::OwnedDispatchPreflight {
            state: (*state).clone(),
            item_ref: parsed_item_ref.clone(),
            project_path: candidate.effective_path.clone(),
            request_engine: base.request_engine.clone(),
            provenance: candidate_provenance.clone(),
            parameters: req.parameters.clone(),
            ref_bindings: req.ref_bindings.clone(),
            product_selections: Vec::new(),
            principal_id: session.owner_principal.clone(),
            principal_scopes: effective_caps.clone(),
            origin_site_id: origin_site_id.clone(),
            call: None,
            launch_mode: "detached".into(),
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            launch_timings: None,
        },
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let class_allowed = match &req.purpose {
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
            preflight.class.produces_admitted_launch_capsule()
        }
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. } => {
            preflight.class == ryeos_executor::dispatch::RootDispatchClass::TerminalSubprocess
        }
    };
    if !class_allowed {
        return Err(HandlerError::BadRequest(
            "candidate operation does not resolve to its required capsule-producing root class"
                .into(),
        ));
    }
    let root_admission = preflight
        .root_admission
        .ok_or_else(|| internal("candidate evaluator preflight returned no root admission"))?;
    let handler_context = ctx
        .narrowed_for_execution(effective_caps.clone(), &current_site_id, &origin_site_id)
        .map_err(internal)?;
    let lifecycle_authority = ryeos_state::objects::ExecutionLifecycleAuthority {
        ownership: ryeos_state::objects::ExecutionOwnershipAuthority::DaemonOwned,
        recovery: ryeos_state::objects::ExecutionRecoveryAuthority::RestartRecoverable,
    };
    let mut options = crate::routes::launch::DispatchLaunchOptions::admitted(
        root_admission,
        preflight.root_dispatch_evidence,
        &candidate.effective_path,
        req.ref_bindings.clone(),
        Vec::new(),
        lifecycle_authority,
        Some(handler_context),
    )
    .map_err(internal)?;
    options.launch_mode = "detached".into();
    let (mut task, ready) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
        &state,
        parsed_item_ref,
        req.parameters,
        session.owner_principal.clone(),
        effective_caps,
        reserved_thread_id.clone(),
        candidate_provenance,
        options,
    );
    launch_guard.armed = false;
    let thread_id = tokio::select! {
        readiness = ready => match readiness {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(failure)) => return Err(HandlerError::Structured {
                code: failure.code,
                status: failure.status,
                body: failure.body,
            }),
            Err(_) => match task.await {
                Ok(Err(error)) => return Err(HandlerError::BadRequest(error.to_string())),
                Ok(Ok(())) => return Err(internal("candidate evaluator completed without launch handoff")),
                Err(error) => return Err(internal(error)),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => return Err(HandlerError::BadRequest(error.to_string())),
            Ok(Ok(())) => return Err(internal("candidate evaluator completed without launch handoff")),
            Err(error) => return Err(internal(error)),
        },
    };
    if thread_id != reserved_thread_id {
        return Err(internal(
            "candidate evaluator launch changed its reserved root identity",
        ));
    }
    Ok(json!({
        "launch_id":req.launch_id,
        "thread_id":thread_id,
        "chain_root_id":thread_id,
        "candidate_operation":evaluation_authority,
    }))
}

fn frozen_candidate_closure_evidence(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_snapshot_hash: &str,
    candidate_validation_hash: &str,
) -> Result<(String, Value), HandlerError> {
    if candidate_validation_identity(candidate_snapshot_hash)? != candidate_validation_hash {
        return Err(HandlerError::BadRequest(
            "candidate validation identity is not canonical for its snapshot".into(),
        ));
    }
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated placement disappeared from its authoritative chain"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        ..
    } = source.project_authority
    else {
        return Err(internal("dedicated candidate authority is not pinned"));
    };
    let cas_read = state.acquire_cas_read().map_err(internal)?;
    let cas = cas_read.cas();
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [candidate_snapshot_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if !closure.is_complete() || !closure.large_object_hashes.is_empty() {
        return Err(HandlerError::BadRequest(
            "candidate snapshot closure is incomplete or unsupported".into(),
        ));
    }
    if !super::project_apply_snapshot::snapshot_history_contains(
        cas,
        candidate_snapshot_hash,
        &base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let candidate = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        cas,
        candidate_snapshot_hash,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let base = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        cas,
        &base_snapshot_hash,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let mut paths = BTreeSet::new();
    paths.extend(candidate.tree().tree().files.keys().cloned());
    paths.extend(base.tree().tree().files.keys().cloned());
    let changed_paths = paths
        .into_iter()
        .filter(|path| {
            candidate.tree().tree().files.get(path) != base.tree().tree().files.get(path)
        })
        .collect::<Vec<_>>();
    let changed_paths_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&changed_paths).map_err(internal)?,
    )
    .map_err(internal)?;
    let evidence = json!({
        "schema":"ryeos.hosted_candidate_closure_and_base_validation.v2",
        "checks":{
            "canonical_snapshot_closure":true,
            "base_ancestry":true,
        },
        "candidate_snapshot_hash":candidate_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
        "base_snapshot_hash":base_snapshot_hash,
        "candidate_tree_hash":candidate.snapshot().project_tree_hash,
        "base_tree_hash":base.snapshot().project_tree_hash,
        "candidate_policy_hash":candidate.snapshot().effective_policy_hash,
        "changed_path_count":changed_paths.len(),
        "changed_paths_digest":changed_paths_digest,
        "object_count":closure.object_hashes.len(),
        "blob_count":closure.blob_hashes.len(),
    });
    Ok((base_snapshot_hash, evidence))
}

fn candidate_validation_identity(candidate_snapshot_hash: &str) -> Result<String, HandlerError> {
    ryeos_app::thread_lifecycle::candidate_validation_identity(candidate_snapshot_hash)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))
}

struct VerifiedCandidateSource {
    base_snapshot_hash: String,
    closure_evidence: Value,
    integration_evidence: Option<Value>,
    integration_operation_hash: Option<String>,
}

/// Re-prove the immutable evaluator and the owner-authorized adoption fact
/// behind one mutable dedicated-session projection. Publication and recovery
/// call this every time; SQLite testimony alone is never publication
/// authority.
fn verify_candidate_evaluation_testimony(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_evaluation_hash: &str,
    evaluation: &Value,
) -> Result<(), HandlerError> {
    if ryeos_state::objects::canonical_value_digest(evaluation).map_err(internal)?
        != candidate_evaluation_hash
        || evaluation.get("schema").and_then(Value::as_str)
            != Some("ryeos.hosted_candidate_evaluation.v1")
        || evaluation
            .pointer("/result/accepted")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(internal(
            "retained candidate evaluation has invalid identity or is not accepted",
        ));
    }
    let candidate_snapshot_hash = evaluation
        .pointer("/candidate/candidate_snapshot_hash")
        .and_then(Value::as_str)
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| internal("candidate evaluation has no canonical candidate identity"))?;
    let base_snapshot_hash = evaluation
        .pointer("/candidate/base_snapshot_hash")
        .and_then(Value::as_str)
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| internal("candidate evaluation has no canonical base identity"))?;
    let qualification_root_id = evaluation
        .pointer("/qualification/root_thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("candidate evaluation has no qualification root"))?;
    if qualification_root_id == session.chain_root_id {
        return Err(internal(
            "candidate qualification fact cannot reopen the terminal worker root",
        ));
    }
    let reserved_integration_launch_id = evaluation
        .pointer("/qualification/reserved_integration_launch_id")
        .and_then(Value::as_str);
    let reserved_integration_thread_id = evaluation
        .pointer("/qualification/reserved_integration_thread_id")
        .and_then(Value::as_str);
    if reserved_integration_launch_id.is_some() != reserved_integration_thread_id.is_some()
        || reserved_integration_launch_id
            .is_some_and(|launch_id| !ryeos_app::state_store::is_canonical_launch_id(launch_id))
        || (evaluation
            .get("integration")
            .is_some_and(|value| !value.is_null())
            && reserved_integration_launch_id.is_some())
    {
        return Err(internal(
            "candidate qualification carries an invalid integration reservation",
        ));
    }
    if let (Some(launch_id), Some(thread_id)) = (
        reserved_integration_launch_id,
        reserved_integration_thread_id,
    ) {
        let planning = state
            .state_store
            .launch_planning_record_for_owner(launch_id, &session.owner_principal)
            .map_err(internal)?
            .ok_or_else(|| {
                internal("qualified integration launch lost its durable planning reservation")
            })?;
        if planning.reserved_thread_id != thread_id
            || !matches!(
                planning.state.as_str(),
                "qualified" | "planning" | "bound" | "cancelled" | "failed" | "expired"
            )
        {
            return Err(internal(
                "qualified integration launch planning contradicts its pre-minted thread",
            ));
        }
    }

    let evaluator_chain_root_id = evaluation
        .pointer("/evaluator/chain_root_id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("candidate evaluation has no evaluator chain root"))?;
    let evaluator_terminal_thread_id = evaluation
        .pointer("/evaluator/terminal_thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("candidate evaluation has no evaluator terminal"))?;
    let evaluator_capsule_hash = evaluation
        .pointer("/evaluator/capsule_hash")
        .and_then(Value::as_str)
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| internal("candidate evaluation has no canonical evaluator capsule"))?;
    if evaluator_chain_root_id == session.chain_root_id
        || qualification_root_id == evaluator_chain_root_id
    {
        return Err(internal(
            "candidate evaluation does not name independent operation roots",
        ));
    }

    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("candidate source root disappeared"))?;
    let source_project_identity = match &source.project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            base_snapshot_hash: source_base,
            ..
        } if source_base == base_snapshot_hash => stable_project_identity,
        _ => {
            return Err(internal(
                "candidate evaluation base differs from source project authority",
            ));
        }
    };

    let (evaluator, evaluator_last_event, evaluator_chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            evaluator_chain_root_id,
            evaluator_terminal_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("candidate evaluator terminal disappeared"))?;
    let evaluator_last_event = evaluator_last_event
        .ok_or_else(|| internal("candidate evaluator has no terminal event"))?;
    let evaluator_last_event_hash = evaluator_last_event
        .event_hash
        .as_deref()
        .ok_or_else(|| internal("candidate evaluator terminal event has no CAS hash"))?;
    if evaluator.thread_id != evaluator_terminal_thread_id
        || evaluator.chain_root_id != evaluator_chain_root_id
        || !candidate_execution_succeeded(&evaluator)
        || evaluator.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || evaluator.current_site_id != source.current_site_id
        || evaluator.origin_site_id != source.origin_site_id
        || evaluator.admitted_launch_capsule_hash.as_deref() != Some(evaluator_capsule_hash)
        || evaluator_last_event.event_type != ryeos_state::event_types::THREAD_COMPLETED
        || evaluator_last_event.chain_root_id != evaluator_chain_root_id
        || evaluator_last_event.thread_id != evaluator_terminal_thread_id
        || evaluator.last_event_hash.as_deref() != Some(evaluator_last_event_hash)
        || u64::try_from(evaluator_last_event.chain_seq).ok() != Some(evaluator.last_chain_seq)
        || u64::try_from(evaluator_last_event.thread_seq).ok() != Some(evaluator.last_thread_seq)
    {
        return Err(internal(
            "retained evaluator coordinate no longer proves exact successful completion",
        ));
    }
    if evaluation
        .pointer("/evaluator/terminal_event_hash")
        .and_then(Value::as_str)
        != Some(evaluator_last_event_hash)
        || evaluation
            .pointer("/evaluator/authoritative_chain_head_hash")
            .and_then(Value::as_str)
            != Some(evaluator_chain_head_hash.as_str())
    {
        return Err(internal(
            "candidate evaluator terminal chain differs from retained testimony",
        ));
    }
    let evaluator_capsule = state
        .state_store
        .admitted_launch_capsule(evaluator_terminal_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("candidate evaluator capsule disappeared"))?;
    if evaluator_capsule.content_hash().map_err(internal)? != evaluator_capsule_hash
        || evaluator_capsule.project_authority != evaluator.project_authority
    {
        return Err(internal(
            "candidate evaluator capsule contradicts its terminal authority",
        ));
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &evaluator_capsule,
        )
        .map_err(internal)?;
    let operation = sealed
        .candidate_evaluation_authority()
        .ok_or_else(|| internal("retained evaluator capsule has no candidate authority"))?;
    let integration_operation_hash = evaluation
        .get("integration_operation_hash")
        .and_then(Value::as_str);
    if operation.source_chain_root_id != session.chain_root_id
        || operation.source_placement_thread_id != session.placement_thread_id
        || operation.owner_principal != session.owner_principal
        || operation.base_snapshot_hash != base_snapshot_hash
        || operation.candidate_snapshot_hash != candidate_snapshot_hash
        || operation.candidate_validation_hash
            != evaluation
                .pointer("/candidate/candidate_validation_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| internal("candidate evaluation has no validation identity"))?
        || operation.integration_operation_hash.as_deref() != integration_operation_hash
        || operation.integration_launch_id.is_some()
        || !matches!(
            &operation.purpose,
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate
        )
    {
        return Err(internal(
            "retained evaluator capsule is sealed to another candidate operation",
        ));
    }
    if sealed.item_ref()
        != evaluation
            .pointer("/evaluator/item_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| internal("candidate evaluation has no evaluator item"))?
        || sealed.effective_definition_digest().as_str()
            != evaluation
                .pointer("/evaluator/effective_definition_digest")
                .and_then(Value::as_str)
                .ok_or_else(|| internal("candidate evaluation has no evaluator definition"))?
        || sealed.admitted_parameters_digest().map_err(internal)?
            != evaluation
                .pointer("/evaluator/parameters_digest")
                .and_then(Value::as_str)
                .ok_or_else(|| internal("candidate evaluation has no parameters digest"))?
    {
        return Err(internal(
            "retained evaluator invocation differs from its testimony",
        ));
    }
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity,
        base_snapshot_hash: evaluator_base,
        snapshot_hash: evaluator_snapshot,
        realization,
        environment,
        capability_ceiling,
        ..
    } = &evaluator.project_authority
    else {
        return Err(internal("retained evaluator authority is not pinned"));
    };
    let realization_kind = match realization {
        ryeos_state::objects::PinnedProjectRealization::ReadOnly => "read_only",
        ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
        } => "cow_discard",
        _ => {
            return Err(internal(
                "retained evaluator has publishing workspace authority",
            ));
        }
    };
    let binding_matches = match (realization, sealed.project_binding_subject_authority()) {
        (
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration { snapshot_hash },
        ) => snapshot_hash.as_str() == candidate_snapshot_hash,
        (
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_engine::contracts::SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash,
                current_operational_generation,
            },
        ) => {
            base_snapshot_hash.as_str() == candidate_snapshot_hash
                && current_operational_generation.as_str() == candidate_snapshot_hash
        }
        _ => false,
    };
    if stable_project_identity != source_project_identity
        || evaluator_base != candidate_snapshot_hash
        || evaluator_snapshot != candidate_snapshot_hash
        || !binding_matches
        || environment != &ryeos_state::objects::EnvironmentAuthority::None
        || capability_ceiling.iter().any(|capability| {
            capability == ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
        })
        || evaluation
            .pointer("/evaluator/candidate_realization")
            .and_then(Value::as_str)
            != Some(realization_kind)
    {
        return Err(internal(
            "retained evaluator no longer proves an exact non-publishing candidate view",
        ));
    }
    let immutable_program = immutable_base_program_evidence(&sealed, base_snapshot_hash)?;
    if evaluation.pointer("/evaluator/immutable_definition_authority") != Some(&immutable_program) {
        return Err(internal(
            "retained evaluator immutable program testimony has diverged",
        ));
    }
    let evaluator_project_authority_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&evaluator.project_authority).map_err(internal)?,
    )
    .map_err(internal)?;
    let evaluator_terminal_snapshot_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&evaluator).map_err(internal)?,
    )
    .map_err(internal)?;
    if evaluation
        .pointer("/evaluator/project_authority_digest")
        .and_then(Value::as_str)
        != Some(evaluator_project_authority_digest.as_str())
        || evaluation
            .pointer("/evaluator/terminal_snapshot_digest")
            .and_then(Value::as_str)
            != Some(evaluator_terminal_snapshot_digest.as_str())
    {
        return Err(internal(
            "retained evaluator authority or terminal snapshot digest has diverged",
        ));
    }
    let evaluator_result_value = evaluator
        .result
        .as_ref()
        .ok_or_else(|| internal("retained successful evaluator has no result"))?;
    let evaluator_result = validate_candidate_evaluation_result(
        evaluator_result_value.clone(),
        candidate_snapshot_hash,
        base_snapshot_hash,
    )?;
    let evaluator_result_digest =
        ryeos_state::objects::canonical_value_digest(evaluator_result_value).map_err(internal)?;
    let evaluator_claim_evidence_digest =
        ryeos_state::objects::canonical_value_digest(&evaluator_result.evidence)
            .map_err(internal)?;
    if !evaluator_result.accepted
        || evaluation
            .pointer("/result/result_digest")
            .and_then(Value::as_str)
            != Some(evaluator_result_digest.as_str())
        || evaluation
            .pointer("/result/claim_evidence_digest")
            .and_then(Value::as_str)
            != Some(evaluator_claim_evidence_digest.as_str())
    {
        return Err(internal(
            "retained evaluator result differs from accepted testimony",
        ));
    }

    // The append-only adoption fact is the durable commit boundary. Do not
    // require this recorded-service root to have reached its later handler
    // terminal event: a daemon crash after fact+projection commit must remain
    // recoverable and idempotent.
    let qualification = state
        .state_store
        .get_thread(qualification_root_id)
        .map_err(internal)?
        .ok_or_else(|| internal("candidate qualification root disappeared"))?;
    if qualification.thread_id != qualification_root_id
        || qualification.chain_root_id != qualification_root_id
        || qualification.item_ref != "service:worker-executions/qualify-candidate"
        || qualification.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || qualification.current_site_id != source.current_site_id
        || qualification.origin_site_id != source.origin_site_id
    {
        return Err(internal(
            "candidate qualification root has contradictory owner authority",
        ));
    }
    let operation_id =
        candidate_qualification_operation_id(qualification_root_id, candidate_evaluation_hash)?;
    let mut expected_fact =
        candidate_qualification_fact_payload(session, candidate_evaluation_hash, evaluation);
    expected_fact
        .as_object_mut()
        .expect("candidate qualification fact is an object")
        .insert("operation_id".into(), Value::String(operation_id.clone()));
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        qualification_root_id,
        "hosted_candidate.qualified",
        &operation_id,
    )
    .map_err(internal)?;
    if fact.count != 1 || fact.payload.as_ref() != Some(&expected_fact) {
        return Err(internal(
            "candidate qualification projection has no exact append-only adoption fact",
        ));
    }
    Ok(())
}

fn candidate_publication_authority(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<CandidatePublicationAuthority, HandlerError> {
    let source_candidate_snapshot_hash = session
        .candidate_snapshot_hash
        .as_ref()
        .ok_or_else(|| HandlerError::BadRequest("session candidate is not ready".into()))?
        .clone();
    let source_candidate_validation_hash = session
        .candidate_validation_hash
        .as_ref()
        .ok_or_else(|| {
            HandlerError::BadRequest("session candidate has no validation identity".into())
        })?
        .clone();
    let candidate_evaluation_hash = session
        .candidate_evaluation_hash
        .as_ref()
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "candidate publication requires accepted immutable evaluator testimony".into(),
            )
        })?
        .clone();
    let evaluation = session
        .candidate_evaluation
        .as_ref()
        .ok_or_else(|| internal("candidate evaluation identity has no retained testimony"))?;
    verify_candidate_evaluation_testimony(state, session, &candidate_evaluation_hash, evaluation)?;
    let candidate_snapshot_hash = evaluation
        .pointer("/candidate/candidate_snapshot_hash")
        .and_then(Value::as_str)
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| internal("candidate evaluation has no canonical candidate identity"))?
        .to_owned();
    let base_snapshot_hash = evaluation
        .pointer("/candidate/base_snapshot_hash")
        .and_then(Value::as_str)
        .filter(|hash| lillux::valid_hash(hash))
        .ok_or_else(|| internal("candidate evaluation has no canonical base identity"))?
        .to_owned();

    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("candidate source root disappeared"))?;
    let (source_base, source_realization) = match &source.project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            realization,
            ..
        } => (base_snapshot_hash, realization),
        _ => return Err(internal("candidate source authority is not pinned")),
    };
    if source_base != &base_snapshot_hash {
        return Err(internal(
            "candidate evaluation base differs from source project authority",
        ));
    }

    let (candidate_validation_hash, principal_key, project_hash) = if let Some(integration) =
        evaluation
            .get("integration")
            .filter(|value| !value.is_null())
    {
        let integration_operation_hash = evaluation
            .get("integration_operation_hash")
            .and_then(Value::as_str)
            .filter(|hash| lillux::valid_hash(hash))
            .ok_or_else(|| internal("integrated evaluation has no operation identity"))?;
        if ryeos_state::objects::canonical_value_digest(integration).map_err(internal)?
            != integration_operation_hash
        {
            return Err(internal(
                "integrated evaluation operation identity is contradictory",
            ));
        }
        let coordinate: CandidateIntegrationCoordinate = serde_json::from_value(
            integration
                .pointer("/operation/coordinate")
                .cloned()
                .ok_or_else(|| internal("integrated evaluation has no operation coordinate"))?,
        )
        .map_err(internal)?;
        let candidate_validation_hash = integration
            .get("result_candidate_validation_hash")
            .and_then(Value::as_str)
            .filter(|hash| lillux::valid_hash(hash))
            .ok_or_else(|| internal("integrated evaluation has no validation identity"))?
            .to_owned();
        if integration
            .get("result_candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(candidate_snapshot_hash.as_str())
        {
            return Err(internal(
                "integrated evaluation candidate identity is contradictory",
            ));
        }
        let observed = verify_candidate_integration_terminal(
            state,
            session,
            &coordinate,
            &candidate_snapshot_hash,
            &candidate_validation_hash,
            &base_snapshot_hash,
        )?;
        if &observed != integration {
            return Err(internal(
                "retained integration testimony differs from authoritative terminal facts",
            ));
        }
        let principal_key = integration
            .pointer("/publication_destination/principal_key")
            .and_then(Value::as_str)
            .ok_or_else(|| internal("integration has no principal HEAD destination"))?
            .to_owned();
        let project_hash = integration
            .pointer("/publication_destination/project_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| internal("integration has no project HEAD destination"))?
            .to_owned();
        (candidate_validation_hash, principal_key, project_hash)
    } else {
        if evaluation
            .get("integration_operation_hash")
            .is_some_and(|value| !value.is_null())
            || candidate_snapshot_hash != source_candidate_snapshot_hash
            || session.candidate_disposition
                == ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        {
            return Err(HandlerError::BadRequest(
                    "bounded retained candidates require isolated integration and re-evaluation before publication"
                        .into(),
                ));
        }
        let ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication:
                ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                    principal_key,
                    project_hash,
                    expected_hash,
                },
        } = source_realization
        else {
            return Err(internal(
                "direct candidate publication has no admitted HEAD destination",
            ));
        };
        if expected_hash != &base_snapshot_hash {
            return Err(internal(
                "direct candidate publication HEAD fence differs from its base",
            ));
        }
        (
            source_candidate_validation_hash,
            principal_key.clone(),
            project_hash.clone(),
        )
    };

    Ok(CandidatePublicationAuthority {
        source_candidate_snapshot_hash,
        candidate_snapshot_hash,
        candidate_validation_hash,
        base_snapshot_hash,
        principal_key,
        project_hash,
        candidate_evaluation_hash,
    })
}

fn accepted_source_evaluation(
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    evaluation_hash: &str,
    candidate_snapshot_hash: &str,
    base_snapshot_hash: &str,
) -> Result<Value, HandlerError> {
    let current = session.candidate_evaluation.as_ref().ok_or_else(|| {
        HandlerError::BadRequest("source candidate has no evaluator testimony".into())
    })?;
    let testimony = if session.candidate_evaluation_hash.as_deref() == Some(evaluation_hash) {
        current
    } else if current
        .pointer("/integration/source_evaluation_hash")
        .and_then(Value::as_str)
        == Some(evaluation_hash)
    {
        current
            .pointer("/integration/source_evaluation")
            .ok_or_else(|| internal("integrated evaluation lost its source testimony"))?
    } else {
        return Err(HandlerError::BadRequest(
            "candidate integration references unretained evaluator testimony".into(),
        ));
    };
    if ryeos_state::objects::canonical_value_digest(testimony).map_err(internal)? != evaluation_hash
        || testimony
            .pointer("/candidate/candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(candidate_snapshot_hash)
        || testimony
            .pointer("/candidate/base_snapshot_hash")
            .and_then(Value::as_str)
            != Some(base_snapshot_hash)
        || testimony
            .pointer("/result/accepted")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(HandlerError::BadRequest(
            "candidate integration source evaluation is not exact accepted testimony".into(),
        ));
    }
    Ok(testimony.clone())
}

fn verify_candidate_integration_terminal(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    coordinate: &CandidateIntegrationCoordinate,
    candidate_snapshot_hash: &str,
    candidate_validation_hash: &str,
    base_snapshot_hash: &str,
) -> Result<Value, HandlerError> {
    if coordinate.chain_root_id == session.chain_root_id
        || !lillux::valid_hash(&coordinate.capsule_hash)
    {
        return Err(HandlerError::BadRequest(
            "candidate integration requires an exact independent terminal-root coordinate".into(),
        ));
    }
    if candidate_validation_identity(candidate_snapshot_hash)? != candidate_validation_hash {
        return Err(HandlerError::BadRequest(
            "integrated candidate validation identity is not canonical".into(),
        ));
    }
    let source_candidate = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no snapshot identity"))?;
    let source_validation = session
        .candidate_validation_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no validation identity"))?;
    let cas_read = state.acquire_cas_read().map_err(internal)?;
    if !super::project_apply_snapshot::snapshot_history_contains(
        cas_read.cas(),
        candidate_snapshot_hash,
        source_candidate,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "integrated candidate does not descend from its exact frozen source candidate".into(),
        ));
    }
    drop(cas_read);
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("candidate source disappeared"))?;
    let (source_project_identity, source_display_path, source_origin_site) =
        match &source.project_authority {
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                stable_project_identity,
                display_path: Some(display_path),
                base_snapshot_hash: source_base,
                ..
            } if source_base == base_snapshot_hash => (
                stable_project_identity,
                display_path,
                source.origin_site_id.as_str(),
            ),
            _ => {
                return Err(HandlerError::BadRequest(
                    "integration source has no exact pinned project authority".into(),
                ));
            }
        };

    let (integration, terminal_event, chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &coordinate.chain_root_id,
            &coordinate.terminal_thread_id,
        )
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    let terminal_event = terminal_event
        .ok_or_else(|| HandlerError::BadRequest("integration has no terminal event".into()))?;
    let terminal_event_hash = terminal_event
        .event_hash
        .as_deref()
        .ok_or_else(|| internal("integration terminal event has no CAS hash"))?;
    if integration.thread_id != coordinate.terminal_thread_id
        || integration.chain_root_id != coordinate.chain_root_id
        || !candidate_execution_succeeded(&integration)
        || integration.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || integration.current_site_id != source.current_site_id
        || integration.origin_site_id != source_origin_site
        || integration.result_project_snapshot_hash.as_deref() != Some(candidate_snapshot_hash)
        || terminal_event.event_type != ryeos_state::event_types::THREAD_COMPLETED
        || terminal_event.chain_root_id != coordinate.chain_root_id
        || terminal_event.thread_id != coordinate.terminal_thread_id
        || integration.last_event_hash.as_deref() != Some(terminal_event_hash)
        || u64::try_from(terminal_event.chain_seq).ok() != Some(integration.last_chain_seq)
        || u64::try_from(terminal_event.thread_seq).ok() != Some(integration.last_thread_seq)
    {
        return Err(HandlerError::BadRequest(
            "candidate integration terminal does not prove exact successful completion".into(),
        ));
    }
    if integration.admitted_launch_capsule_hash.as_deref() != Some(coordinate.capsule_hash.as_str())
    {
        return Err(HandlerError::BadRequest(
            "candidate integration capsule differs from its requested coordinate".into(),
        ));
    }
    let capsule = state
        .state_store
        .admitted_launch_capsule(&coordinate.terminal_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("candidate integration capsule disappeared"))?;
    if capsule.content_hash().map_err(internal)? != coordinate.capsule_hash
        || capsule.project_authority != integration.project_authority
    {
        return Err(internal(
            "candidate integration capsule contradicts its terminal project authority",
        ));
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &capsule,
        )
        .map_err(internal)?;
    let operation = sealed.candidate_evaluation_authority().ok_or_else(|| {
        HandlerError::BadRequest(
            "integration was not launched through candidate-integration authority".into(),
        )
    })?;
    let accepted_evaluation_hash = match &operation.purpose {
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate {
            accepted_evaluation_hash,
        } => accepted_evaluation_hash,
        ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate => {
            return Err(HandlerError::BadRequest(
                "candidate integration coordinate names an evaluator operation".into(),
            ));
        }
    };
    let source_evaluation = accepted_source_evaluation(
        session,
        accepted_evaluation_hash,
        source_candidate,
        base_snapshot_hash,
    )?;
    verify_candidate_evaluation_testimony(
        state,
        session,
        accepted_evaluation_hash,
        &source_evaluation,
    )?;
    let reserved_integration_launch_id = source_evaluation
        .pointer("/qualification/reserved_integration_launch_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "candidate integration source has no reserved launch identity".into(),
            )
        })?;
    let reserved_integration_thread_id = source_evaluation
        .pointer("/qualification/reserved_integration_thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "candidate integration source has no pre-minted thread identity".into(),
            )
        })?;
    if operation.integration_operation_hash.is_some()
        || operation.integration_launch_id.as_deref() != Some(reserved_integration_launch_id)
        || coordinate.chain_root_id != reserved_integration_thread_id
        || operation.source_chain_root_id != session.chain_root_id
        || operation.source_placement_thread_id != session.placement_thread_id
        || operation.owner_principal != session.owner_principal
        || operation.base_snapshot_hash != base_snapshot_hash
        || operation.candidate_snapshot_hash != source_candidate
        || operation.candidate_validation_hash != source_validation
    {
        return Err(HandlerError::BadRequest(
            "candidate integration capsule is sealed to another source candidate".into(),
        ));
    }
    let (
        integration_project_identity,
        integration_display_path,
        integration_base,
        integration_snapshot,
        integration_realization,
        integration_environment,
        integration_capabilities,
    ) = match &integration.project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity,
            display_path: Some(display_path),
            base_snapshot_hash,
            snapshot_hash,
            realization,
            environment,
            capability_ceiling,
            ..
        } => (
            stable_project_identity,
            display_path,
            base_snapshot_hash,
            snapshot_hash,
            realization,
            environment,
            capability_ceiling,
        ),
        _ => {
            return Err(HandlerError::BadRequest(
                "candidate integration did not execute in a pinned retained workspace".into(),
            ));
        }
    };
    let project_path = source_display_path
        .to_str()
        .ok_or_else(|| internal("candidate project identity is not UTF-8"))?;
    let expected_principal_key =
        ryeos_state::refs::principal_storage_key(&session.owner_principal).map_err(internal)?;
    let expected_project_hash = ryeos_state::refs::deployed_project_key(project_path);
    let exact_realization = matches!(
        integration_realization,
        ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication:
                ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                    principal_key,
                    project_hash,
                    expected_hash,
                },
        } if principal_key == expected_principal_key
            && project_hash == &expected_project_hash
            && expected_hash == base_snapshot_hash
    );
    let binding_matches = matches!(
        sealed.project_binding_subject_authority(),
        ryeos_engine::contracts::SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: binding_base,
            current_operational_generation,
        } if binding_base == base_snapshot_hash
            && current_operational_generation == source_candidate
    );
    if integration_project_identity != source_project_identity
        || integration_display_path != source_display_path
        || integration_base != base_snapshot_hash
        || integration_snapshot != source_candidate
        || !exact_realization
        || !binding_matches
        || integration_environment != &ryeos_state::objects::EnvironmentAuthority::None
        || integration_capabilities.iter().any(|capability| {
            capability == ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
        })
    {
        return Err(HandlerError::BadRequest(
            "candidate integration authority is not the exact isolated retained workspace".into(),
        ));
    }
    let completion_operation_id =
        ryeos_app::thread_lifecycle::CandidateIntegrationProcessCompletionFact::operation_id(
            operation,
            &coordinate.chain_root_id,
            &coordinate.capsule_hash,
        )
        .map_err(internal)?;
    let completion_fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &coordinate.chain_root_id,
        ryeos_app::thread_lifecycle::CANDIDATE_INTEGRATION_PROCESS_COMPLETED_EVENT,
        &completion_operation_id,
    )
    .map_err(internal)?;
    if completion_fact.count != 1 {
        return Err(internal(
            "candidate integration has no unique durable process-completion fact",
        ));
    }
    let completion_fact: ryeos_app::thread_lifecycle::CandidateIntegrationProcessCompletionFact =
        serde_json::from_value(
            completion_fact.payload.ok_or_else(|| {
                internal("candidate integration completion payload is unavailable")
            })?,
        )
        .map_err(internal)?;
    completion_fact
        .validate_for(
            operation,
            &coordinate.chain_root_id,
            &coordinate.capsule_hash,
        )
        .map_err(internal)?;
    let canonical_completion = completion_fact
        .canonical_completion(candidate_snapshot_hash)
        .map_err(internal)?;
    if integration.result.as_ref() != canonical_completion.result.as_ref() {
        return Err(internal(
            "candidate integration terminal result is not the daemon-canonical frozen-generation testimony",
        ));
    }
    let immutable_program = immutable_base_program_evidence(&sealed, base_snapshot_hash)?;
    let result_digest = integration
        .result
        .as_ref()
        .map(ryeos_state::objects::canonical_value_digest)
        .transpose()
        .map_err(internal)?;
    let terminal_snapshot_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&integration).map_err(internal)?,
    )
    .map_err(internal)?;
    let project_authority_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&integration.project_authority).map_err(internal)?,
    )
    .map_err(internal)?;
    Ok(json!({
        "schema":"ryeos.hosted_candidate_integration.v1",
        "source_candidate_snapshot_hash":source_candidate,
        "source_candidate_validation_hash":source_validation,
        "source_evaluation_hash":accepted_evaluation_hash,
        "source_evaluation":source_evaluation,
        "operation":{
            "coordinate":{
                "chain_root_id":coordinate.chain_root_id,
                "terminal_thread_id":coordinate.terminal_thread_id,
                "capsule_hash":coordinate.capsule_hash,
            },
            "chain_root_id":coordinate.chain_root_id,
            "terminal_thread_id":coordinate.terminal_thread_id,
            "capsule_hash":coordinate.capsule_hash,
            "item_ref":sealed.item_ref(),
            "effective_definition_digest":sealed.effective_definition_digest(),
            "parameters_digest":sealed.admitted_parameters_digest().map_err(internal)?,
            "immutable_definition_authority":immutable_program,
            "project_authority_digest":project_authority_digest,
            "terminal_snapshot_digest":terminal_snapshot_digest,
            "terminal_event_hash":terminal_event_hash,
            "authoritative_chain_head_hash":chain_head_hash,
            "result_digest":result_digest,
        },
        "publication_destination":{
            "principal_key":expected_principal_key,
            "project_hash":expected_project_hash,
            "expected_hash":base_snapshot_hash,
        },
        "result_candidate_snapshot_hash":candidate_snapshot_hash,
        "result_candidate_validation_hash":candidate_validation_hash,
    }))
}

fn verified_candidate_source(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_snapshot_hash: &str,
    candidate_validation_hash: &str,
    integration: Option<&CandidateIntegrationCoordinate>,
) -> Result<VerifiedCandidateSource, HandlerError> {
    let (base_snapshot_hash, closure_evidence) = frozen_candidate_closure_evidence(
        state,
        session,
        candidate_snapshot_hash,
        candidate_validation_hash,
    )?;
    let direct = session.candidate_snapshot_hash.as_deref() == Some(candidate_snapshot_hash)
        && session.candidate_validation_hash.as_deref() == Some(candidate_validation_hash);
    if integration.is_none() && direct {
        if !candidate_evaluation_source_ready(
            session,
            candidate_snapshot_hash,
            candidate_validation_hash,
        ) {
            return Err(HandlerError::BadRequest(
                "direct retained candidate is not available for independent evaluation".into(),
            ));
        }
        return Ok(VerifiedCandidateSource {
            base_snapshot_hash,
            closure_evidence,
            integration_evidence: None,
            integration_operation_hash: None,
        });
    }
    let integration = integration.ok_or_else(|| {
        HandlerError::BadRequest(
            "integrated candidate requires its exact integration terminal coordinate".into(),
        )
    })?;
    let source_candidate = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no snapshot identity"))?;
    let source_validation = session
        .candidate_validation_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no validation identity"))?;
    if !candidate_evaluation_source_ready(session, source_candidate, source_validation) {
        return Err(HandlerError::BadRequest(
            "integration source is no longer an exact retained candidate".into(),
        ));
    }
    let integration_evidence = verify_candidate_integration_terminal(
        state,
        session,
        integration,
        candidate_snapshot_hash,
        candidate_validation_hash,
        &base_snapshot_hash,
    )?;
    let integration_operation_hash =
        ryeos_state::objects::canonical_value_digest(&integration_evidence).map_err(internal)?;
    Ok(VerifiedCandidateSource {
        base_snapshot_hash,
        closure_evidence,
        integration_evidence: Some(integration_evidence),
        integration_operation_hash: Some(integration_operation_hash),
    })
}

fn immutable_base_program_evidence(
    sealed: &ryeos_app::thread_lifecycle::SealedRootExecutionRequest,
    base_snapshot_hash: &str,
) -> Result<Value, HandlerError> {
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace};
    use ryeos_engine::resolution::TrustClass;

    let resolution = sealed.admitted_effective_resolution().map_err(internal)?;
    if !matches!(
        sealed.resolution_subject_authority(),
        ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash,
        } if snapshot_hash == base_snapshot_hash
    ) {
        return Err(HandlerError::BadRequest(
            "candidate operation program was not resolved in the exact immutable base context"
                .into(),
        ));
    }
    // A bundle-owned root remains bundle authority even though resolution ran
    // with the exact base overlay available. A project-owned root is admitted
    // from that base. The distinction follows the resolved root, not merely
    // the surrounding project context.
    let authority_kind = match (&resolution.root.source_space, &resolution.root.source_root) {
        (ItemSpace::Bundle, ItemSourceRoot::Bundle { .. }) => "installed_bundle",
        (ItemSpace::Project, ItemSourceRoot::Project) => "base_project_generation",
        _ => {
            return Err(HandlerError::BadRequest(
                "candidate operation root is outside immutable bundle/base authority".into(),
            ));
        }
    };
    let trusted_effective = match authority_kind {
        "installed_bundle" => resolution.effective_trust_class == TrustClass::TrustedBundle,
        "base_project_generation" => matches!(
            resolution.effective_trust_class,
            TrustClass::TrustedBundle | TrustClass::TrustedProject
        ),
        _ => unreachable!("evaluator authority kind is selected above"),
    };
    if !trusted_effective {
        return Err(HandlerError::BadRequest(
            "candidate operation definition is not trusted by its immutable authority".into(),
        ));
    }
    let contributors = std::iter::once(&resolution.root)
        .chain(resolution.ancestors.iter())
        .chain(resolution.referenced_items.iter())
        .collect::<Vec<_>>();
    let mut bundles = BTreeSet::new();
    let mut signers = BTreeSet::new();
    for contributor in &contributors {
        match (
            &contributor.source_space,
            &contributor.source_root,
            contributor.trust_class,
        ) {
            (ItemSpace::Bundle, ItemSourceRoot::Bundle { name }, TrustClass::TrustedBundle) => {
                bundles.insert(name.clone());
            }
            (ItemSpace::Project, ItemSourceRoot::Project, TrustClass::TrustedProject)
                if authority_kind == "base_project_generation" => {}
            _ => {
                return Err(HandlerError::BadRequest(
                    "candidate operation closure contains content outside its immutable trusted authority"
                        .into(),
                ));
            }
        }
        let signer = contributor.signer_fingerprint.as_deref().ok_or_else(|| {
            HandlerError::BadRequest(
                "trusted candidate-operation contributor has no signer identity".into(),
            )
        })?;
        if !lillux::valid_hash(signer) {
            return Err(HandlerError::BadRequest(
                "trusted candidate-operation contributor has a non-canonical signer identity"
                    .into(),
            ));
        }
        signers.insert(signer.to_owned());
    }
    if authority_kind == "installed_bundle"
        && resolution
            .references_edges
            .iter()
            .any(|edge| edge.to_source_space != ItemSpace::Bundle)
    {
        return Err(HandlerError::BadRequest(
            "candidate operation resolution edge escapes immutable bundle content".into(),
        ));
    }
    if resolution.references_edges.iter().any(|edge| {
        !matches!(
            edge.trust_class,
            TrustClass::TrustedBundle | TrustClass::TrustedProject
        ) || (authority_kind == "installed_bundle" && edge.trust_class != TrustClass::TrustedBundle)
            || !matches!(edge.to_source_space, ItemSpace::Bundle | ItemSpace::Project)
    }) {
        return Err(HandlerError::BadRequest(
            "candidate operation references content outside its immutable trusted authority".into(),
        ));
    }
    Ok(json!({
        "authority_kind":authority_kind,
        "resolution_subject_authority":sealed.resolution_subject_authority(),
        "resolution_digest":ryeos_state::objects::canonical_value_digest(
            &serde_json::to_value(resolution).map_err(internal)?,
        )
            .map_err(internal)?,
        "contributor_count":contributors.len(),
        "bundles":bundles,
        "signer_fingerprints":signers,
    }))
}

fn validate_candidate_evaluation_result(
    value: Value,
    candidate_snapshot_hash: &str,
    base_snapshot_hash: &str,
) -> Result<CandidateEvaluationResult, HandlerError> {
    let result: CandidateEvaluationResult = serde_json::from_value(value)
        .map_err(|error| HandlerError::BadRequest(format!("invalid evaluator result: {error}")))?;
    if result.schema_version != 1
        || result.candidate_snapshot_hash != candidate_snapshot_hash
        || result.base_snapshot_hash != base_snapshot_hash
        || !result.evidence.is_object()
    {
        return Err(HandlerError::BadRequest(
            "evaluator result contradicts the exact frozen candidate or base".into(),
        ));
    }
    Ok(result)
}

fn candidate_qualification_operation_id(
    qualification_root_id: &str,
    candidate_evaluation_hash: &str,
) -> Result<String, HandlerError> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_qualification_operation.v1",
        "qualification_root_id":qualification_root_id,
        "candidate_evaluation_hash":candidate_evaluation_hash,
    }))
    .map_err(internal)
}

fn candidate_qualification_fact_payload(
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_evaluation_hash: &str,
    candidate_evaluation: &Value,
) -> Value {
    json!({
        "schema":1,
        "origin":"owner_authorized",
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "candidate_evaluation_hash":candidate_evaluation_hash,
        "candidate_evaluation":candidate_evaluation,
    })
}

fn candidate_qualification_reservation(
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<CandidateQualificationReservation, HandlerError> {
    let value = session
        .candidate_qualification
        .clone()
        .ok_or_else(|| internal("qualifying candidate has no durable reservation"))?;
    let reservation: CandidateQualificationReservation =
        serde_json::from_value(value).map_err(internal)?;
    if reservation.schema != "ryeos.hosted_candidate_qualification_reservation.v1"
        || !lillux::valid_hash(&reservation.candidate_evaluation_hash)
        || !lillux::valid_hash(&reservation.qualification_operation_id)
        || reservation
            .previous_candidate_evaluation_hash
            .as_deref()
            .is_some_and(|hash| !lillux::valid_hash(hash))
        || ryeos_state::objects::canonical_value_digest(&reservation.candidate_evaluation)
            .map_err(internal)?
            != reservation.candidate_evaluation_hash
        || session.candidate_disposition_root_id.as_deref()
            != Some(reservation.qualification_root_id.as_str())
        || session.candidate_disposition_operation_id.as_deref()
            != Some(reservation.qualification_operation_id.as_str())
        || session.candidate_evaluation_hash.as_deref()
            != reservation.previous_candidate_evaluation_hash.as_deref()
        || candidate_qualification_operation_id(
            &reservation.qualification_root_id,
            &reservation.candidate_evaluation_hash,
        )? != reservation.qualification_operation_id
    {
        return Err(internal(
            "candidate qualification reservation contradicts its durable projection",
        ));
    }
    Ok(reservation)
}

/// Settle an exact rooted qualification after a crash, or roll back only when
/// complete root replay proves the append-only fact was never contacted. The
/// caller holds the candidate disposition lock, so no live handler can race
/// this decision.
fn reconcile_candidate_qualification(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<bool, HandlerError> {
    if session.state != "qualifying" {
        return Ok(false);
    }
    let reservation = candidate_qualification_reservation(session)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &reservation.qualification_root_id,
        "hosted_candidate.qualified",
        &reservation.qualification_operation_id,
    )
    .map_err(internal)?;
    if fact.count == 0 {
        let reserved_launch_id = reservation
            .candidate_evaluation
            .pointer("/qualification/reserved_integration_launch_id")
            .and_then(Value::as_str);
        let reserved_thread_id = reservation
            .candidate_evaluation
            .pointer("/qualification/reserved_integration_thread_id")
            .and_then(Value::as_str);
        let reserved_integration_launch = match (reserved_launch_id, reserved_thread_id) {
            (Some(launch_id), Some(thread_id)) => {
                Some((launch_id, thread_id, session.owner_principal.as_str()))
            }
            (None, None) => None,
            _ => {
                return Err(internal(
                    "uncommitted candidate qualification has a partial integration reservation",
                ));
            }
        };
        state
            .state_store
            .rollback_dedicated_candidate_evaluation(
                &session.placement_thread_id,
                &reservation.qualification_root_id,
                &reservation.qualification_operation_id,
                reserved_integration_launch,
            )
            .map_err(internal)?;
        return Ok(false);
    }
    verify_candidate_evaluation_testimony(
        state,
        session,
        &reservation.candidate_evaluation_hash,
        &reservation.candidate_evaluation,
    )?;
    let candidate_snapshot_hash = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| internal("qualifying candidate has no source snapshot"))?;
    let candidate_validation_hash = session
        .candidate_validation_hash
        .as_deref()
        .ok_or_else(|| internal("qualifying candidate has no validation identity"))?;
    state
        .state_store
        .settle_dedicated_candidate_evaluation(
            &session.placement_thread_id,
            candidate_snapshot_hash,
            candidate_validation_hash,
            reservation.previous_candidate_evaluation_hash.as_deref(),
            &reservation.candidate_evaluation_hash,
            &reservation.candidate_evaluation,
            &reservation.qualification_root_id,
            &reservation.qualification_operation_id,
        )
        .map_err(internal)?;
    Ok(true)
}

fn append_root_fact_once(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    event_type: &str,
    operation_id: &str,
    payload: Value,
) -> Result<(), HandlerError> {
    ryeos_app::authoritative_root_fact::append_once(
        state,
        &session.placement_thread_id,
        event_type,
        operation_id,
        payload,
    )
    .map_err(internal)
}

async fn validate_candidate_closure_and_base(
    req: ValidateCandidateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if !candidate_evaluation_source_ready(
        &session,
        &req.candidate_snapshot_hash,
        &req.candidate_validation_hash,
    ) {
        return Err(HandlerError::BadRequest(
            "validation identity differs from the exact retained candidate".into(),
        ));
    }
    let (_base_snapshot_hash, evidence) = frozen_candidate_closure_evidence(
        &state,
        &session,
        &req.candidate_snapshot_hash,
        &req.candidate_validation_hash,
    )?;
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "state":"frozen",
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
        "qualification":"closure_verified",
        "publication_authority":false,
        "evidence":evidence,
    }))
}

fn candidate_qualification_request_matches(
    evaluation: &Value,
    req: &QualifyCandidateRequest,
    compare_reserved_launch: bool,
) -> bool {
    let integration_matches = match &req.integration {
        None => evaluation.get("integration").is_none_or(Value::is_null),
        Some(coordinate) => {
            evaluation
                .pointer("/integration/operation/coordinate/chain_root_id")
                .and_then(Value::as_str)
                == Some(coordinate.chain_root_id.as_str())
                && evaluation
                    .pointer("/integration/operation/coordinate/terminal_thread_id")
                    .and_then(Value::as_str)
                    == Some(coordinate.terminal_thread_id.as_str())
                && evaluation
                    .pointer("/integration/operation/coordinate/capsule_hash")
                    .and_then(Value::as_str)
                    == Some(coordinate.capsule_hash.as_str())
        }
    };
    integration_matches
        && evaluation
            .pointer("/candidate/candidate_snapshot_hash")
            .and_then(Value::as_str)
            == Some(req.candidate_snapshot_hash.as_str())
        && evaluation
            .pointer("/candidate/candidate_validation_hash")
            .and_then(Value::as_str)
            == Some(req.candidate_validation_hash.as_str())
        && evaluation
            .pointer("/evaluator/chain_root_id")
            .and_then(Value::as_str)
            == Some(req.evaluator_chain_root_id.as_str())
        && evaluation
            .pointer("/evaluator/terminal_thread_id")
            .and_then(Value::as_str)
            == Some(req.evaluator_terminal_thread_id.as_str())
        && evaluation
            .pointer("/evaluator/capsule_hash")
            .and_then(Value::as_str)
            == Some(req.evaluator_capsule_hash.as_str())
        && evaluation
            .pointer("/evaluator/item_ref")
            .and_then(Value::as_str)
            == Some(req.evaluator_item_ref.as_str())
        && evaluation
            .pointer("/evaluator/effective_definition_digest")
            .and_then(Value::as_str)
            == Some(req.evaluator_definition_digest.as_str())
        && evaluation
            .pointer("/evaluator/parameters_digest")
            .and_then(Value::as_str)
            == Some(req.evaluator_parameters_digest.as_str())
        && (!compare_reserved_launch
            || evaluation
                .pointer("/qualification/reserved_integration_launch_id")
                .and_then(Value::as_str)
                == req.integration_launch_id.as_deref())
}

async fn qualify_candidate(
    req: QualifyCandidateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let qualification_root_id = ctx
        .recorded_service_root_id()
        .ok_or_else(|| {
            internal("candidate qualification requires its recorded service invocation root")
        })?
        .to_owned();
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let mut session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate qualification was reserved".into(),
        ));
    }
    if req.evaluator_chain_root_id == session.chain_root_id {
        return Err(HandlerError::BadRequest(
            "candidate qualification requires an independent evaluator execution chain".into(),
        ));
    }
    if req
        .integration_launch_id
        .as_deref()
        .is_some_and(|launch_id| !ryeos_app::state_store::is_canonical_launch_id(launch_id))
    {
        return Err(HandlerError::BadRequest(
            "reserved candidate-integration launch id is not canonical".into(),
        ));
    }
    if req.integration.is_some() && req.integration_launch_id.is_some() {
        return Err(HandlerError::BadRequest(
            "final integrated-candidate qualification cannot reserve another integration".into(),
        ));
    }
    if req.integration.is_none()
        && session.candidate_disposition
            == ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        && req.integration_launch_id.is_none()
    {
        return Err(HandlerError::BadRequest(
            "bounded retained candidate qualification must reserve its sole integration launch"
                .into(),
        ));
    }
    if session.state == "qualifying" {
        reconcile_candidate_qualification(&state, &session)?;
        session = owned_session(&state, &ctx, &req.chain_root_id)?;
    }
    if let (Some(incumbent_hash), Some(incumbent)) = (
        session.candidate_evaluation_hash.as_deref(),
        session.candidate_evaluation.as_ref(),
    ) {
        verify_candidate_evaluation_testimony(&state, &session, incumbent_hash, incumbent)?;
        let incumbent_launch_id = incumbent
            .pointer("/qualification/reserved_integration_launch_id")
            .and_then(Value::as_str);
        if candidate_qualification_request_matches(incumbent, &req, true) {
            return Ok(json!({
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "state":"publish_ready",
                "candidate_snapshot_hash":req.candidate_snapshot_hash,
                "candidate_validation_hash":req.candidate_validation_hash,
                "candidate_evaluation_hash":incumbent_hash,
                "qualification_root_id":incumbent
                    .pointer("/qualification/root_thread_id")
                    .and_then(Value::as_str),
                "accepted":true,
                "idempotent":true,
            }));
        }
        let incumbent_is_integrated = incumbent
            .get("integration")
            .is_some_and(|value| !value.is_null());
        if req.integration.is_some() {
            if incumbent_is_integrated || incumbent_launch_id.is_none() {
                return Err(HandlerError::BadRequest(
                    "accepted candidate qualification is immutable; only its already-reserved integration result may replace it".into(),
                ));
            }
        } else {
            let old_launch_id = incumbent_launch_id.ok_or_else(|| {
                HandlerError::BadRequest(
                    "accepted candidate qualification cannot acquire or change integration intent"
                        .into(),
                )
            })?;
            if !candidate_qualification_request_matches(incumbent, &req, false)
                || req.integration_launch_id.is_none()
                || req.integration_launch_id.as_deref() == Some(old_launch_id)
            {
                return Err(HandlerError::BadRequest(
                    "replacement integration reservation must preserve the accepted evaluator and use one fresh launch coordinate".into(),
                ));
            }
            let old = state
                .state_store
                .launch_planning_status(old_launch_id, &session.owner_principal)
                .map_err(internal)?
                .ok_or_else(|| {
                    HandlerError::BadRequest(
                        "reserved integration launch no longer has authoritative pre-birth state"
                            .into(),
                    )
                })?;
            let proved_never_bound = old.thread_id.is_none()
                && matches!(old.status.as_str(), "cancelled" | "failed" | "expired")
                && matches!(
                    old.outcome_code.as_deref(),
                    Some("cancelled_by_requester")
                        | Some("thread_creation_failed")
                        | Some("launch_admission_failed")
                        | Some("daemon_restarted_before_thread_bind")
                );
            if !proved_never_bound {
                return Err(HandlerError::BadRequest(
                    "accepted candidate integration reservation is immutable after launch planning may have contacted execution"
                        .into(),
                ));
            }
        }
    }

    let verified_source = verified_candidate_source(
        &state,
        &session,
        &req.candidate_snapshot_hash,
        &req.candidate_validation_hash,
        req.integration.as_ref(),
    )?;
    let base_snapshot_hash = verified_source.base_snapshot_hash.clone();
    let closure_evidence = verified_source.closure_evidence.clone();

    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated placement disappeared from its authoritative chain"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity: source_project_identity,
        base_snapshot_hash: source_base,
        ..
    } = &source.project_authority
    else {
        return Err(internal("dedicated candidate authority is not pinned"));
    };
    if source_base != &base_snapshot_hash {
        return Err(internal(
            "candidate closure base differs from its admitted project authority",
        ));
    }

    let (evaluator, evaluator_last_event, evaluator_chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &req.evaluator_chain_root_id,
            &req.evaluator_terminal_thread_id,
        )
        .map_err(internal)?
        .ok_or(HandlerError::NotFound)?;
    let evaluator_last_event = evaluator_last_event
        .ok_or_else(|| HandlerError::BadRequest("evaluator has no terminal event".into()))?;
    let evaluator_last_event_hash = evaluator_last_event
        .event_hash
        .as_deref()
        .ok_or_else(|| internal("evaluator terminal event has no CAS hash"))?;
    if evaluator.thread_id != req.evaluator_terminal_thread_id
        || evaluator.chain_root_id != req.evaluator_chain_root_id
        || !candidate_execution_succeeded(&evaluator)
        || evaluator.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || evaluator_last_event.event_type != ryeos_state::event_types::THREAD_COMPLETED
        || evaluator_last_event.chain_root_id != req.evaluator_chain_root_id
        || evaluator_last_event.thread_id != req.evaluator_terminal_thread_id
        || evaluator.last_event_hash.as_deref() != Some(evaluator_last_event_hash)
        || u64::try_from(evaluator_last_event.chain_seq).ok() != Some(evaluator.last_chain_seq)
        || u64::try_from(evaluator_last_event.thread_seq).ok() != Some(evaluator.last_thread_seq)
    {
        return Err(HandlerError::BadRequest(
            "evaluator terminal does not prove exact successful owner-authorized completion".into(),
        ));
    }
    let evaluator_capsule_hash = evaluator
        .admitted_launch_capsule_hash
        .as_deref()
        .ok_or_else(|| HandlerError::BadRequest("evaluator has no admitted capsule".into()))?;
    if evaluator_capsule_hash != req.evaluator_capsule_hash {
        return Err(HandlerError::BadRequest(
            "evaluator capsule differs from the requested qualification coordinate".into(),
        ));
    }
    let evaluator_capsule = state
        .state_store
        .admitted_launch_capsule(&req.evaluator_terminal_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("evaluator admitted capsule disappeared"))?;
    if evaluator_capsule.content_hash().map_err(internal)? != req.evaluator_capsule_hash
        || evaluator_capsule.project_authority != evaluator.project_authority
    {
        return Err(internal(
            "evaluator capsule contradicts its authoritative terminal snapshot",
        ));
    }
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &evaluator_capsule,
        )
        .map_err(internal)?;
    let evaluator_operation = sealed.candidate_evaluation_authority().ok_or_else(|| {
        HandlerError::BadRequest(
            "evaluator was not launched through exact candidate-evaluation authority".into(),
        )
    })?;
    if evaluator_operation.source_chain_root_id != session.chain_root_id
        || evaluator_operation.source_placement_thread_id != session.placement_thread_id
        || evaluator_operation.owner_principal != session.owner_principal
        || evaluator_operation.base_snapshot_hash != base_snapshot_hash
        || evaluator_operation.candidate_snapshot_hash != req.candidate_snapshot_hash
        || evaluator_operation.candidate_validation_hash != req.candidate_validation_hash
        || evaluator_operation.integration_operation_hash.as_deref()
            != verified_source.integration_operation_hash.as_deref()
        || !matches!(
            &evaluator_operation.purpose,
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Evaluate
        )
    {
        return Err(HandlerError::BadRequest(
            "evaluator capsule is sealed to another retained candidate operation".into(),
        ));
    }
    if sealed.item_ref() != req.evaluator_item_ref
        || evaluator.item_ref != req.evaluator_item_ref
        || sealed.effective_definition_digest().as_str() != req.evaluator_definition_digest
        || sealed.admitted_parameters_digest().map_err(internal)? != req.evaluator_parameters_digest
    {
        return Err(HandlerError::BadRequest(
            "evaluator invocation differs from the exact requested definition or parameters".into(),
        ));
    }

    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity,
        base_snapshot_hash: evaluator_base,
        snapshot_hash: evaluator_snapshot,
        realization,
        capability_ceiling,
        ..
    } = &evaluator.project_authority
    else {
        return Err(HandlerError::BadRequest(
            "candidate evaluator did not execute against an immutable candidate generation".into(),
        ));
    };
    let realization_kind = match realization {
        ryeos_state::objects::PinnedProjectRealization::ReadOnly => "read_only",
        ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
        } => "cow_discard",
        _ => {
            return Err(HandlerError::BadRequest(
                "candidate evaluator may not retain or publish evaluator workspace changes".into(),
            ));
        }
    };
    let binding_matches = match (realization, sealed.project_binding_subject_authority()) {
        (
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration { snapshot_hash },
        ) => snapshot_hash == &req.candidate_snapshot_hash,
        (
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_engine::contracts::SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash,
                current_operational_generation,
            },
        ) => {
            base_snapshot_hash == &req.candidate_snapshot_hash
                && current_operational_generation == &req.candidate_snapshot_hash
        }
        _ => false,
    };
    if stable_project_identity != source_project_identity
        || evaluator_base != &req.candidate_snapshot_hash
        || evaluator_snapshot != &req.candidate_snapshot_hash
        || !binding_matches
        || capability_ceiling.iter().any(|capability| {
            capability == ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
        })
    {
        return Err(HandlerError::BadRequest(
            "evaluator execution authority is not the exact read-only/discard candidate view"
                .into(),
        ));
    }
    let immutable_evaluator = immutable_base_program_evidence(&sealed, &base_snapshot_hash)?;
    let evaluator_result_value = evaluator
        .result
        .clone()
        .ok_or_else(|| HandlerError::BadRequest("successful evaluator has no result".into()))?;
    let evaluator_result = validate_candidate_evaluation_result(
        evaluator_result_value.clone(),
        &req.candidate_snapshot_hash,
        &base_snapshot_hash,
    )?;
    let evaluator_result_digest =
        ryeos_state::objects::canonical_value_digest(&evaluator_result_value).map_err(internal)?;
    let evaluator_claim_evidence_digest =
        ryeos_state::objects::canonical_value_digest(&evaluator_result.evidence)
            .map_err(internal)?;
    let evaluator_terminal_snapshot_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&evaluator).map_err(internal)?,
    )
    .map_err(internal)?;
    let evaluator_project_authority_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&evaluator.project_authority).map_err(internal)?,
    )
    .map_err(internal)?;
    let reserved_integration_launch_id = evaluator_result
        .accepted
        .then(|| req.integration_launch_id.clone())
        .flatten();
    let reserved_integration_thread_id = reserved_integration_launch_id
        .as_ref()
        .map(|_| ryeos_app::thread_lifecycle::new_thread_id());
    let evaluation_evidence = json!({
        "schema":"ryeos.hosted_candidate_evaluation.v1",
        "candidate":closure_evidence,
        "integration":verified_source.integration_evidence,
        "integration_operation_hash":verified_source.integration_operation_hash,
        "evaluator":{
            "chain_root_id":req.evaluator_chain_root_id,
            "terminal_thread_id":req.evaluator_terminal_thread_id,
            "capsule_hash":req.evaluator_capsule_hash,
            "item_ref":req.evaluator_item_ref,
            "effective_definition_digest":req.evaluator_definition_digest,
            "parameters_digest":req.evaluator_parameters_digest,
            "immutable_definition_authority":immutable_evaluator,
            "project_authority_digest":evaluator_project_authority_digest,
            "candidate_realization":realization_kind,
            "terminal_snapshot_digest":evaluator_terminal_snapshot_digest,
            "terminal_event_hash":evaluator_last_event_hash,
            "authoritative_chain_head_hash":evaluator_chain_head_hash,
        },
        "result":{
            "accepted":evaluator_result.accepted,
            "result_digest":evaluator_result_digest,
            "claim_evidence_digest":evaluator_claim_evidence_digest,
        },
        "qualification":{
            "root_thread_id":qualification_root_id,
            "reserved_integration_launch_id":reserved_integration_launch_id,
            "reserved_integration_thread_id":reserved_integration_thread_id,
        },
    });
    let candidate_evaluation_hash =
        ryeos_state::objects::canonical_value_digest(&evaluation_evidence).map_err(internal)?;
    if !evaluator_result.accepted {
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "state":session.state,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "candidate_evaluation_hash":candidate_evaluation_hash,
            "accepted":false,
        }));
    }
    let qualification_operation_id =
        candidate_qualification_operation_id(&qualification_root_id, &candidate_evaluation_hash)?;
    let source_candidate_snapshot_hash = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no snapshot identity"))?;
    let source_candidate_validation_hash = session
        .candidate_validation_hash
        .as_deref()
        .ok_or_else(|| internal("retained source candidate has no validation identity"))?;
    let reserved_integration_launch = match (
        reserved_integration_launch_id.as_deref(),
        reserved_integration_thread_id.as_deref(),
    ) {
        (Some(launch_id), Some(thread_id)) => {
            Some((launch_id, thread_id, session.owner_principal.as_str()))
        }
        (None, None) => None,
        _ => return Err(internal("candidate integration reservation is partial")),
    };
    state
        .state_store
        .reserve_dedicated_candidate_evaluation(
            &session.placement_thread_id,
            source_candidate_snapshot_hash,
            source_candidate_validation_hash,
            session.candidate_evaluation_hash.as_deref(),
            &candidate_evaluation_hash,
            &evaluation_evidence,
            &qualification_root_id,
            &qualification_operation_id,
            reserved_integration_launch,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::authoritative_root_fact::append_once(
        &state,
        &qualification_root_id,
        "hosted_candidate.qualified",
        &qualification_operation_id,
        candidate_qualification_fact_payload(
            &session,
            &candidate_evaluation_hash,
            &evaluation_evidence,
        ),
    )
    .map_err(internal)?;
    verify_candidate_evaluation_testimony(
        &state,
        &session,
        &candidate_evaluation_hash,
        &evaluation_evidence,
    )?;
    state
        .state_store
        .settle_dedicated_candidate_evaluation(
            &session.placement_thread_id,
            source_candidate_snapshot_hash,
            source_candidate_validation_hash,
            session.candidate_evaluation_hash.as_deref(),
            &candidate_evaluation_hash,
            &evaluation_evidence,
            &qualification_root_id,
            &qualification_operation_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "state":"publish_ready",
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
        "candidate_evaluation_hash":candidate_evaluation_hash,
        "qualification_root_id":qualification_root_id,
        "qualification_operation_id":qualification_operation_id,
        "accepted":true,
        "idempotent":false,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
}

fn candidate_discard_operation_id(
    disposition_root_id: &str,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_snapshot_hash: &str,
) -> Result<String, HandlerError> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_discard_operation.v2",
        "disposition_root_id":disposition_root_id,
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate_snapshot_hash,
    }))
    .map_err(internal)
}

fn candidate_discard_payload(
    disposition_root_id: &str,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    candidate_snapshot_hash: &str,
) -> Value {
    json!({
        "schema":2,
        "origin":"owner_authorized",
        "owner_principal":session.owner_principal,
        "disposition_root_id":disposition_root_id,
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate_snapshot_hash,
    })
}

fn expected_candidate_discard_fact(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    disposition_root_id: &str,
    operation_id: &str,
    candidate_snapshot_hash: &str,
) -> Result<Value, HandlerError> {
    if session.candidate_snapshot_hash.as_deref() != Some(candidate_snapshot_hash)
        || candidate_discard_operation_id(disposition_root_id, session, candidate_snapshot_hash)?
            != operation_id
    {
        return Err(internal(
            "candidate discard operation identity is contradictory",
        ));
    }
    let disposition = state
        .state_store
        .get_thread(disposition_root_id)
        .map_err(internal)?
        .ok_or_else(|| internal("candidate discard operation root disappeared"))?;
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(internal)?
        .ok_or_else(|| internal("candidate source root disappeared"))?;
    if disposition.thread_id != disposition_root_id
        || disposition.chain_root_id != disposition_root_id
        || disposition_root_id == session.chain_root_id
        || disposition.item_ref != "service:worker-executions/discard"
        || disposition.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || disposition.current_site_id != source.current_site_id
        || disposition.origin_site_id != source.origin_site_id
    {
        return Err(internal(
            "candidate discard root has contradictory authority",
        ));
    }
    let mut expected =
        candidate_discard_payload(disposition_root_id, session, candidate_snapshot_hash);
    expected
        .as_object_mut()
        .expect("candidate discard fact is an object")
        .insert(
            "operation_id".into(),
            Value::String(operation_id.to_owned()),
        );
    Ok(expected)
}

fn verify_candidate_discard_fact(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    disposition_root_id: &str,
    operation_id: &str,
    candidate_snapshot_hash: &str,
) -> Result<(), HandlerError> {
    let expected = expected_candidate_discard_fact(
        state,
        session,
        disposition_root_id,
        operation_id,
        candidate_snapshot_hash,
    )?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        disposition_root_id,
        "hosted_candidate.discarded",
        operation_id,
    )
    .map_err(internal)?;
    if fact.count != 1 || fact.payload.as_ref() != Some(&expected) {
        return Err(internal(
            "candidate discard projection has no exact owner fact",
        ));
    }
    Ok(())
}

/// The caller holds the disposition lock. Absence is accepted only after a
/// complete replay of the exact reserved root; a unique owner fact commits
/// the discard regardless of whether the service root itself was settled.
fn reconcile_candidate_discard(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
) -> Result<bool, HandlerError> {
    if session.state != "discarding" {
        return Ok(false);
    }
    let candidate = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| internal("discarding candidate has no snapshot identity"))?;
    let root_id = session
        .candidate_disposition_root_id
        .as_deref()
        .ok_or_else(|| internal("discarding candidate has no disposition root"))?;
    let operation_id = session
        .candidate_disposition_operation_id
        .as_deref()
        .ok_or_else(|| internal("discarding candidate has no disposition operation"))?;
    let expected =
        expected_candidate_discard_fact(state, session, root_id, operation_id, candidate)?;
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        root_id,
        "hosted_candidate.discarded",
        operation_id,
    )
    .map_err(internal)?;
    if !candidate_discard_fact_committed(fact.count, fact.payload.as_ref(), &expected)? {
        state
            .state_store
            .rollback_dedicated_candidate_discard(
                &session.placement_thread_id,
                candidate,
                root_id,
                operation_id,
            )
            .map_err(internal)?;
        return Ok(false);
    }
    state
        .state_store
        .settle_dedicated_candidate_discard(
            &session.placement_thread_id,
            candidate,
            root_id,
            operation_id,
        )
        .map_err(internal)?;
    Ok(true)
}

fn candidate_discard_fact_committed(
    count: u8,
    payload: Option<&Value>,
    expected: &Value,
) -> Result<bool, HandlerError> {
    match (count, payload) {
        (0, None) => Ok(false),
        (1, Some(payload)) if payload == expected => Ok(true),
        _ => Err(internal(
            "candidate discard journal has contradictory rooted facts",
        )),
    }
}

async fn discard(
    req: DiscardRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let disposition_root_id = ctx
        .recorded_service_root_id()
        .ok_or_else(|| internal("candidate discard requires a recorded service root"))?
        .to_owned();
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let mut session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate discard was reserved".into(),
        ));
    }
    if session.state == "discarding"
        && session
            .publication_result
            .as_deref()
            .is_some_and(|result| matches!(result, "retained" | "retained_for_review"))
    {
        if session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        {
            return Err(HandlerError::BadRequest(
                "discard retry changed candidate identity".into(),
            ));
        }
        let committed = reconcile_candidate_discard(&state, &session)?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
        if committed {
            return Ok(json!({
                "chain_root_id":session.chain_root_id,
                "placement_thread_id":session.placement_thread_id,
                "discarded":true,
                "idempotent":true,
            }));
        }
        session = owned_session(&state, &ctx, &req.chain_root_id)?;
    }
    if session.publication_result.as_deref() == Some("discarded") {
        if session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        {
            return Err(HandlerError::BadRequest(
                "discard retry changed candidate identity".into(),
            ));
        }
        let retained_root_id = session
            .candidate_disposition_root_id
            .as_deref()
            .ok_or_else(|| internal("discarded candidate has no disposition root"))?;
        let retained_operation_id = session
            .candidate_disposition_operation_id
            .as_deref()
            .ok_or_else(|| internal("discarded candidate has no disposition operation"))?;
        verify_candidate_discard_fact(
            &state,
            &session,
            retained_root_id,
            retained_operation_id,
            &req.candidate_snapshot_hash,
        )?;
        // The first response may have crashed after the terminal disposition
        // commit but before best-effort kernel-resource retirement. Re-drive
        // the existing closed-workspace journal only after the exact owner
        // fact above is reverified; this does not make a read path mutating.
        state
            .state_store
            .resume_closed_worker_scope_retirement_for_session(&session.placement_thread_id)
            .map_err(internal)?;
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "discarded":true,
            "idempotent":true,
        }));
    }
    let operation_id = candidate_discard_operation_id(
        &disposition_root_id,
        &session,
        &req.candidate_snapshot_hash,
    )?;
    // Persist the exact root coordinate, and reject a wrong candidate or
    // disposition, before the owner fact becomes an irreversible commit.
    state
        .state_store
        .reserve_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &disposition_root_id,
            &operation_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::authoritative_root_fact::append_once(
        &state,
        &disposition_root_id,
        "hosted_candidate.discarded",
        &operation_id,
        candidate_discard_payload(&disposition_root_id, &session, &req.candidate_snapshot_hash),
    )
    .map_err(internal)?;
    verify_candidate_discard_fact(
        &state,
        &session,
        &disposition_root_id,
        &operation_id,
        &req.candidate_snapshot_hash,
    )?;
    state
        .state_store
        .settle_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &disposition_root_id,
            &operation_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "discarded":true,
    }))
}

async fn publish(
    req: PublishRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    state
        .authorizer
        .authorize(
            &ctx.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(
                ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
            ),
        )
        .map_err(|_| {
            HandlerError::Forbidden(format!(
                "candidate publication requires fixed capability `{}`",
                ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
            ))
        })?;
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let invocation_root_id = ctx
        .recorded_service_root_id()
        .ok_or_else(|| internal("candidate publication requires a recorded service root"))?
        .to_owned();
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate publication was reserved".into(),
        ));
    }
    let owner_publishable = session.candidate_disposition
        == ryeos_app::runtime_db::DedicatedCandidateDisposition::OwnerDecision
        && matches!(
            session.state.as_str(),
            "publish_ready" | "publishing" | "terminal"
        );
    let bounded_publishable = session.candidate_disposition
        == ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        && matches!(
            session.state.as_str(),
            "publish_ready" | "publishing" | "terminal"
        )
        && session.terminal_reason.as_deref() == Some("completed");
    if !owner_publishable && !bounded_publishable {
        return Err(HandlerError::BadRequest(
            "session has no unpublished retained candidate".into(),
        ));
    }
    let publication_authority = candidate_publication_authority(&state, &session)?;
    let source_candidate = publication_authority
        .source_candidate_snapshot_hash
        .as_str();
    let candidate = publication_authority.candidate_snapshot_hash.as_str();
    let candidate_validation_hash = publication_authority.candidate_validation_hash.as_str();
    let base_snapshot_hash = publication_authority.base_snapshot_hash.as_str();
    let principal_key = publication_authority.principal_key.as_str();
    let project_hash = publication_authority.project_hash.as_str();
    let candidate_evaluation_hash = publication_authority.candidate_evaluation_hash.as_str();
    let published_result = format!("published:{candidate}");
    let already_published =
        session.publication_result.as_deref() == Some(published_result.as_str());
    let retained = match session.candidate_disposition {
        ryeos_app::runtime_db::DedicatedCandidateDisposition::OwnerDecision => "retained",
        ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview => {
            "retained_for_review"
        }
    };
    if session.publication_result.as_deref() != Some(retained) && !already_published {
        return Err(HandlerError::BadRequest(
            "session has no publishable retained candidate".into(),
        ));
    }
    if ryeos_state::refs::principal_storage_key(&ctx.fingerprint).map_err(internal)?
        != principal_key
    {
        return Err(HandlerError::Forbidden(
            "publication caller differs from the admitted principal HEAD authority".into(),
        ));
    }
    if req.expected_previous_hash != base_snapshot_hash {
        return Err(HandlerError::BadRequest(
            "publication expected hash differs from the admitted base generation".into(),
        ));
    }
    if already_published {
        let publication_root_id = session
            .candidate_disposition_root_id
            .as_deref()
            .ok_or_else(|| internal("published candidate has no publication root"))?;
        let publication_operation_id = session
            .candidate_disposition_operation_id
            .as_deref()
            .ok_or_else(|| internal("published candidate has no publication operation"))?;
        verify_candidate_publication_reservation(
            &state,
            &session,
            &publication_authority,
            publication_root_id,
            publication_operation_id,
        )?;
        let has_result = verify_candidate_publication_result(
            &state,
            &session,
            &publication_authority,
            publication_root_id,
            publication_operation_id,
            "hosted_candidate.published",
            "published",
        )?;
        if !has_result {
            return Err(internal(
                "published candidate has no exact rooted publication result",
            ));
        }
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "snapshot_hash":candidate,
            "previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "candidate_evaluation_hash":candidate_evaluation_hash,
            "published":true,
            "idempotent":true,
        }));
    }
    let project_lock = super::project_apply_snapshot::project_apply_lock(&project_hash);
    let _project_guard = project_lock.lock_owned().await;
    let pinned = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(internal)?;
    let cas_guard = pinned.acquire_shared_guard().map_err(internal)?;
    let cas = pinned.cas_store().map_err(internal)?;
    if !super::project_apply_snapshot::snapshot_history_contains(
        &cas,
        candidate,
        base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let publication_operation_id =
        candidate_publication_operation_id(&invocation_root_id, &session, &publication_authority)?;
    ryeos_app::authoritative_root_fact::append_once(
        &state,
        &invocation_root_id,
        "hosted_candidate.publication_reserved",
        &publication_operation_id,
        candidate_publication_reservation_payload(
            &session,
            &publication_authority,
            &invocation_root_id,
        ),
    )
    .map_err(internal)?;
    verify_candidate_publication_reservation(
        &state,
        &session,
        &publication_authority,
        &invocation_root_id,
        &publication_operation_id,
    )?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_publication(
            &session.placement_thread_id,
            source_candidate,
            &invocation_root_id,
            &publication_operation_id,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let current = state
        .state_store
        .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
        .map_err(internal)?;
    if current.as_deref() != Some(candidate) && current.as_deref() != Some(base_snapshot_hash) {
        ryeos_app::authoritative_root_fact::append_once(
            &state,
            &invocation_root_id,
            "hosted_candidate.publication_not_applied",
            &publication_operation_id,
            candidate_publication_result_payload(
                &session,
                &publication_authority,
                &invocation_root_id,
                &publication_operation_id,
                "not_applied",
                false,
            ),
        )
        .map_err(internal)?;
        state
            .state_store
            .fail_dedicated_candidate_disposition(&session.placement_thread_id, "publishing")
            .map_err(internal)?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
        return Err(HandlerError::BadRequest(format!(
            "publication conflict: expected HEAD {}, current HEAD is {:?}",
            base_snapshot_hash, current
        )));
    }
    let publication = if current.as_deref() == Some(candidate) {
        Ok(())
    } else {
        state.state_store.advance_project_head_ref(
            &principal_key,
            &project_hash,
            candidate,
            base_snapshot_hash,
            &signer,
            &cas_guard,
        )
    };
    if let Err(error) = publication {
        ryeos_app::authoritative_root_fact::append_once(
            &state,
            &invocation_root_id,
            "hosted_candidate.publication_unknown",
            &publication_operation_id,
            candidate_publication_result_payload(
                &session,
                &publication_authority,
                &invocation_root_id,
                &publication_operation_id,
                "unknown",
                false,
            ),
        )
        .map_err(internal)?;
        state
            .state_store
            .settle_dedicated_candidate_publication(
                &session.placement_thread_id,
                source_candidate,
                &invocation_root_id,
                &publication_operation_id,
                &format!("publication_unknown:{candidate}"),
            )
            .map_err(internal)?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
        return Err(HandlerError::Internal(format!(
            "candidate publication outcome is unknown after HEAD contact: {error}"
        )));
    }
    ryeos_app::authoritative_root_fact::append_once(
        &state,
        &invocation_root_id,
        "hosted_candidate.published",
        &publication_operation_id,
        candidate_publication_result_payload(
            &session,
            &publication_authority,
            &invocation_root_id,
            &publication_operation_id,
            "published",
            false,
        ),
    )
    .map_err(internal)?;
    state
        .state_store
        .settle_dedicated_candidate_publication(
            &session.placement_thread_id,
            source_candidate,
            &invocation_root_id,
            &publication_operation_id,
            &format!("published:{candidate}"),
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "snapshot_hash":candidate,
        "previous_hash":base_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
        "candidate_evaluation_hash":candidate_evaluation_hash,
        "published":true,
    }))
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
}

pub const STATUS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/status",
    endpoint: "worker-executions.status",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/status"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: StatusRequest = crate::handler_error::parse_request(params)?;
            status(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const CANDIDATE_RESULT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/candidate-result",
    endpoint: "worker-executions.candidate-result",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/candidate-result"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ryeos_app::hosted_candidate_result::HostedCandidateResultRequest =
                crate::handler_error::parse_request(params)?;
            candidate_result(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const CHECKPOINT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/checkpoint",
    endpoint: "worker-executions.checkpoint",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/checkpoint"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: CheckpointRequest = crate::handler_error::parse_request(params)?;
            checkpoint(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const RESUME_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/resume",
    endpoint: "worker-executions.resume",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/resume"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ResumeRequest = crate::handler_error::parse_request(params)?;
            resume(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const HANDOFF_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/handoff",
    endpoint: "worker-executions.handoff",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/handoff"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: HandoffRequest = crate::handler_error::parse_request(params)?;
            handoff(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const HANDOFF_PREFLIGHT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/handoff-preflight",
    endpoint: "worker-executions.handoff-preflight",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/handoff-preflight"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: HandoffPreflightRequest = crate::handler_error::parse_request(params)?;
            handoff_preflight(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const COMMAND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/command",
    endpoint: "worker-executions.command",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/command"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: CommandRequest = crate::handler_error::parse_request(params)?;
            command(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const COMMAND_OBSERVATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/command-observation",
    endpoint: "worker-executions.command-observation",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/command-observation"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: CommandObservationRequest = crate::handler_error::parse_request(params)?;
            command_observation(req, ctx, state)
                .await
                .map_err(Into::into)
        })
    },
};

pub const APPROVALS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/approvals",
    endpoint: "worker-executions.approvals",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.worker-executions/approvals"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ApprovalListRequest = crate::handler_error::parse_request(params)?;
            approvals(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const RESOLVE_APPROVAL_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/resolve-approval",
    endpoint: "worker-executions.resolve-approval",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/resolve-approval"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ApprovalResolveRequest = crate::handler_error::parse_request(params)?;
            resolve_approval(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const TERMINATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/terminate",
    endpoint: "worker-executions.terminate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/terminate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: TerminateRequest = crate::handler_error::parse_request(params)?;
            terminate(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const PUBLISH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/publish",
    endpoint: "worker-executions.publish",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.worker-executions/publish",
        ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
    ],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: PublishRequest = crate::handler_error::parse_request(params)?;
            publish(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const VALIDATE_CANDIDATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/validate-candidate-closure-and-base",
    endpoint: "worker-executions.validate-candidate-closure-and-base",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/validate-candidate-closure-and-base"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ValidateCandidateRequest = crate::handler_error::parse_request(params)?;
            validate_candidate_closure_and_base(req, ctx, state)
                .await
                .map_err(Into::into)
        })
    },
};

pub const START_CANDIDATE_EVALUATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/start-candidate-evaluation",
    endpoint: "worker-executions.start-candidate-evaluation",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/start-candidate-evaluation"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: StartCandidateEvaluationRequest = crate::handler_error::parse_request(params)?;
            start_candidate_evaluation(req, ctx, state)
                .await
                .map_err(Into::into)
        })
    },
};

pub const START_CANDIDATE_INTEGRATION_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/start-candidate-integration",
    endpoint: "worker-executions.start-candidate-integration",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/start-candidate-integration"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: StartCandidateIntegrationRequest =
                crate::handler_error::parse_request(params)?;
            start_candidate_integration(req, ctx, state)
                .await
                .map_err(Into::into)
        })
    },
};

pub const QUALIFY_CANDIDATE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/qualify-candidate",
    endpoint: "worker-executions.qualify-candidate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/qualify-candidate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: QualifyCandidateRequest = crate::handler_error::parse_request(params)?;
            qualify_candidate(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const DISCARD_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:worker-executions/discard",
    endpoint: "worker-executions.discard",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-executions/discard"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: DiscardRequest = crate::handler_error::parse_request(params)?;
            discard(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::{
        CandidatePublicationRecovery, CheckpointRequest, CommandObservationRequest, CommandRequest,
        HandoffPreflightRequest, HandoffRequest, QualifyCandidateRequest, ResumeRequest,
        StartCandidateEvaluationRequest, StartCandidateIntegrationRequest,
        classify_candidate_publication_recovery, decode_worker_handoff_service_response,
        exact_reaped_source_worker_authority, handoff_status_value, latest_handoff_job_for_chain,
        validate_source_handoff_job_coordinates,
    };
    use ryeos_app::worker_handoff::{
        WORKER_SESSION_HANDOFF_OPERATION, WorkerHandoffJobRole, WorkerSessionHandoffJobOperation,
    };
    use ryeos_state::{NewSyncJob, StateDb, TrustStore};

    #[test]
    fn candidate_return_preserves_interactive_and_completed_bounded_root_lifecycles() {
        use ryeos_app::runtime_db::DedicatedCandidateDisposition::{
            OwnerDecision, RetainedForReview,
        };
        use ryeos_state::objects::ThreadStatus;
        for session_state in ["frozen", "publish_ready"] {
            assert_eq!(
                super::candidate_result_root_status(
                    &OwnerDecision,
                    session_state,
                    Some("completed"),
                    Some("retained"),
                )
                .unwrap(),
                ThreadStatus::Running
            );
        }
        for session_state in ["terminal", "publish_ready"] {
            assert_eq!(
                super::candidate_result_root_status(
                    &RetainedForReview,
                    session_state,
                    Some("completed"),
                    Some("retained_for_review"),
                )
                .unwrap(),
                ThreadStatus::Completed
            );
        }
        for (disposition, state, reason, publication) in [
            (OwnerDecision, "terminal", "completed", "retained"),
            (
                OwnerDecision,
                "publish_ready",
                "completed",
                "retained_for_review",
            ),
            (
                RetainedForReview,
                "terminal",
                "cancelled",
                "retained_for_review",
            ),
            (RetainedForReview, "terminal", "completed", "retained"),
            (
                RetainedForReview,
                "outcome_unknown",
                "completed",
                "retained_for_review",
            ),
            (
                RetainedForReview,
                "qualifying",
                "completed",
                "retained_for_review",
            ),
            (
                RetainedForReview,
                "terminal",
                "completed",
                "publication_unknown",
            ),
            (RetainedForReview, "terminal", "completed", "discarded"),
        ] {
            assert!(
                super::candidate_result_root_status(
                    &disposition,
                    state,
                    Some(reason),
                    Some(publication),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn bounded_candidate_return_rejects_changed_or_unrooted_terminal_testimony() {
        let mut testimony = serde_json::json!({
            "schema":"ryeos.bounded_worker_outcome.v1",
            "workspace":{"candidate_snapshot_hash":"c".repeat(64)},
            "outcome":{"kind":"completed","dimension":null,"approval":null},
        });
        let operation_id = ryeos_state::objects::canonical_value_digest(&testimony).unwrap();
        assert!(
            super::verify_candidate_result_terminal_fact_digest(&testimony, &operation_id).is_err()
        );
        testimony["operation_id"] = operation_id.clone().into();
        super::verify_candidate_result_terminal_fact_digest(&testimony, &operation_id).unwrap();
        let mut changed = testimony.clone();
        changed["workspace"]["candidate_snapshot_hash"] = "d".repeat(64).into();
        assert!(
            super::verify_candidate_result_terminal_fact_digest(&changed, &operation_id).is_err()
        );
        changed = testimony;
        changed["outcome"]["kind"] = "outcome_unknown".into();
        assert!(
            super::verify_candidate_result_terminal_fact_digest(&changed, &operation_id).is_err()
        );
    }

    #[test]
    fn candidate_terminal_uses_lifecycle_status_not_execution_outcome_labels() {
        use ryeos_state::objects::{ThreadSnapshotBuilder, ThreadStatus};
        let mut terminal = ThreadSnapshotBuilder::new(
            "T-evaluator",
            "T-evaluator",
            "tool",
            "tool:test/evaluate",
            "runtime:test",
        )
        .status(ThreadStatus::Completed)
        .build();
        // The installed subprocess evaluator reported this exact terminal
        // combination. No outcome-code allowlist belongs at this boundary.
        for outcome in [
            None,
            Some("success"),
            Some("exit:0"),
            Some("verifier_complete"),
        ] {
            terminal.outcome_code = outcome.map(str::to_owned);
            assert!(super::candidate_execution_succeeded(&terminal));
        }
        terminal.outcome_code = Some("exit:0".to_owned());
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            terminal.status = status;
            assert!(!super::candidate_execution_succeeded(&terminal));
        }
        terminal.status = ThreadStatus::Completed;
        terminal.error = Some(serde_json::json!({"code":"execution_failed"}));
        assert!(!super::candidate_execution_succeeded(&terminal));
    }

    #[test]
    fn candidate_evaluator_completion_does_not_override_negative_acceptance() {
        let candidate = "c".repeat(64);
        let base = "b".repeat(64);
        for accepted in [true, false] {
            let value = serde_json::json!({
                "schema_version":1,
                "candidate_snapshot_hash":candidate,
                "base_snapshot_hash":base,
                "accepted":accepted,
                "evidence":{},
            });
            let result =
                super::validate_candidate_evaluation_result(value.clone(), &candidate, &base)
                    .unwrap();
            assert_eq!(result.accepted, accepted);
            // A rejected candidate remains a valid negative evaluation, never
            // positive testimony merely because its evaluator completed.
            assert!(
                super::validate_candidate_evaluation_result(value.clone(), &base, &base).is_err()
            );
            assert!(
                super::validate_candidate_evaluation_result(value, &candidate, &candidate).is_err()
            );
        }
    }

    #[test]
    fn bounded_candidate_return_waits_for_exact_root_finalization() {
        use ryeos_state::objects::{ThreadSnapshotBuilder, ThreadStatus};
        let candidate = "c".repeat(64);
        let mut root = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "worker_execution",
            "worker_execution:test",
            "runtime:test",
        )
        .status(ThreadStatus::Running)
        .build();
        // A crash can leave the session retained/terminal before the runner
        // finalizes its root. A read must not manufacture that finalization.
        assert!(super::require_bounded_candidate_result_terminal(&root, &candidate).is_err());
        root.status = ThreadStatus::Completed;
        root.outcome_code = Some("success".to_owned());
        root.result_project_snapshot_hash = Some(candidate.clone());
        super::require_bounded_candidate_result_terminal(&root, &candidate).unwrap();
        root.outcome_code = Some("exit:0".to_owned());
        super::require_bounded_candidate_result_terminal(&root, &candidate).unwrap();
        root.result_project_snapshot_hash = Some("d".repeat(64));
        assert!(super::require_bounded_candidate_result_terminal(&root, &candidate).is_err());
        root.result_project_snapshot_hash = Some(candidate.clone());
        root.error = Some(serde_json::json!({"error":"outcome_unknown"}));
        assert!(super::require_bounded_candidate_result_terminal(&root, &candidate).is_err());
    }

    #[test]
    fn discard_recovery_requires_exact_absence_or_unique_committed_fact() {
        let expected = serde_json::json!({
            "operation_id":"a".repeat(64),
            "candidate_snapshot_hash":"c".repeat(64),
        });
        assert!(!super::candidate_discard_fact_committed(0, None, &expected).unwrap());
        assert!(super::candidate_discard_fact_committed(1, Some(&expected), &expected).unwrap());
        assert!(super::candidate_discard_fact_committed(2, Some(&expected), &expected).is_err());
        assert!(super::candidate_discard_fact_committed(1, None, &expected).is_err());
        assert!(super::candidate_discard_fact_committed(0, Some(&expected), &expected).is_err());
        let other = serde_json::json!({"candidate_snapshot_hash":"d".repeat(64)});
        assert!(super::candidate_discard_fact_committed(1, Some(&other), &expected).is_err());
    }

    fn status_job(
        operation: &WorkerSessionHandoffJobOperation,
        state: ryeos_state::SyncJobState,
        phase: &str,
        result: serde_json::Value,
    ) -> ryeos_state::SyncJobRecord {
        ryeos_state::SyncJobRecord {
            job_id: format!("worker-handoff-target:{}", operation.operation_id),
            operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some("source".to_owned()),
            state,
            phase: phase.to_owned(),
            roots: Vec::new(),
            heads: Vec::new(),
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 1,
            max_attempts: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            last_error: None,
            result: Some(result),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:00:00Z".to_owned()),
        }
    }

    fn status_handoff_operation(
        index: usize,
        chain_root_id: &str,
    ) -> WorkerSessionHandoffJobOperation {
        let hash = |offset: usize| format!("{:064x}", index + offset);
        WorkerSessionHandoffJobOperation::new(
            WorkerHandoffJobRole::Target,
            hash(1),
            hash(1_001),
            hash(2_001),
            "fp:owner".to_owned(),
            chain_root_id.to_owned(),
            "site:origin".to_owned(),
            "site:source".to_owned(),
            "site:target".to_owned(),
            format!("T-source-{index}"),
            format!("T-successor-{index}"),
            hash(3_001),
            hash(4_001),
            hash(5_001),
            hash(6_001),
            "source".to_owned(),
            "/source/project".to_owned(),
            "/target/project".to_owned(),
            hash(7_001),
            "credential:target".to_owned(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn status_finds_a_handoff_older_than_the_former_global_job_cap() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = StateDb::open(tempdir.path(), std::sync::Arc::new(TrustStore::new())).unwrap();
        for index in 0..520 {
            let chain_root_id = if index == 0 {
                "T-sought".to_owned()
            } else {
                format!("T-unrelated-{index}")
            };
            let operation = status_handoff_operation(index, &chain_root_id);
            db.create_sync_job(&NewSyncJob {
                job_id: format!("worker-handoff-target:{}", operation.operation_id),
                operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
                operation: operation.to_value().unwrap(),
                peer: Some("source".to_owned()),
                roots: Vec::new(),
                heads: Vec::new(),
                max_attempts: 1,
            })
            .unwrap();
        }

        let (job, operation) = latest_handoff_job_for_chain(&db, "T-sought")
            .unwrap()
            .expect("older exact-chain handoff must remain discoverable");
        assert_eq!(operation.chain_root_id, "T-sought");
        assert_eq!(
            job.job_id,
            format!("worker-handoff-target:{}", operation.operation_id)
        );
    }

    #[test]
    fn status_rejects_terminal_job_rows_without_signed_receipt_authority() {
        let completed_operation = status_handoff_operation(7_001, "T-completed");
        let completed = handoff_status_value(
            None,
            status_job(
                &completed_operation,
                ryeos_state::SyncJobState::Completed,
                "completed",
                serde_json::to_value(ryeos_app::worker_handoff::WorkerPlacementAdoptResponse {
                    operation_id: completed_operation.operation_id.clone(),
                    chain_root_id: completed_operation.chain_root_id.clone(),
                    placement_thread_id: completed_operation.successor_placement_thread_id.clone(),
                    target_chain_head_hash: "a".repeat(64),
                    delivery: "attached".to_owned(),
                })
                .unwrap(),
            ),
            completed_operation,
        );
        assert!(completed.is_err());

        let cancelled_operation = status_handoff_operation(8_001, "T-cancelled");
        let cancelled = handoff_status_value(
            None,
            status_job(
                &cancelled_operation,
                ryeos_state::SyncJobState::Cancelled,
                "aborted",
                serde_json::to_value(ryeos_app::worker_handoff::WorkerPlacementAbortResponse {
                    operation_id: cancelled_operation.operation_id.clone(),
                    chain_root_id: cancelled_operation.chain_root_id.clone(),
                    disposition: "target_absent".to_owned(),
                })
                .unwrap(),
            ),
            cancelled_operation,
        );
        assert!(cancelled.is_err());
    }

    #[test]
    fn source_handoff_job_coordinate_cannot_alias_id_or_peer() {
        let mut operation = status_handoff_operation(9_001, "T-source-coordinate");
        operation.role = WorkerHandoffJobRole::Source;
        operation.validate().unwrap();
        let mut job = status_job(
            &operation,
            ryeos_state::SyncJobState::Running,
            "source_committed",
            ryeos_app::worker_handoff::WorkerSessionHandoffProgress::planned(
                operation.operation_id.clone(),
            )
            .unwrap()
            .to_value()
            .unwrap(),
        );
        assert!(validate_source_handoff_job_coordinates(&job, &operation).is_err());

        job.job_id = format!("worker-handoff-source:{}", operation.operation_id);
        assert!(validate_source_handoff_job_coordinates(&job, &operation).is_ok());
        job.peer = Some("another-source".to_owned());
        assert!(validate_source_handoff_job_coordinates(&job, &operation).is_err());
    }

    #[test]
    fn qualification_evidence_is_separate_from_the_closed_handoff_response() {
        let response = ryeos_app::worker_handoff::WorkerPlacementAdoptResponse {
            operation_id: "1".repeat(64),
            chain_root_id: "T-root".to_owned(),
            placement_thread_id: "T-successor".to_owned(),
            target_chain_head_hash: "2".repeat(64),
            delivery: "attached".to_owned(),
        };
        let mut wire = serde_json::to_value(&response).unwrap();
        wire["qualification_measurements"] = serde_json::json!({"event_replay_ms": 7});

        let (decoded, measurements): (
            ryeos_app::worker_handoff::WorkerPlacementAdoptResponse,
            Option<serde_json::Value>,
        ) = decode_worker_handoff_service_response(wire).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(measurements.unwrap()["event_replay_ms"], 7);

        let mut invalid = serde_json::to_value(response).unwrap();
        invalid["unrecognized_authority"] = serde_json::json!(true);
        assert!(
            decode_worker_handoff_service_response::<
                ryeos_app::worker_handoff::WorkerPlacementAdoptResponse,
            >(invalid)
            .is_err()
        );
    }

    #[test]
    fn hosted_command_address_is_only_the_stable_chain_root() {
        let accepted = serde_json::from_value::<CommandRequest>(serde_json::json!({
            "chain_root_id":"T-root",
            "idempotency_key":"turn-1",
            "route_id":"turn.start",
            "payload":{},
        }));
        assert!(accepted.is_ok());

        for forbidden_field in ["session_id", "placement_thread_id"] {
            let mut value = serde_json::json!({
                "idempotency_key":"turn-1",
                "route_id":"turn.start",
                "payload":{},
            });
            value
                .as_object_mut()
                .expect("request fixture object")
                .insert(forbidden_field.to_owned(), serde_json::json!("T-root"));
            assert!(serde_json::from_value::<CommandRequest>(value).is_err());
        }
    }

    #[test]
    fn hosted_command_observation_requires_its_historical_placement_coordinate() {
        let accepted = serde_json::from_value::<CommandObservationRequest>(serde_json::json!({
            "chain_root_id":"T-root",
            "placement_thread_id":"T-placement",
            "command_sequence":2,
        }));
        assert!(accepted.is_ok());

        for missing_field in ["chain_root_id", "placement_thread_id", "command_sequence"] {
            let mut value = serde_json::json!({
                "chain_root_id":"T-root",
                "placement_thread_id":"T-placement",
                "command_sequence":2,
            });
            value
                .as_object_mut()
                .expect("request fixture object")
                .remove(missing_field);
            assert!(serde_json::from_value::<CommandObservationRequest>(value).is_err());
        }

        assert!(
            serde_json::from_value::<CommandObservationRequest>(serde_json::json!({
                "chain_root_id":"T-root",
                "placement_thread_id":"T-placement",
                "command_sequence":2,
                "latest":true,
            }))
            .is_err()
        );
    }

    #[test]
    fn hosted_checkpoint_address_is_only_the_stable_chain_root() {
        assert!(
            serde_json::from_value::<CheckpointRequest>(serde_json::json!({
                "chain_root_id":"T-root",
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<CheckpointRequest>(serde_json::json!({
                "chain_root_id":"T-root",
                "placement_thread_id":"T-placement",
            }))
            .is_err()
        );
    }

    #[test]
    fn hosted_resume_address_is_only_the_stable_chain_root_and_manifest() {
        assert!(
            serde_json::from_value::<ResumeRequest>(serde_json::json!({
                "chain_root_id":"T-root",
                "manifest_ref":format!("cas:{}", "a".repeat(64)),
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<ResumeRequest>(serde_json::json!({
                "chain_root_id":"T-root",
                "manifest_ref":format!("cas:{}", "a".repeat(64)),
                "placement_thread_id":"T-placement",
            }))
            .is_err()
        );
    }

    #[test]
    fn hosted_handoff_requires_the_exact_completed_preflight_coordinate() {
        let complete = serde_json::json!({
            "chain_root_id":"T-root",
            "manifest_ref":format!("cas:{}", "a".repeat(64)),
            "remote":"target",
            "target_credential_profile_id":"profile-target",
            "preflight_id":"b".repeat(64),
        });
        assert!(serde_json::from_value::<HandoffRequest>(complete.clone()).is_ok());

        let mut missing = complete;
        missing
            .as_object_mut()
            .expect("request fixture object")
            .remove("preflight_id");
        assert!(serde_json::from_value::<HandoffRequest>(missing).is_err());
    }

    #[test]
    fn hosted_handoff_preflight_has_only_non_final_placement_inputs() {
        let request = serde_json::json!({
            "chain_root_id":"T-root",
            "remote":"target",
            "target_credential_profile_id":"profile-target",
        });
        assert!(serde_json::from_value::<HandoffPreflightRequest>(request.clone()).is_ok());

        let mut final_input = request;
        final_input
            .as_object_mut()
            .expect("request fixture object")
            .insert(
                "manifest_ref".to_owned(),
                serde_json::json!(format!("cas:{}", "a".repeat(64))),
            );
        assert!(serde_json::from_value::<HandoffPreflightRequest>(final_input).is_err());
    }

    #[test]
    fn hosted_handoff_fences_the_exact_reaped_source_boot_epoch() {
        use ryeos_app::runtime_db::WorkerProcessState;

        assert!(exact_reaped_source_worker_authority(
            "T-placement",
            Some(7),
            "T-placement",
            7,
            WorkerProcessState::Dead,
            "reaped",
        ));
        assert!(!exact_reaped_source_worker_authority(
            "T-placement",
            Some(8),
            "T-placement",
            7,
            WorkerProcessState::Dead,
            "reaped",
        ));
    }

    #[test]
    fn publication_recovery_only_retries_when_head_is_still_the_admitted_base() {
        assert_eq!(
            classify_candidate_publication_recovery(Some("candidate"), "base", "candidate"),
            CandidatePublicationRecovery::Published
        );
        assert_eq!(
            classify_candidate_publication_recovery(Some("base"), "base", "candidate"),
            CandidatePublicationRecovery::NotPublished
        );
        assert_eq!(
            classify_candidate_publication_recovery(Some("later"), "base", "candidate"),
            CandidatePublicationRecovery::Unknown
        );
        assert_eq!(
            classify_candidate_publication_recovery(None, "base", "candidate"),
            CandidatePublicationRecovery::Unknown
        );
    }

    #[test]
    fn candidate_operations_require_exact_integration_coordinates() {
        let hash = || "a".repeat(64);
        let coordinate = serde_json::json!({
            "chain_root_id":"T-integration",
            "terminal_thread_id":"T-integration",
            "capsule_hash":hash(),
        });
        assert!(
            serde_json::from_value::<StartCandidateEvaluationRequest>(serde_json::json!({
                "chain_root_id":"T-source",
                "candidate_snapshot_hash":hash(),
                "candidate_validation_hash":hash(),
                "evaluator_item_ref":"tool:project/evaluate",
                "evaluator_parameters":{},
                "evaluator_ref_bindings":{},
                "integration":coordinate,
                "launch_id":"L-evaluate",
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<StartCandidateIntegrationRequest>(serde_json::json!({
                "chain_root_id":"T-source",
                "candidate_snapshot_hash":hash(),
                "candidate_validation_hash":hash(),
                "accepted_evaluation_hash":hash(),
                "authoring_wrapper_item_ref":"tool:project/integrate",
                "authoring_parameters":{},
                "authoring_ref_bindings":{},
                "launch_id":"L-integrate",
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<QualifyCandidateRequest>(serde_json::json!({
                "chain_root_id":"T-source",
                "candidate_snapshot_hash":hash(),
                "candidate_validation_hash":hash(),
                "evaluator_chain_root_id":"T-evaluator",
                "evaluator_terminal_thread_id":"T-evaluator",
                "evaluator_capsule_hash":hash(),
                "evaluator_item_ref":"tool:project/evaluate",
                "evaluator_definition_digest":hash(),
                "evaluator_parameters_digest":hash(),
                "integration":{
                    "chain_root_id":"T-integration",
                    "terminal_thread_id":"T-integration"
                }
            }))
            .is_err(),
            "qualification must not accept an incomplete integration coordinate"
        );
    }
}
