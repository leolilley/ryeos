//! Meaning-blind owner-authorized access to one attached exclusive session.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Result;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::persistent_session::ExclusiveRetirementOutcome;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

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

fn has_authoritative_candidate_publication_reservation(
    state: &AppState,
    session: &ryeos_app::state_store::DedicatedSessionRecord,
    operation_id: &str,
    candidate: &str,
    expected_previous_hash: &str,
    validation: &str,
    principal_key: &str,
    project_hash: &str,
) -> Result<bool, HandlerError> {
    let fact = ryeos_app::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_candidate.publication_reserved",
        operation_id,
    )
    .map_err(internal)?;
    if fact.count > 1 {
        return Err(internal(
            "candidate publication reservation is duplicated in the root chain",
        ));
    }
    let Some(payload) = fact.payload else {
        return if fact.count == 0 {
            Ok(false)
        } else {
            Err(internal(
                "candidate publication testimony exceeds the bounded replay cache payload",
            ))
        };
    };
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id)
        && payload.get("origin").and_then(Value::as_str) == Some("owner_authorized")
        && payload.get("owner_principal").and_then(Value::as_str)
            == Some(session.owner_principal.as_str())
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload
            .get("candidate_snapshot_hash")
            .and_then(Value::as_str)
            == Some(candidate)
        && payload
            .get("expected_previous_hash")
            .and_then(Value::as_str)
            == Some(expected_previous_hash)
        && payload
            .get("candidate_validation_hash")
            .and_then(Value::as_str)
            == Some(validation)
        && payload.get("principal_key").and_then(Value::as_str) == Some(principal_key)
        && payload.get("project_hash").and_then(Value::as_str) == Some(project_hash);
    if !exact {
        return Err(internal(
            "candidate publication reservation operation is contradictory",
        ));
    }
    Ok(true)
}

fn owned_session(
    state: &AppState,
    ctx: &HandlerContext,
    chain_root_id: &str,
) -> Result<ryeos_app::state_store::DedicatedSessionRecord, HandlerError> {
    // Initial hosted execution is deliberately a single configured-operator
    // trust domain. Enforce that predicate before lookup so discovery and
    // timing do not turn owner rows into an accidental multi-tenant boundary.
    ryeos_app::operator_external_content::require_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
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
    serde_json::to_value(owned_session(&state, &ctx, &req.chain_root_id)?).map_err(internal)
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
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &session.placement_thread_id,
    )
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
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &source_thread_id,
    )
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
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &source_thread_id,
    )
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
    let source_capsule = source_metadata
        .admitted_launch_capsule()
        .map_err(internal)?
        .ok_or_else(|| internal("preflight source metadata has no launch capsule"))?;
    if source_capsule.content_hash().map_err(internal)? != source_launch_capsule_hash {
        return Err(internal("preflight source launch capsule changed"));
    }
    let source_resume = source_metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| internal("preflight source has no ResumeContext"))?;
    let source_project_path = source_resume
        .stable_project_identity
        .as_ref()
        .map(|identity| identity.display_path.clone())
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
    let source_launch_metadata = serde_json::to_value(&source_metadata).map_err(internal)?;
    let source_launch_metadata_blob_hash = lillux::sha256_hex(
        lillux::canonical_json(&source_launch_metadata)
            .map_err(internal)?
            .as_bytes(),
    );
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
        "schema":"ryeos.worker_session_handoff_preflight_operation.v1",
        "owner_principal":source.owner_principal,
        "chain_root_id":source.chain_root_id,
        "origin_site_id":source_snapshot.origin_site_id,
        "source_site_id":state.threads.site_id(),
        "target_site_id":target_site_id,
        "source_placement_thread_id":source_thread_id,
        "source_chain_head_hash":source_head.target_hash,
        "source_last_event_hash":source_event_hash,
        "source_launch_capsule_hash":source_launch_capsule_hash,
        "source_launch_metadata_blob_hash":source_launch_metadata_blob_hash,
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
        source_launch_metadata,
        source_launch_metadata_blob_hash,
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
        .execute(
            ryeos_app::worker_handoff::WORKER_PLACEMENT_PREFLIGHT_SERVICE,
            &BTreeMap::new(),
            None,
            &serde_json::to_value(&request).map_err(internal)?,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            None,
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
    let target_closure = target_node_client
        .objects_closure_get(
            &[response.preflight_attestation_hash.clone()],
            crate::remote::client::ObjectsClosureRequestOptions {
                max_objects: Some(16_384),
                max_blobs: Some(16_384),
                max_object_bytes: Some(2 * 1024 * 1024),
                max_total_object_bytes: Some(16 * 1024 * 1024),
                max_blob_bytes: Some(48 * 1024 * 1024),
                max_total_blob_bytes: Some(48 * 1024 * 1024),
                max_response_bytes: Some(64 * 1024 * 1024),
                max_links_per_object: Some(65_536),
                allow_incomplete: false,
                allow_untransported_large_objects: true,
            },
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
        if let Some(existing) = payload.entries.iter().find(|entry| &entry.hash == hash) {
            if existing.is_blob || existing.data != bytes {
                return Err(internal(
                    "target preflight closure contradicts the local follow reservation",
                ));
            }
        } else {
            payload.total_bytes = payload
                .total_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| internal("preflight payload byte count overflow"))?;
            payload.entries.push(ryeos_state::sync::SyncEntry {
                hash: hash.clone(),
                is_blob: false,
                data: bytes,
            });
        }
    }
    let operation = ryeos_app::worker_handoff::WorkerPlacementPreflightJobOperation::from_request(
        ryeos_app::worker_handoff::WorkerHandoffJobRole::Source,
        req.remote.clone(),
        &request,
    )
    .map_err(internal)?;
    let job_id = source_preflight_job_id(&preflight_id);
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
                operation_type: ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION
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
    state
        .state_store
        .with_state_db(|db| {
            db.update_sync_job(
                &job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Completed,
                    phase: "preflight_complete".to_owned(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: Vec::new(),
                    fetched_hashes: vec![response.preflight_attestation_hash.clone()],
                    last_error: None,
                    result: Some(serde_json::to_value(&response)?),
                },
            )
        })
        .map_err(internal)?;
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
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &source_thread_id,
    )
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
    if worker.placement_thread_id != source_thread_id
        || worker.state != ryeos_app::runtime_db::WorkerProcessState::Dead
        || worker.cleanup_state != "reaped"
    {
        return Err(HandlerError::BadRequest(
            "handoff requires proof that the exact source worker was reaped".into(),
        ));
    }
    let checkpoint = load_worker_checkpoint(&state, &source.chain_root_id, &req.manifest_ref)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
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
    let source_resume = source_metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| internal("handoff source has no ResumeContext"))?;
    let source_project_path = source_resume
        .stable_project_identity
        .as_ref()
        .map(|identity| identity.display_path.clone())
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
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store().map_err(internal)?,
        &source.chain_root_id,
        &source_head.target_hash,
        &preflight_evidence.source_chain_head_hash,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    drop(guard);
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
    let source_accounting_frontier = match (
        state.accounting.as_ref(),
        source_metadata.accounting_scope.as_ref(),
    ) {
        (Some(accounting), Some(scope)) => Some(
            accounting
                .handoff_frontier(&source_thread_id, scope)
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
        &launch_capsule_hash,
        &source_metadata,
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
    state
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
                max_attempts: 16,
            },
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
    let prepared_result: Result<
        ryeos_app::worker_handoff::WorkerPlacementPrepareResponse,
        HandlerError,
    > = async {
        let target_value = target_client
            .execute(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_PREPARE_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&prepare_request).map_err(internal)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                None,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(error, "target placement preparation")
            })?;
        let prepared: ryeos_app::worker_handoff::WorkerPlacementPrepareResponse =
            serde_json::from_value(target_value)
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
            .objects_closure_get(
                &target_roots,
                crate::remote::client::ObjectsClosureRequestOptions {
                    max_objects: Some(4096),
                    max_blobs: Some(4096),
                    max_object_bytes: Some(2 * 1024 * 1024),
                    max_total_object_bytes: Some(16 * 1024 * 1024),
                    max_blob_bytes: Some(48 * 1024 * 1024),
                    max_total_blob_bytes: Some(48 * 1024 * 1024),
                    max_response_bytes: Some(64 * 1024 * 1024),
                    max_links_per_object: Some(65_536),
                    allow_incomplete: false,
                    allow_untransported_large_objects: true,
                },
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
        verify_target_placement_attestation(&state, &loaded_remote.config, &prepared)
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
        Ok(prepared)
    }
    .await;
    let prepared = match prepared_result {
        Ok(prepared) => {
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
            prepared
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

    let target_authority = &prepared.placement.project_rebind.target_authority;
    let target_overlay_root = matches!(
        target_authority.environment(),
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { .. }
    )
    .then(|| PathBuf::from(&binding.remote_project_path));
    let target_identity = ryeos_app::launch_metadata::StableProjectIdentity::from_path(
        PathBuf::from(&binding.remote_project_path).as_path(),
        &target_site_id,
    )
    .map_err(internal)?;
    let resume_rebind = ryeos_app::worker_handoff::RemoteResumeContextRebind {
        source_site_id: state.threads.site_id().to_owned(),
        target_site_id: target_site_id.clone(),
        target_project_context: ryeos_engine::contracts::ProjectContext::SnapshotHash {
            hash: prepared
                .placement
                .project_rebind
                .source_candidate_snapshot_hash
                .clone(),
        },
        target_project_authority: target_authority.clone(),
        target_stable_project_identity: Some(target_identity),
        target_local_overlay_root: target_overlay_root,
        target_original_snapshot_hash: Some(
            prepared
                .placement
                .project_rebind
                .source_candidate_snapshot_hash
                .clone(),
        ),
        target_original_pushed_head_ref: None,
        target_state_root: None,
        source_credential_profile_id: source.credential_profile_id.clone(),
        credential_reservation: prepared.credential_reservation.clone(),
    };
    let target_resume = source_resume
        .for_remote_worker_adoption(&resume_rebind)
        .map_err(internal)?;
    let final_frontier = match (
        state.accounting.as_ref(),
        source_metadata.accounting_scope.as_ref(),
    ) {
        (Some(accounting), Some(scope)) => Some(
            accounting
                .handoff_frontier(&source_thread_id, scope)
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        ),
        (_, None) => None,
        (None, Some(_)) => return Err(internal("source accounting ledger disappeared")),
    };
    if final_frontier != source_accounting_frontier {
        return Err(HandlerError::BadRequest(
            "source accounting frontier changed during target preparation".into(),
        ));
    }
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
        project_root: None,
        project_authority: target_authority.clone(),
        base_project_snapshot_hash: Some(
            prepared
                .placement
                .project_rebind
                .source_base_snapshot_hash
                .clone(),
        ),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    let publication = state
        .state_store
        .create_remote_adoption_successor(
            &successor,
            &source_thread_id,
            &source.chain_root_id,
            &ryeos_app::state_store::RemoteAdoptionContinuationAuthority {
                placement_attestation_hash: prepared.placement_attestation_hash.clone(),
                placement: prepared.placement.clone(),
                resume_rebind,
                target_resume_context: target_resume,
                source_accounting_frontier: final_frontier,
                target_node_verifying_key: loaded_remote
                    .config
                    .pinned_signing_key()
                    .map_err(internal)?,
            },
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
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
    let adopt_request = ryeos_app::worker_handoff::WorkerPlacementAdoptRequest {
        operation_id: operation_id.clone(),
        chain_root_id: source.chain_root_id.clone(),
        target_chain_head_hash: target_chain_head.target_hash.clone(),
        placement_attestation_hash: prepared.placement_attestation_hash.clone(),
        writer_grant_hash: publication.writer_grant_hash,
    };
    let adopt_attempt =
        begin_worker_handoff_attempt(&state, &job_id, "target_adopt", "source-handoff")?;
    let adopted_result: Result<
        ryeos_app::worker_handoff::WorkerPlacementAdoptResponse,
        HandlerError,
    > = async {
        let adopted_value = target_client
            .execute(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&adopt_request).map_err(internal)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                None,
            )
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(error, "target placement adoption")
            })?;
        let adopted: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
            serde_json::from_value(adopted_value)
                .map_err(|error| internal(format!("decode target adoption response: {error}")))?;
        if adopted.operation_id != operation_id
            || adopted.chain_root_id != source.chain_root_id
            || adopted.placement_thread_id != successor_thread_id
            || adopted.target_chain_head_hash != target_chain_head.target_hash
        {
            return Err(internal(
                "target adoption response changed its authority coordinates",
            ));
        }
        Ok(adopted)
    }
    .await;
    let adopted = match adopted_result {
        Ok(adopted) => {
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
            adopted
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
    Ok(json!({
        "operation_id":operation_id,
        "chain_root_id":source.chain_root_id,
        "source_placement_thread_id":source_thread_id,
        "placement_thread_id":successor_thread_id,
        "current_site_id":target_site_id,
        "target_chain_head_hash":target_chain_head.target_hash,
        "delivery":adopted.delivery,
    }))
}

async fn resume_committed_handoff(
    req: &HandoffRequest,
    ctx: &HandlerContext,
    state: &Arc<AppState>,
) -> Result<Option<Value>, HandlerError> {
    ryeos_app::operator_external_content::require_configured_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("configured operator required".into()))?;
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
    if job.state == ryeos_state::SyncJobState::Completed {
        let adopted: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
            serde_json::from_value(
                job.result
                    .ok_or_else(|| internal("completed handoff job has no result"))?,
            )
            .map_err(internal)?;
        return Ok(Some(handoff_response(&operation, &adopted)?));
    }
    let progress = job
        .result
        .map(ryeos_app::worker_handoff::WorkerSessionHandoffProgress::from_value)
        .transpose()
        .map_err(internal)?
        .ok_or_else(|| internal("committed handoff job has no durable progress"))?;
    if progress.phase < ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted
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
    let value = target_client
        .execute(
            ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
            &BTreeMap::new(),
            None,
            &serde_json::to_value(&adopt_request).map_err(internal)?,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            None,
        )
        .await
        .map_err(|error| {
            crate::remote::client::map_remote_call_error(error, "retry target placement adoption")
        })?;
    let adopted: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
        serde_json::from_value(value).map_err(internal)?;
    if adopted.operation_id != operation.operation_id
        || adopted.chain_root_id != operation.chain_root_id
        || adopted.placement_thread_id != operation.successor_placement_thread_id
        || adopted.target_chain_head_hash != adopt_request.target_chain_head_hash
    {
        return Err(internal(
            "retried target adoption changed its authority coordinates",
        ));
    }
    state
        .state_store
        .with_state_db(|db| {
            db.update_sync_job(
                &job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Completed,
                    phase: "completed".to_owned(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![adopt_request.target_chain_head_hash.clone()],
                    fetched_hashes: Vec::new(),
                    last_error: None,
                    result: Some(serde_json::to_value(&adopted)?),
                },
            )
        })
        .map_err(internal)?;
    Ok(Some(handoff_response(&operation, &adopted)?))
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

/// Recover both safe terminal branches of a durable source handoff. A pre-cut
/// operation first publishes an authoritative source abort successor and only
/// then asks the target to release its reservation. A post-cut operation
/// re-verifies the one-shot writer grant before redriving target adoption.
pub async fn recover_durable_source_handoffs(state: &AppState) -> Result<usize> {
    let jobs = state.state_store.with_state_db(|db| {
        db.list_active_sync_jobs_by_operation_type(
            ryeos_app::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION,
            64,
        )
    })?;
    let mut recovered = 0usize;
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
        let _operation_guard = disposition_operation_lock(&operation.source_placement_thread_id)
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
        if progress.phase < ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted
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
                    anyhow::anyhow!("authoritative successor has no remote-continuation authority")
                })?;
            let head = state
                .state_store
                .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
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
        if progress.phase < ryeos_app::worker_handoff::WorkerHandoffPhase::SourceCommitted {
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
        let loaded_remote =
            crate::remote::config::get_loaded_remote(&report.remotes, &operation.peer_remote_name)?;
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
        let result = client
            .execute(
                ryeos_app::worker_handoff::WORKER_PLACEMENT_ADOPT_SERVICE,
                &BTreeMap::new(),
                None,
                &serde_json::to_value(&request)?,
                &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
                None,
            )
            .await;
        match result {
            Ok(value) => {
                let adopted: ryeos_app::worker_handoff::WorkerPlacementAdoptResponse =
                    serde_json::from_value(value)?;
                if adopted.operation_id != operation.operation_id
                    || adopted.chain_root_id != operation.chain_root_id
                    || adopted.placement_thread_id != operation.successor_placement_thread_id
                    || adopted.target_chain_head_hash != request.target_chain_head_hash
                {
                    anyhow::bail!("recovered target adoption changed its authority coordinates");
                }
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
    Ok(recovered)
}

async fn recover_pre_cut_source_handoff_abort(
    state: &AppState,
    job: &ryeos_state::SyncJobRecord,
    operation: &ryeos_app::worker_handoff::WorkerSessionHandoffJobOperation,
    progress: &ryeos_app::worker_handoff::WorkerSessionHandoffProgress,
) -> Result<bool> {
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
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &operation.source_placement_thread_id,
    )?;
    let current_head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &operation.chain_root_id))?
        .ok_or_else(|| anyhow::anyhow!("pre-cut handoff source head disappeared"))?;
    if current_head.signer != state.identity.fingerprint() {
        anyhow::bail!("pre-cut handoff source head is not locally owned");
    }
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
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
        abort_chain_head_hash: abort_head_hash.clone(),
    };
    let attempt_id = begin_worker_handoff_attempt(
        state,
        &job.job_id,
        "target_abort",
        "source-handoff-recovery",
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    match client
        .execute(
            ryeos_app::worker_handoff::WORKER_PLACEMENT_ABORT_SERVICE,
            &BTreeMap::new(),
            None,
            &serde_json::to_value(&request)?,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
            None,
        )
        .await
    {
        Ok(value) => {
            let response: ryeos_app::worker_handoff::WorkerPlacementAbortResponse =
                serde_json::from_value(value)?;
            response.validate_against(&request)?;
            settle_worker_handoff_attempt(
                state,
                &job.job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Cancelled,
                ryeos_state::SyncJobState::Cancelled,
                "aborted",
                None,
                Some(serde_json::to_value(response)?),
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
                    heads: None,
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
        .collect::<Vec<_>>();
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
            ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
                &state.state_store,
                &session.placement_thread_id,
            )?;
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
/// project HEAD is read back under retained project authority before the root
/// fact/projection is settled. After an authorized possible contact, only the
/// admitted base proves that this operation did not publish. The candidate
/// proves success; a missing or different HEAD is irreducibly ambiguous and is
/// terminalized as unknown rather than reopening an unsafe retry.
pub async fn reconcile_candidate_publications(state: Arc<AppState>) -> anyhow::Result<()> {
    for session in state
        .state_store
        .dedicated_sessions_in_state("publishing")?
    {
        let _operation_guard = disposition_operation_lock(&session.placement_thread_id)
            .lock_owned()
            .await;
        let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
            &state.state_store,
            &session.placement_thread_id,
        )?;
        let candidate = session
            .candidate_snapshot_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no candidate"))?;
        let validation = session
            .candidate_validation_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("publishing session has no validation identity"))?;
        let thread = state
            .state_store
            .get_thread(&session.placement_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("publishing root thread disappeared"))?;
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            realization,
            ..
        } = thread
            .project_authority
            .ok_or_else(|| anyhow::anyhow!("publishing root has no project authority"))?
        else {
            anyhow::bail!("publishing root project authority is not pinned");
        };
        let ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication:
                ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                    principal_key,
                    project_hash,
                    expected_hash,
                },
        } = realization
        else {
            anyhow::bail!("publishing root lacks an admitted explicit project HEAD destination");
        };
        if expected_hash != base_snapshot_hash {
            anyhow::bail!("publishing root HEAD fence differs from its admitted base");
        }
        let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
            "schema":"ryeos.hosted_candidate_publication_operation.v1",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":validation,
            "principal_key":principal_key,
            "project_hash":project_hash,
        }))?;
        let authorized = has_authoritative_candidate_publication_reservation(
            &state,
            &session,
            &operation_id,
            candidate,
            &base_snapshot_hash,
            validation,
            &principal_key,
            &project_hash,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let current = state
            .state_store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))?;
        if !authorized {
            if current.as_deref() == Some(base_snapshot_hash.as_str()) {
                state.state_store.fail_dedicated_candidate_disposition(
                    &session.placement_thread_id,
                    "publishing",
                )?;
                continue;
            }
            anyhow::bail!(
                "publishing session {} contacted project HEAD without authoritative owner reservation",
                session.placement_thread_id
            );
        }
        match classify_candidate_publication_recovery(
            current.as_deref(),
            &base_snapshot_hash,
            candidate,
        ) {
            CandidatePublicationRecovery::Published => {
                append_root_fact_once(
                    &state,
                    &session,
                    "hosted_candidate.published",
                    &operation_id,
                    json!({
                        "schema":1,
                        "origin":"filesystem_verified",
                        "owner_principal":session.owner_principal,
                        "chain_root_id":session.chain_root_id,
                        "placement_thread_id":session.placement_thread_id,
                        "candidate_snapshot_hash":candidate,
                        "expected_previous_hash":base_snapshot_hash,
                        "candidate_validation_hash":validation,
                        "principal_key":principal_key,
                        "project_hash":project_hash,
                        "reservation_operation_id":operation_id,
                        "recovered_after_head_contact":true,
                    }),
                )?;
                state.state_store.settle_dedicated_candidate_publication(
                    &session.placement_thread_id,
                    candidate,
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
                append_root_fact_once(
                    &state,
                    &session,
                    "hosted_candidate.publication_unknown",
                    &operation_id,
                    json!({
                        "schema":1,
                        "origin":"filesystem_verified",
                        "owner_principal":session.owner_principal,
                        "chain_root_id":session.chain_root_id,
                        "placement_thread_id":session.placement_thread_id,
                        "candidate_snapshot_hash":candidate,
                        "expected_previous_hash":base_snapshot_hash,
                        "candidate_validation_hash":validation,
                        "principal_key":principal_key,
                        "project_hash":project_hash,
                        "observed_head":current,
                        "reservation_operation_id":operation_id,
                        "recovered_after_head_contact":true,
                    }),
                )?;
                state.state_store.settle_dedicated_candidate_publication(
                    &session.placement_thread_id,
                    candidate,
                    &format!("publication_unknown:{candidate}"),
                )?;
            }
        }
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
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &placement_thread_id,
    )
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
                .take_exclusive_failure_cleanup_state(&placement_thread_id)
                .map_err(internal)?
                .ok_or_else(|| internal("approval contact failure lost its cleanup proof"))?;
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
            std::time::Duration::from_secs(30),
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
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate validation was reserved".into(),
        ));
    }
    let _root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
        &state.state_store,
        &session.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if session.state == "publish_ready"
        && session.candidate_snapshot_hash.as_deref() == Some(req.candidate_snapshot_hash.as_str())
        && session.candidate_validation_hash.as_deref()
            == Some(req.candidate_validation_hash.as_str())
    {
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "state":"publish_ready",
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "idempotent":true,
        }));
    }
    if !matches!(session.state.as_str(), "frozen" | "verifying")
        || session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        || session.candidate_validation_hash.as_deref()
            != Some(req.candidate_validation_hash.as_str())
    {
        return Err(HandlerError::BadRequest(
            "validation identity differs from the frozen candidate".into(),
        ));
    }
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_validation_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
    }))
    .map_err(internal)?;
    let thread = state
        .state_store
        .get_thread(&session.placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated root thread disappeared"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        ..
    } = thread
        .project_authority
        .ok_or_else(|| internal("dedicated root has no project authority"))?
    else {
        return Err(internal("dedicated candidate authority is not pinned"));
    };
    let pinned = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(internal)?;
    let _guard = pinned.acquire_shared_guard().map_err(internal)?;
    let cas = pinned.cas_store().map_err(internal)?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [req.candidate_snapshot_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if !closure.is_complete() {
        return Err(HandlerError::BadRequest(
            "candidate snapshot closure is incomplete or unsupported".into(),
        ));
    }
    if !super::project_apply_snapshot::snapshot_history_contains(
        &cas,
        &req.candidate_snapshot_hash,
        &base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let evidence = json!({
        "schema":"ryeos.hosted_candidate_closure_and_base_validation.v1",
        "checks":{
            "canonical_snapshot_closure":true,
            "base_ancestry":true,
        },
        "object_count":closure.object_hashes.len(),
        "blob_count":closure.blob_hashes.len(),
    });
    state
        .state_store
        .reserve_dedicated_candidate_validation(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &req.candidate_validation_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.validation_completed",
        &operation_id,
        json!({
            "schema":1,
            "origin":"filesystem_verified",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
            "candidate_validation_hash":req.candidate_validation_hash,
            "evidence":evidence,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_validation(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
            &req.candidate_validation_hash,
            &evidence,
        )
        .map_err(internal)?;
    ryeos_app::dedicated_session_service::notify_projection_change(&session.placement_thread_id);
    Ok(json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "state":"publish_ready",
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
        "candidate_validation_hash":req.candidate_validation_hash,
        "evidence":evidence,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardRequest {
    chain_root_id: String,
    candidate_snapshot_hash: String,
}

async fn discard(
    req: DiscardRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let initial = owned_session(&state, &ctx, &req.chain_root_id)?;
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        &initial.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate discard was reserved".into(),
        ));
    }
    if session.publication_result.as_deref() == Some("discarded") {
        if session.candidate_snapshot_hash.as_deref() != Some(req.candidate_snapshot_hash.as_str())
        {
            return Err(HandlerError::BadRequest(
                "discard retry changed candidate identity".into(),
            ));
        }
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "discarded":true,
            "idempotent":true,
        }));
    }
    let _root_operation = root_operation.ok_or_else(|| {
        HandlerError::BadRequest("nonterminal discard has a terminal hosted execution root".into())
    })?;
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_discard_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":req.candidate_snapshot_hash,
    }))
    .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.discarded",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":req.candidate_snapshot_hash,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_discard(
            &session.placement_thread_id,
            &req.candidate_snapshot_hash,
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
    let placement_thread_id = initial.placement_thread_id.clone();
    let operation_lock = disposition_operation_lock(&placement_thread_id);
    let _operation_guard = operation_lock.lock_owned().await;
    let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        &initial.placement_thread_id,
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let session = owned_session(&state, &ctx, &req.chain_root_id)?;
    if session.placement_thread_id != placement_thread_id {
        return Err(HandlerError::BadRequest(
            "hosted execution placement changed while candidate publication was reserved".into(),
        ));
    }
    if !matches!(
        session.state.as_str(),
        "publish_ready" | "publishing" | "terminal"
    ) {
        return Err(HandlerError::BadRequest(
            "session has no unpublished retained candidate".into(),
        ));
    }
    let candidate = session
        .candidate_snapshot_hash
        .as_deref()
        .ok_or_else(|| HandlerError::BadRequest("session candidate is not ready".into()))?;
    let candidate_validation_hash =
        session
            .candidate_validation_hash
            .as_deref()
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "session candidate has no retained validation identity".into(),
                )
            })?;
    let published_result = format!("published:{candidate}");
    let already_published =
        session.publication_result.as_deref() == Some(published_result.as_str());
    if session.publication_result.as_deref() != Some("retained") && !already_published {
        return Err(HandlerError::BadRequest(
            "session has no publishable retained candidate".into(),
        ));
    }
    let thread = state
        .state_store
        .get_thread(&session.placement_thread_id)
        .map_err(internal)?
        .ok_or_else(|| internal("dedicated root thread disappeared"))?;
    let authority = thread
        .project_authority
        .ok_or_else(|| internal("dedicated root has no project authority"))?;
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        realization,
        ..
    } = authority
    else {
        return Err(internal("dedicated publication authority is not pinned"));
    };
    let ryeos_state::objects::PinnedProjectRealization::Cow {
        terminal_publication:
            ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                principal_key,
                project_hash,
                expected_hash,
            },
    } = realization
    else {
        return Err(internal(
            "dedicated publication authority has no admitted explicit HEAD destination",
        ));
    };
    if expected_hash != base_snapshot_hash {
        return Err(internal(
            "dedicated publication HEAD fence differs from its admitted base",
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
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "snapshot_hash":candidate,
            "previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "published":true,
            "idempotent":true,
        }));
    }
    let _root_operation = root_operation.ok_or_else(|| {
        HandlerError::BadRequest(
            "unpublished candidate has a terminal hosted execution root".into(),
        )
    })?;
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_publication_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate,
        "expected_previous_hash":base_snapshot_hash,
        "candidate_validation_hash":candidate_validation_hash,
        "principal_key":principal_key,
        "project_hash":project_hash,
    }))
    .map_err(internal)?;
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.publication_reserved",
        &operation_id,
        json!({
            "schema":1,
            "origin":"owner_authorized",
            "owner_principal":session.owner_principal,
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "principal_key":principal_key,
            "project_hash":project_hash,
        }),
    )?;
    if !has_authoritative_candidate_publication_reservation(
        &state,
        &session,
        &operation_id,
        candidate,
        &base_snapshot_hash,
        candidate_validation_hash,
        &principal_key,
        &project_hash,
    )? {
        return Err(internal(
            "candidate publication reservation disappeared after authoritative append",
        ));
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
        &base_snapshot_hash,
    )
    .map_err(internal)?
    {
        return Err(HandlerError::BadRequest(
            "candidate does not descend from the admitted base generation".into(),
        ));
    }
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(internal)?;
    state
        .state_store
        .reserve_dedicated_candidate_publication(&session.placement_thread_id, candidate)
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let publication = state.state_store.with_state_db(|db| {
        let current = db.read_project_head(&principal_key, &project_hash)?;
        if current.as_deref() == Some(candidate) {
            return Ok(());
        }
        if current.as_deref() != Some(base_snapshot_hash.as_str()) {
            anyhow::bail!(
                "publication conflict: expected HEAD {}, current HEAD is {:?}",
                base_snapshot_hash,
                current
            );
        }
        db.advance_project_head_ref(
            &principal_key,
            &project_hash,
            candidate,
            &base_snapshot_hash,
            &signer,
            &cas_guard,
        )
    });
    if let Err(error) = publication {
        state
            .state_store
            .fail_dedicated_candidate_disposition(&session.placement_thread_id, "publishing")
            .map_err(internal)?;
        ryeos_app::dedicated_session_service::notify_projection_change(
            &session.placement_thread_id,
        );
        return Err(HandlerError::BadRequest(error.to_string()));
    }
    append_root_fact_once(
        &state,
        &session,
        "hosted_candidate.published",
        &operation_id,
        json!({
            "schema":1,
            "origin":"filesystem_verified",
            "owner_principal":session.owner_principal,
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "candidate_snapshot_hash":candidate,
            "expected_previous_hash":base_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "principal_key":principal_key,
            "project_hash":project_hash,
            "reservation_operation_id":operation_id,
        }),
    )?;
    state
        .state_store
        .settle_dedicated_candidate_publication(
            &session.placement_thread_id,
            candidate,
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
        CandidatePublicationRecovery, CheckpointRequest, CommandRequest, HandoffPreflightRequest,
        HandoffRequest, ResumeRequest, classify_candidate_publication_recovery,
    };

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
}
