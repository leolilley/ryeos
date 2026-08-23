//! Meaning-blind command delivery for one durable exclusive session.
//!
//! The integration runtime owns command bodies and observation meaning. This
//! service owns only the generic durable contact boundary, event/approval
//! ledgers, worker-epoch fencing, and cleanup proof consumption.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hosted_operation::{
    acquire_credential_profile_operation, acquire_credential_profile_operation_sync,
    begin_hosted_root_operation,
};
use crate::persistent_session::ExclusiveRetirementOutcome;
use crate::process::{IdentityLiveness, ShutdownAction, execution_group_liveness, kill_by_action};
use crate::runtime_db::{WorkerProcessRecord, WorkerProcessState, WorkspaceState};
use crate::state::AppState;
use crate::state_store::{
    DedicatedSessionRecord, NewDedicatedSessionApproval, NewDedicatedSessionCommand,
    NewEventRecord, ObservationBatchReservation,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerObservationBatch {
    first_sequence: u64,
    count: u64,
    previous_digest: Option<String>,
    batch_digest: String,
    events: Vec<Value>,
    session_observations: Vec<Value>,
}

fn projection_signal(session_id: &str) -> Arc<tokio::sync::Notify> {
    static SIGNALS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>> = OnceLock::new();
    let mut signals = SIGNALS
        .get_or_init(Default::default)
        .lock()
        .expect("dedicated projection signal map poisoned");
    Arc::clone(
        signals
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new())),
    )
}

pub fn notify_projection_change(session_id: &str) {
    projection_signal(session_id).notify_waiters();
}

pub async fn wait_for_projection_change(
    state: &AppState,
    session_id: &str,
    observed_updated_at_ms: i64,
    timeout: std::time::Duration,
) -> Result<DedicatedSessionRecord> {
    let signal = projection_signal(session_id);
    loop {
        let notified = signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let current = current_session(state, session_id)?;
        if current.updated_at_ms != observed_updated_at_ms || current.state == "terminal" {
            return Ok(current);
        }
        if tokio::time::timeout(timeout, notified).await.is_err() {
            return Ok(current_session(state, session_id)?);
        }
    }
}

/// Ingest one worker-pushed observation batch. The caller is the generic
/// session transport, not the worker: no callback capability is delegated to
/// the App Server or any model-launched child.
pub fn ingest_observation_batch(
    state: &AppState,
    session_id: &str,
    worker_boot_epoch: u64,
    raw: Value,
) -> Result<Value> {
    let session = current_session(state, session_id)?;
    let _root_operation = begin_hosted_root_operation(&session.root_thread_id)?;
    let _credential_operation =
        acquire_credential_profile_operation_sync(&session.credential_profile_id);
    let mut digest_input = raw.clone();
    let supplied_digest = digest_input
        .as_object_mut()
        .and_then(|object| object.remove("batch_digest"))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| anyhow!("worker observation batch has no digest"))?;
    let computed_digest = ryeos_state::objects::canonical_value_digest(&digest_input)?;
    if supplied_digest != computed_digest {
        bail!("worker observation batch digest mismatch");
    }
    let batch: WorkerObservationBatch = serde_json::from_value(raw)?;
    if batch.batch_digest != supplied_digest
        || batch.count == 0
        || batch.count > 128
        || batch.events.len() != usize::try_from(batch.count)?
        || batch.session_observations.len() > batch.events.len()
    {
        bail!("worker observation batch shape is invalid or unbounded");
    }
    let through_sequence = batch
        .first_sequence
        .checked_add(batch.count - 1)
        .ok_or_else(|| anyhow!("worker observation sequence overflow"))?;
    let reservation = state.state_store.reserve_dedicated_observation_batch(
        session_id,
        worker_boot_epoch,
        batch.first_sequence,
        through_sequence,
        batch.previous_digest.as_deref(),
        &batch.batch_digest,
    )?;
    if reservation == ObservationBatchReservation::AlreadySettled {
        return Ok(json!({
            "through_sequence": through_sequence,
            "batch_digest": batch.batch_digest,
        }));
    }
    let result = json!({
        "events": batch.events,
        "session_observations": batch.session_observations,
    });
    if reservation == ObservationBatchReservation::RebuildProjection {
        let authoritative = require_authoritative_batch(
            state,
            &session,
            worker_boot_epoch,
            &batch.batch_digest,
            batch.first_sequence,
            through_sequence,
        )?;
        project_worker_events(state, &session, worker_boot_epoch, &authoritative)?;
        apply_worker_observations(state, session_id, worker_boot_epoch, &authoritative)?;
        state.state_store.settle_dedicated_observation_batch(
            session_id,
            worker_boot_epoch,
            batch.first_sequence,
            &batch.batch_digest,
        )?;
        notify_projection_change(session_id);
        return Ok(json!({
            "through_sequence":through_sequence,
            "batch_digest":batch.batch_digest,
            "projection_rebuilt":true,
        }));
    }
    let append = (|| {
        let thread = state
            .threads
            .get_thread(&session.root_thread_id)?
            .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
        let mut events = vec![NewEventRecord {
            event_type: "hosted_worker_observation_batch".to_owned(),
            storage_class: "indexed".to_owned(),
            payload: json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "session_id":session_id,
                "worker_boot_epoch":worker_boot_epoch,
                "batch_digest":batch.batch_digest,
                "first_sequence":batch.first_sequence,
                "through_sequence":through_sequence,
                "canonical_batch":result.clone(),
            }),
        }];
        events.extend(
            result
                .get("events")
                .and_then(Value::as_array)
                .expect("validated observation events")
                .iter()
                .map(|event| {
                    let event: WorkerEvent = serde_json::from_value(event.clone())?;
                    Ok(NewEventRecord {
                        event_type: "hosted_worker_observation".to_owned(),
                        storage_class: "indexed".to_owned(),
                        payload: json!({
                            "schema": 1,
                            "origin": "worker_asserted",
                            "session_id": session_id,
                            "worker_boot_epoch": worker_boot_epoch,
                            "batch_digest": batch.batch_digest,
                            "first_sequence": batch.first_sequence,
                            "through_sequence": through_sequence,
                            "upstream_event_type": event.event_type,
                            "observation": event.payload,
                        }),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        );
        state
            .threads
            .append_thread_events(&thread.chain_root_id, &thread.thread_id, &events)?
            .ok_or_else(|| anyhow!("hosted execution root is no longer running"))?;
        // The root event chain is the authority. Approval and session tables
        // are rebuildable correlation/projection ledgers and may advance only
        // after the authoritative append has durably succeeded.
        project_worker_events(state, &session, worker_boot_epoch, &result)?;
        apply_worker_observations(state, session_id, worker_boot_epoch, &result)?;
        state.state_store.settle_dedicated_observation_batch(
            session_id,
            worker_boot_epoch,
            batch.first_sequence,
            &batch.batch_digest,
        )?;
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = append {
        state.state_store.mark_dedicated_observation_batch_unknown(
            session_id,
            worker_boot_epoch,
            batch.first_sequence,
            &batch.batch_digest,
        )?;
        return Err(error);
    }
    notify_projection_change(session_id);
    Ok(json!({
        "through_sequence": through_sequence,
        "batch_digest": batch.batch_digest,
    }))
}

fn require_authoritative_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    batch_digest: &str,
    first_sequence: u64,
    through_sequence: u64,
) -> Result<Value> {
    let thread = state
        .state_store
        .get_thread(&session.root_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    let mut authoritative = None;
    let mut after = None;
    loop {
        let page = state.state_store.replay_events(
            &thread.chain_root_id,
            Some(&thread.thread_id),
            after,
            1024,
            8 * 1024 * 1024,
        )?;
        for event in &page.events {
            if event.event_type == "hosted_worker_observation_batch"
                && event.payload.get("session_id").and_then(Value::as_str)
                    == Some(session.session_id.as_str())
                && event
                    .payload
                    .get("worker_boot_epoch")
                    .and_then(Value::as_u64)
                    == Some(worker_boot_epoch)
                && event.payload.get("batch_digest").and_then(Value::as_str) == Some(batch_digest)
                && event.payload.get("first_sequence").and_then(Value::as_u64)
                    == Some(first_sequence)
                && event
                    .payload
                    .get("through_sequence")
                    .and_then(Value::as_u64)
                    == Some(through_sequence)
            {
                let batch = event
                    .payload
                    .get("canonical_batch")
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!("authoritative observation batch has no canonical payload")
                    })?;
                if authoritative.replace(batch).is_some() {
                    bail!("authoritative observation batch identity is duplicated");
                }
            }
        }
        after = page.events.last().map(|event| event.chain_seq);
        if !page.has_more {
            break;
        }
    }
    authoritative.ok_or_else(|| {
        anyhow!("observation projection cannot be rebuilt without its authoritative batch fact")
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerObservation {
    RemoteThread {
        id: String,
    },
    RemoteThreadRecovered {
        id: String,
    },
    RemoteThreadRecoveryStatus {
        id: String,
        status: String,
    },
    State {
        expected: String,
        next: String,
        #[serde(default)]
        turn_id: Option<String>,
        #[serde(default)]
        completed_turn_id: Option<String>,
    },
    CredentialEnrollmentStarted {
        login_id: String,
        ttl_seconds: u64,
    },
    CredentialEnrollmentObserved {
        account: Value,
    },
    CredentialEnrollmentCancelled {
        login_id: String,
    },
    ApprovalExpired {
        approval_id: String,
    },
}

fn apply_worker_observations(
    state: &AppState,
    session_id: &str,
    worker_boot_epoch: u64,
    result: &Value,
) -> Result<()> {
    let Some(values) = result.get("session_observations") else {
        return Ok(());
    };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow!("worker session observations are not a bounded array"))?;
    if values.len() > 16 {
        bail!("worker emitted too many session observations");
    }
    for value in values {
        match serde_json::from_value(value.clone())? {
            WorkerObservation::RemoteThread { id } => {
                let session = state
                    .state_store
                    .dedicated_session(session_id)?
                    .ok_or_else(|| anyhow!("dedicated session disappeared"))?;
                let worker_instance_id = session
                    .worker_instance_id
                    .ok_or_else(|| anyhow!("remote-thread observation has no attached worker"))?;
                state.state_store.bind_dedicated_remote_thread(
                    session_id,
                    &worker_instance_id,
                    worker_boot_epoch,
                    &id,
                )?;
            }
            WorkerObservation::RemoteThreadRecovered { id } => {
                state.state_store.observe_dedicated_remote_reattach(
                    session_id,
                    worker_boot_epoch,
                    &id,
                )?;
            }
            WorkerObservation::RemoteThreadRecoveryStatus { id, status } => {
                state.state_store.settle_dedicated_remote_recovery_status(
                    session_id,
                    worker_boot_epoch,
                    &id,
                    &status,
                )?;
            }
            WorkerObservation::State {
                expected,
                next,
                turn_id,
                completed_turn_id,
            } => {
                let (expected_turn_id, next_turn_id) = match (expected.as_str(), next.as_str()) {
                    ("idle", "turn_running") if completed_turn_id.is_none() => {
                        (None, turn_id.as_deref())
                    }
                    ("turn_running", "idle") if turn_id.is_none() => {
                        (completed_turn_id.as_deref(), None)
                    }
                    _ => bail!("worker emitted an invalid generic session observation shape"),
                };
                state.state_store.observe_dedicated_session_state(
                    session_id,
                    worker_boot_epoch,
                    &expected,
                    &next,
                    expected_turn_id,
                    next_turn_id,
                )?;
            }
            WorkerObservation::CredentialEnrollmentStarted {
                login_id,
                ttl_seconds,
            } => {
                let session = current_session(state, session_id)?;
                let worker_instance_id =
                    session.worker_instance_id.as_deref().ok_or_else(|| {
                        anyhow!("credential enrollment observation has no attached worker")
                    })?;
                let expires_at_ms = (lillux::time::timestamp_millis() as i64)
                    .checked_add(i64::try_from(ttl_seconds.clamp(1, 15 * 60))? * 1000)
                    .ok_or_else(|| anyhow!("credential enrollment expiry overflow"))?;
                let profile = state
                    .state_store
                    .credential_profile(&session.credential_profile_id)?
                    .ok_or_else(|| anyhow!("credential profile disappeared"))?;
                if !(profile.state == "enrolling"
                    && profile.active_login_id.as_deref() == Some(login_id.as_str()))
                {
                    state.state_store.begin_credential_enrollment(
                        &session.credential_profile_id,
                        worker_instance_id,
                        &login_id,
                        expires_at_ms,
                    )?;
                }
            }
            WorkerObservation::CredentialEnrollmentObserved { account } => {
                let session = current_session(state, session_id)?;
                let worker_instance_id =
                    session.worker_instance_id.as_deref().ok_or_else(|| {
                        anyhow!("credential completion observation has no attached worker")
                    })?;
                let profile = state
                    .state_store
                    .credential_profile(&session.credential_profile_id)?
                    .ok_or_else(|| anyhow!("credential profile disappeared"))?;
                let already_observed = profile.state == "confirming"
                    && profile.sanitized_account.as_ref() == Some(&account);
                if profile.state != "active" && !already_observed {
                    state.state_store.observe_session_credential_enrollment(
                        session_id,
                        worker_instance_id,
                        worker_boot_epoch,
                        &account,
                    )?;
                }
            }
            WorkerObservation::CredentialEnrollmentCancelled { login_id } => {
                let session = current_session(state, session_id)?;
                let worker_instance_id =
                    session.worker_instance_id.as_deref().ok_or_else(|| {
                        anyhow!("credential cancellation observation has no attached worker")
                    })?;
                let profile = state
                    .state_store
                    .credential_profile(&session.credential_profile_id)?
                    .ok_or_else(|| anyhow!("credential profile disappeared"))?;
                if profile.state != "unauthenticated" {
                    state.state_store.cancel_credential_enrollment(
                        &session.credential_profile_id,
                        worker_instance_id,
                        &login_id,
                        profile.login_epoch,
                    )?;
                }
            }
            WorkerObservation::ApprovalExpired { approval_id } => {
                state.state_store.expire_dedicated_session_approval(
                    session_id,
                    &approval_id,
                    worker_boot_epoch,
                )?;
            }
        }
    }
    Ok(())
}

fn current_session(state: &AppState, session_id: &str) -> Result<DedicatedSessionRecord> {
    state
        .state_store
        .dedicated_session(session_id)?
        .ok_or_else(|| anyhow!("dedicated session disappeared"))
}

/// Complete canonical command testimony after restart from the durable
/// command outbox. This never contacts a worker. A possible-contact row is
/// reconciled only to outcome-unknown; it is never replayed.
pub fn reconcile_command_outboxes(state: &AppState) -> Result<()> {
    for record in state.state_store.dedicated_command_outbox_records()? {
        let session = current_session(state, &record.session_id)?;
        let _root_operation = begin_hosted_root_operation(&session.root_thread_id)?;
        let _credential_operation =
            acquire_credential_profile_operation_sync(&session.credential_profile_id);
        let (profile_hash, schema_hashes) =
            structured_protocol_identity(state, &session.admitted_capsule_hash)?;
        append_command_fact_once(
            state,
            &session,
            "hosted_command.committed",
            record.command_sequence,
            &record.request_digest,
            json!({
                "schema":1,
                "origin":"daemon_observed_io",
                "worker_boot_epoch":record.worker_boot_epoch,
                "command_kind":&record.command_kind,
                "route_id":record.payload.get("route_id").and_then(Value::as_str),
                "idempotency_key":&record.idempotency_key,
                "canonical_command":&record.payload,
                "admitted_session_capsule_hash":&session.admitted_capsule_hash,
                "protocol_profile_hash":profile_hash,
                "protocol_schema_hashes":schema_hashes,
                "recovered":true,
            }),
        )?;
        match record.state.as_str() {
            "committed" => {}
            "dispatched" | "outcome_unknown" => {
                if let Some((canonical_batch, response_digest)) =
                    find_authoritative_command_observation_batch(
                        state,
                        &session,
                        record.worker_boot_epoch,
                        record.command_sequence,
                        &record.request_digest,
                    )?
                {
                    project_worker_events(
                        state,
                        &session,
                        record.worker_boot_epoch,
                        &canonical_batch,
                    )?;
                    apply_worker_observations(
                        state,
                        &record.session_id,
                        record.worker_boot_epoch,
                        &canonical_batch,
                    )?;
                    append_command_fact_once(
                        state,
                        &session,
                        "hosted_command.settled",
                        record.command_sequence,
                        &record.request_digest,
                        json!({
                            "schema":1,
                            "origin":"daemon_observed_io",
                            "worker_boot_epoch":record.worker_boot_epoch,
                            "response_digest":response_digest,
                            "succeeded":true,
                            "recovered":true,
                        }),
                    )?;
                    state.state_store.settle_recovered_dedicated_command(
                        &record.session_id,
                        record.command_sequence,
                        record.worker_boot_epoch,
                        &json!({
                            "redacted":true,
                            "response_digest":response_digest,
                            "recovered_from_root_chain":true,
                        }),
                    )?;
                    notify_projection_change(&record.session_id);
                    continue;
                }
                append_command_fact_once(
                    state,
                    &session,
                    "hosted_command.contacting",
                    record.command_sequence,
                    &record.request_digest,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "recovered":true,
                    }),
                )?;
                if record.state == "dispatched" {
                    state.state_store.mark_dedicated_command_outcome_unknown(
                        &record.session_id,
                        record.command_sequence,
                        record.worker_boot_epoch,
                    )?;
                }
                append_command_fact_once(
                    state,
                    &session,
                    "hosted_command.outcome_unknown",
                    record.command_sequence,
                    &record.request_digest,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "cleanup_state":"restart_reconciliation",
                        "recovered":true,
                    }),
                )?;
            }
            "completed" => {
                let result = record.result.as_ref().unwrap_or(&Value::Null);
                let response_digest = result
                    .get("response_digest")
                    .and_then(Value::as_str)
                    .filter(|digest| lillux::valid_hash(digest))
                    .map(ToOwned::to_owned)
                    .unwrap_or(ryeos_state::objects::canonical_value_digest(result)?);
                append_command_fact_once(
                    state,
                    &session,
                    "hosted_command.settled",
                    record.command_sequence,
                    &record.request_digest,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "response_digest":response_digest,
                        "succeeded":true,
                        "recovered":true,
                    }),
                )?;
            }
            "failed"
                if record.result.as_ref().is_some_and(|result| {
                    result.get("retryable_uncontacted").and_then(Value::as_bool) == Some(true)
                }) =>
            {
                append_command_fact_once(
                    state,
                    &session,
                    "hosted_command.failed_uncontacted",
                    record.command_sequence,
                    &record.request_digest,
                    json!({
                        "schema":1,
                        "origin":"daemon_verified_process",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "retryable_uncontacted":true,
                        "recovered":true,
                    }),
                )?;
            }
            "failed" => {
                let result = record.result.as_ref().unwrap_or(&Value::Null);
                append_command_fact_once(
                    state,
                    &session,
                    "hosted_command.settled",
                    record.command_sequence,
                    &record.request_digest,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "response_digest":ryeos_state::objects::canonical_value_digest(result)?,
                        "succeeded":false,
                        "recovered":true,
                    }),
                )?;
            }
            other => bail!("dedicated command outbox has invalid state `{other}`"),
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerEvent {
    event_type: String,
    payload: Value,
}

fn project_worker_events(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    result: &Value,
) -> Result<()> {
    let Some(values) = result.get("events") else {
        return Ok(());
    };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow!("worker events are not a bounded array"))?;
    if values.len() > 512 {
        bail!("worker emitted too many events in one response");
    }
    for value in values {
        let event: WorkerEvent = serde_json::from_value(value.clone())?;
        if event.event_type == "approval.requested" {
            let upstream_request_id = event
                .payload
                .get("request_id")
                .ok_or_else(|| anyhow!("approval event has no request id"))?;
            let operation_class = event
                .payload
                .get("operation_class")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("approval event has no operation class"))?;
            let display = event
                .payload
                .get("display")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("approval event has no typed display authority"))?;
            let request_digest = event
                .payload
                .get("request_digest")
                .and_then(Value::as_str)
                .filter(|digest| lillux::valid_hash(digest))
                .ok_or_else(|| anyhow!("approval event has no canonical request digest"))?;
            let observed_thread = display
                .get("thread_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("approval event has no correlated thread id"))?;
            let observed_turn = display
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("approval event has no correlated turn id"))?;
            if session.remote_thread_id.as_deref() != Some(observed_thread)
                || session.current_turn_id.as_deref() != Some(observed_turn)
            {
                bail!("approval event does not correlate to the retained thread and turn");
            }
            let approval_id = ryeos_state::objects::canonical_value_digest(&json!({
                "worker_boot_epoch":worker_boot_epoch,
                "upstream_request_id":upstream_request_id,
                "request_digest":request_digest,
            }))?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| anyhow!("approval event has no attached worker"))?;
            state
                .state_store
                .create_dedicated_session_approval(NewDedicatedSessionApproval {
                    session_id: &session.session_id,
                    approval_id: &approval_id,
                    worker_instance_id,
                    worker_boot_epoch,
                    request_digest,
                    operation_class,
                    requested_authority: &event.payload,
                    expires_at_ms: lillux::time::timestamp_millis() as i64 + 15 * 60 * 1000,
                })?;
        }
    }
    Ok(())
}

/// Execute one opaque integration-owned request across a durable at-most-once
/// contact boundary. This function performs no dispatch based on command kind.
pub async fn execute_command(
    state: &AppState,
    session_id: &str,
    idempotency_key: &str,
    command_kind: &str,
    payload: Value,
) -> Result<Value> {
    let initial = current_session(state, session_id)?;
    let _root_operation = begin_hosted_root_operation(&initial.root_thread_id)?;
    let _credential_operation =
        acquire_credential_profile_operation(&initial.credential_profile_id).await?;
    let session = current_session(state, session_id)?;
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| anyhow!("dedicated session has no attached worker"))?;
    let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind": command_kind,
        "payload": payload,
    }))?;
    let (protocol_profile_hash, protocol_schema_hashes) =
        structured_protocol_identity(state, &session.admitted_capsule_hash)?;
    let record =
        state
            .state_store
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id,
                idempotency_key,
                worker_boot_epoch,
                command_kind,
                request_digest: &request_digest,
                payload: &payload,
            })?;
    match record.state.as_str() {
        "completed" | "failed" => {
            return Ok(json!({
                "command_sequence": record.command_sequence,
                "state": record.state,
                "result": record.result,
            }));
        }
        "outcome_unknown" | "dispatched" => {
            bail!("command may have contacted its worker and will not be resent")
        }
        "committed" => {}
        _ => bail!("dedicated command has an invalid durable state"),
    }
    append_command_fact_once(
        state,
        &session,
        "hosted_command.committed",
        record.command_sequence,
        &request_digest,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "worker_boot_epoch":worker_boot_epoch,
            "command_kind":command_kind,
            "route_id":payload.get("route_id").and_then(Value::as_str),
            "idempotency_key":idempotency_key,
            "canonical_command":payload,
            "admitted_session_capsule_hash":session.admitted_capsule_hash,
            "protocol_profile_hash":protocol_profile_hash,
            "protocol_schema_hashes":protocol_schema_hashes,
        }),
    )?;
    state.state_store.mark_dedicated_command_contacted(
        session_id,
        record.command_sequence,
        worker_boot_epoch,
    )?;
    if let Err(error) = append_command_fact_once(
        state,
        &session,
        "hosted_command.contacting",
        record.command_sequence,
        &request_digest,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "worker_boot_epoch":worker_boot_epoch,
        }),
    ) {
        state.state_store.mark_dedicated_command_outcome_unknown(
            session_id,
            record.command_sequence,
            worker_boot_epoch,
        )?;
        return Err(error.context("persist command possible-contact boundary"));
    }
    let pool = Arc::clone(&state.persistent_sessions);
    let execution_session_id = session_id.to_string();
    let is_runtime_route = command_kind == "reattach";
    let outcome = tokio::task::spawn_blocking(move || {
        if is_runtime_route {
            let route = payload
                .as_object()
                .ok_or_else(|| anyhow!("runtime-owned route payload is not an object"))?;
            let route_id = route
                .get("route_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("runtime-owned route has no route id"))?;
            let body = json!({
                "kind":"runtime_route",
                "route_id":route_id,
                "payload":route.get("payload").cloned().unwrap_or_else(|| json!({})),
            });
            pool.execute_exclusive_control(&execution_session_id, body)
        } else {
            pool.execute_exclusive(&execution_session_id, payload, || false, |_| Ok(()))
        }
    })
    .await?;
    match outcome {
        Ok(result) => {
            if let Err(error) = append_command_observation_batch(
                state,
                &session,
                worker_boot_epoch,
                record.command_sequence,
                &request_digest,
                &result,
            )
            .and_then(|()| project_worker_events(state, &session, worker_boot_epoch, &result))
            .and_then(|()| apply_worker_observations(state, session_id, worker_boot_epoch, &result))
            {
                state.state_store.mark_dedicated_command_outcome_unknown(
                    session_id,
                    record.command_sequence,
                    worker_boot_epoch,
                )?;
                return Err(error);
            }
            let persisted_result =
                if result.get("result_retention").and_then(Value::as_str) == Some("ephemeral") {
                    json!({
                        "redacted": true,
                        "response_digest": ryeos_state::objects::canonical_value_digest(&result)?,
                    })
                } else {
                    result.clone()
                };
            append_command_fact_once(
                state,
                &session,
                "hosted_command.settled",
                record.command_sequence,
                &request_digest,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "worker_boot_epoch":worker_boot_epoch,
                    "response_digest":ryeos_state::objects::canonical_value_digest(&result)?,
                    "succeeded":true,
                }),
            )?;
            state.state_store.settle_dedicated_command(
                session_id,
                record.command_sequence,
                worker_boot_epoch,
                true,
                &persisted_result,
            )?;
            notify_projection_change(session_id);
            Ok(json!({
                "command_sequence": record.command_sequence,
                "state": "completed",
                "result": result,
            }))
        }
        Err(error) => {
            let cleanup_state = state
                .persistent_sessions
                .take_exclusive_failure_cleanup_state(session_id)?
                .ok_or_else(|| anyhow!("exclusive worker failure lost its cleanup proof"))?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| anyhow!("failed command has no worker identity"))?;
            state.state_store.fence_abandoned_worker_process(
                worker_instance_id,
                session_id,
                worker_boot_epoch,
                cleanup_state,
            )?;
            append_command_fact_once(
                state,
                &session,
                "hosted_command.outcome_unknown",
                record.command_sequence,
                &request_digest,
                json!({
                    "schema":1,
                    "origin":"daemon_observed_io",
                    "worker_boot_epoch":worker_boot_epoch,
                    "cleanup_state":cleanup_state,
                }),
            )?;
            notify_projection_change(session_id);
            bail!(
                "worker contact failed; command outcome is unknown, cleanup is {cleanup_state}, and it will not be resent: {error}"
            )
        }
    }
}

fn structured_protocol_identity(
    state: &AppState,
    capsule_hash: &str,
) -> Result<(String, std::collections::BTreeMap<String, String>)> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(capsule_hash)?
        .ok_or_else(|| anyhow!("admitted session capsule disappeared"))?;
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != capsule_hash {
        bail!("admitted session capsule content hash changed");
    }
    let profile = capsule
        .structured_session_profile
        .ok_or_else(|| anyhow!("structured session capsule has no admitted protocol profile"))?;
    Ok((profile.profile_hash, profile.schema_hashes))
}

fn append_command_fact_once(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    mut payload: Value,
) -> Result<()> {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "session_id":session.session_id,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":event_type,
    }))?;
    let thread = state
        .state_store
        .get_thread(&session.root_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    let mut after = None;
    loop {
        let page = state.state_store.replay_events(
            &thread.chain_root_id,
            Some(&thread.thread_id),
            after,
            1024,
            8 * 1024 * 1024,
        )?;
        if page.events.iter().any(|event| {
            event.event_type == event_type
                && event.payload.get("operation_id").and_then(Value::as_str)
                    == Some(operation_id.as_str())
        }) {
            return Ok(());
        }
        after = page.events.last().map(|event| event.chain_seq);
        if !page.has_more {
            break;
        }
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("hosted command fact payload is not an object"))?;
    object.insert("operation_id".to_owned(), Value::String(operation_id));
    object.insert(
        "session_id".to_owned(),
        Value::String(session.session_id.clone()),
    );
    object.insert(
        "command_sequence".to_owned(),
        Value::Number(command_sequence.into()),
    );
    object.insert(
        "request_digest".to_owned(),
        Value::String(request_digest.to_owned()),
    );
    state
        .threads
        .append_thread_events(
            &thread.chain_root_id,
            &thread.thread_id,
            &[NewEventRecord {
                event_type: event_type.to_owned(),
                storage_class: "indexed".to_owned(),
                payload,
            }],
        )?
        .ok_or_else(|| anyhow!("hosted execution root is no longer running"))?;
    Ok(())
}

fn append_command_observation_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    command_sequence: u64,
    request_digest: &str,
    result: &Value,
) -> Result<()> {
    let events = result.get("events").cloned().unwrap_or_else(|| json!([]));
    let observations = result
        .get("session_observations")
        .cloned()
        .unwrap_or_else(|| json!([]));
    if !events.is_array() || !observations.is_array() {
        bail!("command observation batch fields are not arrays");
    }
    let response_digest = ryeos_state::objects::canonical_value_digest(result)?;
    append_command_fact_once(
        state,
        session,
        "hosted_worker_command_observation_batch",
        command_sequence,
        request_digest,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "worker_boot_epoch":worker_boot_epoch,
            "response_digest":response_digest,
            "canonical_batch":{
                "events":events,
                "session_observations":observations,
            },
        }),
    )
}

fn find_authoritative_command_observation_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    command_sequence: u64,
    request_digest: &str,
) -> Result<Option<(Value, String)>> {
    let thread = state
        .state_store
        .get_thread(&session.root_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    let mut after = None;
    loop {
        let page = state.state_store.replay_events(
            &thread.chain_root_id,
            Some(&thread.thread_id),
            after,
            1024,
            8 * 1024 * 1024,
        )?;
        for event in &page.events {
            let payload = &event.payload;
            if event.event_type != "hosted_worker_command_observation_batch"
                || payload.get("session_id").and_then(Value::as_str)
                    != Some(session.session_id.as_str())
                || payload.get("worker_boot_epoch").and_then(Value::as_u64)
                    != Some(worker_boot_epoch)
                || payload.get("command_sequence").and_then(Value::as_u64) != Some(command_sequence)
                || payload.get("request_digest").and_then(Value::as_str) != Some(request_digest)
            {
                continue;
            }
            let response_digest = payload
                .get("response_digest")
                .and_then(Value::as_str)
                .filter(|digest| lillux::valid_hash(digest))
                .ok_or_else(|| anyhow!("authoritative command batch has no response digest"))?;
            let batch = payload
                .get("canonical_batch")
                .cloned()
                .ok_or_else(|| anyhow!("authoritative command batch has no canonical body"))?;
            if !batch.get("events").is_some_and(Value::is_array)
                || !batch
                    .get("session_observations")
                    .is_some_and(Value::is_array)
            {
                bail!("authoritative command batch body is malformed");
            }
            return Ok(Some((batch, response_digest.to_owned())));
        }
        after = page.events.last().map(|event| event.chain_seq);
        if !page.has_more {
            return Ok(None);
        }
    }
}

/// Retire one exact durable worker identity without treating registry absence
/// as process-death proof. The process identity is the final authority when
/// the in-memory registry cannot prove that it reaped the owned group.
pub fn retire_worker_process(
    state: &AppState,
    session_id: &str,
    worker: &WorkerProcessRecord,
) -> Result<&'static str> {
    let registry_outcome = state.persistent_sessions.retire_exclusive(session_id)?;
    if registry_outcome == ExclusiveRetirementOutcome::Reaped {
        return Ok("reaped");
    }
    let prove_from_identity = || match execution_group_liveness(&worker.process_identity) {
        IdentityLiveness::DeadOrStale => true,
        IdentityLiveness::Alive => {
            let killed = kill_by_action(&worker.process_identity, ShutdownAction::Hard);
            killed.success
                && execution_group_liveness(&worker.process_identity)
                    == IdentityLiveness::DeadOrStale
        }
        IdentityLiveness::Unavailable => false,
    };
    Ok(if prove_from_identity() {
        "reaped"
    } else {
        "unproved"
    })
}

/// Drain and terminally settle one session after its caller has already
/// proved owner/root authority. This is shared by authenticated services and
/// the callback-owned controller so duration expiry cannot orphan a worker.
pub async fn terminate_session(state: &AppState, session_id: &str, reason: &str) -> Result<Value> {
    if !matches!(reason, "completed" | "cancelled") {
        bail!("terminal reason must be completed or cancelled");
    }
    let initial = current_session(state, session_id)?;
    let _root_operation = begin_hosted_root_operation(&initial.root_thread_id)?;
    let _credential_operation =
        acquire_credential_profile_operation(&initial.credential_profile_id).await?;
    let session = current_session(state, session_id)?;
    if session.state == "terminal" {
        if session.terminal_reason.as_deref() != Some(reason) {
            bail!("terminal session reason conflicts with the requested retry");
        }
        finish_terminal_credential_cleanup(state, &session)?;
        notify_projection_change(session_id);
        return Ok(json!({
            "session_id":session_id,
            "state":"terminal",
            "reason":reason,
            "idempotent":true,
        }));
    }
    if session.worker_instance_id.is_none() && session.worker_boot_epoch.is_none() {
        if !matches!(session.state.as_str(), "recovering" | "outcome_unknown") {
            bail!("unattached dedicated session is not recoverable or ambiguous");
        }
        if reason == "completed" {
            bail!("an unattached session cannot be declared completed");
        }
        let profile = state
            .state_store
            .credential_profile(&session.credential_profile_id)?
            .ok_or_else(|| anyhow!("dedicated session credential profile disappeared"))?;
        if profile.lock_owner.is_some() {
            bail!("worker cleanup is unproved; the credential profile remains fenced");
        }
        state
            .state_store
            .terminalize_unattached_dedicated_session(session_id, reason)?;
        notify_projection_change(session_id);
        return Ok(json!({
            "session_id":session_id,
            "state":"terminal",
            "reason":reason,
            "prior_outcome":"unknown",
        }));
    }
    let worker_instance_id = session
        .worker_instance_id
        .as_deref()
        .ok_or_else(|| anyhow!("dedicated session has no attached worker"))?;
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| anyhow!("dedicated session has no worker epoch"))?;
    if reason == "completed"
        && !matches!(
            session.state.as_str(),
            "recovering"
                | "freezing"
                | "frozen"
                | "verifying"
                | "publish_ready"
                | "publishing"
                | "discarding"
        )
    {
        state
            .state_store
            .reserve_dedicated_session_completion(session_id, worker_boot_epoch)?;
    }
    let worker = state
        .state_store
        .worker_process(worker_instance_id)?
        .ok_or_else(|| anyhow!("dedicated worker process projection disappeared"))?;
    if worker.state != WorkerProcessState::Dead || worker.cleanup_state != "reaped" {
        let cleanup_state = retire_worker_process(state, session_id, &worker)?;
        if cleanup_state != "reaped" {
            state.state_store.fence_abandoned_worker_process(
                worker_instance_id,
                session_id,
                worker_boot_epoch,
                cleanup_state,
            )?;
            bail!("dedicated worker cleanup remains unproved");
        }
    }
    let after_retire = current_session(state, session_id)?;
    if !matches!(
        after_retire.state.as_str(),
        "recovering"
            | "freezing"
            | "frozen"
            | "verifying"
            | "publish_ready"
            | "publishing"
            | "discarding"
            | "terminal"
    ) {
        state.state_store.settle_worker_process(
            worker_instance_id,
            session_id,
            worker_boot_epoch,
            "reaped",
            reason,
        )?;
    }
    let after_settle = current_session(state, session_id)?;
    if after_settle.state == "recovering" {
        state.state_store.terminalize_dedicated_session(
            session_id,
            worker_instance_id,
            worker_boot_epoch,
            reason,
        )?;
    } else if reason != "completed" && after_settle.state != "terminal" {
        bail!("cancelled termination cannot override a retained candidate disposition");
    }
    finish_terminal_credential_cleanup(state, &session)?;
    let terminal = current_session(state, session_id)?;
    notify_projection_change(session_id);
    Ok(json!({
        "session_id":session_id,
        "state":terminal.state,
        "reason":reason
    }))
}

pub fn finish_terminal_credential_cleanup(
    state: &AppState,
    session: &DedicatedSessionRecord,
) -> Result<()> {
    let Some(worker_instance_id) = session.worker_instance_id.as_deref() else {
        return Ok(());
    };
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| anyhow!("terminal session has a partial worker identity"))?;
    let worker = state
        .state_store
        .worker_process(worker_instance_id)?
        .ok_or_else(|| anyhow!("terminal session worker projection disappeared"))?;
    if worker.session_id != session.session_id || worker.boot_epoch != worker_boot_epoch {
        bail!("terminal session worker identity does not match its durable owner");
    }
    if worker.state != WorkerProcessState::Dead || worker.cleanup_state != "reaped" {
        bail!("terminal credential cleanup requires proved worker death and reap");
    }
    let profile = state
        .state_store
        .credential_profile(&session.credential_profile_id)?
        .ok_or_else(|| anyhow!("dedicated session credential profile disappeared"))?;
    if profile.state == "enrolling" && profile.lock_owner.as_deref() == Some(worker_instance_id) {
        let login_id = profile
            .active_login_id
            .as_deref()
            .ok_or_else(|| anyhow!("enrolling profile has no active login identity"))?;
        state.state_store.cancel_credential_enrollment(
            &session.credential_profile_id,
            worker_instance_id,
            login_id,
            profile.login_epoch,
        )?;
    }
    let refreshed = state
        .state_store
        .credential_profile(&session.credential_profile_id)?
        .ok_or_else(|| anyhow!("dedicated session credential profile disappeared"))?;
    match refreshed.lock_owner.as_deref() {
        Some(owner) if owner == worker_instance_id => state
            .state_store
            .release_credential_profile(&session.credential_profile_id, worker_instance_id)?,
        None => {}
        // This terminal session may be historical: after its exact worker was
        // proved reaped and its lease released, the same profile can safely
        // serve a later session. Idempotent cleanup must never release or
        // reject that later exact owner.
        Some(_) => {}
    }
    Ok(())
}

/// Node-owned cancellation fallback used by the root execution guard. It does
/// not depend on the cooperative controller still being alive.
pub fn abort_session_for_root_stop(state: &AppState, root_thread_id: &str) -> Result<()> {
    let Some(session) = state.state_store.dedicated_session(root_thread_id)? else {
        return Ok(());
    };
    let _credential_operation =
        acquire_credential_profile_operation_sync(&session.credential_profile_id);
    if session.state == "terminal" {
        finish_terminal_credential_cleanup(state, &session)?;
        return close_session_workspace(state, &session);
    }
    if matches!(
        session.state.as_str(),
        "freezing" | "frozen" | "verifying" | "publish_ready" | "discarding"
    ) {
        state
            .state_store
            .cancel_dedicated_candidate_for_root_stop(&session.session_id)?;
        finish_terminal_credential_cleanup(state, &session)?;
        return close_session_workspace(state, &session);
    }
    if session.state == "publishing" {
        bail!("candidate publication is already at a possible irreversible contact boundary");
    }
    match (
        session.worker_instance_id.as_deref(),
        session.worker_boot_epoch,
    ) {
        (Some(worker_instance_id), Some(worker_boot_epoch)) => {
            let worker = state
                .state_store
                .worker_process(worker_instance_id)?
                .ok_or_else(|| anyhow!("root-owned worker process projection disappeared"))?;
            let cleanup_state =
                if worker.state == WorkerProcessState::Dead && worker.cleanup_state == "reaped" {
                    "reaped"
                } else {
                    retire_worker_process(state, &session.session_id, &worker)?
                };
            if cleanup_state != "reaped" {
                state.state_store.fence_abandoned_worker_process(
                    worker_instance_id,
                    &session.session_id,
                    worker_boot_epoch,
                    cleanup_state,
                )?;
                bail!("root-owned worker cleanup remains unproved");
            }
            state.state_store.settle_worker_process(
                worker_instance_id,
                &session.session_id,
                worker_boot_epoch,
                "reaped",
                "root_owner_dropped",
            )?;
            state.state_store.terminalize_dedicated_session(
                &session.session_id,
                worker_instance_id,
                worker_boot_epoch,
                "cancelled",
            )?;
            finish_terminal_credential_cleanup(state, &session)?;
        }
        (None, None) if matches!(session.state.as_str(), "recovering" | "outcome_unknown") => {
            state
                .state_store
                .terminalize_unattached_dedicated_session(&session.session_id, "cancelled")?;
        }
        (None, None) => bail!("root-owned session has no worker and is not recoverable"),
        _ => bail!("root-owned session has a partial worker identity"),
    }
    close_session_workspace(state, &session)
}

fn close_session_workspace(state: &AppState, session: &DedicatedSessionRecord) -> Result<()> {
    let Some(record) = state
        .state_store
        .execution_workspace(&session.workspace_id)?
    else {
        return Ok(());
    };
    if record.state == WorkspaceState::Closed {
        return Ok(());
    }
    if !matches!(
        record.state,
        WorkspaceState::Ready
            | WorkspaceState::Active
            | WorkspaceState::Freezing
            | WorkspaceState::Destroying
            | WorkspaceState::Closing
    ) {
        bail!("session workspace cannot close from state {}", record.state);
    }
    let launch_owner = record
        .launch_owner
        .as_deref()
        .ok_or_else(|| anyhow!("session workspace has no launch owner"))?;
    let mut phase = record.state;
    if matches!(
        phase,
        WorkspaceState::Ready | WorkspaceState::Active | WorkspaceState::Freezing
    ) {
        state.state_store.transition_execution_workspace_owned(
            &record.workspace_id,
            &session.root_thread_id,
            launch_owner,
            &[phase],
            WorkspaceState::Destroying,
            None,
        )?;
        phase = WorkspaceState::Destroying;
    }
    let root = PathBuf::from(&record.root_path);
    if phase == WorkspaceState::Destroying {
        let destroyed = state
            .isolation
            .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy,
                workspace_id: &record.workspace_id,
                launch_owner,
                lower_snapshot: &record.lower_snapshot,
                lower_path: &root.join("lower"),
                upper_path: &root.join("upper"),
                work_path: &root.join("work"),
            })
            .map_err(|error| anyhow!(error.to_string()))?;
        let pinned =
            lillux::canonical_json(&serde_json::to_value(&destroyed.pinned_root_identities)?)?;
        if record.backend_id.as_deref() != Some(destroyed.backend_id.as_str())
            || record.backend_version.as_deref() != Some(destroyed.backend_version.as_str())
            || record.pinned_root_identities.as_deref() != Some(pinned.as_str())
            || record.mount_identity.as_deref() != Some(destroyed.mount_identity.as_str())
        {
            bail!("session workspace destroy evidence differs from its retained identity");
        }
        state.state_store.transition_execution_workspace_owned(
            &record.workspace_id,
            &session.root_thread_id,
            launch_owner,
            &[WorkspaceState::Destroying],
            WorkspaceState::Closing,
            None,
        )?;
    }
    crate::temp_dir_guard::TempDirGuard::new_workspace(root.clone(), root.join("project"))?
        .remove_now()?;
    state.state_store.transition_execution_workspace_owned(
        &record.workspace_id,
        &session.root_thread_id,
        launch_owner,
        &[WorkspaceState::Closing],
        WorkspaceState::Closed,
        None,
    )
}
