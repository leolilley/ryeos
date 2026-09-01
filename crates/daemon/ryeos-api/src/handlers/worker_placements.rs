//! Node-authenticated internal services for cross-site worker placement.
//!
//! These handlers are provider-neutral. They consume a typed portable worker
//! checkpoint and signed workload-program data, then reuse normal launch,
//! admission, sync-job, credential, project, and accounting authorities.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use crate::remote::client::{ObjectsClosureRequestOptions, RemoteClient};
use crate::remote::{config, import};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::worker_handoff::{
    CredentialGenerationReservation, RemoteResumeContextRebind, WORKER_PLACEMENT_ABORT_SERVICE,
    WORKER_PLACEMENT_ADOPT_SERVICE, WORKER_PLACEMENT_PREFLIGHT_SERVICE,
    WORKER_PLACEMENT_PREPARE_SERVICE, WORKER_SESSION_HANDOFF_OPERATION,
    WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT, WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION,
    WorkerHandoffJobRole, WorkerHandoffPhase, WorkerPlacementAbortRequest,
    WorkerPlacementAbortResponse, WorkerPlacementAbortResult, WorkerPlacementAdmissionEvidence,
    WorkerPlacementAdoptRequest, WorkerPlacementAdoptResponse, WorkerPlacementAdoptResult,
    WorkerPlacementCompletionResponse, WorkerPlacementFailureResponse,
    WorkerPlacementPreflightEvidence, WorkerPlacementPreflightJobOperation,
    WorkerPlacementPreflightRequest, WorkerPlacementPreflightResponse,
    WorkerPlacementPrepareRequest, WorkerPlacementPrepareResponse,
    WorkerSessionHandoffJobOperation, WorkerSessionHandoffProgress,
};
use ryeos_executor::executor::ServiceAvailability;

const MAX_HANDOFF_CLOSURE_BYTES: u64 = 48 * 1024 * 1024;

struct PreparedTargetPlacement {
    response: WorkerPlacementPrepareResponse,
    #[cfg(any(test, feature = "handoff-test-support"))]
    project_materialization_ms: u64,
}

#[cfg(any(test, feature = "handoff-test-support"))]
#[derive(Default)]
struct TargetAdoptionQualificationMeasurements {
    event_replay_ms: Option<u64>,
    worker_attach_recovery_ms: Option<u64>,
}

#[cfg(any(test, feature = "handoff-test-support"))]
fn attach_target_adoption_qualification_measurements(
    mut response: Value,
    measurements: TargetAdoptionQualificationMeasurements,
) -> Value {
    response["qualification_measurements"] = serde_json::json!({
        "schema":"ryeos.worker_handoff_target_adoption_measurements.v1",
        "event_replay_ms":measurements.event_replay_ms,
        "worker_attach_recovery_ms":measurements.worker_attach_recovery_ms,
    });
    response
}

#[cfg(any(test, feature = "handoff-test-support"))]
fn reach_target_handoff_crash_boundary(
    state: &AppState,
    boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    operation: &WorkerSessionHandoffJobOperation,
) -> anyhow::Result<()> {
    if let Some(gate) = state
        .extensions
        .get::<ryeos_app::worker_handoff::test_support::HandoffPhaseGate>()
    {
        gate.reach(boundary, operation)?;
    }
    Ok(())
}

fn authenticated_remote_node_site(ctx: &HandlerContext) -> Result<&str, HandlerError> {
    if !ctx.verified
        || ctx.authorized_key_class
            != Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode)
    {
        return Err(HandlerError::Forbidden(
            "authenticated source-node authority required".into(),
        ));
    }
    ctx.authenticated_origin_site_id
        .as_deref()
        .ok_or_else(|| HandlerError::Forbidden("authenticated source site required".into()))
}

fn require_authenticated_source_node(
    ctx: &HandlerContext,
    expected_site_id: &str,
    expected_principal: &str,
) -> Result<(), HandlerError> {
    if authenticated_remote_node_site(ctx)? != expected_site_id
        || ctx.fingerprint != expected_principal
    {
        return Err(HandlerError::Forbidden(
            "authenticated source-node authority required".into(),
        ));
    }
    Ok(())
}

fn target_handoff_operation_lock(job_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("target worker handoff lock map poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(job_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(job_id.to_owned(), Arc::downgrade(&lock));
    lock
}

struct SourcePlacementOperands {
    manifest: ryeos_state::objects::PlacementTransferManifest,
    launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    launch_capsule: ryeos_state::objects::AdmittedLaunchCapsule,
    source_snapshot: ryeos_state::objects::ThreadSnapshot,
    restore: ryeos_state::objects::WorkerSessionRestore,
    portable_tree: ryeos_state::objects::PortableStateTree,
}

struct SourcePreflightOperands {
    launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    launch_capsule: ryeos_state::objects::AdmittedLaunchCapsule,
    source_snapshot: ryeos_state::objects::ThreadSnapshot,
}

pub async fn preflight(
    req: WorkerPlacementPreflightRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let owner = req.owner_principal.clone();
    if req.target_site_id != state.threads.site_id() {
        return Err(HandlerError::Forbidden(
            "worker placement preflight differs from authenticated sites".into(),
        ));
    }

    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let source_remote = config::resolve_remote_by_site_id(&remotes, &req.source_site_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    require_authenticated_source_node(
        &ctx,
        &req.source_site_id,
        &source_remote.remote.principal_id,
    )?;
    let source_client = RemoteClient::from_remote_cfg(&state, &source_remote.remote);
    let mut preflight_roots = vec![
        req.source_chain_head_hash.clone(),
        req.source_launch_capsule_hash.clone(),
    ];
    if let Some(hash) = &req.follow_delivery_reservation_attestation_hash {
        preflight_roots.push(hash.clone());
    }
    let closure = source_client
        .objects_closure_get_with_total_timeout(
            &preflight_roots,
            ObjectsClosureRequestOptions {
                max_objects: Some(16_384),
                max_blobs: Some(16_384),
                max_object_bytes: Some(2 * 1024 * 1024),
                max_total_object_bytes: Some(16 * 1024 * 1024),
                max_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_total_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_response_bytes: Some(64 * 1024 * 1024),
                max_links_per_object: Some(65_536),
                allow_incomplete: false,
                allow_untransported_large_objects: true,
            },
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "fetch source preflight closure")
        })?;
    import::require_local_large_object_dependencies(&state, &closure.closure.large_object_hashes)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if let Some(hash) = &req.follow_delivery_reservation_attestation_hash {
        let value = closure
            .entries
            .iter()
            .find(|entry| entry.kind == "object" && &entry.hash == hash)
            .and_then(|entry| entry.value.clone())
            .ok_or_else(|| internal("source closure omitted its follow delivery reservation"))?;
        let attestation = ryeos_state::objects::Attestation::from_value(&value)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        if ryeos_state::objects::canonical_value_digest(&value).map_err(internal)? != *hash {
            return Err(HandlerError::BadRequest(
                "follow delivery reservation changed digest".into(),
            ));
        }
        let reservation =
            ryeos_app::federated_follow::RemoteFollowReservationEvidence::from_attestation(
                &attestation,
            )
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        let (parent_key, parent_signer) = if reservation.parent_site_id == state.threads.site_id() {
            validate_returned_follow_reservation(&state, &reservation, &owner)
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
            (
                *state.identity.verifying_key(),
                state.identity.fingerprint().to_owned(),
            )
        } else {
            let parent_remote =
                config::resolve_remote_by_site_id(&remotes, &reservation.parent_site_id)
                    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
            let parent_key = parent_remote
                .remote
                .pinned_signing_key()
                .map_err(internal)?;
            let parent_signer = parent_remote
                .remote
                .principal_id
                .strip_prefix("fp:")
                .ok_or_else(|| internal("configured follow-parent principal is not a fingerprint"))?
                .to_owned();
            (parent_key, parent_signer)
        };
        attestation
            .verify_with_key(&parent_key)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        if reservation.owner_principal != owner
            || reservation.parent_node_signer_fingerprint != parent_signer
            || reservation.child_chain_root_id != req.chain_root_id
            || reservation.child_initial_thread_id != req.chain_root_id
        {
            return Err(HandlerError::BadRequest(
                "follow delivery reservation differs from the proposed child placement".into(),
            ));
        }
    }
    let mut payload = import::closure_response_to_export_payload(
        &req.chain_root_id,
        &req.source_chain_head_hash,
        &closure.entries,
    )
    .map_err(internal)?;
    let metadata_bytes = lillux::canonical_json(&req.source_launch_metadata)
        .map_err(internal)?
        .into_bytes();
    if lillux::sha256_hex(&metadata_bytes) != req.source_launch_metadata_blob_hash {
        return Err(HandlerError::BadRequest(
            "source preflight launch metadata changed digest".into(),
        ));
    }
    if let Some(existing) = payload
        .entries
        .iter()
        .find(|entry| entry.hash == req.source_launch_metadata_blob_hash)
    {
        if !existing.is_blob || existing.data != metadata_bytes {
            return Err(HandlerError::BadRequest(
                "source preflight closure contradicts launch metadata".into(),
            ));
        }
    } else {
        payload.total_bytes = payload
            .total_bytes
            .checked_add(metadata_bytes.len())
            .ok_or_else(|| internal("preflight payload byte count overflow"))?;
        payload.entries.push(ryeos_state::sync::SyncEntry {
            hash: req.source_launch_metadata_blob_hash.clone(),
            is_blob: true,
            data: metadata_bytes,
        });
    }
    let operation = WorkerPlacementPreflightJobOperation::from_request(
        WorkerHandoffJobRole::Target,
        source_remote.config_key.clone(),
        &req,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let job_id = target_preflight_job_id(&req.preflight_id);
    let _operation_guard = target_handoff_operation_lock(&job_id).lock_owned().await;
    let (_, job) = state
        .state_store
        .stage_sync_payload_and_create_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(source_remote.remote.principal_id.clone()),
                source_peer: Some(source_remote.config_key.clone()),
                job_id: Some(job_id.clone()),
            },
            &ryeos_state::NewSyncJob {
                job_id: job_id.clone(),
                operation_type: WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION.to_owned(),
                operation: operation.to_value().map_err(internal)?,
                peer: Some(source_remote.config_key.clone()),
                roots: {
                    let mut roots = vec![
                        req.source_chain_head_hash.clone(),
                        req.source_launch_capsule_hash.clone(),
                        req.source_launch_metadata_blob_hash.clone(),
                    ];
                    if let Some(hash) = &req.follow_delivery_reservation_attestation_hash {
                        roots.push(hash.clone());
                    }
                    roots
                },
                heads: vec![req.source_chain_head_hash.clone()],
                max_attempts: 4,
            },
        )
        .map_err(internal)?;
    if job.state == ryeos_state::SyncJobState::Completed {
        let response: WorkerPlacementPreflightResponse = serde_json::from_value(
            job.result
                .ok_or_else(|| internal("completed preflight job has no result"))?,
        )
        .map_err(internal)?;
        response
            .validate_against(&req, state.identity.verifying_key())
            .map_err(internal)?;
        return serde_json::to_value(response).map_err(internal);
    }

    let mut attempt =
        TargetPreflightAttempt::begin(Arc::clone(&state), &job_id).map_err(internal)?;

    let source = load_source_preflight_operands(&state, &req).map_err(internal)?;
    let response = preflight_after_staging(&state, &req, &source)
        .await
        .map_err(|error| HandlerError::BadRequest(format!("target preflight failed: {error:#}")))?;
    let result = serde_json::to_value(&response).map_err(internal)?;
    let stored = attempt
        .complete(&response.preflight_attestation, result.clone())
        .map_err(internal)?;
    if stored != response.preflight_attestation_hash {
        return Err(internal("stored preflight receipt digest changed"));
    }
    Ok(result)
}

fn validate_returned_follow_reservation(
    state: &AppState,
    reservation: &ryeos_app::federated_follow::RemoteFollowReservationEvidence,
    owner: &str,
) -> Result<()> {
    let parent_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &reservation.parent_chain_root_id))?
        .context("return target follow parent head disappeared")?;
    if parent_head.signer != state.identity.fingerprint() {
        bail!("return target follow parent is not locally owned");
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
    let waiter = state
        .state_store
        .get_follow_waiter_by_child_chain(&reservation.child_chain_root_id)?
        .context("return target has no live parent follow waiter")?;
    let child = waiter
        .children
        .iter()
        .find(|child| child.child_chain_root_id == reservation.child_chain_root_id)
        .context("return target parent waiter lost its selected child")?;
    let parent = state
        .state_store
        .get_thread(&reservation.parent_thread_id)?
        .context("return target follow parent disappeared")?;
    let successor = state
        .state_store
        .get_thread(&reservation.parent_successor_thread_id)?
        .context("return target follow successor disappeared")?;
    if reservation.owner_principal != owner
        || waiter.phase != ryeos_app::runtime_db::follow_phase::WAITING
        || waiter.follow_key != reservation.follow_key
        || waiter.parent_thread_id != reservation.parent_thread_id
        || waiter.parent_chain_root_id != reservation.parent_chain_root_id
        || waiter.parent_successor_thread_id.as_deref()
            != Some(reservation.parent_successor_thread_id.as_str())
        || child.item_index != reservation.child_item_index
        || child.item_ref != reservation.child_item_ref
        || child.spec_hash != reservation.child_spec_hash
        || child.child_thread_id != reservation.child_initial_thread_id
        || child.child_chain_root_id != reservation.child_chain_root_id
        || child.terminal_thread_id.is_some()
        || child.terminal_status.is_some()
        || child.terminal_envelope.is_some()
        || parent.chain_root_id != reservation.parent_chain_root_id
        || parent.status != ryeos_state::objects::ThreadStatus::Continued.as_str()
        || parent.requested_by.as_deref() != Some(owner)
        || successor.chain_root_id != reservation.parent_chain_root_id
        || successor.upstream_thread_id.as_deref() != Some(reservation.parent_thread_id.as_str())
        || successor.status != ryeos_state::objects::ThreadStatus::Created.as_str()
        || successor.requested_by.as_deref() != Some(owner)
    {
        bail!("returned child differs from its local parent follow reservation");
    }
    Ok(())
}

pub async fn prepare(
    req: WorkerPlacementPrepareRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let target_site_id = state.threads.site_id();
    if req.target_site_id != target_site_id {
        return Err(HandlerError::BadRequest(
            "placement request targets another site".into(),
        ));
    }
    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let source_remote = config::resolve_remote_by_site_id(&remotes, &req.source_site_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    require_authenticated_source_node(
        &ctx,
        &req.source_site_id,
        &source_remote.remote.principal_id,
    )?;
    let preflight_job_id = target_preflight_job_id(&req.preflight_id);
    let preflight_job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&preflight_job_id))
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("target preflight does not exist".into()))?;
    if preflight_job.state != ryeos_state::SyncJobState::Completed {
        return Err(HandlerError::BadRequest(
            "target preflight has not completed".into(),
        ));
    }
    let preflight_operation =
        WorkerPlacementPreflightJobOperation::from_value(preflight_job.operation.clone())
            .map_err(internal)?;
    let owner = preflight_operation.owner_principal.clone();
    let preflight_response: WorkerPlacementPreflightResponse = serde_json::from_value(
        preflight_job
            .result
            .clone()
            .ok_or_else(|| internal("completed target preflight has no result"))?,
    )
    .map_err(internal)?;
    preflight_response
        .preflight_attestation
        .verify_with_key(state.identity.verifying_key())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let preflight_evidence = WorkerPlacementPreflightEvidence::from_attestation(
        &preflight_response.preflight_attestation,
    )
    .map_err(internal)?;
    if preflight_operation.role != WorkerHandoffJobRole::Target
        || preflight_operation.preflight_id != req.preflight_id
        || preflight_operation.owner_principal != owner
        || preflight_operation.chain_root_id != req.chain_root_id
        || preflight_operation.source_site_id != req.source_site_id
        || preflight_operation.target_site_id != req.target_site_id
        || preflight_operation.target_project_path != req.target_project_path
        || preflight_operation.project_route_digest != req.project_route_digest
        || preflight_operation.target_credential_profile_id != req.target_credential_profile_id
        || preflight_operation.follow_delivery_reservation_attestation_hash
            != req.follow_delivery_reservation_attestation_hash
        || preflight_response.preflight_attestation_hash != req.preflight_attestation_hash
        || preflight_response.preflight_attestation_hash
            != ryeos_state::objects::canonical_value_digest(
                &preflight_response.preflight_attestation.to_value(),
            )
            .map_err(internal)?
        || preflight_evidence.preflight_id != req.preflight_id
        || preflight_evidence.follow_delivery_reservation_attestation_hash
            != req.follow_delivery_reservation_attestation_hash
    {
        return Err(HandlerError::BadRequest(
            "final placement request differs from its target preflight".into(),
        ));
    }

    let job_id = target_job_id(&req.operation_id);
    // Serialize the operation before its first remote wait. Abort publishes a
    // durable claim under this same lock, and the permanent signed fence below
    // survives terminal sync-job retention.
    let _operation_guard = target_handoff_operation_lock(&job_id).lock_owned().await;
    if let Some(fence) = state
        .state_store
        .worker_handoff_abort_fence(&req.operation_id)
        .map_err(internal)?
    {
        fence.validate_prepare_request(&req).map_err(internal)?;
        return Err(HandlerError::BadRequest(
            "target placement was permanently aborted before preparation".into(),
        ));
    }
    if let Some(existing) = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))
        .map_err(internal)?
    {
        if let Some(response) = classify_existing_target_prepare(&state, &req, &existing)? {
            return Ok(response);
        }
    }

    let source_client = RemoteClient::from_remote_cfg(&state, &source_remote.remote);
    let mut roots = vec![
        req.source_chain_head_hash.clone(),
        req.transfer_manifest_hash.clone(),
        req.preflight_attestation_hash.clone(),
    ];
    if let Some(hash) = &req.follow_delivery_reservation_attestation_hash {
        roots.push(hash.clone());
    }
    let closure = source_client
        .objects_closure_get_with_total_timeout(
            &roots,
            ObjectsClosureRequestOptions {
                max_objects: Some(16_384),
                max_blobs: Some(16_384),
                max_object_bytes: Some(2 * 1024 * 1024),
                max_total_object_bytes: Some(16 * 1024 * 1024),
                max_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_total_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_response_bytes: Some(64 * 1024 * 1024),
                max_links_per_object: Some(65_536),
                allow_incomplete: false,
                allow_untransported_large_objects: true,
            },
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "fetch source placement closure")
        })?;
    import::require_local_large_object_dependencies(&state, &closure.closure.large_object_hashes)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let payload = import::closure_response_to_export_payload(
        &req.chain_root_id,
        &req.source_chain_head_hash,
        &closure.entries,
    )
    .map_err(internal)?;

    let transfer_value = closure
        .entries
        .iter()
        .find(|entry| entry.kind == "object" && entry.hash == req.transfer_manifest_hash)
        .and_then(|entry| entry.value.clone())
        .ok_or_else(|| internal("source closure omitted its transfer manifest"))?;
    let transfer =
        ryeos_state::objects::PlacementTransferManifest::from_current_value(transfer_value)
            .map_err(internal)?;
    validate_transfer_request(&transfer, &req, &owner)
        .map_err(|error| HandlerError::BadRequest(format!("transfer manifest: {error:#}")))?;
    let target_project_path = canonical_target_project_path(&req.target_project_path)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let operation = WorkerSessionHandoffJobOperation::new(
        WorkerHandoffJobRole::Target,
        req.operation_id.clone(),
        req.preflight_id.clone(),
        req.preflight_attestation_hash.clone(),
        owner.clone(),
        req.chain_root_id.clone(),
        transfer.origin_site_id.clone(),
        req.source_site_id.clone(),
        req.target_site_id.clone(),
        transfer.source_placement_thread_id.clone(),
        transfer.successor_placement_thread_id.clone(),
        req.source_chain_head_hash.clone(),
        transfer.source_last_event_hash.clone(),
        transfer.checkpoint_manifest_hash.clone(),
        req.transfer_manifest_hash.clone(),
        source_remote.config_key.clone(),
        source_project_path_from_payload(&payload, &transfer)
            .map_err(internal)?
            .display()
            .to_string(),
        target_project_path.display().to_string(),
        req.project_route_digest.clone(),
        req.target_credential_profile_id.clone(),
        req.follow_delivery_reservation_attestation_hash.clone(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let (_, existing_job) = state
        .state_store
        .stage_sync_payload_and_create_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(source_remote.remote.principal_id.clone()),
                source_peer: Some(source_remote.config_key.clone()),
                job_id: Some(job_id.clone()),
            },
            &ryeos_state::NewSyncJob {
                job_id: job_id.clone(),
                operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
                operation: operation.to_value().map_err(internal)?,
                peer: Some(source_remote.config_key.clone()),
                roots,
                heads: vec![req.source_chain_head_hash.clone()],
                max_attempts: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            },
        )
        .map_err(internal)?;

    if let Some(response) = classify_existing_target_prepare(&state, &req, &existing_job)? {
        return Ok(response);
    }

    let operands = load_source_placement_operands(&state, &transfer).map_err(internal)?;
    let attempt_id = begin_target_handoff_attempt(
        &state,
        &job_id,
        "placement_admission",
        "target-handoff-prepare",
    )
    .map_err(internal)?;
    let profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &req.target_credential_profile_id,
    )
    .await
    .map_err(internal)?;
    let result = prepare_after_staging(
        &state,
        &req,
        &operation,
        &owner,
        &target_project_path,
        &job_id,
        &operands,
        &preflight_evidence,
    )
    .await;
    drop(profile_operation);
    match result {
        Ok(prepared) => {
            settle_target_handoff_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Running,
                "target_prepared",
                None,
                None,
            )
            .map_err(internal)?;
            let value = serde_json::to_value(prepared.response).map_err(internal)?;
            #[cfg(any(test, feature = "handoff-test-support"))]
            let value = {
                let mut value = value;
                value["qualification_measurements"] = serde_json::json!({
                    "schema":"ryeos.worker_handoff_target_prepare_measurements.v1",
                    "project_materialization_ms":prepared.project_materialization_ms,
                });
                value
            };
            Ok(value)
        }
        Err(error) => {
            let detail = bounded_recovery_error(&format!("{error:#}"));
            settle_target_handoff_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Failed,
                ryeos_state::SyncJobState::Retryable,
                "target_prepare_failed",
                Some(detail),
                None,
            )
            .map_err(internal)?;
            Err(HandlerError::BadRequest(format!(
                "target placement preparation failed: {error:#}"
            )))
        }
    }
}

fn classify_existing_target_prepare(
    state: &AppState,
    request: &WorkerPlacementPrepareRequest,
    job: &ryeos_state::SyncJobRecord,
) -> Result<Option<Value>, HandlerError> {
    let operation =
        WorkerSessionHandoffJobOperation::from_value(job.operation.clone()).map_err(internal)?;
    operation
        .validate_target_prepare_request(request)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Completed
            | ryeos_state::SyncJobState::Failed
            | ryeos_state::SyncJobState::Cancelled
    ) {
        return Err(HandlerError::BadRequest(
            "terminal target placement cannot be prepared again".into(),
        ));
    }
    if job.phase == "target_abort_claimed" {
        return Err(HandlerError::BadRequest(
            "target placement has an active abort claim".into(),
        ));
    }
    let progress = job
        .result
        .clone()
        .map(WorkerSessionHandoffProgress::from_value)
        .transpose()
        .map_err(internal)?;
    let Some(progress) = progress else {
        return Ok(None);
    };
    match progress.phase {
        WorkerHandoffPhase::Planned => Ok(None),
        WorkerHandoffPhase::TargetPrepared => {
            let response = prepared_response_from_progress(state, request, &operation, &progress)
                .map_err(internal)?;
            serde_json::to_value(response).map(Some).map_err(internal)
        }
        WorkerHandoffPhase::AbortAuthorized => Err(HandlerError::BadRequest(
            "target placement has an active abort claim".into(),
        )),
        WorkerHandoffPhase::SourceCommitted
        | WorkerHandoffPhase::TargetAdopted
        | WorkerHandoffPhase::StateInstalled
        | WorkerHandoffPhase::ProcessAttached
        | WorkerHandoffPhase::Completed => Err(HandlerError::BadRequest(
            "target placement already crossed the source writer cut".into(),
        )),
        WorkerHandoffPhase::SourceExported => Err(internal(
            "target placement retained source-only handoff progress",
        )),
    }
}

fn prepared_response_from_progress(
    state: &AppState,
    request: &WorkerPlacementPrepareRequest,
    operation: &WorkerSessionHandoffJobOperation,
    progress: &WorkerSessionHandoffProgress,
) -> Result<WorkerPlacementPrepareResponse> {
    if progress.phase != WorkerHandoffPhase::TargetPrepared
        || progress.operation_id != operation.operation_id
    {
        bail!("target prepare replay has no exact prepared progress");
    }
    let attestation_hash = progress
        .placement_attestation_hash
        .as_deref()
        .context("prepared progress has no placement attestation")?;
    let runtime_seed_hash = progress
        .target_runtime_seed_hash
        .as_deref()
        .context("prepared progress has no runtime seed")?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(attestation_hash)?
        .context("prepared placement attestation disappeared")?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    attestation.verify_with_key(state.identity.verifying_key())?;
    let placement = WorkerPlacementAdmissionEvidence::from_attestation(&attestation)?;
    drop(guard);
    if placement.operation_id != operation.operation_id
        || placement.target_runtime_seed_hash != runtime_seed_hash
        || progress.credential_reservation_id.as_deref()
            != Some(placement.credential_reservation.reservation_id.as_str())
    {
        bail!("prepared progress contradicts its signed placement evidence");
    }
    let response = WorkerPlacementPrepareResponse {
        operation_id: operation.operation_id.clone(),
        placement_attestation_hash: attestation_hash.to_owned(),
        target_runtime_seed_hash: runtime_seed_hash.to_owned(),
        target_launch_capsule_hash: placement.target_launch_capsule_hash.clone(),
        credential_reservation: placement.credential_reservation.clone(),
        placement,
    };
    response.validate_against(request)?;
    Ok(response)
}

pub async fn adopt(
    req: WorkerPlacementAdoptRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let job_id = target_job_id(&req.operation_id);
    let mut job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))
        .map_err(internal)?;
    if job.is_none() {
        // Job retention and the competing abort branch use the same operation
        // lock as live adoption. Recheck under that lock before consulting the
        // shared target-branch head so a prepare/abort transition cannot race
        // a job-absent terminal replay.
        let operation_guard = target_handoff_operation_lock(&job_id).lock_owned().await;
        job = state
            .state_store
            .with_state_db(|db| db.get_sync_job(&job_id))
            .map_err(internal)?;
        if job.is_none() {
            let (source_site_id, response) = replay_terminal_target_adoption(&state, &req)?
                .ok_or_else(|| {
                    HandlerError::BadRequest("target placement authority does not exist".into())
                })?;
            let remotes =
                config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
            let source_remote = config::resolve_remote_by_site_id(&remotes, &source_site_id)
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
            require_authenticated_source_node(
                &ctx,
                &source_site_id,
                &source_remote.remote.principal_id,
            )?;
            drop(operation_guard);
            return serde_json::to_value(response).map_err(internal);
        }
        drop(operation_guard);
    }
    let job = job.expect("target placement job presence checked");
    let operation = WorkerSessionHandoffJobOperation::from_value(job.operation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let source_remote = config::resolve_remote_by_site_id(&remotes, &operation.source_site_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    require_authenticated_source_node(
        &ctx,
        &operation.source_site_id,
        &source_remote.remote.principal_id,
    )?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let mut qualification_measurements = TargetAdoptionQualificationMeasurements::default();
    let response = adopt_authorized(
        req,
        operation.owner_principal,
        operation.source_site_id,
        state,
        #[cfg(any(test, feature = "handoff-test-support"))]
        &mut qualification_measurements,
    )
    .await?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let response =
        attach_target_adoption_qualification_measurements(response, qualification_measurements);
    Ok(response)
}

/// Replay the exact mutually exclusive target terminal branch independently
/// of operational-job retention.
fn replay_terminal_target_adoption(
    state: &AppState,
    request: &WorkerPlacementAdoptRequest,
) -> Result<Option<(String, WorkerPlacementAdoptResult)>, HandlerError> {
    let Some(terminal) = state
        .state_store
        .worker_handoff_target_terminal(&request.operation_id)
        .map_err(internal)?
    else {
        return Ok(None);
    };
    let terminal_attestation_hash = state
        .state_store
        .worker_handoff_target_branch_hash(&request.operation_id)
        .map_err(internal)?
        .ok_or_else(|| internal("target terminal receipt head disappeared"))?;
    let (source_site_id, result) = match terminal {
        ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Adoption(receipt) => {
            if receipt.request != *request
                || receipt.target_operation.target_site_id != state.threads.site_id()
            {
                return Err(HandlerError::BadRequest(
                    "target adoption retry contradicts its signed terminal receipt".into(),
                ));
            }
            (
                receipt.target_operation.source_site_id,
                WorkerPlacementAdoptResult::Attached {
                    response: receipt.response,
                    terminal_attestation_hash,
                },
            )
        }
        ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Failure(receipt) => {
            if receipt.request != *request
                || receipt.target_operation.target_site_id != state.threads.site_id()
            {
                return Err(HandlerError::BadRequest(
                    "target adoption retry contradicts its signed terminal failure".into(),
                ));
            }
            (
                receipt.target_operation.source_site_id,
                WorkerPlacementAdoptResult::FailedBeforeAttachment {
                    failure: receipt.failure,
                    terminal_attestation_hash,
                },
            )
        }
        ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Completion(receipt) => {
            if receipt.request != *request
                || receipt.target_operation.target_site_id != state.threads.site_id()
            {
                return Err(HandlerError::BadRequest(
                    "target adoption retry contradicts its signed terminal completion".into(),
                ));
            }
            (
                receipt.target_operation.source_site_id,
                WorkerPlacementAdoptResult::CompletedBeforeAttachment {
                    completion: receipt.completion,
                    terminal_attestation_hash,
                },
            )
        }
        ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Abort(_) => {
            return Err(HandlerError::BadRequest(
                "target placement is durably claimed by the abort branch".into(),
            ));
        }
    };
    result.validate().map_err(internal)?;
    Ok(Some((source_site_id, result)))
}

pub async fn abort(
    req: WorkerPlacementAbortRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let source_operation = req.operation.clone();
    let authenticated_source_site = authenticated_remote_node_site(&ctx)?.to_owned();
    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let source_remote = config::resolve_remote_by_site_id(&remotes, &authenticated_source_site)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    require_authenticated_source_node(
        &ctx,
        &authenticated_source_site,
        &source_remote.remote.principal_id,
    )?;
    if source_operation.source_site_id != authenticated_source_site
        || source_operation.target_site_id != state.threads.site_id()
    {
        return Err(HandlerError::Forbidden(
            "abort operation differs from its authenticated sites".into(),
        ));
    }
    let operation = source_operation
        .target_projection(source_remote.config_key.clone())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let job_id = target_job_id(&operation.operation_id);
    let _operation_guard = target_handoff_operation_lock(&job_id).lock_owned().await;
    let existing_fence = state
        .state_store
        .worker_handoff_abort_fence(&operation.operation_id)
        .map_err(internal)?;
    if let Some(fence) = &existing_fence {
        let expected = ryeos_app::worker_handoff::WorkerHandoffAbortFenceEvidence::new(
            operation.clone(),
            req.abort_chain_head_hash.clone(),
            fence.terminal_disposition.clone(),
        )
        .map_err(internal)?;
        if *fence != expected {
            return Err(HandlerError::Forbidden(
                "target abort fence differs from authenticated abort coordinates".into(),
            ));
        }
        if let Some(disposition) = &fence.terminal_disposition {
            let response = WorkerPlacementAbortResponse {
                operation_id: source_operation.operation_id.clone(),
                chain_root_id: source_operation.chain_root_id.clone(),
                disposition: disposition.clone(),
            };
            response.validate_against(&req).map_err(internal)?;
            settle_target_abort_job_from_receipt(
                &state,
                &job_id,
                &operation,
                &req.abort_chain_head_hash,
                &response,
            )
            .map_err(internal)?;
            let terminal_attestation_hash = state
                .state_store
                .worker_handoff_target_branch_hash(&operation.operation_id)
                .map_err(internal)?
                .ok_or_else(|| internal("target abort receipt head disappeared"))?;
            let result = WorkerPlacementAbortResult {
                response,
                terminal_attestation_hash,
            };
            result.validate_against(&req).map_err(internal)?;
            return serde_json::to_value(result).map_err(internal);
        }
    }

    let existing_job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))
        .map_err(internal)?;
    if existing_fence.is_some() && existing_job.is_none() {
        return Err(internal(
            "provisional target abort fence lost its active cleanup job",
        ));
    }
    let target_was_absent = existing_job.is_none();
    let job = state
        .state_store
        .claim_worker_handoff_target_abort(
            &ryeos_state::NewSyncJob {
                job_id: job_id.clone(),
                operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
                operation: operation.to_value().map_err(internal)?,
                peer: Some(source_remote.config_key.clone()),
                roots: Vec::new(),
                heads: Vec::new(),
                max_attempts: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            },
            &req.abort_chain_head_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let retained_operation = WorkerSessionHandoffJobOperation::from_value(job.operation.clone())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if retained_operation != operation {
        return Err(HandlerError::Forbidden(
            "target placement job differs from authenticated abort coordinates".into(),
        ));
    }
    if job.state == ryeos_state::SyncJobState::Cancelled {
        return Err(internal(
            "cancelled target handoff job has no target-signed terminal abort receipt",
        ));
    }
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Completed | ryeos_state::SyncJobState::Failed
    ) {
        return Err(HandlerError::BadRequest(
            "terminal target placement cannot be aborted".into(),
        ));
    }
    let existing_progress = job
        .result
        .clone()
        .map(WorkerSessionHandoffProgress::from_value)
        .transpose()
        .map_err(internal)?;
    if existing_progress
        .as_ref()
        .is_some_and(|progress| progress.phase.successor_is_only_authorized_writer())
    {
        return Err(HandlerError::BadRequest(
            "source-committed target placement cannot be aborted".into(),
        ));
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforeAbortEvidenceStage,
        &operation,
    )
    .map_err(internal)?;

    let source_client = RemoteClient::from_remote_cfg(&state, &source_remote.remote);
    let mut source_roots = vec![req.abort_chain_head_hash.clone()];
    if target_was_absent {
        source_roots.push(source_operation.preflight_attestation_hash.clone());
    }
    let closure = source_client
        .objects_closure_get_with_total_timeout(
            &source_roots,
            ObjectsClosureRequestOptions {
                max_objects: Some(16_384),
                max_blobs: Some(16_384),
                max_object_bytes: Some(2 * 1024 * 1024),
                max_total_object_bytes: Some(16 * 1024 * 1024),
                max_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_total_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                max_response_bytes: Some(64 * 1024 * 1024),
                max_links_per_object: Some(65_536),
                allow_incomplete: false,
                allow_untransported_large_objects: true,
            },
            WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "fetch source handoff-abort chain")
        })?;
    import::require_local_large_object_dependencies(&state, &closure.closure.large_object_hashes)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let payload = import::closure_response_to_export_payload(
        &operation.chain_root_id,
        &req.abort_chain_head_hash,
        &closure.entries,
    )
    .map_err(internal)?;
    let mut progress = existing_progress.unwrap_or(
        WorkerSessionHandoffProgress::planned(source_operation.operation_id.clone())
            .map_err(internal)?,
    );
    progress.phase = WorkerHandoffPhase::AbortAuthorized;
    progress.abort_chain_head_hash = Some(req.abort_chain_head_hash.clone());
    progress.validate().map_err(internal)?;
    state
        .state_store
        .stage_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(source_remote.remote.principal_id.clone()),
                source_peer: Some(source_remote.config_key.clone()),
                job_id: Some(job_id.clone()),
            },
            &job_id,
            progress.phase.as_str(),
            &source_roots,
            Some(progress.to_value().map_err(internal)?),
        )
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortEvidenceStaged,
        &operation,
    )
    .map_err(internal)?;
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(internal)?;
    let guard = authority.acquire_shared_guard().map_err(internal)?;
    ryeos_app::worker_handoff::validate_handoff_abort_authority(
        &authority.cas_store().map_err(internal)?,
        &retained_operation,
        &req.abort_chain_head_hash,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    drop(guard);
    if target_was_absent {
        validate_abort_operation_against_target_preflight_attestation(&state, &source_operation)?;
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortEvidenceVerified,
        &operation,
    )
    .map_err(internal)?;
    state
        .state_store
        .publish_worker_handoff_abort_fence(&retained_operation, &req.abort_chain_head_hash)
        .map_err(internal)?;

    let reservation = state
        .state_store
        .credential_profile_reservation_for_successor(
            &retained_operation.successor_placement_thread_id,
        )
        .map_err(internal)?;
    let disposition = if let Some(reservation) = reservation {
        if reservation.operation_id != retained_operation.operation_id
            || reservation.owner_principal != retained_operation.owner_principal
            || reservation.profile_id != retained_operation.target_credential_profile_id
        {
            return Err(internal(
                "target credential reservation differs from aborted handoff",
            ));
        } else {
            match reservation.state.as_str() {
                "reserved" => {
                    state
                        .state_store
                        .release_credential_profile_reservation(&reservation.reservation_id)
                        .map_err(internal)?;
                    "reservation_released"
                }
                // This reservation is uniquely bound to the same operation.
                // Observing it released after an ambiguous crash therefore
                // reproduces the operation's canonical terminal disposition.
                "released" => "reservation_released",
                "consumed" => {
                    return Err(HandlerError::BadRequest(
                        "target placement already consumed its credential reservation".into(),
                    ));
                }
                _ => return Err(internal("unknown target credential reservation state")),
            }
        }
    } else {
        "target_absent"
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortReservationReleased,
        &operation,
    )
    .map_err(internal)?;
    let response = WorkerPlacementAbortResponse {
        operation_id: source_operation.operation_id.clone(),
        chain_root_id: source_operation.chain_root_id.clone(),
        disposition: disposition.to_owned(),
    };
    response.validate_against(&req).map_err(internal)?;
    let terminal_attestation_hash = state
        .state_store
        .publish_worker_handoff_abort_terminal_receipt(
            &retained_operation,
            &req.abort_chain_head_hash,
            disposition,
        )
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortReceiptPublished,
        &operation,
    )
    .map_err(internal)?;
    settle_target_abort_job_from_receipt(
        &state,
        &job_id,
        &retained_operation,
        &req.abort_chain_head_hash,
        &response,
    )
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortCompletedBeforeResponse,
        &operation,
    )
    .map_err(internal)?;
    let result = WorkerPlacementAbortResult {
        response,
        terminal_attestation_hash,
    };
    result.validate_against(&req).map_err(internal)?;
    serde_json::to_value(result).map_err(internal)
}

fn settle_target_abort_job_from_receipt(
    state: &AppState,
    job_id: &str,
    operation: &WorkerSessionHandoffJobOperation,
    abort_chain_head_hash: &str,
    response: &WorkerPlacementAbortResponse,
) -> Result<()> {
    let Some(job) = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
    else {
        return Ok(());
    };
    let retained = WorkerSessionHandoffJobOperation::from_value(job.operation.clone())?;
    if retained != *operation || job.heads != [abort_chain_head_hash.to_owned()] {
        bail!("terminal abort receipt contradicts its retained target job");
    }
    if job.state == ryeos_state::SyncJobState::Cancelled {
        let existing: WorkerPlacementAbortResponse = serde_json::from_value(
            job.result
                .context("cancelled target abort job has no terminal receipt")?,
        )?;
        if existing != *response {
            bail!("terminal abort receipt changed after target settlement");
        }
        return Ok(());
    }
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Completed | ryeos_state::SyncJobState::Failed
    ) {
        bail!("terminal abort receipt conflicts with another target disposition");
    }
    let progress = WorkerSessionHandoffProgress::from_value(
        job.result
            .clone()
            .context("active target abort job has no progress")?,
    )?;
    if progress.phase != WorkerHandoffPhase::AbortAuthorized
        || progress.abort_chain_head_hash.as_deref() != Some(abort_chain_head_hash)
        || !job.roots.iter().any(|root| root == abort_chain_head_hash)
    {
        bail!("terminal abort receipt has no matching authorized target progress");
    }
    let reservation = state
        .state_store
        .credential_profile_reservation_for_successor(&operation.successor_placement_thread_id)?;
    match response.disposition.as_str() {
        "reservation_released" => {
            let reservation = reservation
                .context("terminal target abort receipt lost its released reservation")?;
            if reservation.operation_id != operation.operation_id
                || reservation.owner_principal != operation.owner_principal
                || reservation.profile_id != operation.target_credential_profile_id
                || reservation.state != "released"
            {
                bail!("terminal target abort receipt contradicts credential cleanup");
            }
        }
        "target_absent" if reservation.is_none() => {}
        "target_absent" => {
            bail!("target-absent abort receipt retained a credential reservation");
        }
        _ => bail!("terminal target abort receipt has an invalid disposition"),
    }
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Cancelled,
                phase: "aborted".to_owned(),
                roots: None,
                heads: Some(vec![abort_chain_head_hash.to_owned()]),
                uploaded_hashes: Vec::new(),
                fetched_hashes: vec![abort_chain_head_hash.to_owned()],
                last_error: None,
                result: Some(serde_json::to_value(response)?),
            },
        )
    })
}

fn validate_abort_operation_against_target_preflight_attestation(
    state: &AppState,
    operation: &WorkerSessionHandoffJobOperation,
) -> Result<(), HandlerError> {
    let authority = state
        .state_store
        .pinned_state_authority()
        .map_err(internal)?;
    let guard = authority.acquire_shared_guard().map_err(internal)?;
    authority.ensure_guard(&guard).map_err(internal)?;
    let value = authority
        .cas_store()
        .map_err(internal)?
        .get_object(&operation.preflight_attestation_hash)
        .map_err(internal)?
        .ok_or_else(|| {
            HandlerError::BadRequest("source retained no exact target preflight attestation".into())
        })?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    attestation
        .verify_with_key(state.identity.verifying_key())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let evidence = WorkerPlacementPreflightEvidence::from_attestation(&attestation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    drop(guard);
    if operation.role != WorkerHandoffJobRole::Source
        || evidence.preflight_id != operation.preflight_id
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
        return Err(HandlerError::BadRequest(
            "abort operation differs from its target-signed preflight".into(),
        ));
    }
    Ok(())
}

async fn adopt_authorized(
    req: WorkerPlacementAdoptRequest,
    owner: String,
    source_site_id: String,
    state: Arc<AppState>,
    #[cfg(any(test, feature = "handoff-test-support"))]
    qualification_measurements: &mut TargetAdoptionQualificationMeasurements,
) -> Result<Value, HandlerError> {
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

    let job_id = target_job_id(&req.operation_id);
    let _operation_guard = target_handoff_operation_lock(&job_id).lock_owned().await;
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&job_id))
        .map_err(internal)?
        .ok_or_else(|| HandlerError::BadRequest("target placement job does not exist".into()))?;
    let operation = WorkerSessionHandoffJobOperation::from_value(job.operation.clone())
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if operation.role != WorkerHandoffJobRole::Target
        || operation.operation_id != req.operation_id
        || operation.owner_principal != owner
        || operation.chain_root_id != req.chain_root_id
        || operation.source_site_id != source_site_id
        || operation.target_site_id != state.threads.site_id()
    {
        return Err(HandlerError::Forbidden(
            "target placement job differs from authenticated adoption coordinates".into(),
        ));
    }
    if job.phase == ryeos_app::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE {
        return Err(HandlerError::BadRequest(
            "target placement is durably claimed by the abort branch".into(),
        ));
    }
    if let Some((terminal_source_site_id, result)) = replay_terminal_target_adoption(&state, &req)?
    {
        if terminal_source_site_id != source_site_id {
            return Err(HandlerError::Forbidden(
                "target terminal receipt belongs to another source site".into(),
            ));
        }
        match state
            .state_store
            .worker_handoff_target_terminal(&req.operation_id)
            .map_err(internal)?
            .ok_or_else(|| internal("target terminal receipt disappeared"))?
        {
            ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Adoption(receipt) => {
                settle_target_adoption_job_from_receipt(&state, &job_id, &receipt)
                    .map_err(internal)?;
            }
            ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Failure(receipt) => {
                let progress = WorkerSessionHandoffProgress::from_value(
                    job.result
                        .clone()
                        .ok_or_else(|| internal("target failure job has no durable progress"))?,
                )
                .map_err(internal)?;
                if !settle_pre_attachment_target_terminal(&state, &job, &operation, &progress)
                    .map_err(internal)?
                {
                    return Err(internal(
                        "target terminal failure no longer has exact cleanup authority",
                    ));
                }
                let retained = state
                    .state_store
                    .worker_handoff_terminal_failure(&req.operation_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("target terminal failure disappeared"))?;
                if retained != receipt {
                    return Err(internal("target terminal failure changed during replay"));
                }
            }
            ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Completion(receipt) => {
                let progress =
                    WorkerSessionHandoffProgress::from_value(job.result.clone().ok_or_else(
                        || internal("target completion job has no durable progress"),
                    )?)
                    .map_err(internal)?;
                if !settle_pre_attachment_target_terminal(&state, &job, &operation, &progress)
                    .map_err(internal)?
                {
                    return Err(internal(
                        "target terminal completion no longer has exact lifecycle testimony",
                    ));
                }
                let retained = state
                    .state_store
                    .worker_handoff_terminal_completion(&req.operation_id)
                    .map_err(internal)?
                    .ok_or_else(|| internal("target terminal completion disappeared"))?;
                if retained != receipt {
                    return Err(internal("target terminal completion changed during replay"));
                }
            }
            ryeos_app::worker_handoff::WorkerHandoffTargetTerminalEvidence::Abort(_) => {
                return Err(HandlerError::BadRequest(
                    "target placement is durably claimed by the abort branch".into(),
                ));
            }
        }
        return serde_json::to_value(result).map_err(internal);
    }
    let prepared_progress = job
        .result
        .clone()
        .map(WorkerSessionHandoffProgress::from_value)
        .transpose()
        .map_err(internal)?
        .ok_or_else(|| internal("target placement job has no preparation progress"))?;
    if settle_pre_attachment_target_terminal(&state, &job, &operation, &prepared_progress)
        .map_err(internal)?
    {
        let (terminal_source_site_id, result) = replay_terminal_target_adoption(&state, &req)?
            .ok_or_else(|| internal("settled target terminal has no signed replay branch"))?;
        if terminal_source_site_id != source_site_id {
            return Err(HandlerError::Forbidden(
                "target terminal receipt belongs to another source site".into(),
            ));
        }
        return serde_json::to_value(result).map_err(internal);
    }
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        return Err(HandlerError::BadRequest(
            "terminal target placement cannot be adopted".into(),
        ));
    }
    if prepared_progress.operation_id != req.operation_id
        || prepared_progress.placement_attestation_hash.as_deref()
            != Some(req.placement_attestation_hash.as_str())
        || prepared_progress.credential_reservation_id.is_none()
    {
        return Err(HandlerError::BadRequest(
            "adoption request differs from target preparation".into(),
        ));
    }
    let source_committed_phase = target_adoption_source_commit_phase(prepared_progress.phase)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if source_committed_phase == WorkerHandoffPhase::ProcessAttached {
        #[cfg(any(test, feature = "handoff-test-support"))]
        let worker_attach_started = lillux::time::MonotonicTimer::start();
        ryeos_app::worker_handoff::validate_projected_target_adoption(
            &job,
            &operation,
            &prepared_progress,
            &req,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        let placement = load_local_placement(&state, &req.placement_attestation_hash)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        validate_target_placement_for_adoption(&placement, &req, &operation)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        let _profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
            &placement.credential_reservation.profile_id,
        )
        .await
        .map_err(internal)?;
        if !target_worker_is_attached(
            &state,
            &operation,
            &placement,
            &req.placement_attestation_hash,
            true,
        )
        .map_err(internal)?
        {
            return Err(internal(
                "projected target attachment lost its exact session testimony",
            ));
        }
        let response = target_adoption_response(&req, &operation);
        validate_adopt_response(&response, &req, &operation).map_err(internal)?;
        let terminal_attestation_hash = state
            .state_store
            .publish_worker_handoff_adoption_receipt(&job_id, &operation, &req, &response)
            .map_err(internal)?;
        let receipt = ryeos_app::worker_handoff::WorkerHandoffAdoptionReceiptEvidence::new(
            operation,
            req,
            response.clone(),
        )
        .map_err(internal)?;
        settle_target_adoption_job_from_receipt(&state, &job_id, &receipt).map_err(internal)?;
        #[cfg(any(test, feature = "handoff-test-support"))]
        {
            qualification_measurements.worker_attach_recovery_ms =
                Some(worker_attach_started.elapsed_millis());
        }
        let result = WorkerPlacementAdoptResult::Attached {
            response,
            terminal_attestation_hash,
        };
        result.validate().map_err(internal)?;
        return serde_json::to_value(result).map_err(internal);
    }
    let mut adoption_attempt = TargetAdoptionAttempt::begin(state.clone(), &job_id, &operation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

    let remotes = config::load_remotes_layered(&state.config.app_root, None).map_err(internal)?;
    let source_remote = config::resolve_remote_by_site_id(&remotes, &source_site_id)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let source_key = source_remote
        .remote
        .pinned_signing_key()
        .map_err(internal)?;
    let source_signer = source_remote
        .remote
        .principal_id
        .strip_prefix("fp:")
        .ok_or_else(|| internal("configured source principal is not a fingerprint"))?;
    let staged_final_closure = prepared_progress
        .phase
        .successor_is_only_authorized_writer()
        && [
            &req.target_chain_head_hash,
            &req.writer_grant_hash,
            &req.placement_attestation_hash,
        ]
        .into_iter()
        .all(|hash| job.roots.iter().any(|root| root == hash));
    let payload = if staged_final_closure {
        let authority = state
            .state_store
            .pinned_state_authority()
            .map_err(internal)?;
        let guard = authority.acquire_shared_guard().map_err(internal)?;
        let (payload, large_object_requirements) =
            ryeos_state::sync::export_exact_chain_head_cas_payload_pinned(
                &authority,
                &req.chain_root_id,
                &req.target_chain_head_hash,
                &guard,
            )
            .map_err(internal)?;
        import::require_local_large_object_dependencies(&state, &large_object_requirements)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        if u64::try_from(payload.total_bytes).map_err(internal)? > 64 * 1024 * 1024 {
            return Err(internal(
                "staged worker chain exceeds the admitted recovery payload ceiling",
            ));
        }
        payload
    } else {
        let source_client = RemoteClient::from_remote_cfg(&state, &source_remote.remote);
        let closure = source_client
            .objects_closure_get_with_total_timeout(
                std::slice::from_ref(&req.target_chain_head_hash),
                ObjectsClosureRequestOptions {
                    max_objects: Some(16_384),
                    max_blobs: Some(16_384),
                    max_object_bytes: Some(2 * 1024 * 1024),
                    max_total_object_bytes: Some(16 * 1024 * 1024),
                    max_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                    max_total_blob_bytes: Some(MAX_HANDOFF_CLOSURE_BYTES),
                    max_response_bytes: Some(64 * 1024 * 1024),
                    max_links_per_object: Some(65_536),
                    allow_incomplete: false,
                    allow_untransported_large_objects: true,
                },
                WORKER_SESSION_HANDOFF_PEER_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(error, "fetch committed worker chain")
            })?;
        import::require_local_large_object_dependencies(
            &state,
            &closure.closure.large_object_hashes,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        import::closure_response_to_export_payload(
            &req.chain_root_id,
            &req.target_chain_head_hash,
            &closure.entries,
        )
        .map_err(internal)?
    };
    let source_committed = WorkerSessionHandoffProgress {
        schema: "ryeos.worker_session_handoff_progress.v1".to_owned(),
        operation_id: req.operation_id.clone(),
        phase: source_committed_phase,
        placement_attestation_hash: Some(req.placement_attestation_hash.clone()),
        target_runtime_seed_hash: prepared_progress.target_runtime_seed_hash.clone(),
        writer_grant_hash: Some(req.writer_grant_hash.clone()),
        target_chain_head_hash: Some(req.target_chain_head_hash.clone()),
        credential_reservation_id: prepared_progress.credential_reservation_id.clone(),
        abort_chain_head_hash: None,
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforeSourceCommitEvidenceStage,
        &operation,
    )
    .map_err(internal)?;
    state
        .state_store
        .stage_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution {
                source_principal: Some(source_remote.remote.principal_id.clone()),
                source_peer: Some(source_remote.config_key.clone()),
                job_id: Some(job_id.clone()),
            },
            &job_id,
            source_committed.phase.as_str(),
            &[
                operation.transfer_manifest_hash.clone(),
                req.placement_attestation_hash.clone(),
                req.writer_grant_hash.clone(),
                req.target_chain_head_hash.clone(),
            ],
            Some(source_committed.to_value().map_err(internal)?),
        )
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetSourceCommitEvidenceStaged,
        &operation,
    )
    .map_err(internal)?;

    let placement = load_local_placement(&state, &req.placement_attestation_hash)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    validate_target_placement_for_adoption(&placement, &req, &operation)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let evidence = ryeos_app::worker_handoff::chain_writer_transition_from_placement(
        &placement,
        req.placement_attestation_hash.clone(),
        source_signer.to_owned(),
        state.identity.fingerprint().to_owned(),
    );
    let writer = load_attestation(&state, &req.writer_grant_hash).map_err(internal)?;
    writer
        .verify_with_key(&source_key)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if ryeos_state::objects::ChainWriterTransitionEvidence::from_attestation(&writer)
        .map_err(internal)?
        != evidence
    {
        return Err(HandlerError::BadRequest(
            "source writer grant differs from target placement evidence".into(),
        ));
    }
    let transition = ryeos_state::sync::AdmittedChainWriterTransition {
        evidence,
        writer_grant_hash: req.writer_grant_hash.clone(),
        target_chain_head_hash: req.target_chain_head_hash.clone(),
        source_node_verifying_key: source_key,
        target_node_verifying_key: *state.identity.verifying_key(),
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetSourceCommitEvidenceVerified,
        &operation,
    )
    .map_err(internal)?;

    // Once the exact admitted worker is attached, its retained remote
    // continuation edge proves that this node already finalized the chain
    // import. Worker observations may have advanced the target-signed chain
    // beyond `target_chain_head_hash`, so retrying the raw import comparison
    // would reject a successful adoption as an unrelated local advance.
    #[cfg(any(test, feature = "handoff-test-support"))]
    let worker_attach_started = lillux::time::MonotonicTimer::start();
    if let Some(response) = complete_if_target_worker_attached(
        &state,
        &job_id,
        &req,
        &operation,
        &placement,
        &source_committed,
        &mut adoption_attempt,
    )
    .await?
    {
        #[cfg(any(test, feature = "handoff-test-support"))]
        {
            qualification_measurements.worker_attach_recovery_ms =
                Some(worker_attach_started.elapsed_millis());
        }
        return Ok(response);
    }

    // A launch claim is the durable/process-local seam that distinguishes an
    // unlaunched successor from one whose exact execution task is already in
    // flight. Do not schedule a second launch merely because the dedicated
    // worker projection has not appeared yet. In particular, the worker's
    // start callback must be able to acquire the credential-profile operation
    // lock while this handler waits for attachment.
    let launch_claim = state
        .state_store
        .get_launch_claim(&operation.successor_placement_thread_id)
        .map_err(internal)?;
    let launch_owner_active = launch_claim
        .as_ref()
        .is_some_and(|claim| state.state_store.is_launch_owner_active(&claim.claimed_by));
    if classify_target_launch_claim(launch_claim.is_some(), launch_owner_active)
        .map_err(internal)?
        == TargetLaunchClaimDisposition::AwaitExisting
    {
        #[cfg(any(test, feature = "handoff-test-support"))]
        let worker_attach_started = lillux::time::MonotonicTimer::start();
        let _ = ryeos_app::dedicated_session_service::wait_for_worker_attachment_projection(
            &state,
            &operation.successor_placement_thread_id,
            lillux::time::Duration::from_secs(60),
        )
        .await
        .map_err(internal)?;
        if let Some(response) = complete_if_target_worker_attached(
            &state,
            &job_id,
            &req,
            &operation,
            &placement,
            &source_committed,
            &mut adoption_attempt,
        )
        .await?
        {
            #[cfg(any(test, feature = "handoff-test-support"))]
            {
                qualification_measurements.worker_attach_recovery_ms =
                    Some(worker_attach_started.elapsed_millis());
            }
            return Ok(response);
        }
        return Err(worker_attachment_pending());
    }

    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforeAdoptionPublication,
        &operation,
    )
    .map_err(internal)?;
    let current_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &req.chain_root_id))
        .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let event_replay_started = lillux::time::MonotonicTimer::start();
    if current_head.as_ref().is_some_and(|head| {
        head.target_hash == req.target_chain_head_hash
            && head.signer == state.identity.fingerprint()
    }) {
        state
            .state_store
            .recover_remote_adoption_runtime(&transition)
            .map_err(internal)?;
    } else {
        let authority = state
            .state_store
            .pinned_state_authority()
            .map_err(internal)?;
        let guard = authority.acquire_shared_guard().map_err(internal)?;
        let staged = ryeos_state::sync::stage_chain_import_pinned(&authority, &payload, &guard)
            .map_err(internal)?;
        drop(guard);
        // The state publication boundary verifies the exact current signed
        // head as an ancestor of the admitted source frontier. That permits a
        // chain to return to a node holding its older local mirror while still
        // rejecting forks and rollbacks under the chain lock. Requiring exact
        // equality here would incorrectly make one-way placement permanent.
        state
            .state_store
            .finalize_remote_adoption_import(staged, &transition)
            .map_err(internal)?;
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    {
        qualification_measurements.event_replay_ms = Some(event_replay_started.elapsed_millis());
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAdoptionPublished,
        &operation,
    )
    .map_err(internal)?;
    update_target_progress(
        &state,
        &job_id,
        WorkerHandoffPhase::TargetAdopted,
        &source_committed,
        ryeos_state::SyncJobState::Running,
    )
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAdoptionProjected,
        &operation,
    )
    .map_err(internal)?;

    let profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &placement.credential_reservation.profile_id,
    )
    .await
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let worker_attach_started = lillux::time::MonotonicTimer::start();
    if let Some(response) = complete_if_target_worker_attached_under_profile_lock(
        &state,
        &job_id,
        &req,
        &operation,
        &placement,
        &source_committed,
        &req.placement_attestation_hash,
        &mut adoption_attempt,
    )? {
        #[cfg(any(test, feature = "handoff-test-support"))]
        {
            qualification_measurements.worker_attach_recovery_ms =
                Some(worker_attach_started.elapsed_millis());
        }
        return Ok(response);
    }
    let transfer =
        load_transfer_manifest(&state, &operation.transfer_manifest_hash).map_err(internal)?;
    let operands = load_source_placement_operands(&state, &transfer).map_err(internal)?;
    let predecessor = load_portable_predecessor(&state, &operands.restore).map_err(internal)?;
    let profile = state
        .state_store
        .credential_profile(&placement.credential_reservation.profile_id)
        .map_err(internal)?
        .ok_or_else(|| internal("target credential profile disappeared"))?;
    if profile.owner_principal != owner
        || profile.state != "active"
        || profile.credential_generation != placement.credential_reservation.generation
        || profile.lock_owner.as_deref()
            != Some(placement.credential_reservation.reservation_id.as_str())
    {
        return Err(HandlerError::BadRequest(
            "target credential generation or reservation changed before state install".into(),
        ));
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforeStateInstall,
        &operation,
    )
    .map_err(internal)?;
    let _install = ryeos_app::private_artifact_home::install_portable_state_conditionally(
        &state.config.runtime_state_dir(),
        &profile.home_id,
        &operands.restore.portable_state.selector_contract,
        &operands.restore.upstream_session_id,
        predecessor.as_ref(),
        &operands.portable_tree,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetStateInstalled,
        &operation,
    )
    .map_err(internal)?;
    update_target_progress(
        &state,
        &job_id,
        WorkerHandoffPhase::StateInstalled,
        &source_committed,
        ryeos_state::SyncJobState::Running,
    )
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        &state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetStateInstallProjected,
        &operation,
    )
    .map_err(internal)?;

    // The worker start callback uses the same profile-operation lock to
    // atomically consume the exact credential reservation and attach its
    // process identity. Holding this guard through launch would deadlock that
    // callback until the attachment wait timed out.
    drop(profile_operation);

    #[cfg(any(test, feature = "handoff-test-support"))]
    let worker_attach_started = lillux::time::MonotonicTimer::start();
    let launch_metadata = state
        .state_store
        .get_launch_metadata(&operation.successor_placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("adopted successor has no installed launch metadata"))?;
    let prepared = ryeos_executor::execution::launch::prepare_existing_machine_successor_launch(
        &state,
        &operation.successor_placement_thread_id,
        &launch_metadata,
    )
    .await
    .map_err(|error| HandlerError::BadRequest(error.diagnostic_message()))?;
    let task_state = (*state).clone();
    let task_id = operation.successor_placement_thread_id.clone();
    let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
    let task = tokio::spawn(async move {
        ryeos_executor::execution::launch::launch_prepared_machine_successor_with_handoff(
            task_state, &task_id, prepared, &handoff,
        )
        .await
    });
    let ready_thread_id = await_worker_adoption_handoff(task, ready).await?;
    if ready_thread_id != operation.successor_placement_thread_id {
        return Err(internal(
            "worker adoption handoff returned another successor",
        ));
    }
    let _ = ryeos_app::dedicated_session_service::wait_for_worker_attachment_projection(
        &state,
        &operation.successor_placement_thread_id,
        lillux::time::Duration::from_secs(60),
    )
    .await
    .map_err(internal)?;
    if let Some(response) = complete_if_target_worker_attached(
        &state,
        &job_id,
        &req,
        &operation,
        &placement,
        &source_committed,
        &mut adoption_attempt,
    )
    .await?
    {
        #[cfg(any(test, feature = "handoff-test-support"))]
        {
            qualification_measurements.worker_attach_recovery_ms =
                Some(worker_attach_started.elapsed_millis());
        }
        return Ok(response);
    }
    Err(worker_attachment_pending())
}

/// Admit only the forward branch from a prepared target into successor-writer
/// authority. `AbortAuthorized` is an irreversible competing branch: once the
/// exact source abort successor is durable, no ordinal comparison may turn it
/// back into source-commit evidence or clear its abort head.
fn target_adoption_source_commit_phase(
    current: WorkerHandoffPhase,
) -> anyhow::Result<WorkerHandoffPhase> {
    match current {
        WorkerHandoffPhase::TargetPrepared => Ok(WorkerHandoffPhase::SourceCommitted),
        WorkerHandoffPhase::SourceCommitted
        | WorkerHandoffPhase::TargetAdopted
        | WorkerHandoffPhase::StateInstalled
        | WorkerHandoffPhase::ProcessAttached => Ok(current),
        WorkerHandoffPhase::AbortAuthorized => {
            anyhow::bail!("abort-authorized target placement cannot be adopted")
        }
        WorkerHandoffPhase::Planned
        | WorkerHandoffPhase::SourceExported
        | WorkerHandoffPhase::Completed => {
            anyhow::bail!("target placement phase cannot accept source-commit evidence")
        }
    }
}

/// Resume target-local post-cut work from durable job and signed-chain
/// authority. This path never fabricates a request identity and never contacts
/// a remote as the operator: it runs only after the final source closure and
/// writer grant are already staged under the exact target job.
pub async fn recover_durable_target_handoffs(state: &AppState) -> Result<usize> {
    let mut recovered = 0usize;
    let mut scan_failed = false;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            let after = after
                .as_ref()
                .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str()));
            if scan_failed {
                db.list_sync_jobs_by_operation_type_and_state_after(
                    WORKER_SESSION_HANDOFF_OPERATION,
                    ryeos_state::SyncJobState::Failed,
                    after,
                    64,
                )
            } else {
                db.list_active_sync_jobs_by_operation_type_after(
                    WORKER_SESSION_HANDOFF_OPERATION,
                    after,
                    64,
                )
            }
        })?;
        let Some(last) = jobs.last() else {
            if !scan_failed {
                scan_failed = true;
                after = None;
                continue;
            }
            break;
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            let operation = match WorkerSessionHandoffJobOperation::from_value(
                job.operation.clone(),
            ) {
                Ok(operation) if operation.role == WorkerHandoffJobRole::Target => operation,
                Ok(_) => continue,
                Err(error) => {
                    tracing::error!(job_id = %job.job_id, error = %error, "invalid target worker handoff job retained for operator inspection");
                    continue;
                }
            };
            let Some(progress) = job
                .result
                .clone()
                .map(WorkerSessionHandoffProgress::from_value)
                .transpose()?
            else {
                continue;
            };
            if progress.phase == WorkerHandoffPhase::AbortAuthorized {
                let _operation_guard = target_handoff_operation_lock(&job.job_id)
                    .lock_owned()
                    .await;
                let latest = state
                    .state_store
                    .with_state_db(|db| db.get_sync_job(&job.job_id))?
                    .context("target worker handoff job disappeared during abort recovery")?;
                if matches!(
                    latest.state,
                    ryeos_state::SyncJobState::Completed
                        | ryeos_state::SyncJobState::Failed
                        | ryeos_state::SyncJobState::Cancelled
                ) {
                    continue;
                }
                let latest_progress = latest
                    .result
                    .clone()
                    .map(WorkerSessionHandoffProgress::from_value)
                    .transpose()?
                    .context("target abort recovery lost its durable progress")?;
                if latest_progress.phase != WorkerHandoffPhase::AbortAuthorized {
                    continue;
                }
                if recover_staged_target_abort(state, &latest, &operation, &latest_progress)? {
                    recovered += 1;
                }
                continue;
            }
            if progress.phase.source_is_only_authorized_writer()
                || progress.placement_attestation_hash.is_none()
                || progress.writer_grant_hash.is_none()
                || progress.target_chain_head_hash.is_none()
                || ![
                    progress
                        .placement_attestation_hash
                        .as_ref()
                        .expect("checked"),
                    progress.writer_grant_hash.as_ref().expect("checked"),
                    progress.target_chain_head_hash.as_ref().expect("checked"),
                ]
                .into_iter()
                .all(|hash| job.roots.iter().any(|root| root == hash))
            {
                continue;
            }
            // The target handoff job is the sole pre-attachment launch owner. If
            // an older daemon allowed generic continuation recovery to finalize
            // that exact successor before a dedicated session attached, retain the
            // signed terminal as the chain outcome and settle the operational job
            // around it. This is also the fail-closed repair for the crash window
            // between a pre-attachment terminal commit and reservation cleanup.
            // A running or formerly attached session never enters this path.
            {
                let _operation_guard = target_handoff_operation_lock(&job.job_id)
                    .lock_owned()
                    .await;
                let latest = state
                    .state_store
                    .with_state_db(|db| db.get_sync_job(&job.job_id))?
                    .context("target worker handoff job disappeared during terminal recovery")?;
                let failed_pre_attachment_cleanup = latest.state
                    == ryeos_state::SyncJobState::Failed
                    && latest.phase == "target_terminal_before_attachment";
                if failed_pre_attachment_cleanup
                    || !matches!(
                        latest.state,
                        ryeos_state::SyncJobState::Completed
                            | ryeos_state::SyncJobState::Failed
                            | ryeos_state::SyncJobState::Cancelled
                    )
                {
                    let latest_progress = latest
                        .result
                        .clone()
                        .map(WorkerSessionHandoffProgress::from_value)
                        .transpose()?
                        .context("target terminal recovery lost its durable progress")?;
                    if settle_pre_attachment_target_terminal(
                        state,
                        &latest,
                        &operation,
                        &latest_progress,
                    )? {
                        recovered += 1;
                        continue;
                    }
                }
                if matches!(
                    latest.state,
                    ryeos_state::SyncJobState::Completed
                        | ryeos_state::SyncJobState::Failed
                        | ryeos_state::SyncJobState::Cancelled
                ) {
                    continue;
                }
            }
            let request = WorkerPlacementAdoptRequest {
                operation_id: operation.operation_id.clone(),
                chain_root_id: operation.chain_root_id.clone(),
                target_chain_head_hash: progress.target_chain_head_hash.clone().expect("checked"),
                placement_attestation_hash: progress
                    .placement_attestation_hash
                    .clone()
                    .expect("checked"),
                writer_grant_hash: progress.writer_grant_hash.clone().expect("checked"),
            };
            #[cfg(any(test, feature = "handoff-test-support"))]
            let mut qualification_measurements = TargetAdoptionQualificationMeasurements::default();
            match adopt_authorized(
                request,
                operation.owner_principal.clone(),
                operation.source_site_id.clone(),
                Arc::new(state.clone()),
                #[cfg(any(test, feature = "handoff-test-support"))]
                &mut qualification_measurements,
            )
            .await
            {
                Ok(_) => recovered += 1,
                Err(error) => {
                    let latest = state
                        .state_store
                        .with_state_db(|db| db.get_sync_job(&job.job_id))?
                        .context("target worker handoff job disappeared during recovery")?;
                    if matches!(
                        latest.state,
                        ryeos_state::SyncJobState::Completed
                            | ryeos_state::SyncJobState::Failed
                            | ryeos_state::SyncJobState::Cancelled
                    ) {
                        continue;
                    }
                    let detail = bounded_recovery_error(&error.to_string());
                    state.state_store.with_state_db(|db| {
                        db.update_sync_job(
                            &job.job_id,
                            &ryeos_state::SyncJobUpdate {
                                state: ryeos_state::SyncJobState::Retryable,
                                phase: latest.phase,
                                roots: None,
                                heads: None,
                                uploaded_hashes: Vec::new(),
                                fetched_hashes: Vec::new(),
                                last_error: Some(detail),
                                result: latest.result,
                            },
                        )
                    })?;
                }
            }
        }
        after = Some(next);
    }
    Ok(recovered)
}

/// Fold an already-durable target worker attachment into its operational
/// handoff projection before startup clears the dead daemon generation's
/// current worker binding.
///
/// `attach_worker_process` publishes the worker row, session epoch, workspace
/// owner, and credential lock in one SQLite transaction. Startup deliberately
/// retains those coordinates while old workers are quiesced, so this pass can
/// distinguish a committed attachment from a pre-attachment start. The normal
/// target recovery path can then publish the permanent receipt after worker
/// detachment without inventing historical process authority or contacting a
/// peer.
pub async fn reconcile_observed_target_handoff_attachments_before_detachment(
    state: &AppState,
) -> Result<usize> {
    let mut repaired = 0usize;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state.state_store.with_state_db(|db| {
            let after = after
                .as_ref()
                .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str()));
            db.list_active_sync_jobs_by_operation_type_after(
                WORKER_SESSION_HANDOFF_OPERATION,
                after,
                64,
            )
        })?;
        let Some(last) = jobs.last() else {
            break;
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            let operation = WorkerSessionHandoffJobOperation::from_value(job.operation.clone())?;
            if operation.role != WorkerHandoffJobRole::Target {
                continue;
            }
            let Some(result) = job.result.clone() else {
                // The target job is rooted before placement preparation is
                // published. A crash in that interval legitimately leaves no
                // progress to fold before worker detachment; the normal target
                // recovery pass will re-drive the exact retained operation.
                // An attachment without that progress would be untestified
                // process authority and must still fail closed.
                if let Some(session) = state
                    .state_store
                    .dedicated_session(&operation.successor_placement_thread_id)?
                {
                    if session.worker_instance_id.is_some() || session.worker_boot_epoch.is_some() {
                        bail!("target handoff attached a worker without durable progress");
                    }
                }
                continue;
            };
            let progress = WorkerSessionHandoffProgress::from_value(result)?;
            if progress.phase != WorkerHandoffPhase::StateInstalled {
                continue;
            }
            let Some(session) = state
                .state_store
                .dedicated_session(&operation.successor_placement_thread_id)?
            else {
                continue;
            };
            if session.worker_instance_id.is_none() || session.worker_boot_epoch.is_none() {
                continue;
            }

            let _operation_guard = target_handoff_operation_lock(&job.job_id)
                .lock_owned()
                .await;
            let latest = state
                .state_store
                .with_state_db(|db| db.get_sync_job(&job.job_id))?
                .context("target handoff job disappeared during attachment repair")?;
            if !matches!(
                latest.state,
                ryeos_state::SyncJobState::Running | ryeos_state::SyncJobState::Retryable
            ) {
                continue;
            }
            let latest_operation =
                WorkerSessionHandoffJobOperation::from_value(latest.operation.clone())?;
            let latest_progress = WorkerSessionHandoffProgress::from_value(
                latest
                    .result
                    .clone()
                    .context("target handoff job lost its attachment-repair progress")?,
            )?;
            if latest_operation != operation {
                bail!("target handoff operation changed during attachment repair");
            }
            if latest_progress.phase != WorkerHandoffPhase::StateInstalled {
                continue;
            }
            let placement_attestation_hash = latest_progress
                .placement_attestation_hash
                .as_deref()
                .context("state-installed target handoff has no placement attestation")?;
            let writer_grant_hash = latest_progress
                .writer_grant_hash
                .as_deref()
                .context("state-installed target handoff has no writer grant")?;
            let target_chain_head_hash = latest_progress
                .target_chain_head_hash
                .as_deref()
                .context("state-installed target handoff has no target chain head")?;
            if ![
                operation.transfer_manifest_hash.as_str(),
                placement_attestation_hash,
                writer_grant_hash,
                target_chain_head_hash,
            ]
            .into_iter()
            .all(|hash| latest.roots.iter().any(|root| root == hash))
            {
                bail!("state-installed target handoff lost its rooted adoption authority");
            }
            let request = WorkerPlacementAdoptRequest {
                operation_id: operation.operation_id.clone(),
                chain_root_id: operation.chain_root_id.clone(),
                target_chain_head_hash: target_chain_head_hash.to_owned(),
                placement_attestation_hash: placement_attestation_hash.to_owned(),
                writer_grant_hash: writer_grant_hash.to_owned(),
            };
            request.validate()?;
            let placement = load_local_placement(state, placement_attestation_hash)?;
            validate_target_placement_for_adoption(&placement, &request, &operation)?;
            // A worker can be atomically attached to its session, then fail
            // readiness and be exactly reaped before ProcessAttached is
            // projected into the handoff job. Close that target-terminal
            // branch before taking the live-attachment profile lease; the
            // failure settlement owns its own exact cleanup lease and signed
            // testimony. Contradictory or unproved process states still fall
            // through to the strict attachment validator below.
            if settle_pre_attachment_target_terminal(state, &latest, &operation, &latest_progress)?
            {
                repaired += 1;
                continue;
            }
            let _profile_operation =
                ryeos_app::hosted_operation::acquire_credential_profile_operation(
                    &placement.credential_reservation.profile_id,
                )
                .await?;
            if !target_worker_is_attached(
                state,
                &operation,
                &placement,
                placement_attestation_hash,
                false,
            )? {
                bail!(
                    "state-installed target handoff retained a session worker identity without exact attachment testimony"
                );
            }
            update_target_progress(
                state,
                &job.job_id,
                WorkerHandoffPhase::ProcessAttached,
                &latest_progress,
                latest.state,
            )?;
            repaired += 1;
        }
        after = Some(next);
    }
    Ok(repaired)
}

fn settle_pre_attachment_target_terminal(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &WorkerSessionHandoffJobOperation,
    progress: &WorkerSessionHandoffProgress,
) -> Result<bool> {
    if !matches!(
        progress.phase,
        WorkerHandoffPhase::TargetAdopted | WorkerHandoffPhase::StateInstalled
    ) {
        return Ok(false);
    }
    let Some(successor) = state
        .threads
        .get_thread(&operation.successor_placement_thread_id)?
    else {
        return Ok(false);
    };
    let Some((mut authoritative_successor, _, _)) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &operation.chain_root_id,
            &operation.successor_placement_thread_id,
        )?
    else {
        return Ok(false);
    };
    if authoritative_successor.chain_root_id != operation.chain_root_id
        || authoritative_successor.upstream_thread_id.as_deref()
            != Some(operation.source_placement_thread_id.as_str())
        || authoritative_successor.current_site_id != operation.target_site_id
        || authoritative_successor.requested_by.as_deref()
            != Some(operation.owner_principal.as_str())
        || successor.chain_root_id != authoritative_successor.chain_root_id
        || successor.thread_id != authoritative_successor.thread_id
    {
        bail!("pre-attachment target terminal contradicts its durable handoff job");
    }
    let terminal_before_attachment = authoritative_successor.status.is_terminal();
    if authoritative_successor.status != ryeos_state::objects::ThreadStatus::Created
        && !terminal_before_attachment
    {
        return Ok(false);
    }
    let has_runtime_process = successor.runtime.pid.is_some()
        || successor.runtime.pgid.is_some()
        || successor.runtime.process_identity.is_some();
    if has_runtime_process && terminal_before_attachment {
        bail!("target terminal retains runtime process authority before worker attachment");
    }
    if has_runtime_process {
        return Ok(false);
    }
    let authority = state
        .state_store
        .remote_continuation_authority(
            &operation.chain_root_id,
            &operation.successor_placement_thread_id,
        )?
        .context("target terminal has no signed remote-adoption edge")?;
    if authority.operation_id != operation.operation_id
        || authority.preflight_id != operation.preflight_id
        || authority.preflight_attestation_hash != operation.preflight_attestation_hash
        || authority.successor_thread_id != operation.successor_placement_thread_id
        || authority.target_site_id != operation.target_site_id
    {
        bail!("target terminal remote-adoption authority differs from its durable job");
    }
    let pre_terminal_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .context("pre-attachment target terminal has no signed chain head")?;
    if pre_terminal_head.signer != state.identity.fingerprint() {
        bail!("pre-attachment target terminal chain is not target-owned");
    }
    if authoritative_successor.status == ryeos_state::objects::ThreadStatus::Created {
        if job.state != ryeos_state::SyncJobState::Failed
            || job.phase != "target_terminal_before_attachment"
        {
            return Ok(false);
        }
        let outcome = state.threads.finalize_created_unattached_if_current(
            &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                thread_id: operation.successor_placement_thread_id.clone(),
                status: ryeos_state::objects::ThreadStatus::Failed
                    .as_str()
                    .to_owned(),
                outcome_code: Some("target_handoff_failed_before_attachment".to_owned()),
                result: None,
                error: Some(serde_json::json!({
                    "code":"target_handoff_failed_before_attachment",
                    "message":"target worker handoff failed before process attachment",
                    "operation_id":operation.operation_id,
                })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            },
        )?;
        if !outcome.is_settled() {
            bail!("failed target handoff successor gained an attachment or launch owner");
        }
        authoritative_successor = state
            .state_store
            .get_authoritative_thread_snapshot_with_last_event(
                &operation.chain_root_id,
                &operation.successor_placement_thread_id,
            )?
            .map(|(snapshot, _, _)| snapshot)
            .context("target successor disappeared after pre-attachment terminalization")?;
    }
    // A successor that terminalized before attachment is authoritative
    // regardless of which terminal cause won the race. In particular,
    // startup reconciliation can durably settle an unattached Created row as
    // `killed` from an already-recorded stop intent. Restricting cleanup to
    // `failed` leaves that proved-dead placement retrying forever and keeps
    // its target-local credential generation reserved. The attachment and
    // started-at fences above make every terminal variant equally safe to
    // settle around; the signed terminal itself remains the chain outcome.
    if !authoritative_successor.status.is_terminal() {
        return Ok(false);
    }
    let terminal_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .context("pre-attachment target terminal has no signed terminal chain head")?;
    if terminal_head.signer != state.identity.fingerprint() {
        bail!("pre-attachment target terminal chain lost target ownership");
    }
    let request = WorkerPlacementAdoptRequest {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        target_chain_head_hash: progress
            .target_chain_head_hash
            .clone()
            .context("target terminal progress has no adoption chain head")?,
        placement_attestation_hash: progress
            .placement_attestation_hash
            .clone()
            .context("target terminal progress has no placement attestation")?,
        writer_grant_hash: progress
            .writer_grant_hash
            .clone()
            .context("target terminal progress has no writer grant")?,
    };
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store()?,
        &operation.chain_root_id,
        &terminal_head.target_hash,
        &request.target_chain_head_hash,
    )?;
    drop(guard);
    drop(authority);
    let reservation = state
        .state_store
        .credential_profile_reservation_for_successor(&operation.successor_placement_thread_id)?
        .context("pre-attachment target terminal lost its credential reservation")?;
    if reservation.operation_id != operation.operation_id
        || reservation.owner_principal != operation.owner_principal
        || reservation.profile_id != operation.target_credential_profile_id
    {
        bail!("target terminal credential reservation differs from its durable job");
    }
    if authoritative_successor.status == ryeos_state::objects::ThreadStatus::Completed {
        return settle_pre_attachment_target_completion(
            state,
            job,
            operation,
            progress,
            &request,
            &terminal_head.target_hash,
            &reservation,
        );
    }
    let session = state
        .state_store
        .dedicated_session(&operation.successor_placement_thread_id)?;
    let credential_disposition = match session {
        None => {
            let _profile_operation =
                ryeos_app::hosted_operation::acquire_credential_profile_operation_sync(
                    &operation.target_credential_profile_id,
                );
            match reservation.state.as_str() {
                "reserved" => state
                    .state_store
                    .release_credential_profile_reservation(&reservation.reservation_id)?,
                "released" => {}
                "consumed" => {
                    bail!("target terminal consumed its reservation without a session")
                }
                other => bail!("unknown target credential reservation state {other:?}"),
            }
            "reservation_released"
        }
        Some(session) => {
            if session.chain_root_id != operation.chain_root_id
                || session.owner_principal != operation.owner_principal
                || session.credential_profile_id != operation.target_credential_profile_id
                || session.credential_generation != reservation.credential_generation
                || reservation.state != "consumed"
            {
                bail!("target terminal session contradicts pre-attachment cleanup authority");
            }
            match (
                session.worker_instance_id.as_deref(),
                session.worker_boot_epoch,
            ) {
                (None, None) => match session.state.as_str() {
                    "recovering" | "terminal" => {}
                    "outcome_unknown" => {
                        bail!("target worker cleanup is unproved before failure receipt")
                    }
                    other => bail!(
                        "target terminal retained nonterminal unattached session state {other:?}"
                    ),
                },
                (Some(worker_instance_id), Some(worker_boot_epoch)) => {
                    let worker = state
                        .state_store
                        .worker_process(worker_instance_id)?
                        .context("target terminal session worker projection disappeared")?;
                    if worker.placement_thread_id != operation.successor_placement_thread_id
                        || worker.boot_epoch != worker_boot_epoch
                        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
                        || worker.cleanup_state != "reaped"
                    {
                        bail!("target worker death and reap are not proved before failure receipt");
                    }
                    if !matches!(
                        session.state.as_str(),
                        "recovering" | "outcome_unknown" | "terminal"
                    ) {
                        bail!(
                            "target terminal retained nonterminal worker session state {:?}",
                            session.state
                        );
                    }
                }
                _ => bail!("target terminal session retains a partial worker identity"),
            }
            ryeos_app::dedicated_session_service::abort_session_for_root_stop(
                state,
                &operation.successor_placement_thread_id,
            )?;
            let terminal_session = state
                .state_store
                .dedicated_session(&operation.successor_placement_thread_id)?
                .context("target terminal session disappeared during cleanup")?;
            if terminal_session.state != "terminal" {
                bail!("target session cleanup is not terminal before failure receipt");
            }
            if terminal_session.worker_instance_id.is_some()
                != terminal_session.worker_boot_epoch.is_some()
            {
                bail!("target terminal session retains a partial worker identity");
            }
            if let Some(worker_instance_id) = terminal_session.worker_instance_id.as_deref() {
                let worker = state
                    .state_store
                    .worker_process(worker_instance_id)?
                    .context("target terminal worker projection disappeared after cleanup")?;
                if worker.boot_epoch != terminal_session.worker_boot_epoch.expect("paired")
                    || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
                    || worker.cleanup_state != "reaped"
                {
                    bail!("target terminal worker cleanup changed before failure receipt");
                }
            }
            let workspace = state
                .state_store
                .execution_workspace(&terminal_session.workspace_id)?
                .context("target terminal workspace disappeared after cleanup")?;
            if workspace.state != ryeos_app::runtime_db::WorkspaceState::Closed {
                bail!("target terminal workspace is not closed before failure receipt");
            }
            let profile = state
                .state_store
                .credential_profile(&operation.target_credential_profile_id)?
                .context("target terminal credential profile disappeared")?;
            if terminal_session
                .worker_instance_id
                .as_deref()
                .is_some_and(|worker| profile.lock_owner.as_deref() == Some(worker))
            {
                bail!("target terminal credential lease remains owned by the dead worker");
            }
            "consumed_session_terminal"
        }
    };
    let failure = WorkerPlacementFailureResponse {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        placement_thread_id: operation.successor_placement_thread_id.clone(),
        target_chain_head_hash: terminal_head.target_hash.clone(),
        terminal_status: authoritative_successor.status.as_str().to_owned(),
        credential_disposition: credential_disposition.to_owned(),
    };
    failure.validate_against(&request, operation)?;
    let terminal_attestation_hash = state
        .state_store
        .publish_worker_handoff_terminal_failure(operation, &request, &failure)?;
    let terminal_error = state
        .state_store
        .get_thread_result(&operation.successor_placement_thread_id)?
        .and_then(|result| result.error)
        .map(|error| error.to_string())
        .unwrap_or_else(|| "target successor failed before worker attachment".to_owned());
    if job.state != ryeos_state::SyncJobState::Failed
        || job.phase != "target_terminal_before_attachment"
        || job.heads.as_slice() != [terminal_head.target_hash.as_str()]
        || !job
            .roots
            .iter()
            .any(|root| root == &terminal_attestation_hash)
    {
        state.state_store.with_state_db(|db| {
            db.update_sync_job(
                &job.job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Failed,
                    phase: "target_terminal_before_attachment".to_owned(),
                    roots: Some({
                        let mut roots = job.roots.clone();
                        roots.push(terminal_attestation_hash.clone());
                        roots.sort();
                        roots.dedup();
                        roots
                    }),
                    heads: Some(vec![terminal_head.target_hash.clone()]),
                    uploaded_hashes: Vec::new(),
                    fetched_hashes: Vec::new(),
                    last_error: Some(bounded_recovery_error(&terminal_error)),
                    result: Some(progress.to_value()?),
                },
            )
        })?;
    }
    tracing::error!(
        job_id = %job.job_id,
        thread_id = %operation.successor_placement_thread_id,
        chain_head_hash = %terminal_head.target_hash,
        "settled target handoff around signed pre-attachment terminal"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn settle_pre_attachment_target_completion(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &WorkerSessionHandoffJobOperation,
    progress: &WorkerSessionHandoffProgress,
    request: &WorkerPlacementAdoptRequest,
    terminal_chain_head_hash: &str,
    reservation: &ryeos_app::runtime_db::CredentialProfileReservationRecord,
) -> Result<bool> {
    if reservation.state != "consumed" {
        bail!("completed target successor did not consume its credential reservation");
    }
    let Some(session) = state
        .state_store
        .dedicated_session(&operation.successor_placement_thread_id)?
    else {
        return Ok(false);
    };
    if session.chain_root_id != operation.chain_root_id
        || session.owner_principal != operation.owner_principal
        || session.credential_profile_id != operation.target_credential_profile_id
        || session.credential_generation != reservation.credential_generation
        || session.terminal_reason.as_deref() != Some("completed")
        || !matches!(
            session.state.as_str(),
            "freezing"
                | "frozen"
                | "verifying"
                | "publish_ready"
                | "publishing"
                | "discarding"
                | "terminal"
        )
    {
        return Ok(false);
    }
    let (Some(worker_instance_id), Some(worker_boot_epoch)) = (
        session.worker_instance_id.as_deref(),
        session.worker_boot_epoch,
    ) else {
        return Ok(false);
    };
    let worker = state
        .state_store
        .worker_process(worker_instance_id)?
        .context("completed target successor lost its worker process testimony")?;
    let placement_attestation = load_attestation(state, &request.placement_attestation_hash)?;
    placement_attestation.verify_with_key(state.identity.verifying_key())?;
    let placement = WorkerPlacementAdmissionEvidence::from_attestation(&placement_attestation)?;
    validate_target_placement_for_adoption(&placement, request, operation)?;
    if session.remote_thread_id.as_deref()
        != Some(
            placement
                .credential_reservation
                .upstream_session_id
                .as_str(),
        )
    {
        return Ok(false);
    }
    let profile = state
        .state_store
        .credential_profile(&session.credential_profile_id)?
        .context("completed target successor lost its credential profile")?;
    if worker.placement_thread_id != operation.successor_placement_thread_id
        || worker.worker_instance_id != worker_instance_id
        || worker.boot_epoch != worker_boot_epoch
        || worker.session_capsule_hash != session.admitted_capsule_hash
        || !placement_admits_session_capsule(
            &placement.target_persistent_session_capsules,
            &session.admitted_capsule_hash,
        )
        || worker.lifecycle_generation != session.credential_generation
        || worker.lifecycle_generation != placement.credential_reservation.generation
        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
        || worker.cleanup_state != "reaped"
        || profile.profile_id != session.credential_profile_id
        || profile.owner_principal != operation.owner_principal
        || profile.lock_owner.as_deref() == Some(worker_instance_id)
        || !matches!(
            profile.state.as_str(),
            "unauthenticated" | "enrolling" | "confirming" | "active"
        )
    {
        return Ok(false);
    }

    let completion = WorkerPlacementCompletionResponse {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        placement_thread_id: operation.successor_placement_thread_id.clone(),
        target_chain_head_hash: terminal_chain_head_hash.to_owned(),
        terminal_status: "completed".to_owned(),
        credential_disposition: "completed_session_released".to_owned(),
    };
    completion.validate_against(request, operation)?;
    let terminal_attestation_hash = state
        .state_store
        .publish_worker_handoff_terminal_completion(operation, request, &completion)?;
    if job.state != ryeos_state::SyncJobState::Completed
        || job.phase != "target_completed_before_attachment"
        || job.heads.as_slice() != [terminal_chain_head_hash]
        || !job
            .roots
            .iter()
            .any(|root| root == &terminal_attestation_hash)
    {
        state.state_store.with_state_db(|db| {
            db.update_sync_job(
                &job.job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Completed,
                    phase: "target_completed_before_attachment".to_owned(),
                    roots: Some({
                        let mut roots = job.roots.clone();
                        roots.push(terminal_attestation_hash.clone());
                        roots.sort();
                        roots.dedup();
                        roots
                    }),
                    heads: Some(vec![terminal_chain_head_hash.to_owned()]),
                    uploaded_hashes: Vec::new(),
                    fetched_hashes: Vec::new(),
                    last_error: None,
                    result: Some(progress.to_value()?),
                },
            )
        })?;
    }
    tracing::info!(
        job_id = %job.job_id,
        thread_id = %operation.successor_placement_thread_id,
        chain_head_hash = %terminal_chain_head_hash,
        "settled target handoff around signed completion before attachment projection"
    );
    Ok(true)
}

fn recover_staged_target_abort(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &WorkerSessionHandoffJobOperation,
    progress: &WorkerSessionHandoffProgress,
) -> Result<bool> {
    let abort_head = progress
        .abort_chain_head_hash
        .as_deref()
        .context("abort-authorized target job has no source abort head")?;
    if let Some(fence) = state
        .state_store
        .worker_handoff_abort_fence(&operation.operation_id)?
    {
        if fence.target_operation != *operation || fence.abort_chain_head_hash != abort_head {
            bail!("target abort recovery contradicts its permanent fence");
        }
        if let Some(disposition) = fence.terminal_disposition {
            let response = WorkerPlacementAbortResponse {
                operation_id: operation.operation_id.clone(),
                chain_root_id: operation.chain_root_id.clone(),
                disposition,
            };
            settle_target_abort_job_from_receipt(
                state,
                &job.job_id,
                operation,
                abort_head,
                &response,
            )?;
            return Ok(true);
        }
    }
    if !job.roots.iter().any(|root| root == abort_head) {
        bail!("target abort head is not retained by its durable job");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_app::worker_handoff::validate_handoff_abort_authority(
        &authority.cas_store()?,
        operation,
        abort_head,
    )?;
    drop(guard);
    state
        .state_store
        .publish_worker_handoff_abort_fence(operation, abort_head)?;
    let reservation = state
        .state_store
        .credential_profile_reservation_for_successor(&operation.successor_placement_thread_id)?;
    let disposition = if let Some(reservation) = reservation {
        if reservation.operation_id != operation.operation_id
            || reservation.owner_principal != operation.owner_principal
            || reservation.profile_id != operation.target_credential_profile_id
        {
            bail!("target credential reservation differs from aborted handoff");
        } else {
            match reservation.state.as_str() {
                "reserved" => {
                    state
                        .state_store
                        .release_credential_profile_reservation(&reservation.reservation_id)?;
                    "reservation_released"
                }
                "released" => "reservation_released",
                "consumed" => bail!("aborted target placement already consumed its reservation"),
                other => bail!("unknown target credential reservation state {other:?}"),
            }
        }
    } else {
        "target_absent"
    };
    let response = WorkerPlacementAbortResponse {
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        disposition: disposition.to_owned(),
    };
    let mut source_operation = operation.clone();
    source_operation.role = WorkerHandoffJobRole::Source;
    let request = WorkerPlacementAbortRequest {
        operation: source_operation,
        abort_chain_head_hash: abort_head.to_owned(),
    };
    response.validate_against(&request)?;
    state
        .state_store
        .publish_worker_handoff_abort_terminal_receipt(operation, abort_head, disposition)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetAbortReceiptPublished,
        operation,
    )?;
    settle_target_abort_job_from_receipt(state, &job.job_id, operation, abort_head, &response)?;
    Ok(true)
}

fn bounded_recovery_error(value: &str) -> String {
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

fn load_attestation(state: &AppState, hash: &str) -> Result<ryeos_state::objects::Attestation> {
    if !lillux::valid_hash(hash) {
        bail!("attestation hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(hash)?
        .context("attestation is absent from CAS")?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    if lillux::sha256_hex(lillux::canonical_json(&value)?.as_bytes()) != hash {
        bail!("attestation content hash changed");
    }
    Ok(attestation)
}

fn load_local_placement(state: &AppState, hash: &str) -> Result<WorkerPlacementAdmissionEvidence> {
    let attestation = load_attestation(state, hash)?;
    attestation.verify_with_key(state.identity.verifying_key())?;
    if attestation.is_expired_at(&lillux::time::iso8601_now())? {
        bail!("target placement attestation is expired");
    }
    WorkerPlacementAdmissionEvidence::from_attestation(&attestation)
}

fn load_transfer_manifest(
    state: &AppState,
    hash: &str,
) -> Result<ryeos_state::objects::PlacementTransferManifest> {
    if !lillux::valid_hash(hash) {
        bail!("transfer manifest hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(hash)?
        .context("worker placement transfer manifest is absent")?;
    let manifest = ryeos_state::objects::PlacementTransferManifest::from_current_value(value)?;
    if manifest.content_hash()? != hash {
        bail!("worker placement transfer manifest content hash changed");
    }
    Ok(manifest)
}

fn load_portable_predecessor(
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
    let value = cas
        .get_object(manifest_hash)?
        .context("portable-state predecessor manifest is absent")?;
    if lillux::sha256_hex(lillux::canonical_json(&value)?.as_bytes()) != manifest_hash {
        bail!("portable-state predecessor manifest content hash changed");
    }
    let manifest = ryeos_state::objects::StateManifest::from_current_value(value)?;
    if manifest.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
        || manifest.publisher_chain_root_id != restore.source_position.chain_root_id
    {
        bail!("portable-state predecessor belongs to another execution");
    }
    let entry = manifest
        .objects
        .iter()
        .find(|entry| entry.blob_hash == tree_hash)
        .context("portable-state predecessor attachment is absent")?;
    if entry.media_type != ryeos_state::objects::PORTABLE_STATE_TREE_MEDIA_TYPE {
        bail!("portable-state predecessor has another media type");
    }
    let bytes = cas
        .get_blob(tree_hash)?
        .context("portable-state predecessor tree is absent")?;
    if lillux::sha256_hex(&bytes) != tree_hash || u64::try_from(bytes.len())? != entry.size_bytes {
        bail!("portable-state predecessor size or digest changed");
    }
    Ok(Some(
        ryeos_state::objects::PortableStateTree::from_canonical_bytes(
            &bytes,
            &restore.portable_state.selector_contract,
            &restore.upstream_session_id,
        )?,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetLaunchClaimDisposition {
    Launch,
    AwaitExisting,
}

fn classify_target_launch_claim(
    claim_present: bool,
    launch_owner_active: bool,
) -> Result<TargetLaunchClaimDisposition> {
    match (claim_present, launch_owner_active) {
        (false, false) => Ok(TargetLaunchClaimDisposition::Launch),
        (true, true) => Ok(TargetLaunchClaimDisposition::AwaitExisting),
        (true, false) => {
            bail!("target successor has a retained launch claim without an active owner")
        }
        (false, true) => bail!("target successor launch owner has no durable claim"),
    }
}

async fn complete_if_target_worker_attached(
    state: &Arc<AppState>,
    job_id: &str,
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
    placement: &WorkerPlacementAdmissionEvidence,
    progress: &WorkerSessionHandoffProgress,
    attempt: &mut TargetAdoptionAttempt,
) -> Result<Option<Value>, HandlerError> {
    if !target_worker_is_attached(
        state,
        operation,
        placement,
        &request.placement_attestation_hash,
        progress.phase == WorkerHandoffPhase::ProcessAttached,
    )
    .map_err(internal)?
    {
        return Ok(None);
    }
    let _profile_operation = ryeos_app::hosted_operation::acquire_credential_profile_operation(
        &placement.credential_reservation.profile_id,
    )
    .await
    .map_err(internal)?;
    complete_if_target_worker_attached_under_profile_lock(
        state,
        job_id,
        request,
        operation,
        placement,
        progress,
        &request.placement_attestation_hash,
        attempt,
    )
}

#[allow(clippy::too_many_arguments)]
fn complete_if_target_worker_attached_under_profile_lock(
    state: &AppState,
    job_id: &str,
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
    placement: &WorkerPlacementAdmissionEvidence,
    progress: &WorkerSessionHandoffProgress,
    placement_attestation_hash: &str,
    attempt: &mut TargetAdoptionAttempt,
) -> Result<Option<Value>, HandlerError> {
    if !target_worker_is_attached(
        state,
        operation,
        placement,
        placement_attestation_hash,
        progress.phase == WorkerHandoffPhase::ProcessAttached,
    )
    .map_err(internal)?
    {
        return Ok(None);
    }
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetProcessAttachmentObserved,
        operation,
    )
    .map_err(internal)?;
    update_target_progress(
        state,
        job_id,
        WorkerHandoffPhase::ProcessAttached,
        progress,
        ryeos_state::SyncJobState::Running,
    )
    .map_err(internal)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetProcessAttachmentProjected,
        operation,
    )
    .map_err(internal)?;
    complete_target_adoption(state, job_id, request, operation, attempt)
        .map(Some)
        .map_err(internal)
}

fn worker_attachment_pending() -> HandlerError {
    HandlerError::Structured {
        code: "worker_attachment_pending".to_owned(),
        status: 503,
        body: serde_json::json!({
            "code":"worker_attachment_pending",
            "error":"target worker launch was scheduled but exact attachment was not observed before the bounded deadline",
            "retryable":true,
        }),
    }
}

fn target_worker_is_attached(
    state: &AppState,
    operation: &WorkerSessionHandoffJobOperation,
    placement: &WorkerPlacementAdmissionEvidence,
    placement_attestation_hash: &str,
    attachment_was_projected: bool,
) -> Result<bool> {
    let Some(session) = state
        .state_store
        .dedicated_session(&operation.successor_placement_thread_id)?
    else {
        return Ok(false);
    };
    if session.chain_root_id != operation.chain_root_id
        || session.owner_principal != operation.owner_principal
        || session.credential_profile_id != placement.credential_reservation.profile_id
        || session.credential_generation != placement.credential_reservation.generation
        || session.remote_thread_id.as_deref()
            != Some(
                placement
                    .credential_reservation
                    .upstream_session_id
                    .as_str(),
            )
    {
        bail!("attached target worker contradicts its placement admission");
    }
    let remote = state
        .state_store
        .remote_continuation_authority(
            &operation.chain_root_id,
            &operation.successor_placement_thread_id,
        )?
        .context("attached target worker has no signed remote-continuation authority")?;
    if remote.operation_id != operation.operation_id
        || remote.preflight_id != operation.preflight_id
        || remote.preflight_attestation_hash != operation.preflight_attestation_hash
        || remote.target_placement_attestation_hash != placement_attestation_hash
        || remote.target_launch_capsule_hash != placement.target_launch_capsule_hash
        || remote.target_runtime_seed_hash != placement.target_runtime_seed_hash
        || remote.source_site_id != operation.source_site_id
        || remote.target_site_id != operation.target_site_id
        || remote.successor_thread_id != operation.successor_placement_thread_id
    {
        bail!("attached target worker remote continuation differs from its durable operation");
    }
    let Some(worker_id) = session.worker_instance_id.as_deref() else {
        // A safely reaped worker clears its current process binding. The
        // ProcessAttached job projection was written only after this exact
        // session and remote-continuation authority were observed together,
        // so it remains sufficient start testimony for publishing the
        // permanent receipt after terminalization.
        return Ok(attachment_was_projected);
    };
    let worker = state
        .state_store
        .worker_process(worker_id)?
        .context("attached target worker projection disappeared")?;
    let profile = state
        .state_store
        .credential_profile(&session.credential_profile_id)?
        .context("attached target worker lost its credential profile")?;
    validate_target_worker_attachment(
        &session,
        &worker,
        &profile,
        &operation.successor_placement_thread_id,
        &operation.owner_principal,
        &placement.target_persistent_session_capsules,
        placement.credential_reservation.generation,
        attachment_was_projected,
    )
}

fn validate_target_worker_attachment(
    session: &ryeos_app::runtime_db::DedicatedSessionRecord,
    worker: &ryeos_app::runtime_db::WorkerProcessRecord,
    profile: &ryeos_app::runtime_db::CredentialProfileRecord,
    expected_placement_thread_id: &str,
    expected_owner_principal: &str,
    expected_session_capsules: &BTreeMap<String, String>,
    expected_credential_generation: u64,
    attachment_was_projected: bool,
) -> Result<bool> {
    if session.state == "outcome_unknown" || worker.cleanup_state == "unproved" {
        bail!("unproved target worker outcome cannot establish attachment");
    }
    if worker.placement_thread_id != expected_placement_thread_id
        || session.worker_instance_id.as_deref() != Some(worker.worker_instance_id.as_str())
        || session.worker_boot_epoch != Some(worker.boot_epoch)
        || worker.session_capsule_hash != session.admitted_capsule_hash
        || !placement_admits_session_capsule(
            expected_session_capsules,
            &session.admitted_capsule_hash,
        )
        || worker.lifecycle_generation != session.credential_generation
        || worker.lifecycle_generation != expected_credential_generation
        || profile.profile_id != session.credential_profile_id
        || profile.owner_principal != expected_owner_principal
        || profile.credential_generation != session.credential_generation
        || profile.lock_owner.as_deref() != Some(worker.worker_instance_id.as_str())
        || !matches!(
            profile.state.as_str(),
            "unauthenticated" | "enrolling" | "confirming" | "active"
        )
    {
        bail!(
            "attached target worker lost its exact session, capsule, lifecycle, or credential authority"
        );
    }

    let committed = match worker.state {
        ryeos_app::runtime_db::WorkerProcessState::Attached => {
            worker.cleanup_state == "owned" && session.state == "binding"
        }
        ryeos_app::runtime_db::WorkerProcessState::Live => {
            worker.cleanup_state == "owned"
                && matches!(
                    session.state.as_str(),
                    "recovering" | "idle" | "turn_running" | "awaiting_approval"
                )
        }
        ryeos_app::runtime_db::WorkerProcessState::Draining => {
            worker.cleanup_state == "draining" && session.state == "draining"
        }
        ryeos_app::runtime_db::WorkerProcessState::Dead => {
            attachment_was_projected && worker.cleanup_state == "reaped"
        }
        ryeos_app::runtime_db::WorkerProcessState::Starting => false,
    };
    if !committed
        && !matches!(
            worker.state,
            ryeos_app::runtime_db::WorkerProcessState::Starting
                | ryeos_app::runtime_db::WorkerProcessState::Dead
        )
    {
        bail!("target worker process and session lifecycle are contradictory");
    }
    Ok(committed)
}

fn placement_admits_session_capsule(
    expected_session_capsules: &BTreeMap<String, String>,
    session_capsule_hash: &str,
) -> bool {
    expected_session_capsules
        .values()
        .any(|capsule| capsule == session_capsule_hash)
}

fn update_target_progress(
    state: &AppState,
    job_id: &str,
    phase: WorkerHandoffPhase,
    basis: &WorkerSessionHandoffProgress,
    job_state: ryeos_state::SyncJobState,
) -> Result<()> {
    let mut progress = basis.clone();
    progress.phase = std::cmp::max(progress.phase, phase);
    progress.validate()?;
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: job_state,
                phase: progress.phase.as_str().to_owned(),
                roots: None,
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(progress.to_value()?),
            },
        )
    })
}

fn validate_target_placement_for_adoption(
    placement: &WorkerPlacementAdmissionEvidence,
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
) -> Result<()> {
    if placement.operation_id != request.operation_id
        || placement.preflight_id != operation.preflight_id
        || placement.preflight_attestation_hash != operation.preflight_attestation_hash
        || placement.chain_root_id != request.chain_root_id
        || placement.source_site_id != operation.source_site_id
        || placement.target_site_id != operation.target_site_id
        || placement.successor_placement_thread_id != operation.successor_placement_thread_id
    {
        bail!("target placement evidence differs from its durable job");
    }
    Ok(())
}

fn target_adoption_response(
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
) -> WorkerPlacementAdoptResponse {
    WorkerPlacementAdoptResponse {
        operation_id: request.operation_id.clone(),
        chain_root_id: request.chain_root_id.clone(),
        placement_thread_id: operation.successor_placement_thread_id.clone(),
        target_chain_head_hash: request.target_chain_head_hash.clone(),
        delivery: "attached".to_owned(),
    }
}

fn validate_adopt_response(
    response: &WorkerPlacementAdoptResponse,
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
) -> Result<()> {
    if response.operation_id != request.operation_id
        || response.chain_root_id != request.chain_root_id
        || response.placement_thread_id != operation.successor_placement_thread_id
        || response.target_chain_head_hash != request.target_chain_head_hash
        || response.delivery != "attached"
    {
        bail!("target adoption response contradicts its durable operation");
    }
    Ok(())
}

/// Fold permanent target-signed adoption testimony into its operational job
/// without consuming another retry attempt. Receipt authority must remain
/// settleable even when the ordinary retry budget was exhausted by crashes
/// immediately after publication.
fn settle_target_adoption_job_from_receipt(
    state: &AppState,
    job_id: &str,
    receipt: &ryeos_app::worker_handoff::WorkerHandoffAdoptionReceiptEvidence,
) -> Result<()> {
    let Some(job) = state
        .state_store
        .with_state_db(|db| db.get_sync_job(job_id))?
    else {
        return Ok(());
    };
    let operation = WorkerSessionHandoffJobOperation::from_value(job.operation.clone())?;
    if operation != receipt.target_operation {
        bail!("terminal adoption receipt contradicts its retained target job");
    }
    validate_adopt_response(&receipt.response, &receipt.request, &operation)?;
    if job.state == ryeos_state::SyncJobState::Completed {
        let existing: WorkerPlacementAdoptResponse = serde_json::from_value(
            job.result
                .context("completed target adoption job has no terminal response")?,
        )?;
        if existing != receipt.response {
            bail!("terminal adoption receipt changed after target settlement");
        }
        return Ok(());
    }
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        bail!("terminal adoption receipt conflicts with another target disposition");
    }
    let progress = WorkerSessionHandoffProgress::from_value(
        job.result
            .clone()
            .context("active target adoption job has no progress")?,
    )?;
    ryeos_app::worker_handoff::validate_projected_target_adoption(
        &job,
        &operation,
        &progress,
        &receipt.request,
    )?;
    state.state_store.with_state_db(|db| {
        db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Completed,
                phase: WorkerHandoffPhase::Completed.as_str().to_owned(),
                roots: None,
                heads: Some(vec![receipt.request.target_chain_head_hash.clone()]),
                uploaded_hashes: job.uploaded_hashes,
                fetched_hashes: job.fetched_hashes,
                last_error: None,
                result: Some(serde_json::to_value(&receipt.response)?),
            },
        )
    })
}

fn complete_target_adoption(
    state: &AppState,
    job_id: &str,
    request: &WorkerPlacementAdoptRequest,
    operation: &WorkerSessionHandoffJobOperation,
    attempt: &mut TargetAdoptionAttempt,
) -> Result<Value> {
    let response = target_adoption_response(request, operation);
    validate_adopt_response(&response, request, operation)?;
    if attempt.job_id != job_id {
        bail!("target adoption attempt belongs to another durable job");
    }
    let terminal_attestation_hash = state
        .state_store
        .publish_worker_handoff_adoption_receipt(job_id, operation, request, &response)?;
    attempt.complete(serde_json::to_value(&response)?)?;
    let result = WorkerPlacementAdoptResult::Attached {
        response,
        terminal_attestation_hash,
    };
    result.validate()?;
    Ok(serde_json::to_value(result)?)
}

async fn await_worker_adoption_handoff(
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

async fn prepare_after_staging(
    state: &Arc<AppState>,
    req: &WorkerPlacementPrepareRequest,
    _operation: &WorkerSessionHandoffJobOperation,
    owner: &str,
    target_project_path: &Path,
    job_id: &str,
    source: &SourcePlacementOperands,
    preflight: &WorkerPlacementPreflightEvidence,
) -> Result<PreparedTargetPlacement> {
    let source_resume = source
        .launch_metadata
        .resume_context
        .as_ref()
        .context("source placement has no ResumeContext")?;
    if source_resume.current_site_id != req.source_site_id
        || source_resume.origin_site_id != source.manifest.origin_site_id
        || source_resume.principal_identifier() != owner
    {
        bail!("source launch ledger contradicts transfer owner or sites");
    }
    if preflight.preflight_id != req.preflight_id
        || preflight.owner_principal != owner
        || preflight.chain_root_id != req.chain_root_id
        || preflight.origin_site_id != source.manifest.origin_site_id
        || preflight.source_site_id != req.source_site_id
        || preflight.target_site_id != req.target_site_id
        || preflight.source_placement_thread_id != source.manifest.source_placement_thread_id
        || preflight.successor_placement_thread_id != source.manifest.successor_placement_thread_id
        || preflight.source_launch_capsule_hash != source.manifest.source_launch_capsule_hash
        || preflight.target_project_path != req.target_project_path
        || preflight.project_route_digest != req.project_route_digest
        || preflight.target_credential_profile_id != req.target_credential_profile_id
        || preflight.follow_delivery_reservation_attestation_hash
            != req.follow_delivery_reservation_attestation_hash
        || preflight.outer_exact_program_hash != source.launch_capsule.exact_program_hash
    {
        bail!("final source placement differs from target preflight evidence");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store()?,
        &req.chain_root_id,
        &req.source_chain_head_hash,
        &preflight.source_chain_head_hash,
    )?;
    drop(guard);
    let candidate = source
        .restore
        .project_candidate_snapshot_hash
        .as_deref()
        .context("remote handoff checkpoint has no project candidate")?;
    state
        .state_store
        .verify_project_snapshot_closure(candidate)?;
    let target_project_ref = ryeos_executor::execution::project_source::canonical_project_ref(
        target_project_path
            .to_str()
            .context("target project path is not UTF-8")?,
    )?;
    let target_project_hash = lillux::sha256_hex(target_project_ref.as_bytes());
    let principal_key = ryeos_state::refs::principal_storage_key(owner)?.to_owned();
    let target_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(&principal_key, &target_project_hash))?
        .context("target project has no principal-scoped HEAD")?;
    let (project_rebind, target_identity, target_overlay_root) =
        ryeos_app::worker_handoff::build_remote_project_rebind(
            &source.source_snapshot.project_authority,
            target_project_path,
            &req.target_site_id,
            owner,
            candidate,
            &target_head,
            &target_project_hash,
            &req.project_route_digest,
        )?;

    let credential_contract = source_credential_contract(state, source)?;
    let target_profile = state
        .state_store
        .credential_profile(&req.target_credential_profile_id)?
        .context("target credential profile does not exist")?;
    if target_profile.owner_principal != owner
        || target_profile.state != "active"
        || target_profile.lock_owner.is_some()
    {
        bail!("target credential profile is not active, unlocked, and owner-exact");
    }
    let target_account = target_profile
        .sanitized_account
        .as_ref()
        .context("target credential profile has no confirmed account")?;
    let subject_contract_digest = credential_contract.contract_digest()?;
    let subject_digest = credential_contract.derive_subject_digest(target_account)?;
    if subject_contract_digest != source.restore.credential_subject_contract_digest
        || subject_digest != source.restore.credential_subject_digest
        || subject_contract_digest != preflight.credential_subject_contract_digest
        || subject_digest != preflight.credential_subject_digest
        || target_profile.credential_generation != preflight.target_credential_generation
        || source.restore.upstream_session_id != preflight.upstream_session_id
    {
        bail!("target credential profile represents another workload account");
    }
    let reservation_id = format!("worker-handoff:{}", req.operation_id);
    let reserved = state.state_store.reserve_credential_profile_generation(
        ryeos_app::runtime_db::NewCredentialProfileReservation {
            reservation_id: &reservation_id,
            operation_id: &req.operation_id,
            successor_thread_id: &source.manifest.successor_placement_thread_id,
            profile_id: &target_profile.profile_id,
            owner_principal: owner,
            credential_generation: target_profile.credential_generation,
            subject_contract_digest: &subject_contract_digest,
            subject_digest: &subject_digest,
            checkpoint_manifest_hash: &source.manifest.checkpoint_manifest_hash,
            upstream_session_id: &source.restore.upstream_session_id,
        },
    )?;
    let credential_reservation = CredentialGenerationReservation {
        profile_id: reserved.profile_id,
        owner_principal: reserved.owner_principal,
        generation: reserved.credential_generation,
        reservation_id: reserved.reservation_id,
        upstream_session_id: reserved.upstream_session_id,
        subject_contract_digest: reserved.subject_contract_digest,
        subject_digest: reserved.subject_digest,
    };

    let target_ledger_identity = req.source_accounting_frontier.as_ref().and_then(|_| {
        state
            .accounting
            .as_ref()
            .map(|ledger| ledger.site_identity())
    });
    let accounting = ryeos_app::worker_handoff::build_target_accounting_conservation(
        req.source_accounting_frontier.as_ref(),
        target_ledger_identity,
        &req.operation_id,
    )?;
    if source.launch_metadata.accounting_scope != accounting.source_scope {
        bail!("source accounting request differs from its launch capsule");
    }
    if let Some(target_scope) = accounting.target_scope.as_ref() {
        let ledger = state
            .accounting
            .as_ref()
            .context("target accounting ledger is unavailable")?;
        ledger.admit_handoff_target_scope(
            target_scope,
            &req.chain_root_id,
            accounting.target_cap_usd_nanos,
            accounting.target_directive_cap_usd_nanos,
        )?;
    }

    let resume_rebind = RemoteResumeContextRebind {
        source_site_id: req.source_site_id.clone(),
        target_site_id: req.target_site_id.clone(),
        target_project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: target_project_path.to_path_buf(),
        },
        target_project_authority: project_rebind.target_authority.clone(),
        target_stable_project_identity: Some(target_identity),
        target_local_overlay_root: target_overlay_root,
        target_original_snapshot_hash: Some(candidate.to_owned()),
        target_original_pushed_head_ref: None,
        target_state_root: None,
        source_credential_profile_id: source_profile_id(source_resume)?,
        credential_reservation: credential_reservation.clone(),
    };
    let target_resume = source_resume.for_remote_worker_adoption(&resume_rebind)?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let project_materialization_started = lillux::time::MonotonicTimer::start();
    let prepared = ryeos_executor::execution::launch::prepare_remote_machine_successor_launch(
        state,
        &source.manifest.successor_placement_thread_id,
        &source.manifest.source_placement_thread_id,
        &source.launch_metadata,
        &source.launch_capsule,
        source_resume,
        &target_resume,
        &resume_rebind,
        accounting.target_scope.clone(),
    )
    .await
    .map_err(|error| anyhow::anyhow!(format!("{error:#}")))?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    let project_materialization_ms = project_materialization_started.elapsed_millis();
    let target_metadata = prepared.launch_metadata().clone();
    let target_capsule = target_metadata
        .admitted_launch_capsule()?
        .context("target placement preparation produced no launch capsule")?;
    let target_capsule_hash = target_capsule.content_hash()?;
    let target_sessions = target_capsule.admitted_persistent_session_capsules()?;
    let target_programs = persistent_programs(state, &target_sessions)?;
    let source_programs = source
        .restore
        .persistent_dependencies
        .iter()
        .map(|(name, dependency)| (name.clone(), dependency.exact_program_hash.clone()))
        .collect::<BTreeMap<_, _>>();
    if target_programs != source_programs {
        bail!("target persistent sessions do not reproduce the source exact programs");
    }
    if target_programs != preflight.persistent_dependency_programs
        || target_sessions != preflight.target_persistent_session_capsules
    {
        bail!("final target program/substrate admission differs from preflight");
    }
    ryeos_app::worker_handoff::validate_target_realization_after_preflight(
        &state.state_store.pinned_state_authority()?.cas_store()?,
        &preflight.target_execution_realization_hash,
        &target_capsule.execution_realization_hash,
    )?;
    let target_isolation_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(
            target_metadata
                .isolation
                .as_ref()
                .context("final target placement has no isolation provenance")?,
        )?)?;
    if target_isolation_digest != preflight.target_isolation_digest
        || target_head != preflight.target_project_head_hash
    {
        bail!("target project or isolation changed after preflight");
    }
    let prepared_seed = ryeos_app::worker_handoff::prepare_placement_runtime_seed(
        &req.operation_id,
        &req.chain_root_id,
        &source.manifest.source_placement_thread_id,
        &source.manifest.successor_placement_thread_id,
        &req.target_site_id,
        owner,
        &target_capsule_hash,
        &target_metadata,
    )?;
    let placement = WorkerPlacementAdmissionEvidence::new(
        req.operation_id.clone(),
        req.preflight_id.clone(),
        req.preflight_attestation_hash.clone(),
        req.follow_delivery_reservation_attestation_hash.clone(),
        owner.to_owned(),
        req.chain_root_id.clone(),
        source.manifest.origin_site_id.clone(),
        req.source_site_id.clone(),
        req.target_site_id.clone(),
        source.manifest.source_placement_thread_id.clone(),
        source.manifest.successor_placement_thread_id.clone(),
        req.source_chain_head_hash.clone(),
        source.manifest.source_last_event_hash.clone(),
        source.manifest.checkpoint_manifest_hash.clone(),
        source.launch_capsule.exact_program_hash.clone(),
        source_programs,
        target_sessions,
        target_capsule.execution_realization_hash.clone(),
        credential_reservation.clone(),
        project_rebind,
        accounting,
        target_capsule_hash.clone(),
        prepared_seed.object_hash()?,
    );
    placement.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_app::worker_handoff::validate_cross_site_capsule_transition(
        &source.launch_capsule,
        source_resume,
        &target_capsule,
        &target_resume,
        &resume_rebind,
        &placement,
        &authority.cas_store()?,
    )?;
    drop(guard);
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let attestation = placement.sign_attestation(&signer)?;
    let attestation_hash =
        lillux::sha256_hex(lillux::canonical_json(&attestation.to_value())?.as_bytes());
    let progress = WorkerSessionHandoffProgress {
        schema: "ryeos.worker_session_handoff_progress.v1".to_owned(),
        operation_id: req.operation_id.clone(),
        phase: WorkerHandoffPhase::TargetPrepared,
        placement_attestation_hash: Some(attestation_hash.clone()),
        target_runtime_seed_hash: Some(prepared_seed.object_hash()?),
        writer_grant_hash: None,
        target_chain_head_hash: None,
        credential_reservation_id: Some(credential_reservation.reservation_id.clone()),
        abort_chain_head_hash: None,
    };
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforePreparationPublication,
        _operation,
    )?;
    let admission = state.state_store.publish_worker_placement_preparation(
        job_id,
        &source.manifest.content_hash()?,
        &target_capsule,
        &prepared_seed,
        &attestation,
        &progress,
    )?;
    #[cfg(any(test, feature = "handoff-test-support"))]
    reach_target_handoff_crash_boundary(
        state,
        ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetPreparationPublished,
        _operation,
    )?;
    if admission.attestation_hash != attestation_hash {
        let value = authority
            .cas_store()?
            .get_object(&admission.attestation_hash)?
            .context("reused placement attestation disappeared")?;
        let reused = ryeos_state::objects::Attestation::from_value(&value)?;
        let reused_placement = WorkerPlacementAdmissionEvidence::from_attestation(&reused)?;
        if reused_placement != placement {
            bail!("reused placement admission has contradictory evidence");
        }
    }
    let response = WorkerPlacementPrepareResponse {
        operation_id: req.operation_id.clone(),
        placement_attestation_hash: admission.attestation_hash,
        target_runtime_seed_hash: prepared_seed.object_hash()?,
        target_launch_capsule_hash: target_capsule_hash,
        credential_reservation,
        placement,
    };
    response.validate_against(req)?;
    Ok(PreparedTargetPlacement {
        response,
        #[cfg(any(test, feature = "handoff-test-support"))]
        project_materialization_ms,
    })
}

async fn preflight_after_staging(
    state: &Arc<AppState>,
    req: &WorkerPlacementPreflightRequest,
    source: &SourcePreflightOperands,
) -> Result<WorkerPlacementPreflightResponse> {
    let owner = req.owner_principal.as_str();
    if state
        .threads
        .get_thread(&req.successor_placement_thread_id)?
        .is_some()
        || state
            .state_store
            .dedicated_session(&req.successor_placement_thread_id)?
            .is_some()
        || state
            .state_store
            .credential_profile_reservation_for_successor(&req.successor_placement_thread_id)?
            .is_some()
    {
        bail!("proposed successor placement already exists or is reserved");
    }
    let source_resume = source
        .launch_metadata
        .resume_context
        .as_ref()
        .context("source preflight placement has no ResumeContext")?;
    if source_resume.current_site_id != req.source_site_id
        || source_resume.origin_site_id != req.origin_site_id
        || source_resume.principal_identifier() != owner
        || source.source_snapshot.requested_by.as_deref() != Some(owner)
    {
        bail!("source preflight launch ledger contradicts owner or sites");
    }
    let target_project_path = canonical_target_project_path(&req.target_project_path)?;
    let target_project_ref = ryeos_executor::execution::project_source::canonical_project_ref(
        target_project_path
            .to_str()
            .context("target project path is not UTF-8")?,
    )?;
    let target_project_hash = lillux::sha256_hex(target_project_ref.as_bytes());
    let principal_key = ryeos_state::refs::principal_storage_key(owner)?.to_owned();
    let target_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(&principal_key, &target_project_hash))?
        .context("target project has no principal-scoped HEAD")?;
    let (project_rebind, target_identity, target_overlay_root) =
        ryeos_app::worker_handoff::build_remote_project_rebind(
            &source.source_snapshot.project_authority,
            &target_project_path,
            &req.target_site_id,
            owner,
            &target_head,
            &target_head,
            &target_project_hash,
            &req.project_route_digest,
        )?;

    let credential_contract = credential_contract_from_capsule(
        state,
        &source.launch_capsule,
        &req.credential_subject_contract_digest,
    )?;
    let target_profile = state
        .state_store
        .credential_profile(&req.target_credential_profile_id)?
        .context("target credential profile does not exist")?;
    if target_profile.owner_principal != owner
        || target_profile.state != "active"
        || target_profile.lock_owner.is_some()
    {
        bail!("target credential profile is not active, unlocked, and owner-exact");
    }
    let target_account = target_profile
        .sanitized_account
        .as_ref()
        .context("target credential profile has no confirmed account")?;
    if credential_contract.contract_digest()? != req.credential_subject_contract_digest
        || credential_contract.derive_subject_digest(target_account)?
            != req.credential_subject_digest
    {
        bail!("target credential profile represents another workload account");
    }
    let preview_reservation = CredentialGenerationReservation {
        profile_id: target_profile.profile_id.clone(),
        owner_principal: owner.to_owned(),
        generation: target_profile.credential_generation,
        reservation_id: format!("worker-preflight:{}", req.preflight_id),
        upstream_session_id: req.upstream_session_id.clone(),
        subject_contract_digest: req.credential_subject_contract_digest.clone(),
        subject_digest: req.credential_subject_digest.clone(),
    };
    preview_reservation.validate()?;
    let resume_rebind = RemoteResumeContextRebind {
        source_site_id: req.source_site_id.clone(),
        target_site_id: req.target_site_id.clone(),
        target_project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: target_project_path.clone(),
        },
        target_project_authority: project_rebind.target_authority,
        target_stable_project_identity: Some(target_identity),
        target_local_overlay_root: target_overlay_root,
        target_original_snapshot_hash: Some(target_head.clone()),
        target_original_pushed_head_ref: None,
        target_state_root: None,
        source_credential_profile_id: source_profile_id(source_resume)?,
        credential_reservation: preview_reservation,
    };
    let target_resume = source_resume.for_remote_worker_adoption(&resume_rebind)?;
    let prepared = ryeos_executor::execution::launch::prepare_remote_machine_successor_launch(
        state,
        &req.successor_placement_thread_id,
        &req.source_placement_thread_id,
        &source.launch_metadata,
        &source.launch_capsule,
        source_resume,
        &target_resume,
        &resume_rebind,
        None,
    )
    .await
    .map_err(|error| anyhow::anyhow!(format!("{error:#}")))?;
    let target_metadata = prepared.launch_metadata();
    let target_capsule = target_metadata
        .admitted_launch_capsule()?
        .context("target preflight produced no launch capsule")?;
    let target_sessions = target_capsule.admitted_persistent_session_capsules()?;
    let target_programs = persistent_programs(state, &target_sessions)?;
    let source_sessions = source
        .launch_capsule
        .admitted_persistent_session_capsules()?;
    let source_programs = persistent_programs(state, &source_sessions)?;
    if target_programs != source_programs {
        bail!("target preflight does not reproduce source persistent programs");
    }
    let isolation = target_metadata
        .isolation
        .as_ref()
        .context("target preflight produced no isolation provenance")?;
    let target_isolation_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(isolation)?)?;
    let evidence = WorkerPlacementPreflightEvidence::new(
        req,
        source.launch_capsule.exact_program_hash.clone(),
        source_programs,
        target_sessions,
        target_capsule.execution_realization_hash.clone(),
        target_isolation_digest,
        target_head,
        target_profile.credential_generation,
    )?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let preflight_attestation = evidence.sign_attestation(&signer)?;
    let preflight_attestation_hash =
        ryeos_state::objects::canonical_value_digest(&preflight_attestation.to_value())?;
    let response = WorkerPlacementPreflightResponse {
        preflight_id: req.preflight_id.clone(),
        preflight_attestation_hash,
        preflight_attestation,
        evidence,
    };
    response.validate_against(req, state.identity.verifying_key())?;
    Ok(response)
}

fn validate_transfer_request(
    transfer: &ryeos_state::objects::PlacementTransferManifest,
    req: &WorkerPlacementPrepareRequest,
    owner: &str,
) -> Result<()> {
    if transfer.operation_id != req.operation_id
        || transfer.owner_principal != owner
        || transfer.chain_root_id != req.chain_root_id
        || transfer.source_site_id != req.source_site_id
        || transfer.target_site_id != req.target_site_id
        || transfer.source_chain_head_hash != req.source_chain_head_hash
        || transfer.content_hash()? != req.transfer_manifest_hash
    {
        bail!("request differs from its exact transfer manifest");
    }
    Ok(())
}

fn canonical_target_project_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        bail!("target project endpoint must be absolute");
    }
    let lexical = path.components().collect::<PathBuf>();
    if lexical.to_str() != Some(raw) {
        bail!(
            "target project endpoint must already be canonical: {}",
            lexical.display()
        );
    }
    let pinned = lillux::PinnedDirectory::open(&lexical)
        .with_context(|| format!("securely open target project endpoint {raw:?}"))?
        .with_context(|| format!("target project endpoint does not exist: {raw:?}"))?;
    if pinned.path() != lexical {
        bail!("target project endpoint authority changed while opening");
    }
    Ok(lexical)
}

fn target_job_id(operation_id: &str) -> String {
    format!("worker-handoff-target:{operation_id}")
}

fn target_preflight_job_id(preflight_id: &str) -> String {
    format!("worker-handoff-preflight-target:{preflight_id}")
}

fn load_source_preflight_operands(
    state: &AppState,
    request: &WorkerPlacementPreflightRequest,
) -> Result<SourcePreflightOperands> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let chain_value = cas
        .get_object(&request.source_chain_head_hash)?
        .context("staged preflight source chain head is absent")?;
    let chain: ryeos_state::objects::ChainState = serde_json::from_value(chain_value)?;
    chain.validate()?;
    if chain.chain_root_id != request.chain_root_id {
        bail!("staged preflight chain belongs to another execution");
    }
    let source_entry = chain
        .threads
        .get(&request.source_placement_thread_id)
        .context("preflight chain omits source placement")?;
    if source_entry.last_event_hash.as_deref() != Some(&request.source_last_event_hash) {
        bail!("preflight source event differs from the signed chain head");
    }
    let snapshot_value = cas
        .get_object(&source_entry.snapshot_hash)?
        .context("preflight source snapshot is absent")?;
    let source_snapshot = ryeos_state::objects::ThreadSnapshot::from_current_value(snapshot_value)?;
    if source_snapshot.thread_id != request.source_placement_thread_id
        || source_snapshot.chain_root_id != request.chain_root_id
        || source_snapshot.current_site_id != request.source_site_id
        || source_snapshot.origin_site_id != request.origin_site_id
        || source_snapshot.admitted_launch_capsule_hash.as_deref()
            != Some(request.source_launch_capsule_hash.as_str())
    {
        bail!("preflight source snapshot contradicts its request");
    }
    let metadata_bytes = cas
        .get_blob(&request.source_launch_metadata_blob_hash)?
        .context("preflight source launch metadata is absent")?;
    let metadata_value: Value = serde_json::from_slice(&metadata_bytes)?;
    if lillux::sha256_hex(&metadata_bytes) != request.source_launch_metadata_blob_hash
        || lillux::canonical_json(&metadata_value)?.as_bytes() != metadata_bytes
        || metadata_value != request.source_launch_metadata
    {
        bail!("preflight source launch metadata changed");
    }
    let launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata =
        serde_json::from_value(metadata_value)?;
    launch_metadata.validate()?;
    let launch_capsule = launch_metadata
        .admitted_launch_capsule()?
        .context("preflight source metadata has no launch capsule")?;
    if launch_capsule.content_hash()? != request.source_launch_capsule_hash {
        bail!("preflight source metadata reproduces another launch capsule");
    }
    Ok(SourcePreflightOperands {
        launch_metadata,
        launch_capsule,
        source_snapshot,
    })
}

fn load_source_placement_operands(
    state: &AppState,
    manifest: &ryeos_state::objects::PlacementTransferManifest,
) -> Result<SourcePlacementOperands> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let chain_value = cas
        .get_object(&manifest.source_chain_head_hash)?
        .context("staged source chain head is absent")?;
    let chain: ryeos_state::objects::ChainState = serde_json::from_value(chain_value)?;
    chain.validate()?;
    if chain.chain_root_id != manifest.chain_root_id {
        bail!("staged source chain head belongs to another chain");
    }
    let source_entry = chain
        .threads
        .get(&manifest.source_placement_thread_id)
        .context("source chain omits transfer placement")?;
    if source_entry.last_event_hash.as_deref() != Some(&manifest.source_last_event_hash) {
        bail!("source chain placement event differs from transfer manifest");
    }
    let snapshot_value = cas
        .get_object(&source_entry.snapshot_hash)?
        .context("source placement snapshot is absent")?;
    let source_snapshot = ryeos_state::objects::ThreadSnapshot::from_current_value(snapshot_value)?;
    if source_snapshot.thread_id != manifest.source_placement_thread_id
        || source_snapshot.chain_root_id != manifest.chain_root_id
        || source_snapshot.current_site_id != manifest.source_site_id
        || source_snapshot.origin_site_id != manifest.origin_site_id
        || source_snapshot.requested_by.as_deref() != Some(&manifest.owner_principal)
        || source_snapshot.admitted_launch_capsule_hash.as_deref()
            != Some(&manifest.source_launch_capsule_hash)
    {
        bail!("source placement snapshot contradicts transfer manifest");
    }
    let metadata_bytes = cas
        .get_blob(&manifest.source_launch_metadata_blob_hash)?
        .context("source launch metadata is absent")?;
    if u64::try_from(metadata_bytes.len())? != manifest.source_launch_metadata_size_bytes
        || lillux::sha256_hex(&metadata_bytes) != manifest.source_launch_metadata_blob_hash
    {
        bail!("source launch metadata size or digest changed");
    }
    let metadata_value: Value = serde_json::from_slice(&metadata_bytes)?;
    if lillux::canonical_json(&metadata_value)?.as_bytes() != metadata_bytes {
        bail!("source launch metadata is not canonical JSON");
    }
    let launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata =
        serde_json::from_value(metadata_value)?;
    launch_metadata.validate()?;
    let launch_capsule = launch_metadata
        .admitted_launch_capsule()?
        .context("source metadata has no admitted launch capsule")?;
    if launch_capsule.content_hash()? != manifest.source_launch_capsule_hash {
        bail!("source launch metadata reproduces another capsule");
    }
    let manifest_value = cas
        .get_object(&manifest.checkpoint_manifest_hash)?
        .context("worker checkpoint manifest is absent")?;
    let checkpoint = ryeos_state::objects::StateManifest::from_current_value(manifest_value)?;
    if checkpoint.publisher_chain_root_id != manifest.chain_root_id
        || checkpoint.publisher_thread_id != manifest.source_placement_thread_id
        || checkpoint.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
    {
        bail!("checkpoint was not published by the transfer source");
    }
    let restore_bytes = cas
        .get_blob(&checkpoint.restore.blob_hash)?
        .context("worker checkpoint restore document is absent")?;
    let restore_value: Value = serde_json::from_slice(&restore_bytes)?;
    if lillux::sha256_hex(&restore_bytes) != checkpoint.restore.blob_hash
        || lillux::canonical_json(&restore_value)?.as_bytes() != restore_bytes
    {
        bail!("worker checkpoint restore document changed");
    }
    let restore = ryeos_state::objects::WorkerSessionRestore::from_current_value(restore_value)?;
    if restore.source_position.chain_root_id != manifest.chain_root_id
        || restore.source_position.placement_thread_id != manifest.source_placement_thread_id
        || restore.source_launch_capsule_hash != manifest.source_launch_capsule_hash
        || restore.source_site_id != manifest.source_site_id
        || restore.project_candidate_snapshot_hash.as_deref()
            != Some(manifest.project_candidate_snapshot_hash.as_str())
    {
        bail!("worker restore document contradicts transfer source");
    }
    let tree_entry = checkpoint
        .objects
        .iter()
        .find(|object| object.name == restore.portable_state.attachment_name)
        .context("worker checkpoint has no portable-state attachment")?;
    if tree_entry.media_type != ryeos_state::objects::PORTABLE_STATE_TREE_MEDIA_TYPE
        || tree_entry.blob_hash != restore.portable_state.incoming_tree_hash
    {
        bail!("worker checkpoint portable-state attachment identity changed");
    }
    let tree_bytes = cas
        .get_blob(&tree_entry.blob_hash)?
        .context("worker checkpoint portable-state attachment is absent")?;
    if lillux::sha256_hex(&tree_bytes) != tree_entry.blob_hash
        || u64::try_from(tree_bytes.len())? != tree_entry.size_bytes
    {
        bail!("worker checkpoint portable-state attachment size or digest changed");
    }
    let portable_tree = ryeos_state::objects::PortableStateTree::from_canonical_bytes(
        &tree_bytes,
        &restore.portable_state.selector_contract,
        &restore.upstream_session_id,
    )?;
    launch_metadata
        .resume_context
        .as_ref()
        .and_then(|resume| resume.stable_project_identity.as_ref())
        .map(|identity| identity.display_path.clone())
        .context("source placement has no stable project endpoint")?;
    Ok(SourcePlacementOperands {
        manifest: manifest.clone(),
        launch_metadata,
        launch_capsule,
        source_snapshot,
        restore,
        portable_tree,
    })
}

fn source_project_path_from_payload(
    payload: &ryeos_state::sync::ExportPayload,
    manifest: &ryeos_state::objects::PlacementTransferManifest,
) -> Result<PathBuf> {
    let entry = payload
        .entries
        .iter()
        .find(|entry| entry.is_blob && entry.hash == manifest.source_launch_metadata_blob_hash)
        .context("transfer payload omits source launch metadata")?;
    if u64::try_from(entry.data.len())? != manifest.source_launch_metadata_size_bytes
        || lillux::sha256_hex(&entry.data) != manifest.source_launch_metadata_blob_hash
    {
        bail!("transfer launch metadata size or digest changed");
    }
    let value: Value = serde_json::from_slice(&entry.data)?;
    if lillux::canonical_json(&value)?.as_bytes() != entry.data {
        bail!("transfer launch metadata is not canonical JSON");
    }
    let metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata =
        serde_json::from_value(value)?;
    metadata.validate()?;
    metadata
        .resume_context
        .as_ref()
        .and_then(|resume| resume.stable_project_identity.as_ref())
        .map(|identity| identity.display_path.clone())
        .context("source placement has no stable project endpoint")
}

fn source_credential_contract(
    state: &AppState,
    source: &SourcePlacementOperands,
) -> Result<ryeos_state::objects::CredentialSubjectProjectionContract> {
    credential_contract_from_capsule(
        state,
        &source.launch_capsule,
        &source.restore.credential_subject_contract_digest,
    )
}

fn credential_contract_from_capsule(
    state: &AppState,
    launch_capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    expected_contract_digest: &str,
) -> Result<ryeos_state::objects::CredentialSubjectProjectionContract> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let mut matches = Vec::new();
    for capsule_hash in launch_capsule
        .admitted_persistent_session_capsules()?
        .values()
    {
        let value = cas
            .get_object(capsule_hash)?
            .context("source persistent-session capsule is absent")?;
        let capsule =
            ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
        if let Some(profile) = capsule.structured_session_profile.as_ref()
            && let Some(contract) = profile.credential_subject_contract()?
            && contract.contract_digest()? == expected_contract_digest
        {
            matches.push(contract);
        }
    }
    drop(guard);
    if matches.len() != 1 {
        bail!("source launch does not identify one exact credential subject contract");
    }
    Ok(matches.remove(0))
}

fn persistent_programs(
    state: &AppState,
    sessions: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let programs = sessions
        .iter()
        .map(|(name, hash)| {
            let value = cas
                .get_object(hash)?
                .with_context(|| format!("target persistent session {name:?} is absent"))?;
            let capsule =
                ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
            Ok((name.clone(), capsule.exact_program_hash))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    drop(guard);
    Ok(programs)
}

fn source_profile_id(resume: &ryeos_app::launch_metadata::ResumeContext) -> Result<String> {
    resume
        .parameters
        .get("credential_profile_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("source worker parameters have no credential profile")
}

struct TargetAdoptionAttempt {
    state: Arc<AppState>,
    job_id: String,
    #[cfg(any(test, feature = "handoff-test-support"))]
    operation: WorkerSessionHandoffJobOperation,
    attempt_id: String,
    active: bool,
}

struct TargetPreflightAttempt {
    state: Arc<AppState>,
    job_id: String,
    attempt_id: String,
    active: bool,
}

impl TargetPreflightAttempt {
    fn begin(state: Arc<AppState>, job_id: &str) -> Result<Self> {
        let attempt_id = begin_target_handoff_attempt(
            &state,
            job_id,
            "target_preflight",
            "target-handoff-preflight",
        )?;
        Ok(Self {
            state,
            job_id: job_id.to_owned(),
            attempt_id,
            active: true,
        })
    }

    fn complete(
        &mut self,
        attestation: &ryeos_state::objects::Attestation,
        result: Value,
    ) -> Result<String> {
        let stored = self.state.state_store.complete_worker_handoff_preflight(
            &self.job_id,
            &self.attempt_id,
            attestation,
            result,
        )?;
        self.active = false;
        Ok(stored)
    }
}

impl Drop for TargetPreflightAttempt {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let error = "target preflight attempt returned before settlement".to_owned();
        if let Err(settle_error) = settle_target_handoff_attempt(
            &self.state,
            &self.job_id,
            &self.attempt_id,
            ryeos_state::SyncJobAttemptState::Failed,
            ryeos_state::SyncJobState::Retryable,
            "target_preflight_failed",
            Some(error),
            None,
        ) {
            tracing::error!(
                job_id = %self.job_id,
                attempt_id = %self.attempt_id,
                error = %settle_error,
                "failed to settle target preflight attempt on handler exit"
            );
        }
    }
}

impl TargetAdoptionAttempt {
    fn begin(
        state: Arc<AppState>,
        job_id: &str,
        _operation: &WorkerSessionHandoffJobOperation,
    ) -> Result<Self> {
        let attempt_id =
            begin_target_handoff_attempt(&state, job_id, "target_adopt", "target-handoff-adopt")?;
        Ok(Self {
            state,
            job_id: job_id.to_owned(),
            #[cfg(any(test, feature = "handoff-test-support"))]
            operation: _operation.clone(),
            attempt_id,
            active: true,
        })
    }

    fn complete(&mut self, result: Value) -> Result<()> {
        let response: WorkerPlacementAdoptResponse = serde_json::from_value(result.clone())?;
        #[cfg(any(test, feature = "handoff-test-support"))]
        reach_target_handoff_crash_boundary(
            &self.state,
            ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetBeforeCompletion,
            &self.operation,
        )?;
        self.state.state_store.with_state_db(|db| {
            let latest = db
                .get_sync_job(&self.job_id)?
                .context("target worker handoff job disappeared")?;
            db.finish_sync_job_attempt_and_update_job(
                &self.attempt_id,
                &ryeos_state::FinishSyncJobAttempt {
                    state: ryeos_state::SyncJobAttemptState::Completed,
                    phase: WorkerHandoffPhase::Completed.as_str().to_owned(),
                    error: None,
                    result: Some(result.clone()),
                },
                &self.job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Completed,
                    phase: WorkerHandoffPhase::Completed.as_str().to_owned(),
                    roots: None,
                    heads: Some(vec![response.target_chain_head_hash.clone()]),
                    uploaded_hashes: latest.uploaded_hashes,
                    fetched_hashes: vec![response.target_chain_head_hash],
                    last_error: None,
                    result: Some(result),
                },
            )
        })?;
        #[cfg(any(test, feature = "handoff-test-support"))]
        reach_target_handoff_crash_boundary(
            &self.state,
            ryeos_app::worker_handoff::test_support::HandoffCrashBoundary::TargetCompletedBeforeResponse,
            &self.operation,
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TargetAdoptionAttempt {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let error = "target adoption attempt returned before settlement".to_owned();
        if let Err(settle_error) = settle_target_handoff_attempt(
            &self.state,
            &self.job_id,
            &self.attempt_id,
            ryeos_state::SyncJobAttemptState::Failed,
            ryeos_state::SyncJobState::Retryable,
            "target_adopt_failed",
            Some(error),
            None,
        ) {
            tracing::error!(
                job_id = %self.job_id,
                attempt_id = %self.attempt_id,
                error = %settle_error,
                "failed to settle target adoption attempt on handler exit"
            );
        }
    }
}

fn begin_target_handoff_attempt(
    state: &AppState,
    job_id: &str,
    phase: &str,
    worker_id: &str,
) -> Result<String> {
    let attempt_id = format!("worker-handoff-attempt:{}", uuid::Uuid::new_v4());
    state.state_store.with_state_db(|db| {
        db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.to_owned(),
            worker_id: Some(worker_id.to_owned()),
            phase: phase.to_owned(),
        })?;
        Ok(())
    })?;
    Ok(attempt_id)
}

#[allow(clippy::too_many_arguments)]
fn settle_target_handoff_attempt(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    error: Option<String>,
    result: Option<Value>,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        let latest = db
            .get_sync_job(job_id)?
            .context("target worker handoff job disappeared")?;
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
                heads: None,
                uploaded_hashes: latest.uploaded_hashes,
                fetched_hashes: latest.fetched_hashes,
                last_error: error,
                result: result.or(latest.result),
            },
        )
    })
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(error.to_string())
}

pub const PREPARE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: WORKER_PLACEMENT_PREPARE_SERVICE,
    endpoint: "worker-placements.prepare",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-placements/prepare"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: WorkerPlacementPrepareRequest = crate::handler_error::parse_request(params)?;
            prepare(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const PREFLIGHT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: WORKER_PLACEMENT_PREFLIGHT_SERVICE,
    endpoint: "worker-placements.preflight",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-placements/preflight"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: WorkerPlacementPreflightRequest = crate::handler_error::parse_request(params)?;
            preflight(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const ADOPT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: WORKER_PLACEMENT_ADOPT_SERVICE,
    endpoint: "worker-placements.adopt",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-placements/adopt"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: WorkerPlacementAdoptRequest = crate::handler_error::parse_request(params)?;
            adopt(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const ABORT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: WORKER_PLACEMENT_ABORT_SERVICE,
    endpoint: "worker-placements.abort",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.worker-placements/abort"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: WorkerPlacementAbortRequest = crate::handler_error::parse_request(params)?;
            abort(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod authority_tests {
    use super::*;

    fn context(
        class: ryeos_app::identity::AuthorizedKeyPrincipalClass,
        fingerprint: &str,
        site_id: Option<&str>,
    ) -> HandlerContext {
        HandlerContext::new_with_authority(
            fingerprint.to_owned(),
            Vec::new(),
            true,
            Some(class),
            site_id.map(str::to_owned),
        )
    }

    #[test]
    fn placement_transport_requires_the_exact_configured_remote_node() {
        let admitted = context(
            ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode,
            "fp:source-node",
            Some("site:source"),
        );
        require_authenticated_source_node(&admitted, "site:source", "fp:source-node").unwrap();

        for rejected in [
            context(
                ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteOperator,
                "fp:operator",
                Some("site:source"),
            ),
            context(
                ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode,
                "fp:other-node",
                Some("site:source"),
            ),
            context(
                ryeos_app::identity::AuthorizedKeyPrincipalClass::RemoteNode,
                "fp:source-node",
                Some("site:other"),
            ),
        ] {
            assert!(
                require_authenticated_source_node(&rejected, "site:source", "fp:source-node")
                    .is_err()
            );
        }
    }

    #[test]
    fn target_adoption_never_relaunches_behind_an_active_launch_claim() {
        assert_eq!(
            classify_target_launch_claim(false, false).unwrap(),
            TargetLaunchClaimDisposition::Launch
        );
        assert_eq!(
            classify_target_launch_claim(true, true).unwrap(),
            TargetLaunchClaimDisposition::AwaitExisting
        );
        assert!(classify_target_launch_claim(true, false).is_err());
        assert!(classify_target_launch_claim(false, true).is_err());
    }

    #[test]
    fn target_project_endpoint_is_existing_canonical_nofollow_authority() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let canonical = project.to_str().unwrap();

        assert_eq!(canonical_target_project_path(canonical).unwrap(), project);
        assert!(
            canonical_target_project_path(&format!("{}/./project", root.path().display())).is_err()
        );

        #[cfg(unix)]
        {
            let alias = root.path().join("project-alias");
            std::os::unix::fs::symlink(&project, &alias).unwrap();
            assert!(canonical_target_project_path(alias.to_str().unwrap()).is_err());
        }
    }

    #[test]
    fn abort_authority_cannot_transition_back_to_target_adoption() {
        assert!(target_adoption_source_commit_phase(WorkerHandoffPhase::AbortAuthorized).is_err());
        assert_eq!(
            target_adoption_source_commit_phase(WorkerHandoffPhase::TargetPrepared).unwrap(),
            WorkerHandoffPhase::SourceCommitted
        );
        for retained in [
            WorkerHandoffPhase::SourceCommitted,
            WorkerHandoffPhase::TargetAdopted,
            WorkerHandoffPhase::StateInstalled,
            WorkerHandoffPhase::ProcessAttached,
        ] {
            assert_eq!(
                target_adoption_source_commit_phase(retained).unwrap(),
                retained
            );
        }
        for invalid in [
            WorkerHandoffPhase::Planned,
            WorkerHandoffPhase::SourceExported,
            WorkerHandoffPhase::Completed,
        ] {
            assert!(target_adoption_source_commit_phase(invalid).is_err());
        }
    }

    #[test]
    fn unproved_worker_start_cannot_become_target_attachment_testimony() {
        let capsule_hash = "a".repeat(64);
        let session = ryeos_app::runtime_db::DedicatedSessionRecord {
            placement_thread_id: "T-successor".to_owned(),
            chain_root_id: "T-root".to_owned(),
            owner_principal: "fp:owner".to_owned(),
            admitted_capsule_hash: capsule_hash.clone(),
            worker_instance_id: Some("worker-unproved".to_owned()),
            worker_boot_epoch: Some(1),
            workspace_id: "workspace:successor".to_owned(),
            candidate_required: true,
            credential_profile_id: "credential:target".to_owned(),
            credential_generation: 7,
            remote_thread_id: Some("upstream-session".to_owned()),
            current_turn_id: None,
            state: "outcome_unknown".to_owned(),
            send_boundary: "outcome_unknown".to_owned(),
            candidate_snapshot_hash: None,
            candidate_validation_hash: None,
            publication_result: None,
            terminal_reason: Some("held child cleanup unproved".to_owned()),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let worker = ryeos_app::runtime_db::WorkerProcessRecord {
            worker_instance_id: "worker-unproved".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: capsule_hash.clone(),
            boot_epoch: 1,
            lifecycle_generation: 7,
            process_identity: ryeos_app::process::ExecutionProcessIdentity {
                schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                boot_id: "boot:test".to_owned(),
                target_pid: 41,
                target_start_time_ticks: 101,
                group_leader_pid: 41,
                group_leader_start_time_ticks: 101,
            },
            control_channel_identity: "channel:unproved".to_owned(),
            state: ryeos_app::runtime_db::WorkerProcessState::Dead,
            daemon_generation_id: "daemon:test".to_owned(),
            placement_thread_id: "T-successor".to_owned(),
            cleanup_state: "unproved".to_owned(),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let profile = ryeos_app::runtime_db::CredentialProfileRecord {
            profile_id: "credential:target".to_owned(),
            owner_principal: "fp:owner".to_owned(),
            home_id: "home:target".to_owned(),
            authority_revision: 1,
            credential_generation: 7,
            state: "active".to_owned(),
            active_login_id: None,
            login_epoch: 1,
            login_expires_at_ms: None,
            sanitized_account: None,
            lock_owner: Some("worker-unproved".to_owned()),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let expected_session_capsules =
            BTreeMap::from([("worker".to_owned(), capsule_hash.clone())]);

        for attachment_was_projected in [false, true] {
            assert!(
                validate_target_worker_attachment(
                    &session,
                    &worker,
                    &profile,
                    "T-successor",
                    "fp:owner",
                    &expected_session_capsules,
                    7,
                    attachment_was_projected,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn target_attachment_uses_the_signed_inner_session_capsule_set() {
        let outer_launch_capsule_hash = "a".repeat(64);
        let inner_session_capsule_hash = "b".repeat(64);
        let admitted = BTreeMap::from([("worker".to_owned(), inner_session_capsule_hash.clone())]);

        assert!(placement_admits_session_capsule(
            &admitted,
            &inner_session_capsule_hash,
        ));
        assert!(!placement_admits_session_capsule(
            &admitted,
            &outer_launch_capsule_hash,
        ));
    }
}
