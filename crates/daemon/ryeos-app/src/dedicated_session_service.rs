//! Meaning-blind command delivery for one durable exclusive session.
//!
//! The integration runtime owns command bodies and observation meaning. This
//! service owns only the generic durable contact boundary, event/approval
//! ledgers, worker-epoch fencing, and cleanup proof consumption.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hosted_operation::{
    acquire_credential_profile_causal_contact_sync, acquire_credential_profile_contact,
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

const MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT: usize = 16;

fn validate_worker_observation_batch_shape(batch: &WorkerObservationBatch) -> Result<u64> {
    if batch.batch_digest.is_empty()
        || batch.count == 0
        || batch.count > 128
        || batch.events.len() != usize::try_from(batch.count)?
        || batch.session_observations.len()
            > batch
                .events
                .len()
                .saturating_mul(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT)
    {
        bail!("worker observation batch shape is invalid or unbounded");
    }
    batch
        .first_sequence
        .checked_add(batch.count - 1)
        .ok_or_else(|| anyhow!("worker observation sequence overflow"))
}

fn validate_session_observation_cardinality(result: &Value, limit: usize) -> Result<()> {
    let values = result
        .get("session_observations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker session observations are not a bounded array"))?;
    if values.len() > limit {
        bail!("worker emitted too many session observations for its admitted ingress");
    }
    Ok(())
}

fn pushed_observation_limit(result: &Value) -> Result<usize> {
    let event_count = result
        .get("events")
        .and_then(Value::as_array)
        .map(Vec::len)
        .filter(|count| *count > 0 && *count <= 128)
        .ok_or_else(|| anyhow!("pushed worker event batch is empty or unbounded"))?;
    let limit = event_count
        .checked_mul(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT)
        .ok_or_else(|| anyhow!("pushed worker observation limit overflow"))?;
    validate_session_observation_cardinality(result, limit)?;
    Ok(limit)
}

fn projection_signals() -> &'static Mutex<HashMap<String, Weak<tokio::sync::Notify>>> {
    static SIGNALS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Notify>>>> = OnceLock::new();
    SIGNALS.get_or_init(Default::default)
}

fn projection_signal(placement_thread_id: &str) -> Arc<tokio::sync::Notify> {
    let mut signals = projection_signals()
        .lock()
        .expect("dedicated projection signal map poisoned");
    signals.retain(|_, signal| signal.strong_count() != 0);
    if let Some(signal) = signals.get(placement_thread_id).and_then(Weak::upgrade) {
        return signal;
    }
    let signal = Arc::new(tokio::sync::Notify::new());
    signals.insert(placement_thread_id.to_owned(), Arc::downgrade(&signal));
    signal
}

fn transition_gates() -> &'static Mutex<HashMap<String, Weak<Mutex<()>>>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    GATES.get_or_init(Default::default)
}

/// Serialize validation, authoritative append, and projection of worker
/// lifecycle observations for one placement. Full-duplex worker I/O remains
/// concurrent; only the short state-acceptance commit is serialized.
fn transition_gate(placement_thread_id: &str) -> Arc<Mutex<()>> {
    let mut gates = transition_gates()
        .lock()
        .expect("dedicated transition gate map poisoned");
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(placement_thread_id).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(placement_thread_id.to_owned(), Arc::downgrade(&gate));
    gate
}

pub fn notify_projection_change(placement_thread_id: &str) {
    let signal = projection_signals()
        .lock()
        .expect("dedicated projection signal map poisoned")
        .get(placement_thread_id)
        .and_then(Weak::upgrade);
    if let Some(signal) = signal {
        signal.notify_waiters();
    }
}

pub async fn wait_for_projection_change(
    state: &AppState,
    placement_thread_id: &str,
    observed_updated_at_ms: i64,
    timeout: std::time::Duration,
) -> Result<DedicatedSessionRecord> {
    let signal = projection_signal(placement_thread_id);
    loop {
        let notified = signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let current = current_session(state, placement_thread_id)?;
        if current.updated_at_ms != observed_updated_at_ms || current.state == "terminal" {
            return Ok(current);
        }
        if tokio::time::timeout(timeout, notified).await.is_err() {
            return Ok(current_session(state, placement_thread_id)?);
        }
    }
}

/// Wait on the pushed dedicated-session projection signal until one exact
/// placement has a durable worker identity, reaches a terminal/recovery
/// boundary, or the caller's bounded deadline expires. The signal is armed
/// before every read, so attachment cannot be lost between observation and
/// sleep. This is the cross-component attachment seam; callers must still
/// validate the exact worker epoch and placement authority after it wakes.
pub async fn wait_for_worker_attachment_projection(
    state: &AppState,
    placement_thread_id: &str,
    timeout: std::time::Duration,
) -> Result<Option<DedicatedSessionRecord>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let signal = projection_signal(placement_thread_id);
        let notified = signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let current = state.state_store.dedicated_session(placement_thread_id)?;
        if current.as_ref().is_some_and(|session| {
            (session.worker_instance_id.is_some() && session.worker_boot_epoch.is_some())
                || matches!(
                    session.state.as_str(),
                    "terminal" | "recovering" | "outcome_unknown"
                )
        }) {
            return Ok(current);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return state.state_store.dedicated_session(placement_thread_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn wait_for_exact_approval_state(
    state: &AppState,
    placement_thread_id: &str,
    approval_id: &str,
    worker_boot_epoch: u64,
    request_digest: &str,
    reservation_token: &str,
    decision_digest: &str,
    approval_state: &str,
    timeout: std::time::Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let signal = projection_signal(placement_thread_id);
        let notified = signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if state.state_store.dedicated_approval_has_exact_state(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            request_digest,
            reservation_token,
            decision_digest,
            approval_state,
        )? {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return state.state_store.dedicated_approval_has_exact_state(
                placement_thread_id,
                approval_id,
                worker_boot_epoch,
                request_digest,
                reservation_token,
                decision_digest,
                approval_state,
            );
        }
    }
}

/// Ingest one worker-pushed observation batch. The caller is the generic
/// session transport, not the worker: no callback capability is delegated to
/// the App Server or any model-launched child.
pub fn ingest_observation_batch(
    state: &AppState,
    placement_thread_id: &str,
    worker_boot_epoch: u64,
    raw: Value,
) -> Result<Value> {
    if serde_json::to_vec(&raw)?.len()
        > ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
    {
        bail!("worker observation batch exceeds its exact serialized-byte ceiling");
    }
    let initial = current_session(state, placement_thread_id)?;
    let _root_operation =
        begin_hosted_root_operation(&state.state_store, &initial.placement_thread_id)?;
    let _credential_contact = acquire_credential_profile_causal_contact_sync(
        &initial.credential_profile_id,
        placement_thread_id,
    );
    let transition_gate = transition_gate(placement_thread_id);
    let _transition_guard = transition_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = current_session(state, placement_thread_id)?;
    if session.credential_profile_id != initial.credential_profile_id {
        bail!("dedicated session credential profile changed across contact admission");
    }
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
    if batch.batch_digest != supplied_digest {
        bail!("worker observation batch retained a contradictory digest");
    }
    let through_sequence = validate_worker_observation_batch_shape(&batch)?;
    let result = json!({
        "events": batch.events,
        "session_observations": batch.session_observations,
    });
    let exact_replay = state.state_store.exact_dedicated_observation_batch_exists(
        placement_thread_id,
        worker_boot_epoch,
        batch.first_sequence,
        through_sequence,
        batch.previous_digest.as_deref(),
        &batch.batch_digest,
        &result,
    )?;
    if !exact_replay {
        validate_new_state_transition_sequence_for_session(&session, worker_boot_epoch, &result)?;
    }
    let reservation = state.state_store.reserve_dedicated_observation_batch(
        placement_thread_id,
        worker_boot_epoch,
        batch.first_sequence,
        through_sequence,
        batch.previous_digest.as_deref(),
        &batch.batch_digest,
        &result,
    )?;
    if reservation == ObservationBatchReservation::AlreadySettled {
        return Ok(json!({
            "through_sequence": through_sequence,
            "batch_digest": batch.batch_digest,
        }));
    }
    if reservation == ObservationBatchReservation::RebuildProjection {
        if let Some(authoritative) = find_authoritative_batch(
            state,
            &session,
            worker_boot_epoch,
            &batch.batch_digest,
            batch.first_sequence,
            through_sequence,
        )? {
            let observation_limit = pushed_observation_limit(&authoritative)?;
            project_worker_events(state, &session, worker_boot_epoch, &authoritative)?;
            apply_worker_observations(
                state,
                placement_thread_id,
                worker_boot_epoch,
                &authoritative,
                observation_limit,
            )?;
            state.state_store.settle_dedicated_observation_batch(
                placement_thread_id,
                worker_boot_epoch,
                batch.first_sequence,
                &batch.batch_digest,
            )?;
        } else {
            append_authoritative_observation_batch(
                state,
                &session,
                worker_boot_epoch,
                &batch.batch_digest,
                batch.first_sequence,
                through_sequence,
                &result,
            )?;
        }
        notify_projection_change(placement_thread_id);
        return Ok(json!({
            "through_sequence":through_sequence,
            "batch_digest":batch.batch_digest,
            "projection_rebuilt":true,
        }));
    }
    let append = append_authoritative_observation_batch(
        state,
        &session,
        worker_boot_epoch,
        &batch.batch_digest,
        batch.first_sequence,
        through_sequence,
        &result,
    );
    if let Err(error) = append {
        state.state_store.mark_dedicated_observation_batch_unknown(
            placement_thread_id,
            worker_boot_epoch,
            batch.first_sequence,
            &batch.batch_digest,
        )?;
        return Err(error);
    }
    notify_projection_change(placement_thread_id);
    Ok(json!({
        "through_sequence": through_sequence,
        "batch_digest": batch.batch_digest,
    }))
}

fn hosted_observation_batch_operation_id(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    batch_digest: &str,
    first_sequence: u64,
    through_sequence: u64,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_observation_batch_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "worker_boot_epoch":worker_boot_epoch,
        "batch_digest":batch_digest,
        "first_sequence":first_sequence,
        "through_sequence":through_sequence,
    }))
}

fn append_authoritative_observation_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    batch_digest: &str,
    first_sequence: u64,
    through_sequence: u64,
    result: &Value,
) -> Result<()> {
    let observation_limit = pushed_observation_limit(result)?;
    validate_new_state_transition_sequence(
        state,
        &session.placement_thread_id,
        worker_boot_epoch,
        result,
    )?;
    let operation_id = hosted_observation_batch_operation_id(
        session,
        worker_boot_epoch,
        batch_digest,
        first_sequence,
        through_sequence,
    )?;
    let mut observation_events = result
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
                    "chain_root_id": session.chain_root_id.as_str(),
                    "placement_thread_id": session.placement_thread_id.as_str(),
                    "worker_boot_epoch": worker_boot_epoch,
                    "batch_digest": batch_digest,
                    "first_sequence": first_sequence,
                    "through_sequence": through_sequence,
                    "upstream_event_type": event.event_type,
                    "observation": event.payload,
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let transition_events = state_transition_fact_events(
        session,
        worker_boot_epoch,
        result,
        json!({
            "kind":"pushed_observation_batch",
            "batch_operation_id":operation_id,
            "batch_digest":batch_digest,
            "first_sequence":first_sequence,
            "through_sequence":through_sequence,
        }),
        None,
    )?;
    require_new_state_transition_facts(state, session, &transition_events)?;
    observation_events.extend(transition_events);
    crate::authoritative_root_fact::append_once_with_followups(
        state,
        &session.placement_thread_id,
        "hosted_worker_observation_batch",
        &operation_id,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "chain_root_id":session.chain_root_id.as_str(),
            "placement_thread_id":session.placement_thread_id.as_str(),
            "worker_boot_epoch":worker_boot_epoch,
            "batch_digest":batch_digest,
            "first_sequence":first_sequence,
            "through_sequence":through_sequence,
            "canonical_batch":result.clone(),
        }),
        &observation_events,
    )?;
    // The root event chain is the authority. Approval and session tables
    // are rebuildable correlation/projection ledgers and may advance only
    // after the authoritative append has durably succeeded.
    project_worker_events(state, session, worker_boot_epoch, result)?;
    apply_worker_observations(
        state,
        &session.placement_thread_id,
        worker_boot_epoch,
        result,
        observation_limit,
    )?;
    state.state_store.settle_dedicated_observation_batch(
        &session.placement_thread_id,
        worker_boot_epoch,
        first_sequence,
        batch_digest,
    )?;
    Ok(())
}

fn find_authoritative_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    batch_digest: &str,
    first_sequence: u64,
    through_sequence: u64,
) -> Result<Option<Value>> {
    let operation_id = hosted_observation_batch_operation_id(
        session,
        worker_boot_epoch,
        batch_digest,
        first_sequence,
        through_sequence,
    )?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_worker_observation_batch",
        &operation_id,
    )?;
    if fact.count > 1 {
        bail!("authoritative observation batch identity is duplicated");
    }
    fact.payload
        .map(|payload| {
            if payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
                && payload.get("chain_root_id").and_then(Value::as_str)
                    == Some(session.chain_root_id.as_str())
                && payload.get("placement_thread_id").and_then(Value::as_str)
                    == Some(session.placement_thread_id.as_str())
                && payload.get("worker_boot_epoch").and_then(Value::as_u64)
                    == Some(worker_boot_epoch)
                && payload.get("batch_digest").and_then(Value::as_str) == Some(batch_digest)
                && payload.get("first_sequence").and_then(Value::as_u64) == Some(first_sequence)
                && payload.get("through_sequence").and_then(Value::as_u64) == Some(through_sequence)
            {
                if payload.get("schema").and_then(Value::as_u64) != Some(1)
                    || payload.get("origin").and_then(Value::as_str) != Some("daemon_observed_io")
                {
                    bail!("authoritative observation batch identity is contradictory");
                }
                let batch = payload.get("canonical_batch").cloned().ok_or_else(|| {
                    anyhow!("authoritative observation batch has no canonical payload")
                })?;
                if !batch.get("events").is_some_and(Value::is_array)
                    || !batch
                        .get("session_observations")
                        .is_some_and(Value::is_array)
                {
                    bail!("authoritative observation batch body is malformed");
                }
                validate_authoritative_state_transition_facts(
                    state,
                    session,
                    worker_boot_epoch,
                    &batch,
                    json!({
                        "kind":"pushed_observation_batch",
                        "batch_operation_id":operation_id,
                        "batch_digest":batch_digest,
                        "first_sequence":first_sequence,
                        "through_sequence":through_sequence,
                    }),
                    None,
                )?;
                return Ok(batch);
            }
            bail!("authoritative observation batch identity is contradictory")
        })
        .transpose()
}

/// Repair pushed-observation projection outboxes during startup, after old
/// worker processes have been quiesced but before their retained epochs are
/// detached. The root chain decides whether an append happened; SQLite never
/// guesses across the append boundary.
pub fn reconcile_observation_outboxes(state: &AppState) -> Result<()> {
    for record in state.state_store.dedicated_observation_outbox_records()? {
        let session = current_session(state, &record.placement_thread_id)?;
        let root_operation = crate::hosted_operation::begin_hosted_root_operation_if_appendable(
            &state.state_store,
            &session.placement_thread_id,
        )?;
        let root_appendable = root_operation.is_some();
        let _credential_operation = root_appendable
            .then(|| acquire_credential_profile_operation_sync(&session.credential_profile_id));
        if let Some(authoritative) = find_authoritative_batch(
            state,
            &session,
            record.worker_boot_epoch,
            &record.batch_digest,
            record.first_sequence,
            record.through_sequence,
        )? {
            if root_appendable {
                let observation_limit = pushed_observation_limit(&authoritative)?;
                project_worker_events(state, &session, record.worker_boot_epoch, &authoritative)?;
                apply_worker_observations(
                    state,
                    &record.placement_thread_id,
                    record.worker_boot_epoch,
                    &authoritative,
                    observation_limit,
                )?;
            }
            state.state_store.settle_dedicated_observation_batch(
                &record.placement_thread_id,
                record.worker_boot_epoch,
                record.first_sequence,
                &record.batch_digest,
            )?;
            notify_projection_change(&record.placement_thread_id);
            continue;
        }

        if !root_appendable {
            bail!("terminal hosted root is missing a durably accepted observation batch fact");
        }
        if record.state == "append_contacting" {
            state.state_store.mark_dedicated_observation_batch_unknown(
                &record.placement_thread_id,
                record.worker_boot_epoch,
                record.first_sequence,
                &record.batch_digest,
            )?;
        }
        append_authoritative_observation_batch(
            state,
            &session,
            record.worker_boot_epoch,
            &record.batch_digest,
            record.first_sequence,
            record.through_sequence,
            &record.canonical_batch,
        )?;
        notify_projection_change(&record.placement_thread_id);
    }
    Ok(())
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
        outcome: String,
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

#[derive(Clone, Debug)]
struct HostedTurnStartAuthority {
    operation_id: String,
    chain_seq: i64,
    command_sequence: Option<u64>,
    request_digest: Option<String>,
}

fn validate_hosted_turn_id(label: &str, turn_id: &str) -> Result<()> {
    if turn_id.is_empty() || turn_id.len() > 256 || turn_id.chars().any(char::is_control) {
        bail!("{label} is not canonical and bounded");
    }
    Ok(())
}

fn hosted_turn_start_operation_id(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    turn_id: &str,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_turn_start.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "worker_boot_epoch":worker_boot_epoch,
        "turn_id":turn_id,
    }))
}

fn hosted_turn_completion_operation_id(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    turn_id: &str,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_turn_completion.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "worker_boot_epoch":worker_boot_epoch,
        "turn_id":turn_id,
    }))
}

fn validate_hosted_transition_source(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    source: &Value,
) -> Result<Option<(u64, String)>> {
    let object = source
        .as_object()
        .ok_or_else(|| anyhow!("hosted turn fact source is not an object"))?;
    match object.get("kind").and_then(Value::as_str) {
        Some("command_response") if object.len() == 4 => {
            let command_sequence = object
                .get("command_sequence")
                .and_then(Value::as_u64)
                .filter(|sequence| *sequence != 0)
                .ok_or_else(|| anyhow!("hosted turn command source has no sequence"))?;
            let request_digest = object
                .get("request_digest")
                .and_then(Value::as_str)
                .filter(|digest| lillux::valid_hash(digest))
                .ok_or_else(|| anyhow!("hosted turn command source has no request digest"))?;
            let expected = command_fact_operation_id(
                session,
                "hosted_worker_command_observation_batch",
                command_sequence,
                request_digest,
            )?;
            if object.get("batch_operation_id").and_then(Value::as_str) != Some(expected.as_str()) {
                bail!("hosted turn command source has a contradictory batch identity");
            }
            Ok(Some((command_sequence, request_digest.to_owned())))
        }
        Some("pushed_observation_batch") if object.len() == 5 => {
            let batch_digest = object
                .get("batch_digest")
                .and_then(Value::as_str)
                .filter(|digest| lillux::valid_hash(digest))
                .ok_or_else(|| anyhow!("hosted turn pushed source has no batch digest"))?;
            let first_sequence = object
                .get("first_sequence")
                .and_then(Value::as_u64)
                .filter(|sequence| *sequence != 0)
                .ok_or_else(|| anyhow!("hosted turn pushed source has no first sequence"))?;
            let through_sequence = object
                .get("through_sequence")
                .and_then(Value::as_u64)
                .filter(|sequence| *sequence >= first_sequence)
                .ok_or_else(|| anyhow!("hosted turn pushed source has no through sequence"))?;
            let expected = hosted_observation_batch_operation_id(
                session,
                worker_boot_epoch,
                batch_digest,
                first_sequence,
                through_sequence,
            )?;
            if object.get("batch_operation_id").and_then(Value::as_str) != Some(expected.as_str()) {
                bail!("hosted turn pushed source has a contradictory batch identity");
            }
            Ok(None)
        }
        _ => bail!("hosted turn fact source is not canonical"),
    }
}

fn hosted_turn_start_authority(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    turn_id: &str,
) -> Result<Option<HostedTurnStartAuthority>> {
    let operation_id = hosted_turn_start_operation_id(session, worker_boot_epoch, turn_id)?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_session.turn_started",
        &operation_id,
    )?;
    if fact.count > 1 {
        bail!("hosted turn start identity is duplicated in the root chain");
    }
    let Some(payload) = fact.payload else {
        return Ok(None);
    };
    let command_sequence = payload.get("command_sequence").and_then(Value::as_u64);
    let request_digest = payload
        .get("request_digest")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_coordinate = validate_hosted_transition_source(
        session,
        worker_boot_epoch,
        payload.get("source").unwrap_or(&Value::Null),
    )?;
    let command_coordinate_is_exact = match (
        command_sequence,
        request_digest.as_deref(),
        source_coordinate.as_ref(),
    ) {
        (Some(sequence), Some(digest), Some((source_sequence, source_digest))) => {
            lillux::valid_hash(digest) && sequence == *source_sequence && digest == source_digest
        }
        (None, None, None) => true,
        _ => false,
    };
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
        && payload.get("origin").and_then(Value::as_str)
            == Some("daemon_accepted_worker_observation")
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload.get("worker_boot_epoch").and_then(Value::as_u64) == Some(worker_boot_epoch)
        && payload.get("turn_id").and_then(Value::as_str) == Some(turn_id)
        && payload.get("expected").and_then(Value::as_str) == Some("idle")
        && payload.get("next").and_then(Value::as_str) == Some("turn_running")
        && command_coordinate_is_exact;
    if !exact {
        bail!("hosted turn start identity is bound to contradictory root testimony");
    }
    Ok(Some(HostedTurnStartAuthority {
        operation_id,
        chain_seq: fact
            .first_chain_seq
            .ok_or_else(|| anyhow!("hosted turn start has no root-chain coordinate"))?,
        command_sequence,
        request_digest,
    }))
}

fn state_transition_fact_events(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    result: &Value,
    source: Value,
    command_coordinate: Option<(u64, &str)>,
) -> Result<Vec<NewEventRecord>> {
    let values = result
        .get("session_observations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker session observations are not a bounded array"))?;
    let mut local_starts = std::collections::HashSet::<String>::new();
    let mut local_completions = std::collections::HashSet::<String>::new();
    let mut command_started_turn = None::<String>;
    let mut events = Vec::new();
    for value in values {
        let WorkerObservation::State {
            expected,
            next,
            turn_id,
            completed_turn_id,
        } = serde_json::from_value(value.clone())?
        else {
            continue;
        };
        match (expected.as_str(), next.as_str()) {
            ("idle", "turn_running") if completed_turn_id.is_none() => {
                let turn_id = turn_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("turn-start transition has no turn id"))?;
                validate_hosted_turn_id("turn-start id", turn_id)?;
                if command_coordinate.is_some()
                    && command_started_turn.replace(turn_id.to_owned()).is_some()
                {
                    bail!("one hosted command started more than one turn");
                }
                let operation_id =
                    hosted_turn_start_operation_id(session, worker_boot_epoch, turn_id)?;
                let (command_sequence, request_digest) = command_coordinate
                    .map(|(sequence, digest)| {
                        (
                            Some(Value::Number(sequence.into())),
                            Some(Value::String(digest.to_owned())),
                        )
                    })
                    .unwrap_or((None, None));
                let mut payload = json!({
                    "schema":1,
                    "operation_id":operation_id,
                    "origin":"daemon_accepted_worker_observation",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":session.placement_thread_id,
                    "worker_boot_epoch":worker_boot_epoch,
                    "turn_id":turn_id,
                    "expected":"idle",
                    "next":"turn_running",
                    "source":source,
                });
                if let Some(command_sequence) = command_sequence {
                    payload["command_sequence"] = command_sequence;
                }
                if let Some(request_digest) = request_digest {
                    payload["request_digest"] = request_digest;
                }
                if !local_starts.insert(turn_id.to_owned()) {
                    bail!("worker observation batch duplicated a hosted turn start");
                }
                events.push(NewEventRecord {
                    event_type: "hosted_session.turn_started".to_owned(),
                    storage_class: "indexed".to_owned(),
                    payload,
                });
            }
            ("turn_running", "idle") if turn_id.is_none() => {
                let completed_turn_id = completed_turn_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("turn-completion transition has no turn id"))?;
                validate_hosted_turn_id("completed turn id", completed_turn_id)?;
                if !local_completions.insert(completed_turn_id.to_owned()) {
                    bail!("worker observation batch duplicated a hosted turn completion");
                }
                let operation_id = hosted_turn_completion_operation_id(
                    session,
                    worker_boot_epoch,
                    completed_turn_id,
                )?;
                let start_operation_id =
                    hosted_turn_start_operation_id(session, worker_boot_epoch, completed_turn_id)?;
                let payload = json!({
                    "schema":1,
                    "operation_id":operation_id,
                    "origin":"daemon_accepted_worker_observation",
                    "chain_root_id":session.chain_root_id,
                    "placement_thread_id":session.placement_thread_id,
                    "worker_boot_epoch":worker_boot_epoch,
                    "turn_id":completed_turn_id,
                    "start_operation_id":start_operation_id,
                    "expected":"turn_running",
                    "next":"idle",
                    "source":source,
                });
                events.push(NewEventRecord {
                    event_type: "hosted_session.turn_completed".to_owned(),
                    storage_class: "indexed".to_owned(),
                    payload,
                });
            }
            _ => bail!("worker emitted an invalid generic session observation shape"),
        }
    }
    Ok(events)
}

/// Prove that every lifecycle transition in one newly observed batch is
/// admissible from the exact current placement projection before any of those
/// transitions become root testimony. The per-placement transition gate
/// keeps this read/simulation/append/apply sequence single-writer.
fn validate_new_state_transition_sequence(
    state: &AppState,
    placement_thread_id: &str,
    worker_boot_epoch: u64,
    result: &Value,
) -> Result<()> {
    let session = current_session(state, placement_thread_id)?;
    validate_new_state_transition_sequence_for_session(&session, worker_boot_epoch, result)
}

fn validate_new_state_transition_sequence_for_session(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    result: &Value,
) -> Result<()> {
    if session.worker_boot_epoch != Some(worker_boot_epoch) {
        bail!("worker lifecycle observation belongs to another boot epoch");
    }
    let mut projected_state = session.state.clone();
    let mut projected_turn_id = session.current_turn_id.clone();
    let values = result
        .get("session_observations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker session observations are not a bounded array"))?;
    for value in values {
        let WorkerObservation::State {
            expected,
            next,
            turn_id,
            completed_turn_id,
        } = serde_json::from_value(value.clone())?
        else {
            continue;
        };
        if projected_state != expected {
            bail!("worker lifecycle observation lost its exact predecessor state");
        }
        match (expected.as_str(), next.as_str()) {
            ("idle", "turn_running")
                if completed_turn_id.is_none()
                    && projected_turn_id.is_none()
                    && turn_id.is_some() =>
            {
                projected_state = next;
                projected_turn_id = turn_id;
            }
            ("turn_running", "idle")
                if turn_id.is_none()
                    && completed_turn_id.as_deref() == projected_turn_id.as_deref() =>
            {
                projected_state = next;
                projected_turn_id = None;
            }
            _ => bail!("worker emitted an invalid generic session observation shape"),
        }
    }
    Ok(())
}

fn validate_authoritative_state_transition_facts(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    result: &Value,
    source: Value,
    command_coordinate: Option<(u64, &str)>,
) -> Result<()> {
    for expected in state_transition_fact_events(
        session,
        worker_boot_epoch,
        result,
        source,
        command_coordinate,
    )? {
        let operation_id = expected
            .payload
            .get("operation_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("accepted hosted transition has no operation id"))?;
        let fact = crate::authoritative_root_fact::lookup(
            state,
            &session.placement_thread_id,
            &expected.event_type,
            operation_id,
        )?;
        if fact.count != 1 || fact.payload.as_ref() != Some(&expected.payload) {
            bail!("hosted worker batch has no exact daemon-accepted transition testimony");
        }
    }
    Ok(())
}

fn require_new_state_transition_facts(
    state: &AppState,
    session: &DedicatedSessionRecord,
    transitions: &[NewEventRecord],
) -> Result<()> {
    for transition in transitions {
        let operation_id = transition
            .payload
            .get("operation_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("accepted hosted transition has no operation id"))?;
        let existing = crate::authoritative_root_fact::lookup(
            state,
            &session.placement_thread_id,
            &transition.event_type,
            operation_id,
        )?;
        if existing.count != 0 || existing.payload.is_some() {
            bail!("worker lifecycle transition reuses prior accepted turn authority");
        }
    }
    Ok(())
}

fn apply_worker_observations(
    state: &AppState,
    placement_thread_id: &str,
    worker_boot_epoch: u64,
    result: &Value,
    observation_limit: usize,
) -> Result<()> {
    let Some(values) = result.get("session_observations") else {
        return Ok(());
    };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow!("worker session observations are not a bounded array"))?;
    validate_session_observation_cardinality(result, observation_limit)?;
    for value in values {
        match serde_json::from_value(value.clone())? {
            WorkerObservation::RemoteThread { id } => {
                let session = state
                    .state_store
                    .dedicated_session(placement_thread_id)?
                    .ok_or_else(|| anyhow!("dedicated session disappeared"))?;
                let worker_instance_id = session
                    .worker_instance_id
                    .ok_or_else(|| anyhow!("remote-thread observation has no attached worker"))?;
                state.state_store.bind_dedicated_remote_thread(
                    placement_thread_id,
                    &worker_instance_id,
                    worker_boot_epoch,
                    &id,
                )?;
            }
            WorkerObservation::RemoteThreadRecovered { id } => {
                state.state_store.observe_dedicated_remote_reattach(
                    placement_thread_id,
                    worker_boot_epoch,
                    &id,
                )?;
            }
            WorkerObservation::RemoteThreadRecoveryStatus { id, outcome } => {
                state.state_store.settle_dedicated_remote_recovery_status(
                    placement_thread_id,
                    worker_boot_epoch,
                    &id,
                    &outcome,
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
                    placement_thread_id,
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
                let session = current_session(state, placement_thread_id)?;
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
                let session = current_session(state, placement_thread_id)?;
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
                        placement_thread_id,
                        worker_instance_id,
                        worker_boot_epoch,
                        &account,
                    )?;
                }
            }
            WorkerObservation::CredentialEnrollmentCancelled { login_id } => {
                let session = current_session(state, placement_thread_id)?;
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
                    placement_thread_id,
                    &approval_id,
                    worker_boot_epoch,
                )?;
            }
        }
    }
    Ok(())
}

fn current_session(state: &AppState, placement_thread_id: &str) -> Result<DedicatedSessionRecord> {
    state
        .state_store
        .dedicated_session(placement_thread_id)?
        .ok_or_else(|| anyhow!("dedicated session disappeared"))
}

/// Complete canonical command testimony after restart from the durable
/// command outbox. This never contacts a worker. A possible-contact row is
/// reconciled only to outcome-unknown; it is never replayed.
pub fn reconcile_command_outboxes(state: &AppState) -> Result<()> {
    for record in state.state_store.dedicated_command_outbox_records()? {
        let session = current_session(state, &record.placement_thread_id)?;
        let root_operation = crate::hosted_operation::begin_hosted_root_operation_if_appendable(
            &state.state_store,
            &session.placement_thread_id,
        )?;
        if root_operation.is_none() {
            // A terminal chain cannot accept repair facts. Its existing facts
            // are nevertheless sufficient to classify every valid crash
            // boundary: committed without contacting is uncontacted;
            // contacting without a response batch is outcome-unknown; and a
            // response batch is an authoritative completed response. Do not
            // turn one historical session's unappendable projection repair
            // into a node-wide startup outage.
            if !committed_command_fact_exists(state, &session, &record)? {
                tracing::warn!(
                    placement_thread_id = %record.placement_thread_id,
                    command_sequence = record.command_sequence,
                    "terminal hosted root has an untestified uncontacted command reservation"
                );
                continue;
            }
            if matches!(record.state.as_str(), "dispatched" | "outcome_unknown") {
                if let Some((canonical_batch, response_digest)) =
                    find_authoritative_command_observation_batch(
                        state,
                        &session,
                        record.worker_boot_epoch,
                        record.command_sequence,
                        &record.request_digest,
                    )?
                {
                    // The batch and its projected events were admitted before
                    // this root became terminal. Replaying lifecycle
                    // observations now could resurrect or mutate authority for
                    // a dead worker epoch; repair only the historical command
                    // row from the terminal root's exact testimony.
                    let _ = canonical_batch;
                    state
                        .state_store
                        .settle_terminal_recovered_dedicated_command(
                            &record.placement_thread_id,
                            record.command_sequence,
                            record.worker_boot_epoch,
                            &json!({
                                "redacted":true,
                                "response_digest":response_digest,
                                "recovered_from_root_chain":true,
                            }),
                        )?;
                    notify_projection_change(&record.placement_thread_id);
                    continue;
                }
                let contacted = command_fact_exists(
                    state,
                    &session,
                    "hosted_command.contacting",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
                )?;
                if record.state == "dispatched" {
                    state.state_store.mark_dedicated_command_outcome_unknown(
                        &record.placement_thread_id,
                        record.command_sequence,
                        record.worker_boot_epoch,
                    )?;
                }
                tracing::warn!(
                    placement_thread_id = %record.placement_thread_id,
                    command_sequence = record.command_sequence,
                    contacted,
                    "terminal hosted root retains a command without a response batch"
                );
                continue;
            }
            let expected_terminal_fact = match record.state.as_str() {
                "committed" => None,
                "completed" | "failed" => Some(
                    if record.result.as_ref().is_some_and(|result| {
                        result.get("retryable_uncontacted").and_then(Value::as_bool) == Some(true)
                    }) {
                        "hosted_command.failed_uncontacted"
                    } else {
                        "hosted_command.settled"
                    },
                ),
                other => bail!("dedicated command outbox has invalid state `{other}`"),
            };
            if let Some(event_type) = expected_terminal_fact
                && !command_fact_exists(
                    state,
                    &session,
                    event_type,
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
                )?
            {
                bail!(
                    "terminal hosted command projection has no authoritative `{event_type}` fact"
                );
            }
            continue;
        }
        let _credential_operation =
            acquire_credential_profile_operation_sync(&session.credential_profile_id);
        // A contacted command must already have crossed the root-chain
        // committed boundary. When that exact fact exists, recovery derives
        // authority from it and does not introduce an unnecessary dependency
        // on mutable CAS availability before reading the response testimony.
        if !committed_command_fact_exists(state, &session, &record)? {
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
        }
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
                    let observation_limit = command_observation_limit(&record.command_kind)?;
                    validate_session_observation_cardinality(&canonical_batch, observation_limit)?;
                    project_worker_events(
                        state,
                        &session,
                        record.worker_boot_epoch,
                        &canonical_batch,
                    )?;
                    apply_worker_observations(
                        state,
                        &record.placement_thread_id,
                        record.worker_boot_epoch,
                        &canonical_batch,
                        observation_limit,
                    )?;
                    append_recovered_command_fact_once(
                        state,
                        &session,
                        "hosted_command.settled",
                        record.command_sequence,
                        &record.request_digest,
                        record.worker_boot_epoch,
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
                        &record.placement_thread_id,
                        record.command_sequence,
                        record.worker_boot_epoch,
                        &json!({
                            "redacted":true,
                            "response_digest":response_digest,
                            "recovered_from_root_chain":true,
                        }),
                    )?;
                    notify_projection_change(&record.placement_thread_id);
                    continue;
                }
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.contacting",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
                    json!({
                        "schema":1,
                        "origin":"daemon_observed_io",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "recovered":true,
                    }),
                )?;
                if record.state == "dispatched" {
                    state.state_store.mark_dedicated_command_outcome_unknown(
                        &record.placement_thread_id,
                        record.command_sequence,
                        record.worker_boot_epoch,
                    )?;
                }
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.outcome_unknown",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
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
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.settled",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
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
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.failed_uncontacted",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
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
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.settled",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
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
            event
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
            let observed_thread = event
                .payload
                .get("upstream_session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("approval event has no upstream-session correlation"))?;
            let observed_turn = event
                .payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("approval event has no operation correlation"))?;
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
                    placement_thread_id: &session.placement_thread_id,
                    approval_id: &approval_id,
                    worker_instance_id,
                    worker_boot_epoch,
                    request_digest,
                    operation_class,
                    requested_authority: &event.payload,
                    expires_at_ms: lillux::time::timestamp_millis() as i64 + 15 * 60 * 1000,
                })?;
        } else if event.event_type == "approval.expired" {
            let upstream_request_id = event
                .payload
                .get("request_id")
                .ok_or_else(|| anyhow!("expired approval event has no request id"))?;
            let request_digest = event
                .payload
                .get("request_digest")
                .and_then(Value::as_str)
                .filter(|digest| lillux::valid_hash(digest))
                .ok_or_else(|| anyhow!("expired approval event has no canonical request digest"))?;
            let observed_thread = event
                .payload
                .get("upstream_session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!("expired approval event has no upstream-session correlation")
                })?;
            let observed_turn = event
                .payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("expired approval event has no operation correlation"))?;
            if session.remote_thread_id.as_deref() != Some(observed_thread)
                || session.current_turn_id.as_deref() != Some(observed_turn)
            {
                bail!("expired approval event does not correlate to the retained thread and turn");
            }
            let approval_id = ryeos_state::objects::canonical_value_digest(&json!({
                "worker_boot_epoch":worker_boot_epoch,
                "upstream_request_id":upstream_request_id,
                "request_digest":request_digest,
            }))?;
            state
                .state_store
                .observe_dedicated_session_approval_expiry(
                    &session.placement_thread_id,
                    &approval_id,
                    worker_boot_epoch,
                    request_digest,
                )?;
        }
    }
    Ok(())
}

/// Execute one opaque integration-owned request across a durable at-most-once
/// contact boundary. The only privileged command class is the fixed generic
/// upstream-session recovery control; public route meaning remains opaque.
pub async fn execute_command(
    state: &AppState,
    placement_thread_id: &str,
    idempotency_key: &str,
    command_kind: &str,
    payload: Value,
) -> Result<Value> {
    let initial = current_session(state, placement_thread_id)?;
    let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind": command_kind,
        "payload": payload,
    }))?;
    // A settled duplicate is a read of retained authority, not a new hosted
    // operation. Resolve it before the appendability/credential/worker gates
    // so an exact retry remains available after the root is terminal without
    // reopening history or touching the old worker.
    if let Some(record) = state.state_store.settled_dedicated_session_command_replay(
        placement_thread_id,
        idempotency_key,
        command_kind,
        &request_digest,
        &payload,
    )? {
        if authoritative_settled_command_replay(state, &initial, &record)? {
            return Ok(json!({
                "command_sequence": record.command_sequence,
                "state": record.state,
                "result": record.result,
            }));
        }
    }
    let _root_operation =
        begin_hosted_root_operation(&state.state_store, &initial.placement_thread_id)?;
    let _credential_contact =
        acquire_credential_profile_contact(&initial.credential_profile_id, placement_thread_id)
            .await?;
    let session = current_session(state, placement_thread_id)?;
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| anyhow!("dedicated session has no attached worker"))?;
    let (protocol_profile_hash, protocol_schema_hashes) =
        structured_protocol_identity(state, &session.admitted_capsule_hash)?;
    let observation_limit = command_observation_limit(command_kind)?;
    let record =
        state
            .state_store
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                placement_thread_id,
                idempotency_key,
                worker_boot_epoch,
                command_kind,
                request_digest: &request_digest,
                payload: &payload,
            })?;
    match record.state.as_str() {
        "completed" | "failed" => {
            if !authoritative_settled_command_replay(state, &session, &record)? {
                bail!("settled command projection has no exact authoritative root testimony");
            }
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
    // Publish the exact possible-contact boundary before advancing the
    // rebuildable outbox projection to `dispatched`. If the root becomes
    // terminal while this append races it, the command remains provably
    // uncontacted and worker cleanup can fail it retryably. Advancing SQLite
    // first would create an outcome-unknown row whose authoritative root
    // could no longer testify to the boundary.
    append_command_fact_once(
        state,
        &session,
        "hosted_command.contacting",
        record.command_sequence,
        &request_digest,
        json!({
            "schema":1,
            "origin":"daemon_reserved_io",
            "worker_boot_epoch":worker_boot_epoch,
        }),
    )
    .context("persist command possible-contact boundary")?;
    state.state_store.mark_dedicated_command_contacted(
        placement_thread_id,
        record.command_sequence,
        worker_boot_epoch,
    )?;
    let pool = Arc::clone(&state.persistent_sessions);
    let execution_session_id = placement_thread_id.to_string();
    let is_runtime_recovery = command_kind == "reattach";
    let outcome = tokio::task::spawn_blocking(move || {
        if is_runtime_recovery {
            let recovery = payload
                .as_object()
                .ok_or_else(|| anyhow!("runtime recovery payload is not an object"))?;
            if recovery.len() != 1 {
                bail!("runtime recovery payload has an unknown field");
            }
            let upstream_session_id = recovery
                .get("upstream_session_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .ok_or_else(|| anyhow!("runtime recovery has no bounded upstream session id"))?;
            let body = json!({
                "kind":"runtime_recover",
                "upstream_session_id":upstream_session_id,
            });
            pool.execute_exclusive_control(&execution_session_id, body)
        } else {
            pool.execute_exclusive(&execution_session_id, payload, || false, |_| Ok(()))
        }
    })
    .await?;
    match outcome {
        Ok(result) => {
            let transition_gate = transition_gate(placement_thread_id);
            let _transition_guard = transition_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = validate_session_observation_cardinality(&result, observation_limit)
                .and_then(|()| {
                    validate_new_state_transition_sequence(
                        state,
                        placement_thread_id,
                        worker_boot_epoch,
                        &result,
                    )
                })
                .and_then(|()| {
                    append_command_observation_batch(
                        state,
                        &session,
                        worker_boot_epoch,
                        record.command_sequence,
                        &request_digest,
                        &result,
                    )
                })
                .and_then(|()| project_worker_events(state, &session, worker_boot_epoch, &result))
                .and_then(|()| {
                    apply_worker_observations(
                        state,
                        placement_thread_id,
                        worker_boot_epoch,
                        &result,
                        observation_limit,
                    )
                })
            {
                state.state_store.mark_dedicated_command_outcome_unknown(
                    placement_thread_id,
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
                placement_thread_id,
                record.command_sequence,
                worker_boot_epoch,
                true,
                &persisted_result,
            )?;
            notify_projection_change(placement_thread_id);
            Ok(json!({
                "command_sequence": record.command_sequence,
                "state": "completed",
                "result": result,
            }))
        }
        Err(error) => {
            let cleanup_state = state
                .persistent_sessions
                .take_exclusive_failure_cleanup_state(placement_thread_id)?
                .ok_or_else(|| anyhow!("exclusive worker failure lost its cleanup proof"))?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| anyhow!("failed command has no worker identity"))?;
            state.state_store.fence_abandoned_worker_process(
                worker_instance_id,
                placement_thread_id,
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
            notify_projection_change(placement_thread_id);
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

fn command_observation_limit(command_kind: &str) -> Result<usize> {
    match command_kind {
        "route" => Ok(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT),
        // Runtime recovery executes exactly the two routes frozen by the
        // structured-session admission compiler (resume, then inspect).
        "reattach" => MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT
            .checked_mul(2)
            .ok_or_else(|| anyhow!("recovery observation limit overflow")),
        other => bail!("dedicated command kind `{other}` is not admitted"),
    }
}

/// Testify the exact retained project generation on the still-running hosted
/// execution root before the mutable session projection may expose it for
/// validation or publication.
///
/// The workspace journal corroborates capture and recovery, but it is not the
/// durable authorization history. This idempotent root fact makes the
/// candidate identity, base, admitted capsule, credential generation, and
/// workspace owner reconstructable from the root chain.
pub fn append_candidate_capture_fact(
    state: &AppState,
    placement_thread_id: &str,
    candidate_snapshot_hash: &str,
) -> Result<()> {
    let root_operation = begin_hosted_root_operation(&state.state_store, placement_thread_id)?;
    append_candidate_capture_fact_under_lease(
        state,
        placement_thread_id,
        candidate_snapshot_hash,
        &root_operation,
    )
}

/// Variant for a caller that already holds the root lease across the complete
/// workspace-close → fact → projection-bind transaction.
pub fn append_candidate_capture_fact_under_lease(
    state: &AppState,
    placement_thread_id: &str,
    candidate_snapshot_hash: &str,
    _root_operation: &crate::hosted_operation::HostedRootOperationLease,
) -> Result<()> {
    if !lillux::valid_hash(candidate_snapshot_hash) {
        bail!("hosted candidate snapshot hash is not canonical");
    }
    let session = current_session(state, placement_thread_id)?;
    if session.placement_thread_id != placement_thread_id
        || !session.candidate_required
        || session.terminal_reason.as_deref() != Some("completed")
        || !matches!(session.state.as_str(), "freezing" | "frozen")
    {
        bail!("hosted candidate capture contradicts the dedicated-session lifecycle");
    }
    let thread = state
        .state_store
        .get_thread(&session.placement_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    if thread.status != "running" {
        bail!("hosted candidate capture requires a running root thread");
    }
    let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
        base_snapshot_hash,
        ..
    } = thread
        .project_authority
        .as_ref()
        .ok_or_else(|| anyhow!("hosted execution root has no project authority"))?
    else {
        bail!("hosted candidate capture requires pinned project authority");
    };
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_capture_operation.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "candidate_snapshot_hash":candidate_snapshot_hash,
    }))?;
    crate::authoritative_root_fact::append_once(
        state,
        &session.placement_thread_id,
        "hosted_candidate.captured",
        &operation_id,
        json!({
            "schema":1,
            "origin":"filesystem_verified",
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":session.placement_thread_id,
            "workspace_id":session.workspace_id,
            "candidate_snapshot_hash":candidate_snapshot_hash,
            "base_snapshot_hash":base_snapshot_hash,
            "admitted_capsule_hash":session.admitted_capsule_hash,
            "credential_profile_id":session.credential_profile_id,
            "credential_generation":session.credential_generation,
        }),
    )
}

fn append_command_fact_once(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    payload: Value,
) -> Result<()> {
    append_command_fact_once_with_followups(
        state,
        session,
        event_type,
        command_sequence,
        request_digest,
        payload,
        &[],
    )
}

fn command_fact_operation_id(
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":event_type,
    }))
}

fn append_command_fact_once_with_followups(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    mut payload: Value,
    followups: &[NewEventRecord],
) -> Result<()> {
    let operation_id =
        command_fact_operation_id(session, event_type, command_sequence, request_digest)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("hosted command fact payload is not an object"))?;
    object.insert(
        "chain_root_id".to_owned(),
        Value::String(session.chain_root_id.clone()),
    );
    object.insert(
        "placement_thread_id".to_owned(),
        Value::String(session.placement_thread_id.clone()),
    );
    object.insert(
        "command_sequence".to_owned(),
        Value::Number(command_sequence.into()),
    );
    object.insert(
        "request_digest".to_owned(),
        Value::String(request_digest.to_owned()),
    );
    crate::authoritative_root_fact::append_once_with_followups(
        state,
        &session.placement_thread_id,
        event_type,
        &operation_id,
        payload,
        followups,
    )
    .map(|_| ())
}

/// Startup recovery completes a missing fact but never rewrites testimony that
/// already crossed the authoritative root boundary. The live and recovered
/// payloads may legitimately differ in diagnostic recovery fields; the stable
/// operation identity, command digest, and worker epoch remain exact.
fn append_recovered_command_fact_once(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    worker_boot_epoch: u64,
    payload: Value,
) -> Result<()> {
    if command_fact_exists(
        state,
        session,
        event_type,
        command_sequence,
        request_digest,
        worker_boot_epoch,
    )? {
        return Ok(());
    }
    append_command_fact_once(
        state,
        session,
        event_type,
        command_sequence,
        request_digest,
        payload,
    )
}

fn command_fact_exists(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    worker_boot_epoch: u64,
) -> Result<bool> {
    Ok(command_fact_payload(
        state,
        session,
        event_type,
        command_sequence,
        request_digest,
        worker_boot_epoch,
    )?
    .is_some())
}

fn command_fact_payload(
    state: &AppState,
    session: &DedicatedSessionRecord,
    event_type: &str,
    command_sequence: u64,
    request_digest: &str,
    worker_boot_epoch: u64,
) -> Result<Option<Value>> {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":event_type,
    }))?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        event_type,
        &operation_id,
    )?;
    if fact.count > 1 {
        bail!("hosted command operation is duplicated in the root chain");
    }
    if let Some(payload) = fact.payload {
        let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
            && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
            && payload.get("chain_root_id").and_then(Value::as_str)
                == Some(session.chain_root_id.as_str())
            && payload.get("placement_thread_id").and_then(Value::as_str)
                == Some(session.placement_thread_id.as_str())
            && payload.get("command_sequence").and_then(Value::as_u64) == Some(command_sequence)
            && payload.get("request_digest").and_then(Value::as_str) == Some(request_digest)
            && payload.get("worker_boot_epoch").and_then(Value::as_u64) == Some(worker_boot_epoch);
        if !exact {
            bail!("hosted command operation id is bound to contradictory root testimony");
        }
        return Ok(Some(payload));
    }
    Ok(None)
}

fn authoritative_settled_command_replay(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<bool> {
    if !matches!(record.state.as_str(), "completed" | "failed") {
        return Ok(false);
    }
    if !committed_command_fact_exists(state, session, record)? {
        return Ok(false);
    }
    let retryable_uncontacted = record.result.as_ref().is_some_and(|result| {
        result.get("retryable_uncontacted").and_then(Value::as_bool) == Some(true)
    });
    if retryable_uncontacted {
        if record.state != "failed" {
            bail!("retryable uncontacted command projection is not failed");
        }
        let expected_result = json!({
            "error":"worker epoch ended before contact",
            "retryable_uncontacted":true,
        });
        if record.result.as_ref() != Some(&expected_result) {
            bail!("retryable uncontacted command projection has a contradictory result");
        }
        let fact = command_fact_payload(
            state,
            session,
            "hosted_command.failed_uncontacted",
            record.command_sequence,
            &record.request_digest,
            record.worker_boot_epoch,
        )?;
        return Ok(fact.is_some_and(|payload| {
            payload.get("origin").and_then(Value::as_str) == Some("daemon_verified_process")
                && payload
                    .get("retryable_uncontacted")
                    .and_then(Value::as_bool)
                    == Some(true)
        }));
    }

    let result = record.result.as_ref().unwrap_or(&Value::Null);
    let redacted = result.get("redacted").and_then(Value::as_bool) == Some(true);
    let recovered_from_root_chain = result
        .get("recovered_from_root_chain")
        .and_then(Value::as_bool)
        == Some(true);
    let projected_response_digest = if redacted {
        let object = result
            .as_object()
            .ok_or_else(|| anyhow!("redacted command projection is not an object"))?;
        let exact_shape = object.len() == 2
            || (object.len() == 3
                && object
                    .get("recovered_from_root_chain")
                    .and_then(Value::as_bool)
                    == Some(true));
        if !exact_shape
            || !object.contains_key("redacted")
            || !object.contains_key("response_digest")
        {
            bail!("redacted command projection has a contradictory shape");
        }
        result
            .get("response_digest")
            .and_then(Value::as_str)
            .filter(|digest| lillux::valid_hash(digest))
            .ok_or_else(|| anyhow!("redacted command projection has no response digest"))?
            .to_owned()
    } else {
        ryeos_state::objects::canonical_value_digest(result)?
    };
    if let Some(payload) = command_fact_payload(
        state,
        session,
        "hosted_command.settled",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )? {
        let exact = payload.get("origin").and_then(Value::as_str) == Some("daemon_observed_io")
            && payload.get("response_digest").and_then(Value::as_str)
                == Some(projected_response_digest.as_str())
            && payload.get("succeeded").and_then(Value::as_bool)
                == Some(record.state == "completed");
        if !exact {
            bail!("settled command projection contradicts authoritative root testimony");
        }
        return Ok(true);
    }
    if record.state != "completed" {
        return Ok(false);
    }
    if !redacted || !recovered_from_root_chain {
        return Ok(false);
    }
    Ok(find_authoritative_command_observation_batch(
        state,
        session,
        record.worker_boot_epoch,
        record.command_sequence,
        &record.request_digest,
    )?
    .is_some_and(|(_, response_digest)| response_digest == projected_response_digest))
}

fn committed_command_fact_exists(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<bool> {
    if !command_fact_exists(
        state,
        session,
        "hosted_command.committed",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )? {
        return Ok(false);
    }
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "command_sequence":record.command_sequence,
        "request_digest":record.request_digest,
        "event_type":"hosted_command.committed",
    }))?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_command.committed",
        &operation_id,
    )?;
    if fact.count != 1 {
        bail!("hosted command fact disappeared or duplicated during authoritative lookup");
    }
    let payload = fact
        .payload
        .ok_or_else(|| anyhow!("authoritative hosted command fact has no canonical payload"))?;
    let route_matches = match record.payload.get("route_id").and_then(Value::as_str) {
        Some(route_id) => payload.get("route_id").and_then(Value::as_str) == Some(route_id),
        None => payload.get("route_id").is_some_and(Value::is_null),
    };
    let (protocol_profile_hash, protocol_schema_hashes) =
        structured_protocol_identity(state, &session.admitted_capsule_hash)?;
    let protocol_schema_hashes = serde_json::to_value(protocol_schema_hashes)?;
    let exact = payload.get("origin").and_then(Value::as_str) == Some("daemon_observed_io")
        && payload.get("worker_boot_epoch").and_then(Value::as_u64)
            == Some(record.worker_boot_epoch)
        && payload.get("command_kind").and_then(Value::as_str)
            == Some(record.command_kind.as_str())
        && route_matches
        && payload.get("idempotency_key").and_then(Value::as_str)
            == Some(record.idempotency_key.as_str())
        && payload.get("canonical_command") == Some(&record.payload)
        && payload
            .get("admitted_session_capsule_hash")
            .and_then(Value::as_str)
            == Some(session.admitted_capsule_hash.as_str())
        && payload.get("protocol_profile_hash").and_then(Value::as_str)
            == Some(protocol_profile_hash.as_str())
        && payload.get("protocol_schema_hashes") == Some(&protocol_schema_hashes);
    if !exact {
        bail!("authoritative hosted command fact does not retain its exact command contract");
    }
    Ok(true)
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
    let batch_operation_id = command_fact_operation_id(
        session,
        "hosted_worker_command_observation_batch",
        command_sequence,
        request_digest,
    )?;
    let transitions = state_transition_fact_events(
        session,
        worker_boot_epoch,
        result,
        json!({
            "kind":"command_response",
            "batch_operation_id":batch_operation_id,
            "command_sequence":command_sequence,
            "request_digest":request_digest,
        }),
        Some((command_sequence, request_digest)),
    )?;
    require_new_state_transition_facts(state, session, &transitions)?;
    append_command_fact_once_with_followups(
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
        &transitions,
    )
}

fn find_authoritative_command_observation_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    command_sequence: u64,
    request_digest: &str,
) -> Result<Option<(Value, String)>> {
    let operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_command_fact.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "command_sequence":command_sequence,
        "request_digest":request_digest,
        "event_type":"hosted_worker_command_observation_batch",
    }))?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_worker_command_observation_batch",
        &operation_id,
    )?;
    if fact.count > 1 {
        bail!("authoritative command batch identity is duplicated");
    }
    let Some(payload) = fact.payload else {
        return Ok(None);
    };
    if payload.get("chain_root_id").and_then(Value::as_str) != Some(session.chain_root_id.as_str())
        || payload.get("placement_thread_id").and_then(Value::as_str)
            != Some(session.placement_thread_id.as_str())
        || payload.get("worker_boot_epoch").and_then(Value::as_u64) != Some(worker_boot_epoch)
        || payload.get("command_sequence").and_then(Value::as_u64) != Some(command_sequence)
        || payload.get("request_digest").and_then(Value::as_str) != Some(request_digest)
    {
        bail!("authoritative command batch identity is contradictory");
    }
    if payload.get("operation_id").and_then(Value::as_str) != Some(operation_id.as_str())
        || payload.get("schema").and_then(Value::as_u64) != Some(1)
        || payload.get("origin").and_then(Value::as_str) != Some("daemon_observed_io")
    {
        bail!("authoritative command batch identity is contradictory");
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
    validate_authoritative_state_transition_facts(
        state,
        session,
        worker_boot_epoch,
        &batch,
        json!({
            "kind":"command_response",
            "batch_operation_id":operation_id,
            "command_sequence":command_sequence,
            "request_digest":request_digest,
        }),
        Some((command_sequence, request_digest)),
    )?;
    Ok(Some((batch, response_digest.to_owned())))
}

fn hosted_turn_completion_payload(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    turn_id: &str,
) -> Result<Option<(String, Value)>> {
    let operation_id = hosted_turn_completion_operation_id(session, worker_boot_epoch, turn_id)?;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_session.turn_completed",
        &operation_id,
    )?;
    if fact.count > 1 {
        bail!("hosted turn completion identity is duplicated in the root chain");
    }
    let Some(payload) = fact.payload else {
        return Ok(None);
    };
    let completion_chain_seq = fact
        .first_chain_seq
        .ok_or_else(|| anyhow!("hosted turn completion has no root-chain coordinate"))?;
    let start = hosted_turn_start_authority(state, session, worker_boot_epoch, turn_id)?
        .ok_or_else(|| anyhow!("hosted turn completion has no matching accepted start"))?;
    validate_hosted_transition_source(
        session,
        worker_boot_epoch,
        payload.get("source").unwrap_or(&Value::Null),
    )?;
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str) == Some(operation_id.as_str())
        && payload.get("origin").and_then(Value::as_str)
            == Some("daemon_accepted_worker_observation")
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(session.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(session.placement_thread_id.as_str())
        && payload.get("worker_boot_epoch").and_then(Value::as_u64) == Some(worker_boot_epoch)
        && payload.get("turn_id").and_then(Value::as_str) == Some(turn_id)
        && payload.get("start_operation_id").and_then(Value::as_str)
            == Some(start.operation_id.as_str())
        && payload.get("expected").and_then(Value::as_str) == Some("turn_running")
        && payload.get("next").and_then(Value::as_str) == Some("idle")
        && start.chain_seq < completion_chain_seq;
    if !exact {
        bail!("hosted turn completion identity is bound to contradictory root testimony");
    }
    Ok(Some((operation_id, payload)))
}

/// Project one exact placement-local command and the asynchronous turn, if
/// any, that its authoritative response started. SQLite selects the bounded
/// coordinate; immutable placement-thread facts grant all returned authority.
pub fn command_observation(
    state: &AppState,
    placement_thread_id: &str,
    command_sequence: u64,
) -> Result<Value> {
    let session = state
        .state_store
        .dedicated_session(placement_thread_id)?
        .ok_or_else(|| anyhow!("dedicated session is not admitted"))?;
    let record = state
        .state_store
        .dedicated_session_command(placement_thread_id, command_sequence)?
        .ok_or_else(|| anyhow!("dedicated session command does not exist"))?;
    if record.placement_thread_id != session.placement_thread_id
        || record.command_sequence != command_sequence
    {
        bail!("dedicated command projection contradicts its requested coordinate");
    }
    let committed = command_fact_payload(
        state,
        &session,
        "hosted_command.committed",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )?
    .ok_or_else(|| anyhow!("dedicated command has no authoritative committed fact"))?;
    if !committed_command_fact_exists(state, &session, &record)? {
        bail!("dedicated command committed fact is not authoritative");
    }
    if !matches!(record.state.as_str(), "completed" | "failed") {
        bail!("dedicated command is not authoritatively settled");
    }
    if !authoritative_settled_command_replay(state, &session, &record)? {
        bail!("settled command projection has no exact authoritative root testimony");
    }
    let route_id = committed.get("route_id").cloned().unwrap_or(Value::Null);
    let mut result = json!({
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "admitted_capsule_hash":session.admitted_capsule_hash,
        "worker_boot_epoch":record.worker_boot_epoch,
        "command_sequence":record.command_sequence,
        "command_kind":record.command_kind,
        "idempotency_key":record.idempotency_key,
        "route_id":route_id,
        "request_digest":record.request_digest,
        "command_state":record.state,
        "operation":Value::Null,
    });
    if record.state != "completed" {
        return Ok(result);
    }
    let Some((batch, response_digest)) = find_authoritative_command_observation_batch(
        state,
        &session,
        record.worker_boot_epoch,
        record.command_sequence,
        &record.request_digest,
    )?
    else {
        bail!("completed command has no authoritative observation batch");
    };
    result["response_digest"] = Value::String(response_digest);
    let mut turn_ids = Vec::new();
    for value in batch
        .get("session_observations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("authoritative command observation batch is malformed"))?
    {
        if let WorkerObservation::State {
            expected,
            next,
            turn_id,
            completed_turn_id,
        } = serde_json::from_value(value.clone())?
        {
            if expected == "idle" && next == "turn_running" && completed_turn_id.is_none() {
                let turn_id =
                    turn_id.ok_or_else(|| anyhow!("authoritative turn start has no turn id"))?;
                validate_hosted_turn_id("authoritative turn-start id", &turn_id)?;
                turn_ids.push(turn_id);
            }
        }
    }
    if turn_ids.len() > 1 {
        bail!("one hosted command started more than one turn");
    }
    let Some(turn_id) = turn_ids.pop() else {
        return Ok(result);
    };
    let start =
        hosted_turn_start_authority(state, &session, record.worker_boot_epoch, &turn_id)?
            .ok_or_else(|| anyhow!("command-started turn has no exact authoritative start fact"))?;
    if start.command_sequence != Some(record.command_sequence)
        || start.request_digest.as_deref() != Some(record.request_digest.as_str())
    {
        bail!("hosted turn start is bound to another command coordinate");
    }
    let completion =
        hosted_turn_completion_payload(state, &session, record.worker_boot_epoch, &turn_id)?;
    let (state_name, completion_operation_id, completion_source) = match completion {
        Some((operation_id, payload)) => (
            "completed",
            Some(operation_id),
            payload.get("source").cloned().unwrap_or(Value::Null),
        ),
        None => ("running", None, Value::Null),
    };
    result["operation"] = json!({
        "kind":"turn",
        "id":turn_id,
        "state":state_name,
        "start_operation_id":start.operation_id,
        "completion_operation_id":completion_operation_id.clone(),
        "completion_source":completion_source,
    });
    if let Some(completion_operation_id) = completion_operation_id {
        result["completion_fence"] = serde_json::to_value(HostedCommandCompletionFence {
            placement_thread_id: session.placement_thread_id,
            admitted_capsule_hash: session.admitted_capsule_hash,
            worker_boot_epoch: record.worker_boot_epoch,
            command_sequence: record.command_sequence,
            request_digest: record.request_digest,
            turn_id,
            completion_operation_id,
        })?;
    }
    Ok(result)
}

/// Retire one exact durable worker identity without treating registry absence
/// as process-death proof. The process identity is the final authority when
/// the in-memory registry cannot prove that it reaped the owned group.
pub fn retire_worker_process(
    state: &AppState,
    placement_thread_id: &str,
    worker: &WorkerProcessRecord,
) -> Result<&'static str> {
    let registry_outcome = state
        .persistent_sessions
        .retire_exclusive(placement_thread_id)?;
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCommandCompletionFence {
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub worker_boot_epoch: u64,
    pub command_sequence: u64,
    pub request_digest: String,
    pub turn_id: String,
    pub completion_operation_id: String,
}

fn validate_hosted_command_completion_fence(
    state: &AppState,
    session: &DedicatedSessionRecord,
    fence: &HostedCommandCompletionFence,
) -> Result<()> {
    if fence.placement_thread_id != session.placement_thread_id
        || fence.admitted_capsule_hash != session.admitted_capsule_hash
        || fence.command_sequence == 0
        || !lillux::valid_hash(&fence.request_digest)
        || !lillux::valid_hash(&fence.completion_operation_id)
    {
        bail!("completed termination fence differs from the current hosted placement");
    }
    validate_hosted_turn_id("completed termination turn id", &fence.turn_id)?;
    let observation =
        command_observation(state, &fence.placement_thread_id, fence.command_sequence)?;
    let operation = observation
        .get("operation")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("completed termination command did not start a turn"))?;
    let exact = observation.get("chain_root_id").and_then(Value::as_str)
        == Some(session.chain_root_id.as_str())
        && observation
            .get("admitted_capsule_hash")
            .and_then(Value::as_str)
            == Some(fence.admitted_capsule_hash.as_str())
        && observation.get("worker_boot_epoch").and_then(Value::as_u64)
            == Some(fence.worker_boot_epoch)
        && observation.get("command_sequence").and_then(Value::as_u64)
            == Some(fence.command_sequence)
        && observation.get("request_digest").and_then(Value::as_str)
            == Some(fence.request_digest.as_str())
        && operation.get("id").and_then(Value::as_str) == Some(fence.turn_id.as_str())
        && operation.get("state").and_then(Value::as_str) == Some("completed")
        && operation
            .get("completion_operation_id")
            .and_then(Value::as_str)
            == Some(fence.completion_operation_id.as_str());
    if !exact {
        bail!("completed termination fence has no exact authoritative turn completion");
    }
    Ok(())
}

/// Drain and terminally settle one session after its caller has already
/// proved owner/root authority. This is shared by authenticated services and
/// the callback-owned controller so duration expiry cannot orphan a worker.
pub async fn terminate_session(
    state: &AppState,
    placement_thread_id: &str,
    reason: &str,
    completion_fence: Option<&HostedCommandCompletionFence>,
) -> Result<Value> {
    if !matches!(reason, "completed" | "cancelled") {
        bail!("terminal reason must be completed or cancelled");
    }
    if reason == "cancelled" && completion_fence.is_some() {
        bail!("cancelled termination cannot carry a completed-command fence");
    }
    let initial = current_session(state, placement_thread_id)?;
    let root_operation = crate::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        &initial.placement_thread_id,
    )?;
    let _credential_operation =
        acquire_credential_profile_operation(&initial.credential_profile_id).await?;
    let session = current_session(state, placement_thread_id)?;
    if session.state == "terminal" {
        if session.terminal_reason.as_deref() != Some(reason) {
            bail!("terminal session reason conflicts with the requested retry");
        }
        if let Some(fence) = completion_fence {
            validate_hosted_command_completion_fence(state, &session, fence)?;
            state.state_store.require_dedicated_session_route_frontier(
                placement_thread_id,
                fence.command_sequence,
            )?;
        }
        finish_terminal_credential_cleanup(state, &session)?;
        notify_projection_change(placement_thread_id);
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "state":"terminal",
            "reason":reason,
            "idempotent":true,
        }));
    }
    let _root_operation = root_operation
        .ok_or_else(|| anyhow!("nonterminal session has a terminal hosted execution root"))?;
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
            .terminalize_unattached_dedicated_session(placement_thread_id, reason)?;
        notify_projection_change(placement_thread_id);
        return Ok(json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
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
    if let Some(fence) = completion_fence {
        validate_hosted_command_completion_fence(state, &session, fence)?;
    }
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
        state.state_store.reserve_dedicated_session_completion(
            placement_thread_id,
            worker_boot_epoch,
            completion_fence.map(|fence| fence.command_sequence),
        )?;
    }
    let worker = state
        .state_store
        .worker_process(worker_instance_id)?
        .ok_or_else(|| anyhow!("dedicated worker process projection disappeared"))?;
    if worker.state != WorkerProcessState::Dead || worker.cleanup_state != "reaped" {
        let cleanup_state = retire_worker_process(state, placement_thread_id, &worker)?;
        if cleanup_state != "reaped" {
            state.state_store.fence_abandoned_worker_process(
                worker_instance_id,
                placement_thread_id,
                worker_boot_epoch,
                cleanup_state,
            )?;
            bail!("dedicated worker cleanup remains unproved");
        }
    }
    let after_retire = current_session(state, placement_thread_id)?;
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
            placement_thread_id,
            worker_boot_epoch,
            "reaped",
            reason,
        )?;
    }
    let after_settle = current_session(state, placement_thread_id)?;
    if after_settle.state == "recovering" {
        state.state_store.terminalize_dedicated_session(
            placement_thread_id,
            worker_instance_id,
            worker_boot_epoch,
            reason,
        )?;
    } else if reason != "completed" && after_settle.state != "terminal" {
        bail!("cancelled termination cannot override a retained candidate disposition");
    }
    finish_terminal_credential_cleanup(state, &session)?;
    let terminal = current_session(state, placement_thread_id)?;
    notify_projection_change(placement_thread_id);
    Ok(json!({
        "chain_root_id":terminal.chain_root_id,
        "placement_thread_id":placement_thread_id,
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
    if worker.placement_thread_id != session.placement_thread_id
        || worker.boot_epoch != worker_boot_epoch
    {
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

/// Node-owned owner-drop cancellation path used by the root execution guard.
/// It does not depend on the cooperative controller still being alive.
pub fn abort_session_for_root_stop(state: &AppState, placement_thread_id: &str) -> Result<()> {
    let Some(session) = state.state_store.dedicated_session(placement_thread_id)? else {
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
            .cancel_dedicated_candidate_for_root_stop(&session.placement_thread_id)?;
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
                    retire_worker_process(state, &session.placement_thread_id, &worker)?
                };
            if cleanup_state != "reaped" {
                state.state_store.fence_abandoned_worker_process(
                    worker_instance_id,
                    &session.placement_thread_id,
                    worker_boot_epoch,
                    cleanup_state,
                )?;
                bail!("root-owned worker cleanup remains unproved");
            }
            state.state_store.settle_worker_process(
                worker_instance_id,
                &session.placement_thread_id,
                worker_boot_epoch,
                "reaped",
                "root_owner_dropped",
            )?;
            state.state_store.terminalize_dedicated_session(
                &session.placement_thread_id,
                worker_instance_id,
                worker_boot_epoch,
                "cancelled",
            )?;
            finish_terminal_credential_cleanup(state, &session)?;
        }
        (None, None) if matches!(session.state.as_str(), "recovering" | "outcome_unknown") => {
            state.state_store.terminalize_unattached_dedicated_session(
                &session.placement_thread_id,
                "cancelled",
            )?;
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
            &session.placement_thread_id,
            launch_owner,
            &[phase],
            WorkspaceState::Destroying,
            None,
        )?;
        phase = WorkspaceState::Destroying;
    }
    let root = PathBuf::from(&record.root_path);
    let layout = ryeos_engine::execution_workspace::WorkspaceLayout::from_root(root.clone());
    if phase == WorkspaceState::Destroying {
        let destroyed = state
            .isolation
            .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy,
                workspace_id: &record.workspace_id,
                launch_owner,
                base_snapshot: &record.base_snapshot,
                project_path: &layout.project,
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
            &session.placement_thread_id,
            launch_owner,
            &[WorkspaceState::Destroying],
            WorkspaceState::Closing,
            None,
        )?;
    }
    crate::temp_dir_guard::TempDirGuard::new_workspace(root.clone(), layout.project)?
        .remove_now()?;
    state.state_store.transition_execution_workspace_owned(
        &record.workspace_id,
        &session.placement_thread_id,
        launch_owner,
        &[WorkspaceState::Closing],
        WorkspaceState::Closed,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_fixture() -> DedicatedSessionRecord {
        DedicatedSessionRecord {
            placement_thread_id: "T-placement".to_owned(),
            chain_root_id: "T-root".to_owned(),
            owner_principal: "fp:owner".to_owned(),
            admitted_capsule_hash: "a".repeat(64),
            worker_instance_id: Some("worker-one".to_owned()),
            worker_boot_epoch: Some(3),
            workspace_id: "W-one".to_owned(),
            candidate_required: false,
            credential_profile_id: "P-one".to_owned(),
            credential_generation: 1,
            remote_thread_id: Some("upstream-thread".to_owned()),
            current_turn_id: None,
            state: "idle".to_owned(),
            send_boundary: "settled".to_owned(),
            candidate_snapshot_hash: None,
            candidate_validation_hash: None,
            publication_result: None,
            terminal_reason: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn batch_with_observation_count(count: usize) -> WorkerObservationBatch {
        WorkerObservationBatch {
            first_sequence: 1,
            count: 1,
            previous_digest: None,
            batch_digest: "a".repeat(64),
            events: vec![json!({"sequence": 1})],
            session_observations: (0..count)
                .map(|index| json!({"kind": "fixture", "index": index}))
                .collect(),
        }
    }

    #[test]
    fn observation_shape_matches_the_admitted_per_event_cardinality() {
        let admitted = batch_with_observation_count(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT);
        assert_eq!(
            validate_worker_observation_batch_shape(&admitted).unwrap(),
            1
        );

        let excessive = batch_with_observation_count(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT + 1);
        assert!(validate_worker_observation_batch_shape(&excessive).is_err());

        let multi_event = json!({
            "events":[
                {"event_type":"fixture.first","payload":{}},
                {"event_type":"fixture.second","payload":{}},
            ],
            "session_observations":(0..(MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT + 1))
                .map(|index| json!({"kind":"fixture","index":index}))
                .collect::<Vec<_>>(),
        });
        assert_eq!(
            pushed_observation_limit(&multi_event).unwrap(),
            MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT * 2
        );
        assert_eq!(
            command_observation_limit("route").unwrap(),
            MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT
        );
        assert_eq!(
            command_observation_limit("reattach").unwrap(),
            MAX_SESSION_OBSERVATIONS_PER_WORKER_EVENT * 2
        );
    }

    #[test]
    fn fast_turn_gets_exact_start_and_completion_facts_from_one_command_batch() {
        let session = session_fixture();
        let request_digest = "b".repeat(64);
        let facts = state_transition_fact_events(
            &session,
            3,
            &json!({
                "events":[{"event_type":"turn.completed","payload":{"turn_id":"turn-one"}}],
                "session_observations":[
                    {
                        "kind":"state",
                        "expected":"idle",
                        "next":"turn_running",
                        "turn_id":"turn-one",
                    },
                    {
                        "kind":"state",
                        "expected":"turn_running",
                        "next":"idle",
                        "completed_turn_id":"turn-one",
                    },
                ],
            }),
            json!({"kind":"command_response","batch_operation_id":"batch-one"}),
            Some((2, &request_digest)),
        )
        .unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].event_type, "hosted_session.turn_started");
        assert_eq!(facts[0].payload["command_sequence"], 2);
        assert_eq!(facts[0].payload["request_digest"], request_digest);
        assert_eq!(
            facts[0].payload["origin"],
            "daemon_accepted_worker_observation"
        );
        assert_eq!(facts[1].event_type, "hosted_session.turn_completed");
        assert_eq!(facts[1].payload["turn_id"], "turn-one");
        assert!(facts[1].payload.get("command_sequence").is_none());
    }

    #[test]
    fn idle_session_rejects_an_unaccepted_completion_before_root_testimony() {
        let session = session_fixture();
        let error = validate_new_state_transition_sequence_for_session(
            &session,
            3,
            &json!({
                "events":[],
                "session_observations":[{
                    "kind":"state",
                    "expected":"turn_running",
                    "next":"idle",
                    "completed_turn_id":"turn-one",
                }],
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exact predecessor state"));

        validate_new_state_transition_sequence_for_session(
            &session,
            3,
            &json!({
                "events":[],
                "session_observations":[
                    {
                        "kind":"state",
                        "expected":"idle",
                        "next":"turn_running",
                        "turn_id":"turn-one",
                    },
                    {
                        "kind":"state",
                        "expected":"turn_running",
                        "next":"idle",
                        "completed_turn_id":"turn-one",
                    },
                ],
            }),
        )
        .expect("one atomically testified start/completion sequence");
    }

    #[test]
    fn one_command_cannot_claim_multiple_started_turns() {
        let session = session_fixture();
        let request_digest = "b".repeat(64);
        let error = state_transition_fact_events(
            &session,
            3,
            &json!({
                "events":[],
                "session_observations":[
                    {
                        "kind":"state",
                        "expected":"idle",
                        "next":"turn_running",
                        "turn_id":"turn-one",
                    },
                    {
                        "kind":"state",
                        "expected":"idle",
                        "next":"turn_running",
                        "turn_id":"turn-two",
                    },
                ],
            }),
            json!({"kind":"command_response","batch_operation_id":"batch-one"}),
            Some((2, &request_digest)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("more than one turn"));
    }

    #[test]
    fn asynchronous_completion_fact_names_its_required_start_identity() {
        let session = session_fixture();
        let facts = state_transition_fact_events(
            &session,
            3,
            &json!({
                "events":[{"event_type":"turn.completed","payload":{"turn_id":"turn-one"}}],
                "session_observations":[{
                    "kind":"state",
                    "expected":"turn_running",
                    "next":"idle",
                    "completed_turn_id":"turn-one",
                }],
            }),
            json!({"kind":"pushed_observation_batch","batch_operation_id":"batch-one"}),
            None,
        )
        .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].event_type, "hosted_session.turn_completed");
        assert_eq!(facts[0].payload["turn_id"], "turn-one");
        assert_eq!(
            facts[0].payload["start_operation_id"],
            hosted_turn_start_operation_id(&session, 3, "turn-one").unwrap()
        );
    }

    #[test]
    fn turn_fact_source_cannot_claim_another_command_batch() {
        let session = session_fixture();
        let request_digest = "b".repeat(64);
        let error = validate_hosted_transition_source(
            &session,
            3,
            &json!({
                "kind":"command_response",
                "batch_operation_id":"c".repeat(64),
                "command_sequence":2,
                "request_digest":request_digest,
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("contradictory batch identity"));
    }
}
