//! Node-authenticated terminal delivery for followed executions placed on a
//! different RyeOS site.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use crate::remote::client::{
    NodeAdmittedObjectsClosureRequestOptions, ObjectsClosureRequestOptions, RemoteClient,
};
use crate::remote::{config, import};
use ryeos_app::federated_follow::{
    REMOTE_FOLLOW_DELIVERY_OPERATION, REMOTE_FOLLOW_DELIVERY_SERVICE,
    RemoteFollowDeliveryJobOperation, RemoteFollowDeliveryJobRole, RemoteFollowReservationEvidence,
    RemoteFollowTerminalDeliveryRequest, RemoteFollowTerminalDeliveryResponse,
    RemoteFollowTerminalEvidence,
};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const DELIVERY_EVENT: &str = "remote_follow_terminal.delivered";

fn authenticated_target_node(
    ctx: &HandlerContext,
    target_site_id: &str,
) -> Result<String, HandlerError> {
    if !ctx.verified
        || ctx.authorized_key_class
            != Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode)
        || ctx.authenticated_origin_site_id.as_deref() != Some(target_site_id)
    {
        return Err(HandlerError::Forbidden(
            "authenticated target-node authority required".into(),
        ));
    }
    Ok(ctx.fingerprint.clone())
}

fn delivery_lock(operation_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("remote follow delivery lock map poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(operation_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(operation_id.to_owned(), Arc::downgrade(&lock));
    lock
}

pub async fn deliver(
    req: RemoteFollowTerminalDeliveryRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let authenticated_target_principal = authenticated_target_node(&ctx, &req.target_site_id)?;
    if req.parent_site_id != state.threads.site_id() {
        return Err(HandlerError::Forbidden(
            "remote follow delivery differs from authenticated authority".into(),
        ));
    }
    let _guard = delivery_lock(&req.operation_id).lock_owned().await;
    deliver_locked(req, authenticated_target_principal, state).await
}

async fn deliver_locked(
    req: RemoteFollowTerminalDeliveryRequest,
    authenticated_target_principal: String,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let target_remote = config::resolve_remote_by_site_id(&remotes, &req.target_site_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if target_remote.remote.principal_id != authenticated_target_principal {
        return Err(HandlerError::Forbidden(
            "terminal delivery caller differs from configured target node".into(),
        ));
    }
    let target_key = target_remote
        .remote
        .pinned_signing_key()
        .map_err(internal)?;
    let target_signer = target_remote
        .remote
        .principal_id
        .strip_prefix("fp:")
        .ok_or_else(|| internal("configured target principal is not a fingerprint"))?;
    let target_client = RemoteClient::from_remote_cfg(&state, &target_remote.remote);
    let reservation_value =
        load_local_object(&state, &req.reservation_attestation_hash).map_err(internal)?;
    let reservation_data = lillux::canonical_json(&reservation_value)
        .map_err(internal)?
        .into_bytes();
    let reservation_entry = ryeos_state::sync::SyncEntry {
        hash: req.reservation_attestation_hash.clone(),
        is_blob: false,
        data: reservation_data,
    };
    let closure_options = NodeAdmittedObjectsClosureRequestOptions::for_node(
        &state,
        ObjectsClosureRequestOptions {
            allow_incomplete: false,
            allow_untransported_large_objects: true,
            ..Default::default()
        },
    )
    .map_err(internal)?;
    let fetch_options = closure_options
        .reserving_supplemental_entry(&reservation_entry)
        .map_err(internal)?;
    let closure = target_client
        .objects_closure_get(
            &[
                req.target_chain_head_hash.clone(),
                req.terminal_attestation_hash.clone(),
            ],
            fetch_options,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(
                error,
                "fetch remote follow terminal closure",
            )
        })?;
    let terminal_value = closure
        .entries
        .iter()
        .find(|entry| entry.kind == "object" && entry.hash == req.terminal_attestation_hash)
        .and_then(|entry| entry.value.clone())
        .ok_or_else(|| internal("terminal closure omitted its receipt"))?;
    if ryeos_state::objects::canonical_value_digest(&terminal_value).map_err(internal)?
        != req.terminal_attestation_hash
    {
        return Err(HandlerError::BadRequest(
            "remote follow terminal receipt changed digest".into(),
        ));
    }
    let terminal_attestation = ryeos_state::objects::Attestation::from_value(&terminal_value)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    terminal_attestation
        .verify_with_key(&target_key)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let terminal = RemoteFollowTerminalEvidence::from_attestation(&terminal_attestation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if terminal.operation_id != req.operation_id
        || terminal.reservation_attestation_hash != req.reservation_attestation_hash
        || terminal.child_chain_root_id != req.child_chain_root_id
        || terminal.target_site_id != req.target_site_id
        || terminal.target_node_signer_fingerprint != target_signer
        || terminal.target_chain_head_hash != req.target_chain_head_hash
    {
        return Err(HandlerError::BadRequest(
            "remote follow terminal receipt contradicts its delivery request".into(),
        ));
    }

    let reservation_attestation = ryeos_state::objects::Attestation::from_value(&reservation_value)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    reservation_attestation
        .verify_with_key(state.identity.verifying_key())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let reservation = RemoteFollowReservationEvidence::from_attestation(&reservation_attestation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if reservation.parent_site_id != req.parent_site_id
        || reservation.parent_node_signer_fingerprint != state.identity.fingerprint()
        || reservation.child_chain_root_id != req.child_chain_root_id
    {
        return Err(HandlerError::BadRequest(
            "remote follow delivery differs from its parent reservation".into(),
        ));
    }
    validate_parent_waiter(&state, &reservation, &terminal).map_err(|error| {
        HandlerError::BadRequest(format!("remote follow parent reservation: {error:#}"))
    })?;
    let source_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &req.child_chain_root_id))
        .map_err(internal)?
        .ok_or_else(|| internal("parent site lost its followed-child source head"))?;
    if source_head.signer != state.identity.fingerprint() {
        return Err(internal(
            "parent followed-child source head is not locally owned",
        ));
    }

    let mut payload = import::closure_response_to_export_payload(
        &req.child_chain_root_id,
        &req.target_chain_head_hash,
        &closure.entries,
    )
    .map_err(internal)?;
    import::append_admitted_supplemental_entry(
        &mut payload,
        reservation_entry,
        closure_options.limits(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let operation = RemoteFollowDeliveryJobOperation::new(
        RemoteFollowDeliveryJobRole::Parent,
        req.operation_id.clone(),
        req.reservation_attestation_hash.clone(),
        reservation.owner_principal.clone(),
        req.child_chain_root_id.clone(),
        req.parent_site_id.clone(),
        req.target_site_id.clone(),
    )
    .map_err(internal)?;
    let job_id = format!("remote-follow-terminal-parent:{}", req.operation_id);
    let (_, job) = state
        .state_store
        .stage_sync_payload_and_create_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(target_remote.remote.principal_id.clone()),
                source_peer: Some(target_remote.config_key.clone()),
                job_id: Some(job_id.clone()),
            },
            &ryeos_state::NewSyncJob {
                job_id: job_id.clone(),
                operation_type: REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
                operation: operation.to_value().map_err(internal)?,
                peer: Some(target_remote.config_key.clone()),
                roots: vec![
                    req.reservation_attestation_hash.clone(),
                    req.terminal_attestation_hash.clone(),
                    req.target_chain_head_hash.clone(),
                ],
                heads: vec![req.target_chain_head_hash.clone()],
                max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
            },
        )
        .map_err(internal)?;
    if job.state == ryeos_state::SyncJobState::Completed {
        let response: RemoteFollowTerminalDeliveryResponse = serde_json::from_value(
            job.result
                .ok_or_else(|| internal("completed remote follow delivery has no result"))?,
        )
        .map_err(internal)?;
        response
            .validate_against(&req)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        return serde_json::to_value(response).map_err(internal);
    }
    let attempt_id = format!("remote-follow-parent-attempt:{}", uuid::Uuid::new_v4());
    state
        .state_store
        .with_state_db(|db| {
            db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
                attempt_id: attempt_id.clone(),
                job_id: job_id.clone(),
                worker_id: Some("remote-follow-parent-delivery".to_owned()),
                phase: "settling_parent".to_owned(),
            })?;
            Ok(())
        })
        .map_err(internal)?;

    let outcome = (|| -> Result<Value, HandlerError> {
        let authority = state
            .state_store
            .pinned_state_authority()
            .map_err(internal)?;
        let guard = authority.acquire_shared_guard().map_err(internal)?;
        ryeos_state::sync::verify_chain_closure_anchored_pinned(
            &authority.cas_store().map_err(internal)?,
            &req.child_chain_root_id,
            &req.target_chain_head_hash,
            &source_head.target_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        drop(guard);

        let fact_payload = serde_json::json!({
            "schema":"ryeos.remote_follow_terminal_delivery.v1",
            "operation_id":req.operation_id,
            "reservation_attestation_hash":req.reservation_attestation_hash,
            "terminal_attestation_hash":req.terminal_attestation_hash,
            "child_chain_root_id":req.child_chain_root_id,
            "child_terminal_thread_id":terminal.child_terminal_thread_id,
            "terminal_status":terminal.terminal_status,
            "terminal_envelope_digest":terminal.terminal_envelope_digest,
            "target_site_id":req.target_site_id,
            "target_chain_head_hash":req.target_chain_head_hash,
            "target_last_event_hash":terminal.target_last_event_hash,
        });
        ryeos_app::authoritative_root_fact::append_once_to_created_thread(
            &state,
            &reservation.parent_successor_thread_id,
            DELIVERY_EVENT,
            &req.operation_id,
            fact_payload,
        )
        .map_err(internal)?;
        if let Some(waiter) = state
            .state_store
            .get_follow_waiter_by_child_chain(&req.child_chain_root_id)
            .map_err(internal)?
        {
            validate_waiter_tuple(&state, &reservation, &terminal, &waiter).map_err(internal)?;
            state
                .state_store
                .mark_follow_child_terminal(
                    &req.child_chain_root_id,
                    &terminal.child_terminal_thread_id,
                    &terminal.terminal_status,
                    &terminal.terminal_envelope,
                )
                .map_err(internal)?;
        } else {
            let successor = state
                .state_store
                .get_thread(&reservation.parent_successor_thread_id)
                .map_err(internal)?
                .ok_or_else(|| internal("remote follow parent successor disappeared"))?;
            if successor.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
                return Err(internal(
                    "remote follow waiter disappeared before its successor was launched",
                ));
            }
        }
        ryeos_executor::execution::launch::kick_follow_resume_if_ready(
            &state,
            &req.child_chain_root_id,
        );
        let response = RemoteFollowTerminalDeliveryResponse {
            operation_id: req.operation_id.clone(),
            child_chain_root_id: req.child_chain_root_id.clone(),
            parent_chain_root_id: reservation.parent_chain_root_id.clone(),
            parent_successor_thread_id: reservation.parent_successor_thread_id.clone(),
            delivery: "settled".to_owned(),
        };
        response.validate_against(&req).map_err(internal)?;
        serde_json::to_value(response).map_err(internal)
    })();

    match outcome {
        Ok(response_value) => {
            state
                .state_store
                .with_state_db(|db| {
                    let latest = db
                        .get_sync_job(&job_id)?
                        .context("remote follow parent delivery job disappeared")?;
                    let mut fetched_hashes = latest.fetched_hashes;
                    fetched_hashes.push(req.target_chain_head_hash);
                    fetched_hashes.sort();
                    fetched_hashes.dedup();
                    db.finish_sync_job_attempt_and_update_job(
                        &attempt_id,
                        &ryeos_state::FinishSyncJobAttempt {
                            state: ryeos_state::SyncJobAttemptState::Completed,
                            phase: "settled".to_owned(),
                            error: None,
                            result: Some(response_value.clone()),
                        },
                        &job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Completed,
                            phase: "settled".to_owned(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: latest.uploaded_hashes,
                            fetched_hashes,
                            last_error: None,
                            result: Some(response_value.clone()),
                        },
                    )
                })
                .map_err(internal)?;
            Ok(response_value)
        }
        Err(error) => {
            settle_attempt(
                &state,
                &job,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                "settlement_retryable",
                Some(bounded_error(&error.to_string())),
                None,
            )
            .map_err(internal)?;
            Err(error)
        }
    }
}

fn validate_parent_waiter(
    state: &AppState,
    reservation: &RemoteFollowReservationEvidence,
    terminal: &RemoteFollowTerminalEvidence,
) -> Result<()> {
    let parent_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &reservation.parent_chain_root_id))?
        .context("remote follow parent chain head disappeared")?;
    if parent_head.signer != state.identity.fingerprint() {
        bail!("remote follow parent chain is not locally owned");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store()?,
        &reservation.parent_chain_root_id,
        &parent_head.target_hash,
        &reservation.parent_chain_head_hash,
    )?;
    drop(guard);
    if let Some(waiter) = state
        .state_store
        .get_follow_waiter_by_child_chain(&reservation.child_chain_root_id)?
    {
        validate_waiter_tuple(state, reservation, terminal, &waiter)?;
    } else {
        let successor = state
            .state_store
            .get_thread(&reservation.parent_successor_thread_id)?
            .context("remote follow parent successor disappeared")?;
        if successor.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
            bail!("remote follow waiter is absent while its successor remains unstarted");
        }
    }
    Ok(())
}

fn validate_waiter_tuple(
    state: &AppState,
    reservation: &RemoteFollowReservationEvidence,
    terminal: &RemoteFollowTerminalEvidence,
    waiter: &ryeos_app::runtime_db::FollowWaiter,
) -> Result<()> {
    let child = waiter
        .children
        .iter()
        .find(|child| child.child_chain_root_id == reservation.child_chain_root_id)
        .context("parent waiter lost its reserved followed child")?;
    let parent = state
        .state_store
        .get_thread(&reservation.parent_thread_id)?
        .context("remote follow parent disappeared")?;
    let successor = state
        .state_store
        .get_thread(&reservation.parent_successor_thread_id)?
        .context("remote follow parent successor disappeared")?;
    if waiter.follow_key != reservation.follow_key
        || waiter.parent_thread_id != reservation.parent_thread_id
        || waiter.parent_chain_root_id != reservation.parent_chain_root_id
        || waiter.parent_successor_thread_id.as_deref()
            != Some(reservation.parent_successor_thread_id.as_str())
        || child.item_index != reservation.child_item_index
        || child.item_ref != reservation.child_item_ref
        || child.spec_hash != reservation.child_spec_hash
        || child.child_thread_id != reservation.child_initial_thread_id
        || parent.requested_by.as_deref() != Some(reservation.owner_principal.as_str())
        || parent.chain_root_id != reservation.parent_chain_root_id
        || successor.requested_by.as_deref() != Some(reservation.owner_principal.as_str())
        || successor.chain_root_id != reservation.parent_chain_root_id
        || successor.upstream_thread_id.as_deref() != Some(reservation.parent_thread_id.as_str())
        || terminal.child_chain_root_id != reservation.child_chain_root_id
    {
        bail!("live parent waiter differs from its signed delivery reservation");
    }
    let stored_tuple = (
        child.terminal_thread_id.as_deref(),
        child.terminal_status.as_deref(),
        child.terminal_envelope.as_ref(),
    );
    match stored_tuple {
        (None, None, None) => {}
        (Some(thread_id), Some(status), Some(envelope)) => {
            if thread_id != terminal.child_terminal_thread_id
                || status != terminal.terminal_status
                || ryeos_state::objects::canonical_value_digest(envelope)?
                    != ryeos_state::objects::canonical_value_digest(&terminal.terminal_envelope)?
            {
                bail!("parent waiter is already settled by another terminal delivery");
            }
        }
        _ => bail!("parent waiter contains a partial terminal delivery tuple"),
    }
    Ok(())
}

fn load_local_object(state: &AppState, hash: &str) -> Result<Value> {
    if !lillux::valid_hash(hash) {
        bail!("local object hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(hash)?
        .context("local object is absent from CAS")?;
    if ryeos_state::objects::canonical_value_digest(&value)? != hash {
        bail!("local object changed digest");
    }
    Ok(value)
}

/// Retry target-owned terminal deliveries from durable jobs. No parent token
/// or waiter state is present on the target; the admitted operator identity
/// and source-signed reservation are the complete network authority.
pub async fn recover_durable_remote_follow_deliveries(state: &AppState) -> Result<usize> {
    let mut recovered = 0usize;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            db.list_active_sync_jobs_by_operation_type_after(
                REMOTE_FOLLOW_DELIVERY_OPERATION,
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
            if job.state == ryeos_state::SyncJobState::Running {
                continue;
            }
            let operation =
                match RemoteFollowDeliveryJobOperation::from_value(job.operation.clone()) {
                    Ok(operation) if operation.role == RemoteFollowDeliveryJobRole::Target => {
                        operation
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        terminalize_invalid_delivery_job(state, &job, &error)?;
                        continue;
                    }
                };
            let Some(request_value) = job.result.clone() else {
                continue;
            };
            let request: RemoteFollowTerminalDeliveryRequest = match serde_json::from_value(
                request_value,
            )
            .and_then(|request: RemoteFollowTerminalDeliveryRequest| {
                request.validate().map_err(serde::de::Error::custom)?;
                Ok(request)
            }) {
                Ok(request) => request,
                Err(error) => {
                    terminalize_invalid_delivery_job(state, &job, &anyhow::Error::new(error))?;
                    continue;
                }
            };
            if request.operation_id != operation.operation_id
                || request.reservation_attestation_hash != operation.reservation_attestation_hash
                || request.child_chain_root_id != operation.child_chain_root_id
                || request.parent_site_id != operation.parent_site_id
                || request.target_site_id != operation.target_site_id
            {
                terminalize_invalid_delivery_job(
                    state,
                    &job,
                    &anyhow::anyhow!("remote follow delivery job request changed coordinates"),
                )?;
                continue;
            }
            if let Err(error) = validate_target_delivery_job_binding(&job, &operation, &request) {
                terminalize_invalid_delivery_job(state, &job, &error)?;
                continue;
            }
            let remotes = config::load_remotes_layered(&state.config.app_root, None)?;
            let parent_remote =
                config::resolve_remote_by_site_id(&remotes, &operation.parent_site_id)?;
            let client = RemoteClient::from_remote_cfg(state, &parent_remote.remote);
            let attempt_id = format!("remote-follow-attempt:{}", uuid::Uuid::new_v4());
            state.state_store.with_state_db(|db| {
                db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
                    attempt_id: attempt_id.clone(),
                    job_id: job.job_id.clone(),
                    worker_id: Some("remote-follow-delivery".to_owned()),
                    phase: "parent_delivery".to_owned(),
                })?;
                Ok(())
            })?;
            let result = client
                .execute_service_result(
                    REMOTE_FOLLOW_DELIVERY_SERVICE,
                    &BTreeMap::new(),
                    None,
                    &serde_json::to_value(&request)?,
                    &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                        ryeos_app::execution_policy::ExecutionResponse::Wait,
                    ),
                )
                .await;
            match result {
                Ok(value) => {
                    let response: RemoteFollowTerminalDeliveryResponse =
                        serde_json::from_value(value.clone())?;
                    response.validate_against(&request)?;
                    settle_attempt(
                        state,
                        &job,
                        &attempt_id,
                        ryeos_state::SyncJobAttemptState::Completed,
                        ryeos_state::SyncJobState::Completed,
                        "settled",
                        None,
                        Some(value),
                    )?;
                    recovered += 1;
                }
                Err(error) => {
                    settle_attempt(
                        state,
                        &job,
                        &attempt_id,
                        ryeos_state::SyncJobAttemptState::Failed,
                        ryeos_state::SyncJobState::Retryable,
                        "delivery_retryable",
                        Some(bounded_error(&error.to_string())),
                        Some(serde_json::to_value(request)?),
                    )?;
                }
            }
        }
        after = Some(next);
    }
    Ok(recovered)
}

#[allow(clippy::too_many_arguments)]
fn settle_attempt(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        let latest = db
            .get_sync_job(&job.job_id)?
            .context("remote follow delivery job disappeared")?;
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &ryeos_state::FinishSyncJobAttempt {
                state: attempt_state,
                phase: phase.to_owned(),
                error: error.clone(),
                result: result.clone(),
            },
            &job.job_id,
            &ryeos_state::SyncJobUpdate {
                state: job_state,
                phase: phase.to_owned(),
                roots: None,
                heads: None,
                uploaded_hashes: latest.uploaded_hashes,
                fetched_hashes: latest.fetched_hashes,
                last_error: error,
                result: result.or(latest.result),
            },
        )
    })
}

fn validate_target_delivery_job_binding(
    job: &ryeos_state::SyncJobRecord,
    operation: &RemoteFollowDeliveryJobOperation,
    request: &RemoteFollowTerminalDeliveryRequest,
) -> Result<()> {
    if job.operation_type != REMOTE_FOLLOW_DELIVERY_OPERATION
        || job.job_id != format!("remote-follow-terminal-target:{}", operation.operation_id)
        || job.peer.as_deref() != Some(operation.parent_site_id.as_str())
        || job.roots
            != vec![
                request.reservation_attestation_hash.clone(),
                request.terminal_attestation_hash.clone(),
                request.target_chain_head_hash.clone(),
            ]
        || job.heads != vec![request.target_chain_head_hash.clone()]
        || job.max_attempts != ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS
        || !job.attempt_count_is_valid()
    {
        bail!("remote follow delivery job changed its retained authority");
    }
    Ok(())
}

fn terminalize_invalid_delivery_job(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    error: &anyhow::Error,
) -> Result<()> {
    terminalize_delivery_job(
        state,
        job,
        "authority_invalid",
        bounded_error(&format!("{error:#}")),
    )
}

fn terminalize_delivery_job(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    phase: &str,
    error: String,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            &job.job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Failed,
                phase: phase.to_owned(),
                roots: None,
                heads: None,
                uploaded_hashes: job.uploaded_hashes.clone(),
                fetched_hashes: job.fetched_hashes.clone(),
                last_error: Some(error),
                result: job.result.clone(),
            },
        )
    })
}

fn bounded_error(value: &str) -> String {
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
    let output = output.trim();
    if output.is_empty() {
        "remote follow delivery failed".to_owned()
    } else {
        output.to_owned()
    }
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: REMOTE_FOLLOW_DELIVERY_SERVICE,
    endpoint: "federation.follow-terminal-deliver",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.federation/follow-terminal-deliver"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RemoteFollowTerminalDeliveryRequest =
                crate::handler_error::parse_request(params)?;
            deliver(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod authority_tests {
    use super::*;

    #[test]
    fn terminal_delivery_accepts_only_the_exact_remote_node_site() {
        let admitted = HandlerContext::new_with_authority(
            "fp:target-node".into(),
            Vec::new(),
            true,
            Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode),
            Some("site:target".into()),
        );
        assert_eq!(
            authenticated_target_node(&admitted, "site:target").unwrap(),
            "fp:target-node"
        );

        for class in [
            ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient,
            ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteOperator,
        ] {
            let rejected = HandlerContext::new_with_authority(
                "fp:operator".into(),
                Vec::new(),
                true,
                Some(class),
                Some("site:target".into()),
            );
            assert!(authenticated_target_node(&rejected, "site:target").is_err());
        }
        assert!(authenticated_target_node(&admitted, "site:other").is_err());
    }

    #[test]
    fn target_delivery_job_retains_exact_network_and_closure_authority() {
        let operation = RemoteFollowDeliveryJobOperation::new(
            RemoteFollowDeliveryJobRole::Target,
            "1".repeat(64),
            "2".repeat(64),
            "fp:owner".to_owned(),
            "T-child".to_owned(),
            "site:parent".to_owned(),
            "site:target".to_owned(),
        )
        .unwrap();
        let request = RemoteFollowTerminalDeliveryRequest {
            operation_id: operation.operation_id.clone(),
            reservation_attestation_hash: operation.reservation_attestation_hash.clone(),
            terminal_attestation_hash: "3".repeat(64),
            child_chain_root_id: operation.child_chain_root_id.clone(),
            target_chain_head_hash: "4".repeat(64),
            parent_site_id: operation.parent_site_id.clone(),
            target_site_id: operation.target_site_id.clone(),
        };
        let mut job = ryeos_state::SyncJobRecord {
            job_id: format!("remote-follow-terminal-target:{}", operation.operation_id),
            operation_type: REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some(operation.parent_site_id.clone()),
            state: ryeos_state::SyncJobState::Retryable,
            phase: "delivery_retryable".to_owned(),
            roots: vec![
                request.reservation_attestation_hash.clone(),
                request.terminal_attestation_hash.clone(),
                request.target_chain_head_hash.clone(),
            ],
            heads: vec![request.target_chain_head_hash.clone()],
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 1,
            max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
            last_error: None,
            result: Some(serde_json::to_value(&request).unwrap()),
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:00Z".to_owned(),
            finished_at: None,
        };
        validate_target_delivery_job_binding(&job, &operation, &request).unwrap();
        job.peer = Some("site:other".to_owned());
        assert!(validate_target_delivery_job_binding(&job, &operation, &request).is_err());
    }

    #[test]
    fn delivery_binding_requires_logically_unbounded_retry_authority() {
        let operation = RemoteFollowDeliveryJobOperation::new(
            RemoteFollowDeliveryJobRole::Target,
            "1".repeat(64),
            "2".repeat(64),
            "fp:owner".to_owned(),
            "T-child".to_owned(),
            "site:parent".to_owned(),
            "site:target".to_owned(),
        )
        .unwrap();
        let request = RemoteFollowTerminalDeliveryRequest {
            operation_id: operation.operation_id.clone(),
            reservation_attestation_hash: operation.reservation_attestation_hash.clone(),
            terminal_attestation_hash: "3".repeat(64),
            child_chain_root_id: operation.child_chain_root_id.clone(),
            target_chain_head_hash: "4".repeat(64),
            parent_site_id: operation.parent_site_id.clone(),
            target_site_id: operation.target_site_id.clone(),
        };
        let mut job = ryeos_state::SyncJobRecord {
            job_id: format!("remote-follow-terminal-target:{}", operation.operation_id),
            operation_type: REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some(operation.parent_site_id.clone()),
            state: ryeos_state::SyncJobState::Retryable,
            phase: "delivery_retryable".to_owned(),
            roots: vec!["2".repeat(64), "3".repeat(64), "4".repeat(64)],
            heads: vec!["4".repeat(64)],
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 50_000,
            max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
            last_error: None,
            result: Some(serde_json::to_value(&request).unwrap()),
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:00Z".to_owned(),
            finished_at: None,
        };
        validate_target_delivery_job_binding(&job, &operation, &request).unwrap();
        job.max_attempts = 16;
        assert!(validate_target_delivery_job_binding(&job, &operation, &request).is_err());
    }
}
