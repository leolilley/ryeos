//! Meaning-blind command delivery for one durable exclusive session.
//!
//! The integration runtime owns command bodies and observation meaning. This
//! service owns only the generic durable contact boundary, event/approval
//! ledgers, worker-epoch fencing, and cleanup proof consumption.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::accounting_db::{ExecutionResourceClaimOutcome, ExecutionResourceDimension};
use crate::hosted_operation::{
    acquire_credential_profile_causal_contact_sync, acquire_credential_profile_contact,
    acquire_credential_profile_operation, acquire_credential_profile_operation_sync,
    begin_hosted_root_operation,
};
use crate::persistent_session::ExclusiveRetirementOutcome;
use crate::process::{IdentityLiveness, ShutdownAction, execution_group_liveness, kill_by_action};
use crate::runtime_db::{WorkerProcessRecord, WorkerProcessState};
use crate::state::AppState;
use crate::state_store::{
    DedicatedSessionRecord, NewDedicatedSessionApproval, NewDedicatedSessionCommand,
    NewEventRecord, ObservationBatchReservation,
};

/// Readiness for generic workspace capture, under the caller's existing root
/// operation barrier. Pool absence is not worker death, and a missing journal
/// PID is not proof that a failed held launch never contacted the workspace.
/// The returned exact identity still needs quiescence (live capture) or group
/// death (terminal capture); this function grants no signal or new ownership.
pub fn workspace_worker_capture_identity(
    state: &AppState,
    workspace: &crate::runtime_db::WorkspaceRecord,
) -> Result<Option<crate::process::ExecutionProcessIdentity>> {
    let root = workspace
        .thread_id
        .as_deref()
        .ok_or_else(|| anyhow!("workspace capture has no root owner"))?;
    let worker = state
        .state_store
        .workspace_worker_capture_record(workspace)?;
    let pooled_boot = state
        .persistent_sessions
        .exclusive_capture_boot_identity(root)?;
    let Some(worker) = worker else {
        if pooled_boot.is_some() {
            bail!("workspace capture found a pool owner without durable worker authority");
        }
        return Ok(None);
    };
    if let Some(boot) = pooled_boot.as_deref() {
        if boot != worker.boot_identity_hash
            || worker.state != WorkerProcessState::Live
            || worker.cleanup_state != "owned"
        {
            bail!("workspace capture pool and durable worker boot disagree");
        }
    } else {
        crate::process::assert_reaped_process_group_absent(&worker.process_identity)
            .context("absent pool entry does not prove dedicated worker cleanup")?;
    }
    if let Some(raw) = workspace.process_identity.as_deref() {
        let recorded: crate::process::ExecutionProcessIdentity = serde_json::from_str(raw)?;
        if recorded != worker.process_identity {
            bail!("workspace journal and current dedicated worker identities disagree");
        }
    } else {
        // A retained reaped worker need not remain in the journal, but only
        // exact death—not absence—can permit omitting that live owner.
        crate::process::assert_reaped_process_group_absent(&worker.process_identity)?;
    }
    Ok(Some(worker.process_identity))
}

pub use ryeos_runtime::callback::{
    DEDICATED_SESSION_AGGREGATE_TERMINALIZATION_RESERVE_MS, DedicatedSessionBoundedOutcome,
    DedicatedSessionBoundedOutcomeKind, HostedApprovalFence, HostedCommandCompletionFence,
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
const MAX_WORKER_EVENTS_PER_RESPONSE: usize = 512;
const APPROVAL_REQUEST_TTL_MS: i64 = 15 * 60 * 1000;

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

fn canonical_command_observation_batch(result: &Value, observation_limit: usize) -> Result<Value> {
    // Command replies carry session observations, but asynchronous events use
    // the pushed-batch channel and need not appear in a reply. Normalize that
    // omission once for every fact/projection consumer, without changing the
    // raw response (its digest and ephemeral retention remain authoritative).
    validate_session_observation_cardinality(result, observation_limit)?;
    let events = match result.get("events") {
        None => json!([]),
        Some(value) => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("worker events are not a bounded array"))?;
            if values.len() > MAX_WORKER_EVENTS_PER_RESPONSE {
                bail!("worker emitted too many events in one response");
            }
            value.clone()
        }
    };
    Ok(json!({
        "events":events,
        "session_observations":result["session_observations"],
    }))
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
    let notified = signal.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let current = current_session(state, placement_thread_id)?;
    if current.updated_at_ms != observed_updated_at_ms || current.state == "terminal" {
        return Ok(current);
    }
    // The pushed signal covers correlated projection ledgers (notably an
    // approval row) as well as fields on the session row itself. Return after
    // any signalled projection commit even when the session timestamp is
    // unchanged so callers can read the newly attached exact authority.
    let _ = tokio::time::timeout(timeout, notified).await;
    current_session(state, placement_thread_id)
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
    observation_events.extend(approval_request_fact_events(
        session,
        worker_boot_epoch,
        result,
    )?);
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
        Some("command_response" | "command_progress") if object.len() == 4 => {
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
                if object["kind"] == "command_progress" {
                    "hosted_worker_command_progress"
                } else {
                    "hosted_worker_command_observation_batch"
                },
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
        if fact.count != 1
            || !fact.payload.as_ref().is_some_and(|actual| {
                actual == &expected.payload
                    || corroborates_command_progress(session, &expected, actual)
            })
        {
            bail!("hosted worker batch has no exact daemon-accepted transition testimony");
        }
        if fact
            .payload
            .as_ref()
            .is_some_and(|actual| actual != &expected.payload)
        {
            // A corroborating final needs the actual earlier batch as well
            // as the start fact; a source-shaped claim alone is insufficient.
            let (sequence, digest) = command_coordinate
                .ok_or_else(|| anyhow!("corroborating start has no command coordinate"))?;
            let progress =
                retained_command_progress(state, session, worker_boot_epoch, sequence, digest)?
                    .ok_or_else(|| {
                        anyhow!("corroborating start lacks its original progress batch")
                    })?;
            if progress["session_observations"][0]["turn_id"] != expected.payload["turn_id"] {
                bail!("corroborating start contradicts its progress batch");
            }
        }
    }
    Ok(())
}

/// Only the final response of the very same command may corroborate its
/// previously accepted start. This never blesses an unrelated duplicate
/// transition, nor lets a later command relabel the original source fact.
fn corroborates_command_progress(
    session: &DedicatedSessionRecord,
    expected: &NewEventRecord,
    actual: &Value,
) -> bool {
    if expected.event_type != "hosted_session.turn_started"
        || expected
            .payload
            .pointer("/source/kind")
            .and_then(Value::as_str)
            != Some("command_response")
        || actual.pointer("/source/kind").and_then(Value::as_str) != Some("command_progress")
    {
        return false;
    }
    let Some(epoch) = expected.payload["worker_boot_epoch"].as_u64() else {
        return false;
    };
    let Ok(Some((sequence, digest))) =
        validate_hosted_transition_source(session, epoch, &actual["source"])
    else {
        return false;
    };
    if expected.payload["command_sequence"].as_u64() != Some(sequence)
        || expected.payload["request_digest"].as_str() != Some(digest.as_str())
    {
        return false;
    }
    let mut corroborated = actual.clone();
    corroborated["source"] = expected.payload["source"].clone();
    corroborated == expected.payload
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

fn aggregate_hosted_contact_coordinate(placement_thread_id: &str, command_sequence: u64) -> String {
    format!("hosted-command:{placement_thread_id}:{command_sequence}")
}

fn execution_budget_id_for_hosted_root(
    state: &AppState,
    placement_thread_id: &str,
) -> Result<Option<String>> {
    let launch = state
        .state_store
        .admitted_launch_capsule(placement_thread_id)?
        .ok_or_else(|| anyhow!("hosted root lost its admitted launch capsule"))?;
    Ok(launch
        .accounting_scope
        .map(|scope| scope.execution_budget_id))
}

fn admitted_worker_execution_config(
    launch: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<&Value> {
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &launch.execution_closure
    else {
        bail!("hosted root has no managed runtime closure");
    };
    prepared_runtime_launch
        .get("runtime_data")
        .and_then(|value| value.get("worker_execution"))
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow!("hosted root has no admitted worker-execution config"))
}

fn admitted_bounded_command_step(
    worker_execution: &Value,
    parameters: &Value,
    placement_thread_id: &str,
    idempotency_key: &str,
    command_kind: &str,
    payload: &Value,
) -> Result<Option<(&'static str, u32)>> {
    let mode = worker_execution
        .get("mode")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("hosted root has no admitted worker-execution mode"))?;
    match mode.get("kind").and_then(Value::as_str) {
        Some("session") => Ok(None),
        Some("bounded_turn") => {
            // Fixed daemon recovery resumes/inspects the existing upstream
            // session; it is not another admitted model-turn attempt.
            if command_kind == "reattach" {
                return Ok(None);
            }
            let (step, attempt) = bounded_attempt_key(placement_thread_id, idempotency_key)?
                .ok_or_else(|| anyhow!("bounded worker command has no admitted step coordinate"))?;
            if command_kind != "route" {
                bail!("bounded worker command is not a route command");
            }
            let maximum = mode
                .get("max_uncontacted_attempts")
                .and_then(Value::as_u64)
                .filter(|value| (1..=8).contains(value))
                .ok_or_else(|| anyhow!("bounded worker mode lost its admitted attempt ceiling"))?;
            if u64::from(attempt) > maximum {
                bail!("bounded worker command exceeds its signed attempt ceiling");
            }
            let (route_field, payload_field) = match step {
                "session-start" => ("session_start_route", "session_start_payload"),
                "turn-start" => ("turn_start_route", "turn_start_payload"),
                _ => unreachable!("bounded attempt parser returned an unknown step"),
            };
            let admitted_route = mode
                .get(route_field)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("bounded worker mode lost its admitted route"))?;
            let admitted_payload = parameters
                .get("goal")
                .and_then(|goal| goal.get(payload_field))
                .filter(|value| value.is_object())
                .ok_or_else(|| anyhow!("bounded worker invocation lost its sealed goal payload"))?;
            if *payload != json!({"route_id":admitted_route,"payload":admitted_payload}) {
                bail!("bounded worker command contradicts its sealed route or goal payload");
            }
            Ok(Some((step, attempt)))
        }
        _ => bail!("hosted root has an unknown admitted worker-execution mode"),
    }
}

fn is_admitted_bounded_turn_contact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<bool> {
    let launch = state
        .state_store
        .admitted_launch_capsule(&session.placement_thread_id)?
        .ok_or_else(|| anyhow!("hosted root lost its admitted launch capsule"))?;
    Ok(admitted_bounded_command_step(
        admitted_worker_execution_config(&launch)?,
        &launch.sealed_invocation["parameters"],
        &session.placement_thread_id,
        &record.idempotency_key,
        &record.command_kind,
        &record.payload,
    )?
    .is_some_and(|(step, _)| step == "turn-start"))
}

fn validate_bounded_command_admission(
    state: &AppState,
    session: &DedicatedSessionRecord,
    idempotency_key: &str,
    command_kind: &str,
    payload: &Value,
    command_sequence: Option<u64>,
) -> Result<()> {
    let launch = state
        .state_store
        .admitted_launch_capsule(&session.placement_thread_id)?
        .ok_or_else(|| anyhow!("hosted root lost its admitted launch capsule"))?;
    let config = admitted_worker_execution_config(&launch)?;
    let parameters = &launch.sealed_invocation["parameters"];
    let Some((step, attempt)) = admitted_bounded_command_step(
        config,
        parameters,
        &session.placement_thread_id,
        idempotency_key,
        command_kind,
        payload,
    )?
    else {
        return Ok(());
    };
    let mut session_attempts = Vec::new();
    let mut turn_attempts = Vec::new();
    for record in state
        .state_store
        .dedicated_session_commands(&session.placement_thread_id)?
    {
        // The exact current reservation is checked separately. All other
        // attempts must already have rooted, immutable contact testimony.
        if record.idempotency_key == idempotency_key {
            continue;
        }
        let Some((prior_step, prior_attempt)) = admitted_bounded_command_step(
            config,
            parameters,
            &session.placement_thread_id,
            &record.idempotency_key,
            &record.command_kind,
            &record.payload,
        )?
        else {
            continue;
        };
        let contact = bounded_command_contact(state, session, &record)?;
        match prior_step {
            "session-start" => session_attempts.push((prior_attempt, record, contact)),
            "turn-start" => turn_attempts.push((prior_attempt, record, contact)),
            _ => unreachable!("bounded attempt parser returned an unknown step"),
        }
    }
    validate_next_bounded_attempt(
        step,
        attempt,
        command_sequence,
        &mut session_attempts,
        &mut turn_attempts,
    )
}

fn bounded_session_deadline(config: &Value, created_at_ms: i64) -> Result<Option<i64>> {
    match config
        .get("mode")
        .and_then(|mode| mode.get("kind"))
        .and_then(Value::as_str)
    {
        Some("session") => Ok(None),
        Some("bounded_turn") => {
            let lifetime_seconds = config
                .get("max_lifetime_seconds")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0 && *value <= 603_600)
                .ok_or_else(|| anyhow!("bounded worker closure has no signed lifetime"))?;
            let lifetime_ms = i64::try_from(lifetime_seconds)?
                .checked_mul(1_000)
                .ok_or_else(|| anyhow!("bounded worker lifetime exceeds timestamp range"))?;
            Ok(Some(created_at_ms.checked_add(lifetime_ms).ok_or_else(
                || anyhow!("bounded worker deadline exceeds timestamp range"),
            )?))
        }
        _ => bail!("hosted root has an unknown admitted worker-execution mode"),
    }
}

fn validate_hosted_contact_deadlines(
    session_deadline: Option<i64>,
    aggregate_deadline: Option<i64>,
    now_ms: i64,
) -> Result<()> {
    if session_deadline.is_some_and(|deadline| now_ms >= deadline) {
        bail!("budget_exhausted: bounded worker absolute lifetime expired before contact");
    }
    if aggregate_deadline.is_some_and(|deadline| now_ms >= deadline) {
        bail!("budget_exhausted: aggregate execution absolute deadline expired before contact");
    }
    Ok(())
}

fn ensure_hosted_contact_deadline(
    state: &AppState,
    session: &DedicatedSessionRecord,
) -> Result<Option<i64>> {
    let launch = state
        .state_store
        .admitted_launch_capsule(&session.placement_thread_id)?
        .ok_or_else(|| anyhow!("hosted root lost its admitted launch capsule"))?;
    let session_deadline = bounded_session_deadline(
        admitted_worker_execution_config(&launch)?,
        session.created_at_ms,
    )?;
    let aggregate_deadline = if let Some(scope) = launch.accounting_scope.as_ref() {
        state
            .accounting
            .as_ref()
            .ok_or_else(|| anyhow!("hosted root accounting scope has no live ledger"))?
            .execution_resource_budget_snapshot(&scope.execution_budget_id)?
            .ok_or_else(|| anyhow!("hosted root lost its aggregate budget authority"))?
            .deadline_at_ms
    } else {
        None
    };
    validate_hosted_contact_deadlines(
        session_deadline,
        aggregate_deadline,
        lillux::time::timestamp_millis(),
    )?;
    Ok(match (session_deadline, aggregate_deadline) {
        (Some(session), Some(aggregate)) => Some(session.min(aggregate)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    })
}

fn claim_bounded_turn_contact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<Option<String>> {
    if !is_admitted_bounded_turn_contact(state, session, record)? {
        return Ok(None);
    }
    let Some(execution_budget_id) =
        execution_budget_id_for_hosted_root(state, &session.placement_thread_id)?
    else {
        return Ok(None);
    };
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("hosted root accounting scope has no live ledger"))?;
    let coordinate =
        aggregate_hosted_contact_coordinate(&session.placement_thread_id, record.command_sequence);
    match accounting.claim_execution_resource(
        &execution_budget_id,
        ExecutionResourceDimension::ProviderContact,
        &coordinate,
        &record.request_digest,
        lillux::time::timestamp_millis(),
    )? {
        ExecutionResourceClaimOutcome::Admitted { .. } => Ok(None),
        ExecutionResourceClaimOutcome::ReleasedUncontacted { .. } => {
            bail!("released aggregate provider-contact coordinate cannot be contacted again")
        }
        ExecutionResourceClaimOutcome::Denied { reason, .. } => Ok(Some(reason)),
    }
}

fn release_bounded_turn_contact_if_verified_uncontacted(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<()> {
    if !is_admitted_bounded_turn_contact(state, session, record)? {
        return Ok(());
    }
    let Some(execution_budget_id) =
        execution_budget_id_for_hosted_root(state, &session.placement_thread_id)?
    else {
        return Ok(());
    };
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("hosted root accounting scope has no live ledger"))?;
    accounting.release_provider_contact_uncontacted(
        &execution_budget_id,
        &aggregate_hosted_contact_coordinate(&session.placement_thread_id, record.command_sequence),
        &record.request_digest,
        lillux::time::timestamp_millis(),
    )?;
    Ok(())
}

fn retained_bounded_turn_contact_claim(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<Option<ExecutionResourceClaimOutcome>> {
    if !is_admitted_bounded_turn_contact(state, session, record)? {
        return Ok(None);
    }
    let Some(execution_budget_id) =
        execution_budget_id_for_hosted_root(state, &session.placement_thread_id)?
    else {
        return Ok(None);
    };
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("hosted root accounting scope has no live ledger"))?;
    accounting.execution_resource_claim_outcome(
        &execution_budget_id,
        ExecutionResourceDimension::ProviderContact,
        &aggregate_hosted_contact_coordinate(&session.placement_thread_id, record.command_sequence),
        &record.request_digest,
    )
}

fn aggregate_contact_budget_refusal(reason: &str) -> Result<Value> {
    let dimension = match reason {
        "aggregate_duration_exhausted" => "duration",
        "aggregate_provider_contacts_exhausted" => "provider_contacts",
        _ => bail!("aggregate hosted contact refusal has an unknown budget reason"),
    };
    Ok(json!({
        "error":"budget_exhausted",
        "budget_dimension":dimension,
        "budget_reason":reason,
        "retryable_uncontacted":false,
    }))
}

fn append_contact_budget_refusal_fact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
    result: &Value,
    recovered: bool,
) -> Result<()> {
    let mut payload = json!({
        "schema":1,
        "origin":"daemon_budget_authority",
        "worker_boot_epoch":record.worker_boot_epoch,
        "retryable_uncontacted":false,
        "budget_exhausted":true,
        "budget_dimension":result.get("budget_dimension"),
        "budget_reason":result.get("budget_reason"),
    });
    if recovered {
        payload
            .as_object_mut()
            .expect("budget refusal fact is an object")
            .insert("recovered".to_owned(), Value::Bool(true));
        append_recovered_command_fact_once(
            state,
            session,
            "hosted_command.failed_uncontacted",
            record.command_sequence,
            &record.request_digest,
            record.worker_boot_epoch,
            payload,
        )
    } else {
        append_command_fact_once(
            state,
            session,
            "hosted_command.failed_uncontacted",
            record.command_sequence,
            &record.request_digest,
            payload,
        )
    }
}

fn retained_contact_budget_refusal(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<Option<Value>> {
    let Some(payload) = command_fact_payload(
        state,
        session,
        "hosted_command.failed_uncontacted",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )?
    else {
        return Ok(None);
    };
    if payload.get("origin").and_then(Value::as_str) != Some("daemon_budget_authority") {
        return Ok(None);
    }
    let reason = payload
        .get("budget_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("retained budget refusal has no reason"))?;
    let result = aggregate_contact_budget_refusal(reason)?;
    if payload.get("budget_exhausted").and_then(Value::as_bool) != Some(true)
        || payload
            .get("retryable_uncontacted")
            .and_then(Value::as_bool)
            != Some(false)
        || payload.get("budget_dimension") != result.get("budget_dimension")
    {
        bail!("retained budget refusal fact has contradictory authority");
    }
    let execution_budget_id =
        execution_budget_id_for_hosted_root(state, &session.placement_thread_id)?
            .ok_or_else(|| anyhow!("retained budget refusal has no execution budget"))?;
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("retained budget refusal has no live accounting ledger"))?;
    let claim = accounting.execution_resource_claim_outcome(
        &execution_budget_id,
        ExecutionResourceDimension::ProviderContact,
        &aggregate_hosted_contact_coordinate(&session.placement_thread_id, record.command_sequence),
        &record.request_digest,
    )?;
    match claim {
        Some(ExecutionResourceClaimOutcome::Denied {
            reason: retained_reason,
            ..
        }) if retained_reason == reason => Ok(Some(result)),
        _ => bail!("retained budget refusal contradicts its aggregate resource claim"),
    }
}

/// Complete canonical command testimony after restart from the durable
/// command outbox. This never contacts a worker. A possible-contact row is
/// reconciled only to outcome-unknown; it is never replayed.
pub fn reconcile_command_outboxes(state: &AppState) -> Result<()> {
    for mut record in state.state_store.dedicated_command_outbox_records()? {
        let session = current_session(state, &record.placement_thread_id)?;
        if terminal_session_retains_predecessor_capsule(state, &session, &record)? {
            continue;
        }
        let fenced_uncontacted = json!({
            "error":"worker epoch ended before contact",
            "retryable_uncontacted":true,
        });
        if record.state == "failed"
            && record.result.as_ref() == Some(&fenced_uncontacted)
            && let Some(result) = retained_contact_budget_refusal(state, &session, &record)?
        {
            // Root testimony and the aggregate ledger both prove this was a
            // budget denial, but the daemon crashed before its rebuildable
            // command row crossed the same boundary. Repair only that exact
            // worker-fenced projection.
            state
                .state_store
                .repair_fenced_dedicated_command_budget_refusal(
                    &record.placement_thread_id,
                    record.command_sequence,
                    record.worker_boot_epoch,
                    &result,
                )?;
            record.result = Some(result);
            notify_projection_change(&record.placement_thread_id);
        }
        if record.state == "committed"
            && let Some(ExecutionResourceClaimOutcome::Denied { reason, .. }) =
                retained_bounded_turn_contact_claim(state, &session, &record)?
        {
            if command_fact_exists(
                state,
                &session,
                "hosted_command.contacting",
                record.command_sequence,
                &record.request_digest,
                record.worker_boot_epoch,
            )? {
                bail!(
                    "aggregate provider-contact denial contradicts exact possible-contact testimony"
                );
            }
            // The shared ledger crossed its authoritative refusal boundary,
            // but the daemon crashed before publishing the refusal fact and
            // advancing the rebuildable command row. Complete that exact
            // denial while the old worker epoch is quiescent and before its
            // committed rows can be fenced as retryable-uncontacted.
            let result = aggregate_contact_budget_refusal(&reason)?;
            append_contact_budget_refusal_fact(state, &session, &record, &result, true)?;
            state.state_store.settle_dedicated_command_uncontacted(
                &record.placement_thread_id,
                record.command_sequence,
                record.worker_boot_epoch,
                &result,
            )?;
            record.state = "failed".to_owned();
            record.result = Some(result);
            notify_projection_change(&record.placement_thread_id);
        }
        if record.state == "committed"
            && command_fact_exists(
                state,
                &session,
                "hosted_command.contacting",
                record.command_sequence,
                &record.request_digest,
                record.worker_boot_epoch,
            )?
        {
            // The root fact is the possible-contact boundary. A crash between
            // that append and the rebuildable SQLite transition must never let
            // worker fencing reclassify this command as retryable-uncontacted.
            state
                .state_store
                .mark_committed_dedicated_command_contact_unknown(
                    &record.placement_thread_id,
                    record.command_sequence,
                    record.worker_boot_epoch,
                )?;
            record.state = "outcome_unknown".to_owned();
            notify_projection_change(&record.placement_thread_id);
        }
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
                        result
                            .get("retryable_uncontacted")
                            .and_then(Value::as_bool)
                            .is_some()
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
            if expected_terminal_fact.is_some()
                && !authoritative_settled_command_replay(state, &session, &record)?
            {
                bail!("terminal hosted command projection contradicts its root testimony");
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
        if let Some(progress) = retained_command_progress(
            state,
            &session,
            record.worker_boot_epoch,
            record.command_sequence,
            &record.request_digest,
        )? {
            let current = current_session(state, &session.placement_thread_id)?;
            let turn_id = progress["session_observations"][0]["turn_id"]
                .as_str()
                .ok_or_else(|| anyhow!("retained command progress lost its turn identity"))?;
            // Replay only a lagging start projection in its original boot.
            // Completion or recovery is a later frontier, never permission
            // to resurrect an old turn as newly running.
            if current.worker_boot_epoch == Some(record.worker_boot_epoch)
                && current.state == "idle"
                && current.current_turn_id.is_none()
                && hosted_turn_completion_payload(
                    state,
                    &session,
                    record.worker_boot_epoch,
                    turn_id,
                )?
                .is_none()
            {
                apply_worker_observations(
                    state,
                    &session.placement_thread_id,
                    record.worker_boot_epoch,
                    &progress,
                    1,
                )?;
            }
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
                    let remaining = command_batch_after_progress(
                        state,
                        &session,
                        record.worker_boot_epoch,
                        record.command_sequence,
                        &record.request_digest,
                        &canonical_batch,
                    )?;
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
                        &remaining,
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
            "failed"
                if record.result.as_ref().is_some_and(|result| {
                    result.get("error").and_then(Value::as_str) == Some("budget_exhausted")
                        && result.get("retryable_uncontacted").and_then(Value::as_bool)
                            == Some(false)
                }) =>
            {
                let result = record.result.as_ref().expect("budget result checked above");
                append_recovered_command_fact_once(
                    state,
                    &session,
                    "hosted_command.failed_uncontacted",
                    record.command_sequence,
                    &record.request_digest,
                    record.worker_boot_epoch,
                    json!({
                        "schema":1,
                        "origin":"daemon_budget_authority",
                        "worker_boot_epoch":record.worker_boot_epoch,
                        "retryable_uncontacted":false,
                        "budget_exhausted":true,
                        "budget_dimension":result.get("budget_dimension"),
                        "budget_reason":result.get("budget_reason"),
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
                if let Some(reason) = claim_bounded_turn_contact(state, &session, &record)? {
                    // A crash can land after the shared ledger's denial but
                    // before either the root fact or SQLite settlement. The
                    // old worker epoch proves no contact, so complete the
                    // already-decided refusal instead of converting it into a
                    // retry that could escape its aggregate budget.
                    let result = aggregate_contact_budget_refusal(&reason)?;
                    append_contact_budget_refusal_fact(state, &session, &record, &result, true)?;
                    state
                        .state_store
                        .repair_fenced_dedicated_command_budget_refusal(
                            &record.placement_thread_id,
                            record.command_sequence,
                            record.worker_boot_epoch,
                            &result,
                        )?;
                    notify_projection_change(&record.placement_thread_id);
                    continue;
                }
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
                if !authoritative_settled_command_replay(state, &session, &record)? {
                    bail!("recovered uncontacted command has no exact root testimony");
                }
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

#[derive(Clone)]
struct ApprovalRequestAuthority {
    fence: HostedApprovalFence,
    operation_class: String,
    requested_authority: Value,
}

/// Public, presentation-safe portion of a retained worker approval request.
/// Upstream request/session/operation coordinates are delivery authority and
/// must remain inside the dedicated-session owner.
pub fn public_approval_authority(requested_authority: &Value) -> Value {
    json!({
        "accept_allowed": requested_authority
            .get("accept_allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "display": requested_authority.get("display").cloned().unwrap_or(Value::Null),
    })
}

fn approval_request_authority(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    payload: &Value,
    require_current_turn: bool,
) -> Result<ApprovalRequestAuthority> {
    let upstream_request_id = payload
        .get("request_id")
        .ok_or_else(|| anyhow!("approval event has no request id"))?;
    let operation_class = payload
        .get("operation_class")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| anyhow!("approval event has no bounded operation class"))?;
    payload
        .get("display")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("approval event has no typed display authority"))?;
    let request_digest = payload
        .get("request_digest")
        .and_then(Value::as_str)
        .filter(|digest| lillux::valid_hash(digest))
        .ok_or_else(|| anyhow!("approval event has no canonical request digest"))?;
    let observed_thread = payload
        .get("upstream_session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("approval event has no upstream-session correlation"))?;
    let turn_id = payload
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("approval event has no operation correlation"))?;
    validate_hosted_turn_id("approval turn id", turn_id)?;
    if session.remote_thread_id.as_deref() != Some(observed_thread)
        || (require_current_turn && session.current_turn_id.as_deref() != Some(turn_id))
    {
        bail!("approval event does not correlate to the retained thread and turn");
    }
    let approval_id = ryeos_state::objects::canonical_value_digest(&json!({
        "worker_boot_epoch":worker_boot_epoch,
        "upstream_request_id":upstream_request_id,
        "request_digest":request_digest,
    }))?;
    let approval_operation_id = ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_approval_request.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "admitted_capsule_hash":session.admitted_capsule_hash,
        "worker_boot_epoch":worker_boot_epoch,
        "turn_id":turn_id,
        "approval_id":approval_id,
        "request_digest":request_digest,
    }))?;
    Ok(ApprovalRequestAuthority {
        fence: HostedApprovalFence {
            chain_root_id: session.chain_root_id.clone(),
            placement_thread_id: session.placement_thread_id.clone(),
            admitted_capsule_hash: session.admitted_capsule_hash.clone(),
            worker_boot_epoch,
            turn_id: turn_id.to_owned(),
            approval_id,
            request_digest: request_digest.to_owned(),
            approval_operation_id,
        },
        operation_class: operation_class.to_owned(),
        requested_authority: payload.clone(),
    })
}

fn approval_request_fact_events(
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    result: &Value,
) -> Result<Vec<NewEventRecord>> {
    result
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker events are not a bounded array"))?
        .iter()
        .filter_map(|event| {
            let event: WorkerEvent = match serde_json::from_value(event.clone()) {
                Ok(event) => event,
                Err(error) => return Some(Err(error.into())),
            };
            if event.event_type != "approval.requested" {
                return None;
            }
            Some(
                approval_request_authority(session, worker_boot_epoch, &event.payload, true).map(
                    |authority| NewEventRecord {
                        event_type: "hosted_session.approval_requested".to_owned(),
                        storage_class: "indexed".to_owned(),
                        payload: json!({
                            "schema":1,
                            "operation_id":authority.fence.approval_operation_id,
                            "origin":"daemon_accepted_worker_observation",
                            "chain_root_id":authority.fence.chain_root_id,
                            "placement_thread_id":authority.fence.placement_thread_id,
                            "admitted_capsule_hash":authority.fence.admitted_capsule_hash,
                            "worker_boot_epoch":authority.fence.worker_boot_epoch,
                            "turn_id":authority.fence.turn_id,
                            "approval_id":authority.fence.approval_id,
                            "request_digest":authority.fence.request_digest,
                            "observation":authority.requested_authority,
                        }),
                    },
                ),
            )
        })
        .collect()
}

fn validate_approval_request_fact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    authority: &ApprovalRequestAuthority,
) -> Result<i64> {
    let fence = &authority.fence;
    let fact = crate::authoritative_root_fact::lookup(
        state,
        &session.placement_thread_id,
        "hosted_session.approval_requested",
        &fence.approval_operation_id,
    )?;
    if fact.count != 1 {
        bail!("pending approval has no unique authoritative root fact");
    }
    let payload = fact
        .payload
        .ok_or_else(|| anyhow!("pending approval root fact has no retained payload"))?;
    let exact = payload.get("schema").and_then(Value::as_u64) == Some(1)
        && payload.get("operation_id").and_then(Value::as_str)
            == Some(fence.approval_operation_id.as_str())
        && payload.get("origin").and_then(Value::as_str)
            == Some("daemon_accepted_worker_observation")
        && payload.get("chain_root_id").and_then(Value::as_str)
            == Some(fence.chain_root_id.as_str())
        && payload.get("placement_thread_id").and_then(Value::as_str)
            == Some(fence.placement_thread_id.as_str())
        && payload.get("admitted_capsule_hash").and_then(Value::as_str)
            == Some(fence.admitted_capsule_hash.as_str())
        && payload.get("worker_boot_epoch").and_then(Value::as_u64)
            == Some(fence.worker_boot_epoch)
        && payload.get("turn_id").and_then(Value::as_str) == Some(fence.turn_id.as_str())
        && payload.get("approval_id").and_then(Value::as_str) == Some(fence.approval_id.as_str())
        && payload.get("request_digest").and_then(Value::as_str)
            == Some(fence.request_digest.as_str())
        && payload.get("observation") == Some(&authority.requested_authority);
    if !exact {
        bail!("pending approval root fact contradicts its exact coordinate");
    }
    let turn_start =
        hosted_turn_start_authority(state, session, fence.worker_boot_epoch, &fence.turn_id)?
            .ok_or_else(|| anyhow!("pending approval turn has no authoritative start fact"))?;
    let approval_chain_seq = fact
        .first_chain_seq
        .ok_or_else(|| anyhow!("pending approval has no root-chain coordinate"))?;
    if turn_start.chain_seq >= approval_chain_seq {
        bail!("pending approval does not follow its exact turn start");
    }
    let accepted_at_ms = ryeos_state::objects::thread_snapshot::parse_canonical_timestamp(
        fact.first_event_ts
            .as_deref()
            .ok_or_else(|| anyhow!("pending approval has no authoritative event timestamp"))?,
    )?
    .timestamp_millis();
    accepted_at_ms
        .checked_add(APPROVAL_REQUEST_TTL_MS)
        .ok_or_else(|| anyhow!("pending approval expiry exceeds timestamp range"))
}

fn approval_fence_from_record(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionApprovalRecord,
    require_pending: bool,
) -> Result<HostedApprovalFence> {
    let authority = approval_request_authority(
        session,
        record.worker_boot_epoch,
        &record.requested_authority,
        false,
    )?;
    if record.placement_thread_id != session.placement_thread_id
        || record.approval_id != authority.fence.approval_id
        || record.request_digest != authority.fence.request_digest
        || record.operation_class != authority.operation_class
    {
        bail!("approval projection contradicts its exact worker/root identity");
    }
    if record.decision_principal.is_some()
        || record.decision.is_some()
        || record.decision_digest.is_some()
        || record.reservation_token.is_some()
        || record.delivery_contacted_at_ms.is_some()
        || record.delivery_settled_at_ms.is_some()
    {
        bail!("approval projection contains a decision or possible delivery");
    }
    let authoritative_expires_at_ms = validate_approval_request_fact(state, session, &authority)?;
    if record.expires_at_ms != authoritative_expires_at_ms {
        bail!("approval projection expiry contradicts its authoritative root fact");
    }
    let current_worker_matches = match (
        session.worker_instance_id.as_deref(),
        session.worker_boot_epoch,
    ) {
        (Some(worker_instance_id), Some(worker_boot_epoch)) => {
            worker_instance_id == record.worker_instance_id
                && worker_boot_epoch == record.worker_boot_epoch
        }
        (None, None) => !require_pending,
        _ => false,
    };
    if !current_worker_matches {
        bail!("approval projection belongs to another current worker epoch");
    }
    if require_pending {
        if record.state != "pending"
            || session.current_turn_id.as_deref() != Some(authority.fence.turn_id.as_str())
            || record.expires_at_ms <= lillux::time::timestamp_millis() as i64
            || record.resolved_at_ms.is_some()
        {
            bail!("approval projection is not pending for the current worker turn");
        }
    } else if record.state != "stale_epoch" || record.resolved_at_ms.is_none() {
        bail!("terminal approval testimony was not retired from an unresolved request");
    }
    Ok(authority.fence)
}

fn exact_pending_approval_projection(
    state: &AppState,
    session: &DedicatedSessionRecord,
) -> Result<Option<HostedApprovalFence>> {
    let approvals = state
        .state_store
        .pending_dedicated_session_approvals(&session.placement_thread_id)?;
    if approvals.is_empty() {
        if session.state == "awaiting_approval" {
            bail!("approval-waiting session has no exact durable pending request");
        }
        return Ok(None);
    }
    if approvals.len() != 1 {
        bail!("dedicated session has ambiguous unresolved approval authority");
    }
    Ok(Some(approval_fence_from_record(
        state,
        session,
        &approvals[0],
        true,
    )?))
}

/// Return one exact unresolved approval only after the rebuildable approval
/// row, current session epoch/turn, and immutable root testimony all agree.
/// Any competing decision or ambiguous cardinality fails closed.
pub fn exact_pending_approval(
    state: &AppState,
    placement_thread_id: &str,
) -> Result<Option<HostedApprovalFence>> {
    let root_operation = crate::hosted_operation::begin_hosted_root_operation_if_appendable(
        &state.state_store,
        placement_thread_id,
    )?;
    let session = current_session(state, placement_thread_id)?;
    let approval = exact_pending_approval_projection(state, &session)?;
    if approval.is_some() && root_operation.is_none() {
        bail!("nonappendable hosted root retains unresolved approval authority");
    }
    Ok(approval)
}

/// Preserve a bounded unattended approval refusal across daemon restart.
/// Approval outbox reconciliation runs first, while the old worker epoch is
/// still attached. Only one exact pending row that agrees with immutable root
/// testimony may reserve `ApprovalRequired`; every ambiguous, decided, or
/// contact-uncertain approval is left unreserved so ordinary worker fencing
/// classifies the session outcome as unknown.
pub fn reserve_restart_pending_approval_outcomes(state: &AppState) -> Result<usize> {
    let current_generation = crate::runtime_db::daemon_generation_id();
    let mut reserved = 0usize;
    for worker in state.state_store.live_worker_processes()? {
        if worker.daemon_generation_id == current_generation {
            continue;
        }
        let root_operation = crate::hosted_operation::begin_hosted_root_operation_if_appendable(
            &state.state_store,
            &worker.placement_thread_id,
        )?;
        let Some(session) = state
            .state_store
            .dedicated_session(&worker.placement_thread_id)?
        else {
            continue;
        };
        if session.state != "awaiting_approval"
            || session.candidate_disposition
                != crate::runtime_db::DedicatedCandidateDisposition::RetainedForReview
            || session.bounded_outcome.is_some()
        {
            continue;
        }
        if root_operation.is_none() {
            tracing::warn!(
                placement_thread_id = %session.placement_thread_id,
                worker_boot_epoch = worker.boot_epoch,
                "restart pending approval belongs to a nonappendable hosted root"
            );
            continue;
        }
        let fence = match exact_pending_approval_projection(state, &session) {
            Ok(Some(fence)) => fence,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    placement_thread_id = %session.placement_thread_id,
                    worker_boot_epoch = worker.boot_epoch,
                    error = %error,
                    "restart could not prove one exact pending hosted approval; preserving outcome-unknown classification"
                );
                continue;
            }
        };
        if fence.worker_boot_epoch != worker.boot_epoch
            || session.worker_instance_id.as_deref() != Some(worker.worker_instance_id.as_str())
        {
            tracing::warn!(
                placement_thread_id = %session.placement_thread_id,
                worker_boot_epoch = worker.boot_epoch,
                "restart pending approval belongs to another worker epoch"
            );
            continue;
        }
        let outcome = DedicatedSessionBoundedOutcome {
            kind: DedicatedSessionBoundedOutcomeKind::ApprovalRequired,
            dimension: None,
            approval: Some(fence),
        };
        validate_bounded_budget_outcome_authority(state, &session, &outcome)?;
        state
            .state_store
            .reserve_dedicated_session_bounded_outcome(&session.placement_thread_id, &outcome)?;
        notify_projection_change(&session.placement_thread_id);
        reserved += 1;
    }
    Ok(reserved)
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
    if values.len() > MAX_WORKER_EVENTS_PER_RESPONSE {
        bail!("worker emitted too many events in one response");
    }
    for value in values {
        let event: WorkerEvent = serde_json::from_value(value.clone())?;
        if event.event_type == "approval.requested" {
            let authority =
                approval_request_authority(session, worker_boot_epoch, &event.payload, true)?;
            let worker_instance_id = session
                .worker_instance_id
                .as_deref()
                .ok_or_else(|| anyhow!("approval event has no attached worker"))?;
            let expires_at_ms = validate_approval_request_fact(state, session, &authority)?;
            state
                .state_store
                .create_dedicated_session_approval(NewDedicatedSessionApproval {
                    placement_thread_id: &session.placement_thread_id,
                    approval_id: &authority.fence.approval_id,
                    worker_instance_id,
                    worker_boot_epoch,
                    request_digest: &authority.fence.request_digest,
                    operation_class: &authority.operation_class,
                    requested_authority: &authority.requested_authority,
                    expires_at_ms,
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
    let _root_operation = crate::hosted_operation::begin_hosted_root_operation_async(
        &state.state_store,
        &initial.placement_thread_id,
    )
    .await?;
    let _credential_contact =
        acquire_credential_profile_contact(&initial.credential_profile_id, placement_thread_id)
            .await?;
    let session = current_session(state, placement_thread_id)?;
    // An unavailable caller route must not reserve a command or cross possible
    // contact: the bridge treats a protocol refusal as a worker failure. Consult the
    // same frozen profile/route selection used at launch, not live bundle
    // definitions. Runtime recovery has its separate daemon-owned surface.
    let public_protocol = if command_kind == "route" {
        let launch = state
            .state_store
            .admitted_launch_capsule(placement_thread_id)?
            .ok_or_else(|| anyhow!("hosted root lost its admitted launch capsule"))?;
        let profile = admitted_structured_protocol(state, &session.admitted_capsule_hash)?;
        validate_public_command_surface(
            &profile.contract,
            admitted_worker_execution_config(&launch)?,
            &payload,
        )?;
        Some(profile)
    } else {
        None
    };
    if command_kind == "route"
        && session.candidate_disposition
            == crate::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        && session.state != "idle"
    {
        // Exact settled replay returned above without contacting a worker.
        // A new bounded route is legal only from the approval-free idle
        // boundary; approval resolution uses its separate owner-authorized
        // service and cannot be smuggled through a generic route command.
        bail!("bounded worker route requires an approval-free idle session");
    }
    ensure_hosted_contact_deadline(state, &session)?;
    validate_bounded_command_admission(
        state,
        &session,
        idempotency_key,
        command_kind,
        &payload,
        None,
    )?;
    let worker_boot_epoch = session
        .worker_boot_epoch
        .ok_or_else(|| anyhow!("dedicated session has no attached worker"))?;
    let (protocol_profile_hash, protocol_schema_hashes) = match public_protocol {
        Some(profile) => (profile.profile_hash, profile.schema_hashes),
        None => structured_protocol_identity(state, &session.admitted_capsule_hash)?,
    };
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
    if command_fact_exists(
        state,
        &session,
        "hosted_command.contacting",
        record.command_sequence,
        &request_digest,
        record.worker_boot_epoch,
    )? {
        // A failed append/CAS response can leave the projection committed
        // after its root already crossed possible contact, without a daemon
        // restart. Neither that coordinate nor its retained budget claim
        // authorizes another send.
        bail!("committed command has possible-contact testimony and will not be resent");
    }
    // The committed reservation excludes another active command. Repeat the
    // bounded ledger check here to close a race with a command that settled
    // between the initial validation and this reservation.
    validate_bounded_command_admission(
        state,
        &session,
        idempotency_key,
        command_kind,
        &payload,
        Some(record.command_sequence),
    )?;
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
    if let Some(reason) = claim_bounded_turn_contact(state, &session, &record)? {
        let result = aggregate_contact_budget_refusal(&reason)?;
        append_contact_budget_refusal_fact(state, &session, &record, &result, false)?;
        state.state_store.settle_dedicated_command_uncontacted(
            placement_thread_id,
            record.command_sequence,
            worker_boot_epoch,
            &result,
        )?;
        notify_projection_change(placement_thread_id);
        return Ok(json!({
            "command_sequence":record.command_sequence,
            "state":"failed",
            "result":result,
        }));
    }
    let contact_deadline_at_ms = {
        // Pushed observations hold this same gate through root append and
        // approval/session projection. The successful contact CAS below is
        // the send linearization: an earlier approval prevents contact; an
        // approval accepted after it is an ordinary post-contact event.
        // Never carry this synchronous gate into an await or worker I/O.
        let transition_gate = transition_gate(placement_thread_id);
        let _transition_guard = transition_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.state_store.require_bounded_route_contact_admission(
            placement_thread_id,
            worker_boot_epoch,
            command_kind,
        )?;
        // A retained Admitted claim reserves count, not permission for first
        // contact after an absolute deadline, including committed replays.
        ensure_hosted_contact_deadline(state, &session)?;
        // Root testimony must precede the rebuildable dispatched projection.
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
        // If the append blocked past expiry, retain conservative contact
        // testimony but do not send. Exact replay may classify, never resend.
        let deadline = ensure_hosted_contact_deadline(state, &session)?;
        state.state_store.mark_dedicated_command_contacted(
            placement_thread_id,
            record.command_sequence,
            worker_boot_epoch,
        )?;
        deadline
    };
    let pool = Arc::clone(&state.persistent_sessions);
    let execution_session_id = placement_thread_id.to_string();
    let is_runtime_recovery = command_kind == "reattach";
    let progress_state = state.clone();
    let progress_session = session.clone();
    let progress_sequence = record.command_sequence;
    let progress_digest = request_digest.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let contact_deadline = contact_deadline_at_ms.map(|deadline_at_ms| {
            let now = std::time::Instant::now();
            let remaining_ms = deadline_at_ms
                .saturating_sub(lillux::time::timestamp_millis())
                .max(0) as u64;
            now + std::time::Duration::from_millis(remaining_ms)
        });
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
            pool.execute_exclusive_control_with_deadline(
                &execution_session_id,
                body,
                contact_deadline,
            )
        } else {
            // Recheck at the pool's actual pre-write boundary, after blocking
            // task scheduling and pool-lock acquisition. A retained count
            // reservation cannot turn queueing past expiry into new contact.
            pool.execute_exclusive_with_deadline(
                &execution_session_id,
                payload,
                || {
                    contact_deadline_at_ms
                        .is_some_and(|deadline| lillux::time::timestamp_millis() >= deadline)
                },
                |delta| {
                    let gate = transition_gate(&execution_session_id);
                    let _guard = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let batch = canonical_command_observation_batch(&delta, 1)?;
                    validate_command_progress_batch(&batch)?;
                    validate_new_state_transition_sequence(&progress_state, &execution_session_id, worker_boot_epoch, &batch)?;
                    // The enclosing command already owns root and credential
                    // contact leases. This is its causal settlement, not new
                    // ingress: reacquiring a root gate here can deadlock stop.
                    append_command_observation_batch_phase(&progress_state, &progress_session, worker_boot_epoch,
                        progress_sequence, &progress_digest, &delta, &batch, true)?;
                    apply_worker_observations(&progress_state, &execution_session_id, worker_boot_epoch, &batch, 1)?;
                    notify_projection_change(&execution_session_id);
                    Ok(Some(json!({"command_progress_digest":ryeos_state::objects::canonical_value_digest(&delta)?})))
                },
                contact_deadline,
            )
        }
    })
    .await?;
    match outcome {
        Ok(result) => {
            let transition_gate = transition_gate(placement_thread_id);
            let _transition_guard = transition_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = canonical_command_observation_batch(&result, observation_limit)
                .and_then(|canonical_batch| {
                    let remaining = command_batch_after_progress(
                        state,
                        &session,
                        worker_boot_epoch,
                        record.command_sequence,
                        &request_digest,
                        &canonical_batch,
                    )?;
                    validate_new_state_transition_sequence(
                        state,
                        placement_thread_id,
                        worker_boot_epoch,
                        &remaining,
                    )?;
                    append_command_observation_batch(
                        state,
                        &session,
                        worker_boot_epoch,
                        record.command_sequence,
                        &request_digest,
                        &result,
                        &canonical_batch,
                    )?;
                    project_worker_events(state, &session, worker_boot_epoch, &canonical_batch)?;
                    apply_worker_observations(
                        state,
                        placement_thread_id,
                        worker_boot_epoch,
                        &remaining,
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
    let profile = admitted_structured_protocol(state, capsule_hash)?;
    Ok((profile.profile_hash, profile.schema_hashes))
}

fn admitted_structured_protocol(
    state: &AppState,
    capsule_hash: &str,
) -> Result<ryeos_state::objects::AdmittedStructuredSessionProfile> {
    admitted_session_capsule(state, capsule_hash)?
        .structured_session_profile
        .ok_or_else(|| anyhow!("structured session capsule has no admitted protocol profile"))
}

fn admitted_session_capsule(
    state: &AppState,
    capsule_hash: &str,
) -> Result<ryeos_state::objects::AdmittedPersistentSessionCapsule> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(capsule_hash)?
        .ok_or_else(|| anyhow!("admitted session capsule disappeared"))?;
    // Check the retained bytes before even classifying an unsupported envelope.
    // A schema mismatch must not hide CAS corruption.
    if ryeos_state::objects::canonical_value_digest(&value)? != capsule_hash {
        bail!("admitted session capsule content hash changed");
    }
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != capsule_hash {
        bail!("admitted session capsule content hash changed");
    }
    Ok(capsule)
}

/// An already-terminal, detached placement has no command recovery authority.
/// Retain its predecessor capsule and outbox as opaque history, not a decoded
/// current protocol, successful settlement, or permission to retry. Live or
/// still-attached placements must continue through the ordinary cleanup/recovery
/// fences; a nonappendable handoff alone is not terminal history.
fn terminal_session_retains_predecessor_capsule(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<bool> {
    if session.state != "terminal"
        || session.worker_instance_id.is_some()
        || session.worker_boot_epoch.is_some()
        || state
            .state_store
            .placement_has_unsettled_worker(&session.placement_thread_id)?
        || state
            .state_store
            .get_thread_terminal_authority(&session.placement_thread_id)?
            .is_none()
    {
        return Ok(false);
    }
    match admitted_session_capsule(state, &session.admitted_capsule_hash) {
        Ok(_) => Ok(false),
        Err(error)
            if error
                .downcast_ref::<ryeos_state::IncompatibleCurrentObjectSchema>()
                .is_some_and(ryeos_state::IncompatibleCurrentObjectSchema::is_predecessor) =>
        {
            // The projection cannot choose an unrelated old capsule to evade
            // replay validation. Its immutable command fact must independently
            // name this exact capsule and canonical invocation. This does not
            // decode the predecessor protocol or confer settlement authority.
            if retained_committed_command_fact(state, session, record)?.is_none() {
                bail!("terminal predecessor command has no immutable capsule association");
            }
            tracing::warn!(
                placement_thread_id = %session.placement_thread_id,
                admitted_capsule_hash = %session.admitted_capsule_hash,
                %error,
                "preserving terminal predecessor session command outbox as opaque history"
            );
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Select from already-compiled launch authority; this is not another profile
/// compiler or provider request-schema validator. The bridge still validates
/// payload semantics and runtime session binding. Visibility is checked here
/// because even sending a refused command crosses the durable contact boundary.
fn validate_public_command_surface(
    contract: &Value,
    worker_execution: &Value,
    payload: &Value,
) -> Result<()> {
    let command = payload
        .as_object()
        .filter(|command| {
            command.len() == 2
                && command.contains_key("route_id")
                && command.contains_key("payload")
        })
        .ok_or_else(|| {
            anyhow!("public structured-session command requires only route_id and payload")
        })?;
    let route_id = command["route_id"]
        .as_str()
        .ok_or_else(|| anyhow!("public structured-session command has no route id"))?;
    let route_set = worker_execution
        .get("route_set")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("hosted root lost its admitted route selection"))?;
    let selected = contract
        .get("route_sets")
        .and_then(|sets| sets.get(route_set))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("admitted structured-session route set disappeared"))?;
    if !selected.iter().any(|id| id.as_str() == Some(route_id)) {
        bail!("structured-session route is not admitted for this execution");
    }
    let route = contract
        .get("routes")
        .and_then(Value::as_array)
        .and_then(|routes| {
            routes
                .iter()
                .find(|route| route.get("id").and_then(Value::as_str) == Some(route_id))
        })
        .ok_or_else(|| anyhow!("admitted structured-session route is undefined"))?;
    // Omitted audience means public in the existing signed profile contract.
    // Null/unknown values are not omitted and cannot acquire public authority.
    if route
        .get("audience")
        .is_some_and(|audience| audience.as_str() != Some("public"))
    {
        bail!("structured-session route is not available on this command surface");
    }
    Ok(())
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
    let budget_uncontacted = record.result.as_ref().is_some_and(|result| {
        result.get("error").and_then(Value::as_str) == Some("budget_exhausted")
            && result.get("retryable_uncontacted").and_then(Value::as_bool) == Some(false)
    });
    if retryable_uncontacted || budget_uncontacted {
        if record.state != "failed" {
            bail!("uncontacted command projection is not failed");
        }
        if command_fact_exists(
            state,
            session,
            "hosted_command.contacting",
            record.command_sequence,
            &record.request_digest,
            record.worker_boot_epoch,
        )? {
            bail!("uncontacted command has exact possible-contact testimony");
        }
        if retryable_uncontacted {
            let expected_result = json!({
                "error":"worker epoch ended before contact",
                "retryable_uncontacted":true,
            });
            if record.result.as_ref() != Some(&expected_result) {
                bail!("retryable uncontacted command projection has a contradictory result");
            }
        } else {
            let result = record.result.as_ref().expect("budget result checked above");
            let object = result
                .as_object()
                .ok_or_else(|| anyhow!("budget refusal result is not an object"))?;
            let dimension = object.get("budget_dimension").and_then(Value::as_str);
            let reason = object.get("budget_reason").and_then(Value::as_str);
            if object.len() != 4
                || !matches!(
                    (dimension, reason),
                    (Some("duration"), Some("aggregate_duration_exhausted"))
                        | (
                            Some("provider_contacts"),
                            Some("aggregate_provider_contacts_exhausted")
                        )
                )
            {
                bail!("budget refusal command projection has a contradictory result");
            }
        }
        let fact = command_fact_payload(
            state,
            session,
            "hosted_command.failed_uncontacted",
            record.command_sequence,
            &record.request_digest,
            record.worker_boot_epoch,
        )?;
        let valid = fact.is_some_and(|payload| {
            if retryable_uncontacted {
                payload.get("origin").and_then(Value::as_str) == Some("daemon_verified_process")
                    && payload
                        .get("retryable_uncontacted")
                        .and_then(Value::as_bool)
                        == Some(true)
            } else {
                payload.get("origin").and_then(Value::as_str) == Some("daemon_budget_authority")
                    && payload.get("budget_exhausted").and_then(Value::as_bool) == Some(true)
                    && payload
                        .get("retryable_uncontacted")
                        .and_then(Value::as_bool)
                        == Some(false)
                    && payload.get("budget_dimension")
                        == record
                            .result
                            .as_ref()
                            .and_then(|result| result.get("budget_dimension"))
                    && payload.get("budget_reason")
                        == record
                            .result
                            .as_ref()
                            .and_then(|result| result.get("budget_reason"))
            }
        });
        if valid && retryable_uncontacted {
            release_bounded_turn_contact_if_verified_uncontacted(state, session, record)?;
        }
        return Ok(valid);
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
    let Some(payload) = retained_committed_command_fact(state, session, record)? else {
        return Ok(false);
    };
    let (protocol_profile_hash, protocol_schema_hashes) =
        structured_protocol_identity(state, &session.admitted_capsule_hash)?;
    if payload.get("protocol_profile_hash").and_then(Value::as_str)
        != Some(protocol_profile_hash.as_str())
        || payload.get("protocol_schema_hashes")
            != Some(&serde_json::to_value(protocol_schema_hashes)?)
    {
        bail!("authoritative hosted command fact does not retain its exact command contract");
    }
    Ok(true)
}

/// Immutable command/capsule association only. Current replay additionally
/// validates the frozen protocol in `committed_command_fact_exists`; opaque
/// historical retention must never be mistaken for that stronger authority.
fn retained_committed_command_fact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<Option<Value>> {
    if !command_fact_exists(
        state,
        session,
        "hosted_command.committed",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )? {
        return Ok(None);
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
            == Some(session.admitted_capsule_hash.as_str());
    if !exact {
        bail!("authoritative hosted command fact does not retain its exact command contract");
    }
    Ok(Some(payload))
}

fn append_command_observation_batch(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    command_sequence: u64,
    request_digest: &str,
    result: &Value,
    canonical_batch: &Value,
) -> Result<()> {
    append_command_observation_batch_phase(
        state,
        session,
        worker_boot_epoch,
        command_sequence,
        request_digest,
        result,
        canonical_batch,
        false,
    )
}

fn validate_command_progress_batch(batch: &Value) -> Result<()> {
    if batch
        .get("events")
        .and_then(Value::as_array)
        .is_none_or(|events| !events.is_empty())
    {
        bail!("command progress must not carry unrelated events");
    }
    let values = batch
        .get("session_observations")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 1)
        .ok_or_else(|| anyhow!("command progress must contain one lifecycle start"))?;
    match serde_json::from_value::<WorkerObservation>(values[0].clone())? {
        WorkerObservation::State {
            expected,
            next,
            turn_id: Some(turn_id),
            completed_turn_id: None,
        } if expected == "idle" && next == "turn_running" => {
            validate_hosted_turn_id("command progress turn", &turn_id)
        }
        _ => bail!("command progress is not a turn-start observation"),
    }
}

fn retained_command_progress(
    state: &AppState,
    session: &DedicatedSessionRecord,
    epoch: u64,
    sequence: u64,
    request_digest: &str,
) -> Result<Option<Value>> {
    let Some(fact) = command_fact_payload(
        state,
        session,
        "hosted_worker_command_progress",
        sequence,
        request_digest,
        epoch,
    )?
    else {
        return Ok(None);
    };
    let batch = fact
        .get("canonical_batch")
        .ok_or_else(|| anyhow!("command progress lacks its canonical batch"))?;
    validate_command_progress_batch(batch)?;
    validate_authoritative_state_transition_facts(
        state,
        session,
        epoch,
        batch,
        json!({
            "kind":"command_progress","batch_operation_id":fact["operation_id"],
            "command_sequence":sequence,"request_digest":request_digest,
        }),
        Some((sequence, request_digest)),
    )?;
    Ok(Some(batch.clone()))
}

fn command_batch_after_progress(
    state: &AppState,
    session: &DedicatedSessionRecord,
    epoch: u64,
    sequence: u64,
    request_digest: &str,
    batch: &Value,
) -> Result<Value> {
    let Some(progress) =
        retained_command_progress(state, session, epoch, sequence, request_digest)?
    else {
        return Ok(batch.clone());
    };
    let early = &progress["session_observations"][0];
    let mut remaining = batch.clone();
    let observations = remaining["session_observations"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("command observations are absent"))?;
    if observations.iter().filter(|value| *value == early).count() != 1 {
        bail!("final command batch does not corroborate its exact early start");
    }
    observations.retain(|value| value != early);
    Ok(remaining)
}

#[allow(clippy::too_many_arguments)]
fn append_command_observation_batch_phase(
    state: &AppState,
    session: &DedicatedSessionRecord,
    worker_boot_epoch: u64,
    command_sequence: u64,
    request_digest: &str,
    result: &Value,
    canonical_batch: &Value,
    progress: bool,
) -> Result<()> {
    let event_type = if progress {
        "hosted_worker_command_progress"
    } else {
        "hosted_worker_command_observation_batch"
    };
    let response_digest = ryeos_state::objects::canonical_value_digest(result)?;
    let batch_operation_id =
        command_fact_operation_id(session, event_type, command_sequence, request_digest)?;
    let mut followups = state_transition_fact_events(
        session,
        worker_boot_epoch,
        canonical_batch,
        json!({
            "kind":if progress { "command_progress" } else { "command_response" },
            "batch_operation_id":batch_operation_id,
            "command_sequence":command_sequence,
            "request_digest":request_digest,
        }),
        Some((command_sequence, request_digest)),
    )?;
    if !progress {
        let mut new = Vec::new();
        for transition in followups {
            let existing = crate::authoritative_root_fact::lookup(
                state,
                &session.placement_thread_id,
                &transition.event_type,
                transition.payload["operation_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("transition has no operation identity"))?,
            )?;
            if existing.count == 1
                && existing.payload.as_ref().is_some_and(|actual| {
                    corroborates_command_progress(session, &transition, actual)
                })
            {
                continue;
            }
            new.push(transition);
        }
        followups = new;
    }
    require_new_state_transition_facts(state, session, &followups)?;
    followups.extend(approval_request_fact_events(
        session,
        worker_boot_epoch,
        canonical_batch,
    )?);
    append_command_fact_once_with_followups(
        state,
        session,
        event_type,
        command_sequence,
        request_digest,
        json!({
            "schema":1,
            "origin":"daemon_observed_io",
            "worker_boot_epoch":worker_boot_epoch,
            "response_digest":response_digest,
            "canonical_batch":canonical_batch,
        }),
        &followups,
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
        let upstream_session_id = session.remote_thread_id.as_deref().ok_or_else(|| {
            anyhow!("completed command-started turn has no retained upstream session")
        })?;
        let child_executions =
            workload_child_execution_facts(state, &session, upstream_session_id, &turn_id)?;
        result["completion_fence"] = serde_json::to_value(HostedCommandCompletionFence {
            placement_thread_id: session.placement_thread_id,
            admitted_capsule_hash: session.admitted_capsule_hash,
            worker_boot_epoch: record.worker_boot_epoch,
            command_sequence: record.command_sequence,
            request_digest: record.request_digest,
            turn_id,
            completion_operation_id,
        })?;
        result["child_executions"] = child_executions;
    }
    Ok(result)
}

/// Project every retained workload-client child execution of one exact
/// structured-session turn from existing state: typed runtime action intents
/// joined to each child thread's authoritative terminal snapshot. This is an
/// observation read for the hosted operator, who cannot reach the generic
/// thread-children listing surface; it re-executes nothing and grants
/// nothing. A dispatch whose child snapshot contradicts the placement's
/// ownership fails closed instead of projecting mixed authority.
fn workload_child_execution_facts(
    state: &AppState,
    session: &DedicatedSessionRecord,
    upstream_session_id: &str,
    upstream_operation_id: &str,
) -> Result<Value> {
    let dispatches = state.state_store.workload_child_dispatches(
        &session.placement_thread_id,
        upstream_session_id,
        upstream_operation_id,
    )?;
    let owner = session.owner_principal.as_str();
    let mut children = Vec::with_capacity(dispatches.len());
    for dispatch in dispatches {
        let child = state
            .state_store
            .get_authoritative_root_thread_snapshot(&dispatch.child_thread_id)?
            .ok_or_else(|| {
                anyhow!(
                    "workload child dispatch `{}` has no authoritative snapshot",
                    dispatch.operation_id
                )
            })?;
        validate_workload_child_identity(
            owner,
            &dispatch.child_thread_id,
            &child.thread_id,
            &child.chain_root_id,
            child.requested_by.as_deref(),
        )?;
        children.push(json!({
            "operation_id": dispatch.operation_id,
            "mode": dispatch.mode.as_str(),
            "child_thread_id": dispatch.child_thread_id,
            "created_at_ms": dispatch.created_at_ms,
            "invocation": dispatch.workload_invocation,
            "item_ref": child.item_ref,
            "status": child.status.as_str(),
            "outcome_code": child.outcome_code,
            "finished_at": child.finished_at,
            "error": child.error,
            "admitted_launch_capsule_hash": child.admitted_launch_capsule_hash,
            "result": child.result,
        }));
    }
    Ok(Value::Array(children))
}

fn validate_workload_child_identity(
    owner: &str,
    dispatched_child_thread_id: &str,
    child_thread_id: &str,
    child_chain_root_id: &str,
    child_requested_by: Option<&str>,
) -> Result<()> {
    if child_thread_id != dispatched_child_thread_id
        || child_chain_root_id != dispatched_child_thread_id
        || child_requested_by != Some(owner)
    {
        bail!("workload child dispatch contradicts its placement ownership");
    }
    Ok(())
}

fn bounded_attempt_key(
    placement_thread_id: &str,
    idempotency_key: &str,
) -> Result<Option<(&'static str, u32)>> {
    let prefix = format!("bounded:{placement_thread_id}:");
    let Some(remainder) = idempotency_key.strip_prefix(&prefix) else {
        if idempotency_key.starts_with("bounded:") {
            bail!("bounded command key names another placement");
        }
        return Ok(None);
    };
    for step in ["session-start", "turn-start"] {
        let attempt_prefix = format!("{step}:attempt:");
        let Some(raw_attempt) = remainder.strip_prefix(&attempt_prefix) else {
            continue;
        };
        let attempt = raw_attempt
            .parse::<u32>()
            .context("parse bounded command attempt")?;
        if !(1..=8).contains(&attempt) || raw_attempt != attempt.to_string() {
            bail!("bounded command attempt is not canonical");
        }
        return Ok(Some((step, attempt)));
    }
    bail!("bounded command key has an unknown step")
}

fn bounded_command_contact(
    state: &AppState,
    session: &DedicatedSessionRecord,
    record: &crate::runtime_db::DedicatedSessionCommandRecord,
) -> Result<&'static str> {
    if record.command_kind != "route" || !committed_command_fact_exists(state, session, record)? {
        bail!("bounded command has no exact committed route testimony");
    }
    let contacting = command_fact_exists(
        state,
        session,
        "hosted_command.contacting",
        record.command_sequence,
        &record.request_digest,
        record.worker_boot_epoch,
    )?;
    match record.state.as_str() {
        "completed" => {
            if !contacting || !authoritative_settled_command_replay(state, session, record)? {
                bail!("completed bounded command has no exact contact settlement");
            }
            Ok("contacted_settled")
        }
        "failed"
            if record.result.as_ref().is_some_and(|result| {
                result.as_object().is_some_and(|object| {
                    object.len() == 2
                        && object.get("error").and_then(Value::as_str)
                            == Some("worker epoch ended before contact")
                        && object.get("retryable_uncontacted").and_then(Value::as_bool)
                            == Some(true)
                })
            }) =>
        {
            if contacting || !authoritative_settled_command_replay(state, session, record)? {
                bail!("uncontacted bounded command has contradictory contact testimony");
            }
            Ok("verified_uncontacted")
        }
        "failed"
            if record.result.as_ref().is_some_and(|result| {
                result.get("error").and_then(Value::as_str) == Some("budget_exhausted")
                    && result.get("retryable_uncontacted").and_then(Value::as_bool) == Some(false)
            }) =>
        {
            if contacting || !authoritative_settled_command_replay(state, session, record)? {
                bail!("budget-exhausted command has contradictory contact testimony");
            }
            Ok("budget_exhausted_uncontacted")
        }
        "failed" => {
            if !contacting || !authoritative_settled_command_replay(state, session, record)? {
                bail!("failed bounded command has no exact contact settlement");
            }
            Ok("contacted_failed")
        }
        "committed" if !contacting => Ok("reserved_uncontacted"),
        "committed" | "dispatched" | "outcome_unknown" if contacting => Ok("outcome_unknown"),
        _ => bail!("bounded command has a contradictory durable contact state"),
    }
}

fn validate_next_bounded_attempt(
    step: &str,
    attempt: u32,
    command_sequence: Option<u64>,
    session_attempts: &mut Vec<(
        u32,
        crate::runtime_db::DedicatedSessionCommandRecord,
        &'static str,
    )>,
    turn_attempts: &mut Vec<(
        u32,
        crate::runtime_db::DedicatedSessionCommandRecord,
        &'static str,
    )>,
) -> Result<()> {
    let (_, session_settled, session_contact) = canonical_bounded_attempts(session_attempts)?;
    let (_, _, turn_contact) = canonical_bounded_attempts(turn_attempts)?;
    if let Some(sequence) = command_sequence {
        if session_attempts
            .iter()
            .chain(turn_attempts.iter())
            .any(|(_, record, _)| record.command_sequence >= sequence)
        {
            bail!("bounded command reservation does not follow its predecessor ledger");
        }
    }
    let (prior_count, prior_contact) = match step {
        "session-start" => {
            if !turn_attempts.is_empty() {
                bail!("bounded session start cannot follow a turn attempt");
            }
            (session_attempts.len(), session_contact)
        }
        "turn-start" => {
            let (_, session_sequence) = session_settled
                .ok_or_else(|| anyhow!("bounded turn requires an exactly settled session start"))?;
            if turn_attempts
                .iter()
                .any(|(_, record, _)| record.command_sequence <= session_sequence)
            {
                bail!("bounded turn attempts precede their settled session start");
            }
            (turn_attempts.len(), turn_contact)
        }
        _ => bail!("bounded command has an unknown step"),
    };
    if attempt != u32::try_from(prior_count + 1)? {
        bail!("bounded command attempt does not immediately follow its predecessor ledger");
    }
    if prior_count > 0 && prior_contact != Some("verified_uncontacted") {
        bail!("bounded command retry requires exact verified-uncontacted predecessor testimony");
    }
    Ok(())
}

fn canonical_bounded_attempts(
    attempts: &mut Vec<(
        u32,
        crate::runtime_db::DedicatedSessionCommandRecord,
        &'static str,
    )>,
) -> Result<(Vec<Value>, Option<(u32, u64)>, Option<&'static str>)> {
    attempts.sort_by_key(|(attempt, _, _)| *attempt);
    let mut projected = Vec::with_capacity(attempts.len());
    let mut settled = None;
    let mut last_contact = None;
    let mut last_sequence = 0;
    for (index, (attempt, record, contact)) in attempts.iter().enumerate() {
        let expected_attempt = u32::try_from(index + 1)?;
        if *attempt != expected_attempt || record.command_sequence <= last_sequence {
            bail!("bounded command attempts are not a contiguous ordered ledger");
        }
        if settled.is_some() {
            bail!("bounded command ledger continued after a settled attempt");
        }
        if index + 1 < attempts.len() && *contact != "verified_uncontacted" {
            bail!("bounded command advanced without verified-uncontacted testimony");
        }
        if record.state == "completed" {
            settled = Some((*attempt, record.command_sequence));
        }
        last_sequence = record.command_sequence;
        last_contact = Some(*contact);
        projected.push(json!({
            "attempt":attempt,
            "command_sequence":record.command_sequence,
            "idempotency_key":record.idempotency_key,
            "worker_boot_epoch":record.worker_boot_epoch,
            "request_digest":record.request_digest,
            "state":record.state,
            "contact":contact,
        }));
    }
    Ok((projected, settled, last_contact))
}

fn bounded_duration_authority_reached(
    state: &AppState,
    session: &DedicatedSessionRecord,
) -> Result<bool> {
    let launch = state
        .state_store
        .admitted_launch_capsule(&session.placement_thread_id)?
        .ok_or_else(|| anyhow!("bounded worker admitted launch capsule disappeared"))?;
    let session_deadline = bounded_session_deadline(
        admitted_worker_execution_config(&launch)?,
        session.created_at_ms,
    )?
    .ok_or_else(|| anyhow!("retained worker session was not admitted as a bounded turn"))?;
    let now_ms = lillux::time::timestamp_millis();
    if now_ms >= session_deadline {
        return Ok(true);
    }
    let Some(scope) = launch.accounting_scope.as_ref() else {
        return Ok(false);
    };
    let accounting = state
        .accounting
        .as_ref()
        .ok_or_else(|| anyhow!("bounded worker accounting scope has no live ledger"))?;
    let budget = accounting
        .execution_resource_budget_snapshot(&scope.execution_budget_id)?
        .ok_or_else(|| anyhow!("bounded worker lost its aggregate budget authority"))?;
    Ok(budget.deadline_at_ms.is_some_and(|deadline| {
        let terminalization_boundary = deadline
            .checked_sub(DEDICATED_SESSION_AGGREGATE_TERMINALIZATION_RESERVE_MS)
            .unwrap_or(i64::MIN);
        now_ms >= terminalization_boundary
    }))
}

fn validate_hosted_approval_fence(
    state: &AppState,
    session: &DedicatedSessionRecord,
    outcome: &DedicatedSessionBoundedOutcome,
    fence: &HostedApprovalFence,
) -> Result<()> {
    if fence.chain_root_id != session.chain_root_id
        || fence.placement_thread_id != session.placement_thread_id
        || fence.admitted_capsule_hash != session.admitted_capsule_hash
        || fence.worker_boot_epoch == 0
        || !lillux::valid_hash(&fence.admitted_capsule_hash)
        || !lillux::valid_hash(&fence.approval_id)
        || !lillux::valid_hash(&fence.request_digest)
        || !lillux::valid_hash(&fence.approval_operation_id)
    {
        bail!("bounded approval fence differs from its hosted placement");
    }
    validate_hosted_turn_id("bounded approval turn id", &fence.turn_id)?;
    let record = state
        .state_store
        .dedicated_session_approval(&fence.placement_thread_id, &fence.approval_id)?
        .ok_or_else(|| anyhow!("bounded approval fence has no durable approval projection"))?;
    let require_pending = match record.state.as_str() {
        "pending" => true,
        "stale_epoch" if session.bounded_outcome.as_ref() == Some(outcome) => false,
        _ => bail!("bounded approval fence is not unresolved at its terminal boundary"),
    };
    let projected = approval_fence_from_record(state, session, &record, require_pending)?;
    if projected != *fence {
        bail!("bounded approval fence names another durable approval request");
    }
    Ok(())
}

fn validate_bounded_budget_outcome_authority(
    state: &AppState,
    session: &DedicatedSessionRecord,
    outcome: &DedicatedSessionBoundedOutcome,
) -> Result<()> {
    if outcome.kind == DedicatedSessionBoundedOutcomeKind::ApprovalRequired {
        if outcome.dimension.is_some() {
            bail!("bounded approval outcome cannot carry a budget dimension");
        }
        let fence = outcome
            .approval
            .as_ref()
            .ok_or_else(|| anyhow!("bounded approval outcome has no exact approval fence"))?;
        return validate_hosted_approval_fence(state, session, outcome, fence);
    }
    if outcome.approval.is_some() {
        bail!("only bounded approval-required may carry an approval fence");
    }
    if outcome.kind != DedicatedSessionBoundedOutcomeKind::BudgetExhausted {
        if outcome.dimension.is_some() {
            bail!("only bounded budget exhaustion may carry a dimension");
        }
        return Ok(());
    }
    match outcome.dimension {
        Some(ryeos_runtime::callback::DedicatedSessionBoundedBudgetDimension::Duration) => {
            if !bounded_duration_authority_reached(state, session)? {
                bail!("bounded duration exhaustion has no reached durable deadline");
            }
        }
        Some(ryeos_runtime::callback::DedicatedSessionBoundedBudgetDimension::ProviderContacts) => {
            let mut proved = false;
            for record in state
                .state_store
                .dedicated_session_commands(&session.placement_thread_id)?
            {
                if bounded_attempt_key(&session.placement_thread_id, &record.idempotency_key)?
                    .is_some_and(|(step, _)| step == "turn-start")
                    && bounded_command_contact(state, session, &record)?
                        == "budget_exhausted_uncontacted"
                    && record.result.as_ref().is_some_and(|result| {
                        result.get("budget_dimension").and_then(Value::as_str)
                            == Some("provider_contacts")
                            && result.get("budget_reason").and_then(Value::as_str)
                                == Some("aggregate_provider_contacts_exhausted")
                    })
                {
                    proved = true;
                }
            }
            if !proved {
                bail!("bounded provider-contact exhaustion has no exact uncontacted budget fact");
            }
        }
        Some(ryeos_runtime::callback::DedicatedSessionBoundedBudgetDimension::WorkerExecutions) => {
            bail!("worker-execution exhaustion is valid only before session admission")
        }
        None => bail!("bounded budget exhaustion requires its exact dimension"),
    }
    Ok(())
}

fn candidate_publication_error(
    error: impl std::fmt::Display,
) -> crate::handler_error::HandlerError {
    crate::handler_error::HandlerError::Internal(error.to_string())
}

#[derive(serde::Deserialize)]
pub struct CandidatePublicationAuthority {
    pub source_candidate_snapshot_hash: String,
    pub candidate_snapshot_hash: String,
    pub candidate_validation_hash: String,
    #[serde(rename = "expected_previous_hash")]
    pub base_snapshot_hash: String,
    pub principal_key: String,
    pub project_hash: String,
    pub candidate_evaluation_hash: String,
}

pub fn candidate_publication_operation_id(
    publication_root_id: &str,
    session: &crate::state_store::DedicatedSessionRecord,
    authority: &CandidatePublicationAuthority,
) -> Result<String, crate::handler_error::HandlerError> {
    ryeos_state::objects::canonical_value_digest(&json!({
        "schema":"ryeos.hosted_candidate_publication_operation.v2",
        "publication_root_id":publication_root_id,
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "source_candidate_snapshot_hash":authority.source_candidate_snapshot_hash,
        "candidate_snapshot_hash":authority.candidate_snapshot_hash,
        "expected_previous_hash":authority.base_snapshot_hash,
        "candidate_validation_hash":authority.candidate_validation_hash,
        "candidate_evaluation_hash":authority.candidate_evaluation_hash,
        "principal_key":authority.principal_key,
        "project_hash":authority.project_hash,
    }))
    .map_err(candidate_publication_error)
}

pub fn candidate_publication_reservation_payload(
    session: &crate::state_store::DedicatedSessionRecord,
    authority: &CandidatePublicationAuthority,
    publication_root_id: &str,
) -> Value {
    json!({
        "schema":2,
        "origin":"owner_authorized",
        "owner_principal":session.owner_principal,
        "publication_root_id":publication_root_id,
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "source_candidate_snapshot_hash":authority.source_candidate_snapshot_hash,
        "candidate_snapshot_hash":authority.candidate_snapshot_hash,
        "expected_previous_hash":authority.base_snapshot_hash,
        "candidate_validation_hash":authority.candidate_validation_hash,
        "candidate_evaluation_hash":authority.candidate_evaluation_hash,
        "principal_key":authority.principal_key,
        "project_hash":authority.project_hash,
    })
}

pub fn candidate_publication_result_payload(
    session: &crate::state_store::DedicatedSessionRecord,
    authority: &CandidatePublicationAuthority,
    publication_root_id: &str,
    operation_id: &str,
    outcome: &str,
    recovered_after_head_contact: bool,
) -> Value {
    json!({
        "schema":2,
        "origin":"project_head_cas_observed",
        "owner_principal":session.owner_principal,
        "publication_root_id":publication_root_id,
        "source_chain_root_id":session.chain_root_id,
        "source_placement_thread_id":session.placement_thread_id,
        "source_candidate_snapshot_hash":authority.source_candidate_snapshot_hash,
        "candidate_snapshot_hash":authority.candidate_snapshot_hash,
        "expected_previous_hash":authority.base_snapshot_hash,
        "candidate_validation_hash":authority.candidate_validation_hash,
        "candidate_evaluation_hash":authority.candidate_evaluation_hash,
        "principal_key":authority.principal_key,
        "project_hash":authority.project_hash,
        "reservation_operation_id":operation_id,
        "outcome":outcome,
        "recovered_after_head_contact":recovered_after_head_contact,
    })
}

pub fn verify_candidate_publication_reservation(
    state: &AppState,
    session: &crate::state_store::DedicatedSessionRecord,
    authority: &CandidatePublicationAuthority,
    publication_root_id: &str,
    operation_id: &str,
) -> Result<(), crate::handler_error::HandlerError> {
    let expected_operation_id =
        candidate_publication_operation_id(publication_root_id, session, authority)?;
    if operation_id != expected_operation_id {
        return Err(candidate_publication_error(
            "candidate publication projection names a contradictory operation",
        ));
    }
    let publication = state
        .state_store
        .get_thread(publication_root_id)
        .map_err(candidate_publication_error)?
        .ok_or_else(|| {
            candidate_publication_error("candidate publication operation root disappeared")
        })?;
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )
        .map_err(candidate_publication_error)?
        .ok_or_else(|| candidate_publication_error("candidate source root disappeared"))?;
    if publication.thread_id != publication_root_id
        || publication.chain_root_id != publication_root_id
        || publication_root_id == session.chain_root_id
        || publication.item_ref != "service:worker-executions/publish"
        || publication.requested_by.as_deref() != Some(session.owner_principal.as_str())
        || publication.current_site_id != source.current_site_id
        || publication.origin_site_id != source.origin_site_id
    {
        return Err(candidate_publication_error(
            "candidate publication operation root has contradictory authority",
        ));
    }
    let mut expected =
        candidate_publication_reservation_payload(session, authority, publication_root_id);
    expected
        .as_object_mut()
        .expect("candidate publication reservation is an object")
        .insert(
            "operation_id".into(),
            Value::String(operation_id.to_owned()),
        );
    let fact = crate::authoritative_root_fact::lookup(
        state,
        publication_root_id,
        "hosted_candidate.publication_reserved",
        operation_id,
    )
    .map_err(candidate_publication_error)?;
    if fact.count != 1 || fact.payload.as_ref() != Some(&expected) {
        return Err(candidate_publication_error(
            "candidate publication projection has no exact append-only reservation",
        ));
    }
    Ok(())
}

pub fn verify_candidate_publication_result(
    state: &AppState,
    session: &crate::state_store::DedicatedSessionRecord,
    authority: &CandidatePublicationAuthority,
    publication_root_id: &str,
    operation_id: &str,
    event_type: &str,
    outcome: &str,
) -> Result<bool, crate::handler_error::HandlerError> {
    let fact = crate::authoritative_root_fact::lookup(
        state,
        publication_root_id,
        event_type,
        operation_id,
    )
    .map_err(candidate_publication_error)?;
    if fact.count == 0 {
        return Ok(false);
    }
    let mut expected_live = candidate_publication_result_payload(
        session,
        authority,
        publication_root_id,
        operation_id,
        outcome,
        false,
    );
    expected_live
        .as_object_mut()
        .expect("candidate publication result is an object")
        .insert(
            "operation_id".into(),
            Value::String(operation_id.to_owned()),
        );
    let mut expected_recovered = candidate_publication_result_payload(
        session,
        authority,
        publication_root_id,
        operation_id,
        outcome,
        true,
    );
    expected_recovered
        .as_object_mut()
        .expect("candidate publication result is an object")
        .insert(
            "operation_id".into(),
            Value::String(operation_id.to_owned()),
        );
    if fact.count != 1
        || !matches!(fact.payload.as_ref(), Some(payload) if payload == &expected_live || payload == &expected_recovered)
    {
        return Err(candidate_publication_error(
            "candidate publication result fact contradicts its reservation",
        ));
    }
    Ok(true)
}

fn terminal_candidate_publication_outcome(
    session: &DedicatedSessionRecord,
    source_candidate_snapshot_hash: &str,
    authority: &CandidatePublicationAuthority,
) -> Result<&'static str> {
    let evaluation = session
        .candidate_evaluation
        .as_ref()
        .ok_or_else(|| anyhow!("terminal candidate publication has no accepted evaluation"))?;
    if session.state != "terminal"
        || session.terminal_reason.as_deref() != Some("completed")
        || session.candidate_snapshot_hash.as_deref() != Some(source_candidate_snapshot_hash)
        || authority.source_candidate_snapshot_hash != source_candidate_snapshot_hash
        || session.candidate_evaluation_hash.as_deref()
            != Some(authority.candidate_evaluation_hash.as_str())
        || ryeos_state::objects::canonical_value_digest(evaluation)?
            != authority.candidate_evaluation_hash
        || evaluation
            .pointer("/result/accepted")
            .and_then(Value::as_bool)
            != Some(true)
        || evaluation
            .pointer("/candidate/candidate_snapshot_hash")
            .and_then(Value::as_str)
            != Some(authority.candidate_snapshot_hash.as_str())
        || evaluation
            .pointer("/candidate/candidate_validation_hash")
            .and_then(Value::as_str)
            != Some(authority.candidate_validation_hash.as_str())
        || evaluation
            .pointer("/candidate/base_snapshot_hash")
            .and_then(Value::as_str)
            != Some(authority.base_snapshot_hash.as_str())
        || [
            &authority.source_candidate_snapshot_hash,
            &authority.candidate_snapshot_hash,
            &authority.candidate_validation_hash,
            &authority.candidate_evaluation_hash,
            &authority.base_snapshot_hash,
        ]
        .into_iter()
        .any(|hash| !lillux::valid_hash(hash))
    {
        bail!("terminal candidate publication contradicts its source or accepted target");
    }
    let published = format!("published:{}", authority.candidate_snapshot_hash);
    let unknown = format!("publication_unknown:{}", authority.candidate_snapshot_hash);
    match session.publication_result.as_deref() {
        Some(result) if result == published => Ok("published"),
        Some(result) if result == unknown => Ok("unknown"),
        _ => bail!("terminal candidate publication result names another target or outcome"),
    }
}

/// Verify a settled worker disposition without treating its retained C as the
/// publication target D. The exact publication root's reservation and unique
/// result are commit authority, including after a later unrelated HEAD move.
pub fn verify_terminal_candidate_publication(
    state: &AppState,
    session: &DedicatedSessionRecord,
    source_candidate_snapshot_hash: &str,
) -> Result<()> {
    let root_id = session
        .candidate_disposition_root_id
        .as_deref()
        .ok_or_else(|| anyhow!("terminal candidate publication has no reserved root"))?;
    let operation_id = session
        .candidate_disposition_operation_id
        .as_deref()
        .ok_or_else(|| anyhow!("terminal candidate publication has no reserved operation"))?;
    let reservation = crate::authoritative_root_fact::lookup(
        state,
        root_id,
        "hosted_candidate.publication_reserved",
        operation_id,
    )?;
    if reservation.count != 1 {
        bail!("terminal candidate publication has no unique rooted reservation");
    }
    let authority: CandidatePublicationAuthority = serde_json::from_value(
        reservation
            .payload
            .ok_or_else(|| anyhow!("publication reservation payload is absent"))?,
    )?;
    let outcome = terminal_candidate_publication_outcome(
        session,
        source_candidate_snapshot_hash,
        &authority,
    )?;
    verify_candidate_publication_reservation(state, session, &authority, root_id, operation_id)?;
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &session.chain_root_id,
            &session.placement_thread_id,
        )?
        .ok_or_else(|| anyhow!("candidate source root disappeared"))?;
    if !matches!(
        &source.project_authority,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { base_snapshot_hash, .. }
            if base_snapshot_hash == &authority.base_snapshot_hash
    ) {
        bail!("terminal candidate publication changed its admitted base");
    }
    let mut rooted_outcome = None;
    for (event_type, fact_outcome) in [
        ("hosted_candidate.published", "published"),
        ("hosted_candidate.publication_not_applied", "not_applied"),
        ("hosted_candidate.publication_unknown", "unknown"),
    ] {
        if verify_candidate_publication_result(
            state,
            session,
            &authority,
            root_id,
            operation_id,
            event_type,
            fact_outcome,
        )? && rooted_outcome.replace(fact_outcome).is_some()
        {
            bail!("terminal candidate publication has contradictory result facts");
        }
    }
    if rooted_outcome != Some(outcome) {
        bail!("terminal candidate publication has no exact rooted result");
    }
    Ok(())
}

/// Build the bounded worker's compact terminal evidence from its existing
/// durable authorities. The controller's process-local return value is never
/// retained as proof: command contact is revalidated against root facts, the
/// candidate base comes from the owned workspace, and accounting remains an
/// explicit unavailable value when this launch admitted no financial ledger.
pub fn canonical_terminal_session_projection(
    state: &AppState,
    placement_thread_id: &str,
) -> Result<Value> {
    let session = state
        .state_store
        .dedicated_session(placement_thread_id)?
        .ok_or_else(|| anyhow!("dedicated session is not admitted"))?;
    if session.state != "terminal" {
        bail!("canonical terminal session projection requires terminal state");
    }
    let mut result = serde_json::to_value(&session)?;
    if session.candidate_disposition
        != crate::runtime_db::DedicatedCandidateDisposition::RetainedForReview
    {
        return Ok(result);
    }
    let workspace = state
        .state_store
        .execution_workspace(&session.workspace_id)?
        .ok_or_else(|| anyhow!("bounded worker workspace disappeared"))?;
    if workspace.thread_id.as_deref() != Some(placement_thread_id) {
        bail!("bounded worker workspace belongs to another thread");
    }

    let mut session_attempts = Vec::new();
    let mut turn_attempts = Vec::new();
    for record in state
        .state_store
        .dedicated_session_commands(placement_thread_id)?
    {
        let Some((step, attempt)) =
            bounded_attempt_key(placement_thread_id, &record.idempotency_key)?
        else {
            if record.command_kind == "route" {
                bail!("bounded worker ledger contains a route outside its admitted steps");
            }
            continue;
        };
        let contact = bounded_command_contact(state, &session, &record)?;
        match step {
            "session-start" => session_attempts.push((attempt, record, contact)),
            "turn-start" => turn_attempts.push((attempt, record, contact)),
            _ => unreachable!("bounded attempt parser returns only admitted steps"),
        }
    }
    let (session_attempts, session_settled, session_contact) =
        canonical_bounded_attempts(&mut session_attempts)?;
    let (turn_attempts, turn_settled, turn_contact) =
        canonical_bounded_attempts(&mut turn_attempts)?;
    if !turn_attempts.is_empty() && session_settled.is_none() {
        bail!("bounded turn was attempted before session start settled");
    }
    if let (Some((_, session_sequence)), Some((_, turn_sequence))) = (session_settled, turn_settled)
        && session_sequence >= turn_sequence
    {
        bail!("bounded step command order is contradictory");
    }
    if let Some(fence) = session.completion_fence.as_ref() {
        if turn_settled.map(|(_, sequence)| sequence) != Some(fence.command_sequence) {
            bail!("bounded completion fence names another turn attempt");
        }
    } else if session.terminal_reason.as_deref() == Some("completed") {
        bail!("completed bounded worker has no completion fence");
    }

    let contact_outcome = if session.completion_fence.is_some() {
        "turn_completed"
    } else if matches!(turn_contact, Some("outcome_unknown" | "contacted_failed")) {
        "turn_outcome_unknown"
    } else if turn_settled.is_some() {
        "turn_contacted_not_completed"
    } else if turn_contact == Some("budget_exhausted_uncontacted") {
        "turn_budget_exhausted"
    } else if turn_contact == Some("verified_uncontacted") {
        "turn_verified_uncontacted"
    } else if matches!(
        session_contact,
        Some("outcome_unknown" | "contacted_failed")
    ) {
        "session_outcome_unknown"
    } else if session_settled.is_some() {
        "turn_not_contacted"
    } else if session_contact == Some("verified_uncontacted") {
        "session_verified_uncontacted"
    } else {
        "no_command_contact"
    };
    let launch_capsule = state
        .state_store
        .admitted_launch_capsule(placement_thread_id)?
        .ok_or_else(|| anyhow!("bounded worker admitted launch capsule disappeared"))?;
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &launch_capsule.execution_closure
    else {
        bail!("bounded worker does not have a managed runtime closure");
    };
    let admitted_mode = prepared_runtime_launch
        .get("runtime_data")
        .and_then(|value| value.get("worker_execution"))
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("bounded worker closure has no admitted mode"))?;
    if admitted_mode.get("kind").and_then(Value::as_str) != Some("bounded_turn") {
        bail!("retained worker session was not admitted as a bounded turn");
    }
    let max_uncontacted_attempts = admitted_mode
        .get("max_uncontacted_attempts")
        .and_then(Value::as_u64)
        .filter(|value| (1..=8).contains(value))
        .ok_or_else(|| anyhow!("bounded worker closure has no attempt ceiling"))?;
    let accounting_scope = serde_json::to_value(&launch_capsule.accounting_scope)?;
    let spend_authority = if launch_capsule.accounting_scope.is_some() {
        "unavailable"
    } else {
        "not_admitted"
    };
    let bounded_outcome = session
        .bounded_outcome
        .as_ref()
        .ok_or_else(|| anyhow!("terminal bounded worker has no durable bounded outcome"))?;
    validate_bounded_budget_outcome_authority(state, &session, bounded_outcome)?;
    let terminal_turn = turn_settled
        .map(|(_, command_sequence)| {
            match command_observation(state, placement_thread_id, command_sequence) {
                Ok(value) => value
                    .get("operation")
                    .cloned()
                    .ok_or_else(|| anyhow!("bounded turn command lost its operation coordinate")),
                Err(_)
                    if bounded_outcome.kind
                        == DedicatedSessionBoundedOutcomeKind::OutcomeUnknown =>
                {
                    Ok(Value::Null)
                }
                Err(error) => Err(error),
            }
        })
        .transpose()?;
    if (session.terminal_reason.as_deref() == Some("completed"))
        != (bounded_outcome.kind == DedicatedSessionBoundedOutcomeKind::Completed)
    {
        bail!("durable bounded outcome contradicts its lifecycle terminal reason");
    }
    let terminal_contact = turn_contact.or(session_contact);
    let exhausted_attempts = if turn_attempts.is_empty() {
        session_attempts.len()
    } else {
        turn_attempts.len()
    };
    match bounded_outcome.kind {
        DedicatedSessionBoundedOutcomeKind::Completed if session.completion_fence.is_none() => {
            bail!("completed bounded outcome has no exact completion fence")
        }
        DedicatedSessionBoundedOutcomeKind::RetryableUncontactedExhausted
            if terminal_contact != Some("verified_uncontacted")
                || u64::try_from(exhausted_attempts)? != max_uncontacted_attempts =>
        {
            bail!("bounded retry exhaustion is not proved by its admitted attempt ledger")
        }
        DedicatedSessionBoundedOutcomeKind::WorkerFailure
            if terminal_contact != Some("contacted_failed") =>
        {
            bail!("bounded worker failure has no exact failed-contact testimony")
        }
        _ => {}
    }
    let profile_ref = launch_capsule
        .exact_program
        .get("item_ref")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("worker_execution:"))
        .ok_or_else(|| anyhow!("bounded launch exact program has no worker-execution profile"))?;
    let bounded_execution = json!({
        "schema":"ryeos.bounded_worker_execution.v1",
        "profile":{
            "item_ref":profile_ref,
            "exact_program_hash":launch_capsule.exact_program_hash,
            "max_uncontacted_attempts":max_uncontacted_attempts,
        },
        "session_start":{
            "attempts":session_attempts,
            "settled":session_settled.map(|(attempt, command_sequence)| json!({
                "attempt":attempt,
                "command_sequence":command_sequence,
            })),
        },
        "turn_start":{
            "attempts":turn_attempts,
            "settled":turn_settled.map(|(attempt, command_sequence)| json!({
                "attempt":attempt,
                "command_sequence":command_sequence,
            })),
            "turn":terminal_turn,
        },
        "contact_outcome":contact_outcome,
        "spend":{
            "authority":spend_authority,
            "accounting_scope":accounting_scope,
            "value":Value::Null,
        },
    });
    let terminal_command = bounded_execution["turn_start"]["attempts"]
        .as_array()
        .and_then(|values| values.last())
        .map(|value| json!({"step":"turn_start","coordinate":value}))
        .or_else(|| {
            bounded_execution["session_start"]["attempts"]
                .as_array()
                .and_then(|values| values.last())
                .map(|value| json!({"step":"session_start","coordinate":value}))
        });
    let testimony = json!({
        "schema":"ryeos.bounded_worker_outcome.v1",
        "chain_root_id":session.chain_root_id,
        "placement_thread_id":session.placement_thread_id,
        "admitted_capsule_hash":session.admitted_capsule_hash,
        "profile":{
            "item_ref":profile_ref,
            "exact_program_hash":launch_capsule.exact_program_hash,
        },
        "workspace":{
            "workspace_id":session.workspace_id,
            "base_snapshot_hash":workspace.base_snapshot,
            "candidate_snapshot_hash":session.candidate_snapshot_hash,
        },
        "outcome":bounded_outcome,
        "terminal_reason":session.terminal_reason,
        "terminal_command":terminal_command,
        "completion_fence":session.completion_fence,
        "execution":bounded_execution,
    });
    let testimony_operation_id = ryeos_state::objects::canonical_value_digest(&testimony)?;
    crate::authoritative_root_fact::append_once(
        state,
        &session.placement_thread_id,
        "hosted_bounded_execution.terminal_outcome",
        &testimony_operation_id,
        testimony,
    )?;
    let object = result
        .as_object_mut()
        .ok_or_else(|| anyhow!("dedicated session projection is not an object"))?;
    object.insert(
        "base_snapshot_hash".to_owned(),
        Value::String(workspace.base_snapshot),
    );
    object.insert("bounded_execution".to_owned(), bounded_execution);
    object.insert(
        "bounded_outcome_testimony_operation_id".to_owned(),
        Value::String(testimony_operation_id),
    );
    Ok(result)
}

/// Retire one exact durable worker identity without treating registry absence
/// as process-death proof. Durable identity recovery is permitted only when
/// the registry owner is absent, never to overrule a current owner's pending
/// attachment or unproved reap obligation.
pub fn retire_worker_process(
    state: &AppState,
    placement_thread_id: &str,
    worker: &WorkerProcessRecord,
) -> Result<&'static str> {
    let registry_outcome = state
        .persistent_sessions
        .retire_exclusive(placement_thread_id)?;
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
    Ok(resolve_worker_retirement(
        registry_outcome,
        prove_from_identity,
    ))
}

fn resolve_worker_retirement(
    registry_outcome: ExclusiveRetirementOutcome,
    prove_from_identity: impl FnOnce() -> bool,
) -> &'static str {
    match registry_outcome {
        ExclusiveRetirementOutcome::Reaped => "reaped",
        // The current owner still has an attachment or a failed reap duty.
        // An empty scope/absent group cannot overrule that positive evidence.
        ExclusiveRetirementOutcome::Reserved | ExclusiveRetirementOutcome::Unproved => "unproved",
        ExclusiveRetirementOutcome::Absent => {
            if prove_from_identity() {
                "reaped"
            } else {
                "unproved"
            }
        }
    }
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

fn require_persisted_completion_fence(
    session: &DedicatedSessionRecord,
    fence: &HostedCommandCompletionFence,
) -> Result<()> {
    if session.completion_fence.as_ref() != Some(fence) {
        bail!("completed termination fence differs from the durable completion reservation");
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
    terminate_session_with_bounded_outcome(
        state,
        placement_thread_id,
        reason,
        completion_fence,
        None,
    )
    .await
}

pub async fn terminate_session_with_bounded_outcome(
    state: &AppState,
    placement_thread_id: &str,
    reason: &str,
    completion_fence: Option<&HostedCommandCompletionFence>,
    requested_bounded_outcome: Option<&DedicatedSessionBoundedOutcome>,
) -> Result<Value> {
    if !matches!(reason, "completed" | "cancelled") {
        bail!("terminal reason must be completed or cancelled");
    }
    if reason == "completed" && completion_fence.is_none() {
        bail!("completed termination requires an exact completed-command fence");
    }
    if reason == "cancelled" && completion_fence.is_some() {
        bail!("cancelled termination cannot carry a completed-command fence");
    }
    let initial = current_session(state, placement_thread_id)?;
    let bounded_outcome = match initial.candidate_disposition {
        crate::runtime_db::DedicatedCandidateDisposition::OwnerDecision => {
            if requested_bounded_outcome.is_some() {
                bail!("interactive dedicated sessions cannot carry a bounded outcome");
            }
            None
        }
        crate::runtime_db::DedicatedCandidateDisposition::RetainedForReview => {
            let outcome = if reason == "completed" {
                if requested_bounded_outcome.is_some() {
                    bail!("completed bounded outcome is derived from its completion fence");
                }
                DedicatedSessionBoundedOutcome {
                    kind: DedicatedSessionBoundedOutcomeKind::Completed,
                    dimension: None,
                    approval: None,
                }
            } else {
                requested_bounded_outcome
                    .cloned()
                    .unwrap_or(DedicatedSessionBoundedOutcome {
                        kind: DedicatedSessionBoundedOutcomeKind::OutcomeUnknown,
                        dimension: None,
                        approval: None,
                    })
            };
            if (reason == "completed")
                != (outcome.kind == DedicatedSessionBoundedOutcomeKind::Completed)
            {
                bail!("bounded outcome contradicts the dedicated lifecycle reason");
            }
            Some(outcome)
        }
    };
    if let Some(outcome) = bounded_outcome.as_ref() {
        validate_bounded_budget_outcome_authority(state, &initial, outcome)?;
    }
    // Session termination is a state transition, not another concurrent root
    // operation. The exclusive existing root gate first drains any admitted
    // workload child/capture, then prevents a new one from starting while the
    // worker is retired. This guard is deliberately not committed: the root
    // runner owns the later authoritative thread terminalization.
    let root_terminalization = if initial.state == "terminal" {
        None
    } else {
        // Draining a workload child may require that child's async task to
        // resume and release its root lease. Never park a Tokio worker here.
        Some(
            crate::hosted_operation::begin_hosted_root_terminalization_async(
                &state.state_store,
                &initial.placement_thread_id,
            )
            .await?,
        )
    };
    let _credential_operation =
        acquire_credential_profile_operation(&initial.credential_profile_id).await?;
    let session = current_session(state, placement_thread_id)?;
    if let Some(outcome) = bounded_outcome.as_ref() {
        // Validation above rejects malformed requests before taking the root
        // lease. Repeat it under that lease so an owner approval decision
        // cannot race an unattended approval-required terminal reservation.
        validate_bounded_budget_outcome_authority(state, &session, outcome)?;
    }
    if session.state == "terminal" {
        if let Some(outcome) = bounded_outcome.as_ref() {
            state
                .state_store
                .reserve_dedicated_session_bounded_outcome(placement_thread_id, outcome)?;
        }
        if session.terminal_reason.as_deref() != Some(reason) {
            bail!("terminal session reason conflicts with the requested retry");
        }
        if let Some(fence) = completion_fence {
            require_persisted_completion_fence(&session, fence)?;
            validate_hosted_command_completion_fence(state, &session, fence)?;
            state.state_store.require_dedicated_session_route_frontier(
                placement_thread_id,
                fence.command_sequence,
            )?;
        }
        finish_terminal_credential_cleanup(state, &session)?;
        notify_projection_change(placement_thread_id);
        let mut result = json!({
            "chain_root_id":session.chain_root_id,
            "placement_thread_id":placement_thread_id,
            "state":"terminal",
            "reason":reason,
            "idempotent":true,
        });
        if bounded_outcome.is_some() {
            result = canonical_terminal_session_projection(state, placement_thread_id)?;
        }
        return Ok(result);
    }
    let _root_terminalization = root_terminalization
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
        if let Some(outcome) = bounded_outcome.as_ref() {
            state
                .state_store
                .reserve_dedicated_session_bounded_outcome(placement_thread_id, outcome)?;
        }
        state
            .state_store
            .terminalize_unattached_dedicated_session(placement_thread_id, reason)?;
        notify_projection_change(placement_thread_id);
        if bounded_outcome.is_some() {
            return canonical_terminal_session_projection(state, placement_thread_id);
        }
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
                | "qualifying"
                | "publish_ready"
                | "publishing"
                | "discarding"
        )
    {
        let fence = completion_fence.expect("completed termination fence checked above");
        if let Some(outcome) = bounded_outcome.as_ref() {
            state
                .state_store
                .reserve_dedicated_session_bounded_completion(
                    placement_thread_id,
                    worker_boot_epoch,
                    fence,
                    outcome,
                )?;
        } else {
            state.state_store.reserve_dedicated_session_completion(
                placement_thread_id,
                worker_boot_epoch,
                fence,
            )?;
        }
    } else if reason == "completed" {
        require_persisted_completion_fence(
            &session,
            completion_fence.expect("completed termination fence checked above"),
        )?;
        if let Some(outcome) = bounded_outcome.as_ref() {
            state
                .state_store
                .reserve_dedicated_session_bounded_outcome(placement_thread_id, outcome)?;
        }
    } else if let Some(outcome) = bounded_outcome.as_ref() {
        state
            .state_store
            .reserve_dedicated_session_bounded_outcome(placement_thread_id, outcome)?;
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
            | "qualifying"
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
    if terminal.state == "terminal" && bounded_outcome.is_some() {
        return canonical_terminal_session_projection(state, placement_thread_id);
    }
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
/// This owner retires only session/worker/credential authority. Filesystem
/// closure belongs to the launch guard holding the ORIGINAL workspace view;
/// constructing a fresh path guard here would invent physical-close proof.
pub fn abort_session_for_root_stop(state: &AppState, placement_thread_id: &str) -> Result<()> {
    let Some(session) = state.state_store.dedicated_session(placement_thread_id)? else {
        return Ok(());
    };
    let _credential_operation =
        acquire_credential_profile_operation_sync(&session.credential_profile_id);
    if session.state == "terminal" {
        finish_terminal_credential_cleanup(state, &session)?;
        return Ok(());
    }
    if matches!(
        session.state.as_str(),
        "freezing" | "frozen" | "verifying" | "qualifying" | "publish_ready" | "discarding"
    ) {
        state
            .state_store
            .cancel_dedicated_candidate_for_root_stop(&session.placement_thread_id)?;
        finish_terminal_credential_cleanup(state, &session)?;
        return Ok(());
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_child_identity_refuses_thread_root_and_owner_contradictions() {
        assert!(
            validate_workload_child_identity(
                "fp:owner",
                "T-child",
                "T-child",
                "T-child",
                Some("fp:owner")
            )
            .is_ok()
        );
        for (thread, root, owner) in [
            ("T-other", "T-child", Some("fp:owner")),
            ("T-child", "T-parent", Some("fp:owner")),
            ("T-child", "T-child", Some("fp:other")),
            ("T-child", "T-child", None),
        ] {
            assert!(
                validate_workload_child_identity("fp:owner", "T-child", thread, root, owner)
                    .is_err()
            );
        }
    }

    #[test]
    fn public_command_surface_uses_only_frozen_selected_public_routes() {
        let contract = json!({
            "route_sets": {"selected": ["public.explicit", "public.default", "recovery.inspect"]},
            "routes": [
                {"id": "public.explicit", "audience": "public"},
                {"id": "public.default"},
                {"id": "recovery.inspect", "audience": "runtime"},
                {"id": "other.public", "audience": "public"}
            ]
        });
        let config = json!({"route_set": "selected"});
        for route_id in ["public.explicit", "public.default"] {
            validate_public_command_surface(
                &contract,
                &config,
                &json!({"route_id": route_id, "payload": {}}),
            )
            .unwrap();
        }
        for route_id in ["recovery.inspect", "other.public", "unknown"] {
            assert!(
                validate_public_command_surface(
                    &contract,
                    &config,
                    &json!({"route_id": route_id, "payload": {}}),
                )
                .is_err(),
                "{route_id} must be rejected before reservation or worker contact"
            );
        }
        for payload in [
            json!({"route_id": "public.explicit"}),
            json!({"route_id": 1, "payload": {}}),
            json!({"route_id": "public.explicit", "payload": {}, "ryeos_control": {}}),
        ] {
            assert!(validate_public_command_surface(&contract, &config, &payload).is_err());
        }
        let command = json!({"route_id": "public.explicit", "payload": {}});
        for config in [json!({}), json!({"route_set": "other"})] {
            assert!(validate_public_command_surface(&contract, &config, &command).is_err());
        }
        for audience in [Value::Null, json!("unknown")] {
            let mut changed = contract.clone();
            changed["routes"][0]["audience"] = audience;
            assert!(validate_public_command_surface(&changed, &config, &command).is_err());
        }
    }

    #[test]
    fn retirement_recovery_cannot_overrule_a_retained_process_owner() {
        for outcome in [
            ExclusiveRetirementOutcome::Reserved,
            ExclusiveRetirementOutcome::Unproved,
        ] {
            assert_eq!(
                resolve_worker_retirement(outcome, || {
                    panic!("a current owner must not be overridden by identity-based recovery")
                }),
                "unproved"
            );
        }
        assert_eq!(
            resolve_worker_retirement(ExclusiveRetirementOutcome::Reaped, || {
                panic!("proved owner settlement must not signal a potentially reused process")
            }),
            "reaped"
        );
        for (proved, expected) in [(true, "reaped"), (false, "unproved")] {
            assert_eq!(
                resolve_worker_retirement(ExclusiveRetirementOutcome::Absent, || proved),
                expected
            );
        }
    }

    fn bounded_command(
        attempt: u32,
        sequence: u64,
        state: &str,
    ) -> crate::runtime_db::DedicatedSessionCommandRecord {
        crate::runtime_db::DedicatedSessionCommandRecord {
            placement_thread_id: "T-placement".to_owned(),
            command_sequence: sequence,
            idempotency_key: format!("bounded:T-placement:turn-start:attempt:{attempt}"),
            worker_boot_epoch: u64::from(attempt),
            command_kind: "route".to_owned(),
            request_digest: "a".repeat(64),
            payload: json!({"route_id":"turn.start","payload":{}}),
            state: state.to_owned(),
            result: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn bounded_attempt_projection_advances_only_after_verified_uncontacted() {
        let mut attempts = vec![
            (1, bounded_command(1, 2, "failed"), "verified_uncontacted"),
            (2, bounded_command(2, 4, "completed"), "contacted_settled"),
        ];
        let (projection, settled, last_contact) =
            canonical_bounded_attempts(&mut attempts).unwrap();
        assert_eq!(projection.len(), 2);
        assert_eq!(settled, Some((2, 4)));
        assert_eq!(last_contact, Some("contacted_settled"));

        let mut advanced_after_unknown = vec![
            (
                1,
                bounded_command(1, 2, "outcome_unknown"),
                "outcome_unknown",
            ),
            (2, bounded_command(2, 3, "completed"), "contacted_settled"),
        ];
        assert!(canonical_bounded_attempts(&mut advanced_after_unknown).is_err());

        let mut gap = vec![(2, bounded_command(2, 2, "completed"), "contacted_settled")];
        assert!(canonical_bounded_attempts(&mut gap).is_err());
    }

    #[test]
    fn bounded_attempt_key_is_exactly_placement_and_step_scoped() {
        assert_eq!(
            bounded_attempt_key("T-placement", "bounded:T-placement:session-start:attempt:3")
                .unwrap(),
            Some(("session-start", 3))
        );
        assert!(
            bounded_attempt_key("T-placement", "bounded:T-other:turn-start:attempt:1").is_err()
        );
        assert!(
            bounded_attempt_key("T-placement", "bounded:T-placement:turn-start:attempt:01")
                .is_err()
        );
    }

    fn bounded_admission_config() -> Value {
        json!({
            "max_lifetime_seconds":60,
            "mode":{
                "kind":"bounded_turn",
                "session_start_route":"session.start",
                "turn_start_route":"turn.start",
                "max_uncontacted_attempts":3,
            },
        })
    }

    #[test]
    fn bounded_command_admission_binds_signed_ceiling_and_sealed_goal() {
        let config = bounded_admission_config();
        let parameters = json!({
            "goal":{
                "session_start_payload":{"workspace":"candidate"},
                "turn_start_payload":{"prompt":"admitted task"},
            },
        });
        let admitted = json!({"route_id":"turn.start","payload":{"prompt":"admitted task"}});
        let check = |key: &str, payload: &Value| {
            admitted_bounded_command_step(
                &config,
                &parameters,
                "T-placement",
                key,
                "route",
                payload,
            )
        };
        assert_eq!(
            check("bounded:T-placement:turn-start:attempt:3", &admitted).unwrap(),
            Some(("turn-start", 3)),
        );
        assert!(check("bounded:T-placement:turn-start:attempt:4", &admitted).is_err());
        assert!(
            check(
                "bounded:T-placement:turn-start:attempt:2",
                &json!({"route_id":"turn.start","payload":{"prompt":"replacement task"}}),
            )
            .is_err()
        );
        assert!(
            check(
                "bounded:T-placement:turn-start:attempt:1",
                &json!({"route_id":"session.start","payload":{"prompt":"admitted task"}}),
            )
            .is_err()
        );
        assert!(
            check(
                "bounded:T-placement:turn-start:attempt:1",
                &json!({"route_id":"turn.start","payload":{"prompt":"admitted task"},"extra":true}),
            )
            .is_err()
        );
        assert!(
            admitted_bounded_command_step(
                &config,
                &json!({}),
                "T-placement",
                "bounded:T-placement:turn-start:attempt:1",
                "route",
                &admitted,
            )
            .is_err()
        );
        assert_eq!(
            admitted_bounded_command_step(
                &json!({"mode":{"kind":"session"}}),
                &json!({}),
                "T-placement",
                "owner-command",
                "route",
                &json!({"payload":"interactive"}),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn bounded_contact_admission_requires_exact_uncontacted_predecessors() {
        let session_start = || {
            let mut record = bounded_command(1, 1, "completed");
            record.idempotency_key = "bounded:T-placement:session-start:attempt:1".to_owned();
            record.payload = json!({"route_id":"session.start","payload":{}});
            vec![(1, record, "contacted_settled")]
        };
        assert!(
            validate_next_bounded_attempt(
                "turn-start",
                1,
                Some(1),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .is_err()
        );
        assert!(
            validate_next_bounded_attempt(
                "turn-start",
                1,
                Some(2),
                &mut session_start(),
                &mut Vec::new(),
            )
            .is_ok()
        );
        assert!(
            validate_next_bounded_attempt(
                "session-start",
                2,
                Some(2),
                &mut session_start(),
                &mut Vec::new(),
            )
            .is_err()
        );
        for (state, contact) in [
            ("completed", "contacted_settled"),
            ("outcome_unknown", "outcome_unknown"),
            ("failed", "budget_exhausted_uncontacted"),
            ("committed", "reserved_uncontacted"),
        ] {
            let mut prior = vec![(1, bounded_command(1, 2, state), contact)];
            assert!(
                validate_next_bounded_attempt(
                    "turn-start",
                    2,
                    Some(3),
                    &mut session_start(),
                    &mut prior,
                )
                .is_err()
            );
        }
        let mut uncontacted = vec![(1, bounded_command(1, 2, "failed"), "verified_uncontacted")];
        assert!(
            validate_next_bounded_attempt(
                "turn-start",
                2,
                Some(3),
                &mut session_start(),
                &mut uncontacted,
            )
            .is_ok()
        );
        assert!(
            validate_next_bounded_attempt(
                "turn-start",
                3,
                Some(3),
                &mut session_start(),
                &mut uncontacted,
            )
            .is_err()
        );
        assert!(
            validate_next_bounded_attempt(
                "turn-start",
                2,
                Some(2),
                &mut session_start(),
                &mut uncontacted,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_contact_deadline_is_creation_anchored_and_checked_for_overflow() {
        let config = bounded_admission_config();
        assert_eq!(
            bounded_session_deadline(&config, 123).unwrap(),
            Some(60_123)
        );
        assert!(bounded_session_deadline(&config, i64::MAX - 1).is_err());
        let mut invalid = config;
        invalid["max_lifetime_seconds"] = json!(0);
        assert!(bounded_session_deadline(&invalid, 123).is_err());
        invalid["max_lifetime_seconds"] = json!(603_601);
        assert!(bounded_session_deadline(&invalid, 123).is_err());
        assert_eq!(
            bounded_session_deadline(&json!({"mode":{"kind":"session"}}), 123,).unwrap(),
            None
        );
        assert!(validate_hosted_contact_deadlines(Some(60_123), Some(70_000), 60_122).is_ok());
        assert!(validate_hosted_contact_deadlines(Some(60_123), Some(70_000), 60_123).is_err());
        assert!(validate_hosted_contact_deadlines(Some(60_123), Some(60_000), 60_000).is_err());
        assert!(validate_hosted_contact_deadlines(None, Some(60_000), 60_000).is_err());
        assert!(validate_hosted_contact_deadlines(None, None, i64::MAX).is_ok());
    }

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
            candidate_disposition: crate::runtime_db::DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "P-one".to_owned(),
            credential_generation: 1,
            remote_thread_id: Some("upstream-thread".to_owned()),
            current_turn_id: None,
            state: "idle".to_owned(),
            send_boundary: "settled".to_owned(),
            candidate_snapshot_hash: None,
            candidate_validation_hash: None,
            candidate_evaluation_hash: None,
            candidate_evaluation: None,
            candidate_qualification: None,
            candidate_disposition_root_id: None,
            candidate_disposition_operation_id: None,
            publication_result: None,
            completion_fence: None,
            bounded_outcome: None,
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
    fn command_reply_without_events_preserves_enrollment_and_raw_response_identity() {
        let result = json!({
            "response":{"device_code":"fixture-ephemeral-code"},
            "result_retention":"ephemeral",
            "session_observations":[{
                "kind":"credential_enrollment_started",
                "login_id":"login-one",
                "ttl_seconds":600,
            }],
        });
        let original = result.clone();
        let response_digest = ryeos_state::objects::canonical_value_digest(&result).unwrap();
        let batch = canonical_command_observation_batch(&result, 16).unwrap();
        assert_eq!(batch["events"], json!([]));
        assert_eq!(
            batch["session_observations"],
            result["session_observations"]
        );
        assert_eq!(batch.as_object().unwrap().len(), 2);
        assert!(!batch.to_string().contains("fixture-ephemeral-code"));
        assert_eq!(result, original);
        assert_eq!(
            ryeos_state::objects::canonical_value_digest(&result).unwrap(),
            response_digest
        );
        assert_ne!(
            ryeos_state::objects::canonical_value_digest(&batch).unwrap(),
            response_digest
        );
        let session = session_fixture();
        validate_new_state_transition_sequence_for_session(&session, 3, &batch).unwrap();
        assert!(
            approval_request_fact_events(&session, 3, &batch)
                .unwrap()
                .is_empty()
        );
        assert!(
            state_transition_fact_events(
                &session,
                3,
                &batch,
                json!({"kind":"command_response"}),
                Some((1, &"b".repeat(64))),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn command_batch_rejects_malformed_arrays_and_requires_session_observations() {
        for invalid in [Value::Null, json!({}), json!(true), json!("[]")] {
            assert!(
                canonical_command_observation_batch(
                    &json!({"events":invalid,"session_observations":[]}),
                    16,
                )
                .is_err()
            );
            assert!(
                canonical_command_observation_batch(
                    &json!({"events":[],"session_observations":invalid}),
                    16,
                )
                .is_err()
            );
        }
        assert!(canonical_command_observation_batch(&json!({"events":[]}), 16).is_err());
        assert!(
            canonical_command_observation_batch(&json!({"session_observations":[]}), 16).is_ok()
        );
    }

    #[test]
    fn command_batch_bounds_apply_before_authoritative_append() {
        let mut result = json!({
            "events":vec![json!({"event_type":"fixture","payload":{}}); MAX_WORKER_EVENTS_PER_RESPONSE],
            "session_observations":vec![json!({"kind":"remote_thread","id":"upstream-thread"}); 17],
        });
        assert!(
            canonical_command_observation_batch(
                &result,
                command_observation_limit("route").unwrap()
            )
            .is_err()
        );
        assert!(
            canonical_command_observation_batch(
                &result,
                command_observation_limit("reattach").unwrap()
            )
            .is_ok()
        );
        result["events"]
            .as_array_mut()
            .unwrap()
            .push(json!({"event_type":"fixture","payload":{}}));
        let error = canonical_command_observation_batch(
            &result,
            command_observation_limit("reattach").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("too many events"));
    }

    #[test]
    fn command_batch_preserves_approval_authority() {
        let mut session = session_fixture();
        session.current_turn_id = Some("turn-one".to_owned());
        session.state = "turn_running".to_owned();
        let result = json!({
            "events":[{"event_type":"approval.requested","payload":{
                "request_id":5,
                "request_digest":"b".repeat(64),
                "operation_class":"command",
                "display":{},
                "upstream_session_id":"upstream-thread",
                "operation_id":"turn-one",
            }}],
            "session_observations":[],
        });
        let batch = canonical_command_observation_batch(&result, 16).unwrap();
        assert_eq!(batch, result);
        let facts = approval_request_fact_events(&session, 3, &batch).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].payload["turn_id"], "turn-one");
        assert_eq!(facts[0].payload["chain_root_id"], session.chain_root_id);
        assert_eq!(
            facts[0].payload["admitted_capsule_hash"],
            session.admitted_capsule_hash
        );
        session.current_turn_id = Some("another-turn".to_owned());
        assert!(approval_request_fact_events(&session, 3, &batch).is_err());
    }

    #[test]
    fn command_optional_events_do_not_relax_pushed_batch_contract() {
        let result = json!({"session_observations":[]});
        let batch = canonical_command_observation_batch(&result, 16).unwrap();
        assert!(pushed_observation_limit(&result).is_err());
        assert!(pushed_observation_limit(&batch).is_err());
        assert!(
            serde_json::from_value::<WorkerObservationBatch>(json!({
                "first_sequence":1,"count":1,"previous_digest":null,"batch_digest":"a".repeat(64),
                "session_observations":[],
            }))
            .is_err()
        );
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
        let batch = canonical_command_observation_batch(
            &json!({
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
            command_observation_limit("route").unwrap(),
        )
        .unwrap();
        validate_new_state_transition_sequence_for_session(&session, 3, &batch).unwrap();
        assert!(
            approval_request_fact_events(&session, 3, &batch)
                .unwrap()
                .is_empty()
        );
        let facts = state_transition_fact_events(
            &session,
            3,
            &batch,
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
    fn command_progress_corroboration_retains_exact_original_source() {
        let session = session_fixture();
        let digest = "b".repeat(64);
        let batch = json!({"events":[],"session_observations":[{
            "kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"
        }]});
        validate_command_progress_batch(&batch).unwrap();
        let make = |phase: &str, event_type: &str| {
            state_transition_fact_events(&session, 3, &batch, json!({
                "kind":phase,"command_sequence":2,"request_digest":digest,
                "batch_operation_id":command_fact_operation_id(&session, event_type, 2, &digest).unwrap()
            }), Some((2, &digest))).unwrap().remove(0)
        };
        let early = make("command_progress", "hosted_worker_command_progress");
        let final_fact = make(
            "command_response",
            "hosted_worker_command_observation_batch",
        );
        assert!(corroborates_command_progress(
            &session,
            &final_fact,
            &early.payload
        ));
        for (pointer, replacement) in [
            ("/source/command_sequence", json!(3)),
            ("/source/request_digest", json!("c".repeat(64))),
            ("/source/batch_operation_id", json!("d".repeat(64))),
            ("/worker_boot_epoch", json!(4)),
            ("/turn_id", json!("turn-other")),
        ] {
            let mut changed = early.payload.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                !corroborates_command_progress(&session, &final_fact, &changed),
                "{pointer}"
            );
        }
        let completion = json!({"events":[],"session_observations":[{
            "kind":"state","expected":"turn_running","next":"idle","completed_turn_id":"turn-one"
        }]});
        assert!(validate_command_progress_batch(&completion).is_err());
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
    fn settled_publication_recovery_keeps_source_c_and_exact_target_d() {
        let mut session = session_fixture();
        let source = "c".repeat(64);
        let target = "d".repeat(64);
        let base = "b".repeat(64);
        let validation = "e".repeat(64);
        let evaluation = json!({
            "candidate":{
                "candidate_snapshot_hash":target,
                "candidate_validation_hash":validation,
                "base_snapshot_hash":base,
            },
            "result":{"accepted":true},
        });
        let evaluation_hash = ryeos_state::objects::canonical_value_digest(&evaluation).unwrap();
        session.state = "terminal".into();
        session.terminal_reason = Some("completed".into());
        session.candidate_snapshot_hash = Some(source.clone());
        session.candidate_evaluation_hash = Some(evaluation_hash.clone());
        session.candidate_evaluation = Some(evaluation);
        let authority = CandidatePublicationAuthority {
            source_candidate_snapshot_hash: source.clone(),
            candidate_snapshot_hash: target.clone(),
            candidate_validation_hash: validation,
            base_snapshot_hash: base,
            principal_key: "owner".into(),
            project_hash: "f".repeat(64),
            candidate_evaluation_hash: evaluation_hash,
        };
        // Round-trip the state left by a crash after session settlement but
        // before the source worker's root finalization. C is not rewritten D.
        for (prefix, outcome) in [
            ("published", "published"),
            ("publication_unknown", "unknown"),
        ] {
            session.publication_result = Some(format!("{prefix}:{target}"));
            let recovered: DedicatedSessionRecord =
                serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
            assert_eq!(
                terminal_candidate_publication_outcome(&recovered, &source, &authority).unwrap(),
                outcome
            );
            assert_eq!(
                recovered.candidate_snapshot_hash.as_deref(),
                Some(source.as_str())
            );
            let terminal = ryeos_runtime::envelope::dedicated_session_terminal_result(
                recovered.placement_thread_id.clone(),
                serde_json::to_value(&recovered).unwrap(),
            );
            // This is the completed worker root, not the separate publication
            // service result: publication ambiguity must remain explicit.
            assert!(terminal.success);
            assert_eq!(
                terminal.result.unwrap()["session"]["publication_result"],
                format!("{prefix}:{target}")
            );
            session.publication_result = Some(format!("{prefix}:{source}"));
            assert!(terminal_candidate_publication_outcome(&session, &source, &authority).is_err());
        }
        session.publication_result = Some(format!("publication_unknown:{target}"));
        assert!(terminal_candidate_publication_outcome(&session, &target, &authority).is_err());
        session.candidate_evaluation.as_mut().unwrap()["candidate"]["candidate_snapshot_hash"] =
            json!(source);
        assert!(terminal_candidate_publication_outcome(&session, &source, &authority).is_err());
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
