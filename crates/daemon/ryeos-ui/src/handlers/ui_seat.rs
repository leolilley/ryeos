//! Session-authenticated RyeOS UI seat services for browser renderers.
//!
//! Renderers arrive through a `session:<id>` wrapper and open a seat against
//! one exact retained binding-attachment triple. The triple is only a lookup
//! coordinate: surface/project authority comes from the retained attachment,
//! and placement or display paths cannot rebind it.
//!
//! The signed services use ordinary verified dispatch, like session/current.
//! Here "verified" selects descriptor verification and preservation of the
//! route context; it does not promote a cookie to signing-key authority.
//! Do not mark these lifecycle services session_local: that class is for
//! compiled UI binding dispatch and generic node routes deliberately refuse it.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewEventRecord;
use ryeos_app::thread_lifecycle::{ThreadCreateParams, ThreadFinalizeParams};
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::{BindingAttachmentCoordinate, BrowserSession};
use crate::state::get_ui_state;

const SEAT_KIND: &str = "seat_session";
const SEAT_EVENT_PREFIX: &str = "seat.";
const SEAT_PRODUCER_EVENT: &str = "ui_seat.producer.v1";
const SEAT_APPEND_RECEIPT_EVENT: &str = "ui_seat.append_receipt.v1";
const MAX_SEAT_APPEND_EVENTS: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    binding_attachment_id: String,
    binding_generation: u64,
    binding_digest: String,
}

impl OpenRequest {
    fn coordinate(&self) -> BindingAttachmentCoordinate {
        BindingAttachmentCoordinate {
            binding_attachment_id: self.binding_attachment_id.clone(),
            binding_generation: self.binding_generation,
            binding_digest: self.binding_digest.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendRequest {
    thread_id: String,
    producer_incarnation: String,
    operation_id: String,
    first_engine_seq: u64,
    last_engine_seq: u64,
    event_count: usize,
    payload_digest: String,
    events: Vec<AppendEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AppendEvent {
    engine_seq: u64,
    event_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SeatProducer {
    schema_version: String,
    producer_incarnation: String,
    next_engine_seq: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SeatAppendReceipt {
    schema_version: String,
    producer_incarnation: String,
    operation_id: String,
    first_engine_seq: u64,
    last_engine_seq: u64,
    event_count: usize,
    payload_digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayRequest {
    chain_root_id: String,
    #[serde(default)]
    after_chain_seq: Option<i64>,
    #[serde(default = "default_replay_limit")]
    limit: usize,
}

fn default_replay_limit() -> usize {
    500
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn browser_session(ctx: &HandlerContext, state: &AppState) -> Result<BrowserSession, HandlerError> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("session cookie required".into()))?;
    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))
}

fn seat_owner(session: &BrowserSession) -> String {
    format!("session:{}", session.session_id)
}

fn canonical_digest(value: &str) -> bool {
    lillux::valid_hash(value) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

/// Hash a seat event batch with the browser's explicitly typed digest format.
///
/// JSON text itself is not used as the digest input because JavaScript and
/// serde_json can choose different, equally valid spellings for one f64. Exact
/// integers retain their decimal value while non-integral numbers commit their
/// IEEE-754 bits.
pub fn seat_payload_digest(value: &Value) -> Result<String> {
    fn encode(value: &Value, output: &mut String) -> Result<()> {
        use std::fmt::Write as _;

        match value {
            Value::Null => output.push('n'),
            Value::Bool(false) => output.push('f'),
            Value::Bool(true) => output.push('t'),
            Value::String(value) => {
                write!(output, "s{}:{value}", value.len())?;
            }
            Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    write!(output, "i{value};")?;
                } else if let Some(value) = number.as_u64() {
                    write!(output, "i{value};")?;
                } else if let Some(value) = number.as_f64() {
                    write!(output, "d{:016x};", value.to_bits())?;
                } else {
                    anyhow::bail!("seat digest contains a number outside the daemon value domain");
                }
            }
            Value::Array(values) => {
                write!(output, "a{}:{{", values.len())?;
                for value in values {
                    encode(value, output)?;
                }
                output.push('}');
            }
            Value::Object(values) => {
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort();
                write!(output, "o{}:{{", keys.len())?;
                for key in keys {
                    encode(&Value::String(key.clone()), output)?;
                    encode(&values[key], output)?;
                }
                output.push('}');
            }
        }
        Ok(())
    }

    let mut typed = String::from("ryeos.seat.payload.v1|");
    encode(value, &mut typed)?;
    Ok(lillux::sha256_hex(typed.as_bytes()))
}

fn canonical_operation_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.hyphenated().to_string() == value)
}

fn decode_control<T: serde::de::DeserializeOwned>(
    event: ryeos_app::state_store::PersistedEventRecord,
    label: &str,
) -> Result<T, HandlerError> {
    serde_json::from_value(event.payload).map_err(|error| {
        HandlerError::Internal(format!("retained {label} event is malformed: {error}"))
    })
}

fn latest_receipt(
    state: &AppState,
    thread_id: &str,
) -> Result<Option<(SeatAppendReceipt, i64)>, HandlerError> {
    state
        .state_store
        .latest_thread_event_by_type(thread_id, SEAT_APPEND_RECEIPT_EVENT)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .map(|event| {
            let chain_seq = event.chain_seq;
            let receipt: SeatAppendReceipt = decode_control(event, "seat append receipt")?;
            if receipt.schema_version != "ryeos.ui.seat.append-receipt.v1" {
                return Err(HandlerError::Internal(
                    "retained seat append receipt has an unsupported schema".into(),
                ));
            }
            Ok((receipt, chain_seq))
        })
        .transpose()
}

fn next_engine_seq(state: &AppState, thread_id: &str) -> Result<u64, HandlerError> {
    if let Some((receipt, _)) = latest_receipt(state, thread_id)? {
        return receipt
            .last_engine_seq
            .checked_add(1)
            .ok_or_else(|| HandlerError::BadRequest("seat engine sequence is exhausted".into()));
    }
    let Some(event) = state
        .state_store
        .latest_thread_event_by_type_prefix(thread_id, SEAT_EVENT_PREFIX)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
    else {
        return Ok(0);
    };
    let seq = event
        .payload
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            HandlerError::Internal("retained seat event has no engine sequence".into())
        })?;
    seq.checked_add(1)
        .ok_or_else(|| HandlerError::BadRequest("seat engine sequence is exhausted".into()))
}

fn append_acknowledgement(receipt: &SeatAppendReceipt, chain_seq: i64) -> Value {
    json!({
        "producer_incarnation": receipt.producer_incarnation,
        "operation_id": receipt.operation_id,
        "first_engine_seq": receipt.first_engine_seq.to_string(),
        "last_engine_seq": receipt.last_engine_seq.to_string(),
        "event_count": receipt.event_count,
        "payload_digest": receipt.payload_digest,
        "appended": receipt.event_count,
        "chain_seq": chain_seq.to_string(),
    })
}

fn issue_producer(
    state: &AppState,
    detail: &ryeos_app::state_store::ThreadDetail,
    owner: &str,
) -> Result<(String, u64), HandlerError> {
    state
        .state_store
        .touch_seat_lease(
            &detail.thread_id,
            owner,
            &detail.item_ref,
            &detail.executor_ref,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    let next_engine_seq = next_engine_seq(state, &detail.thread_id)?;
    let producer_incarnation = lillux::sha256_hex(&rand::random::<[u8; 32]>());
    let producer = SeatProducer {
        schema_version: "ryeos.ui.seat.producer.v1".to_string(),
        producer_incarnation: producer_incarnation.clone(),
        next_engine_seq,
    };
    let persisted = state
        .threads
        .append_thread_events(
            &detail.chain_root_id,
            &detail.thread_id,
            &[NewEventRecord {
                event_type: SEAT_PRODUCER_EVENT.to_string(),
                storage_class: "indexed".to_string(),
                payload: serde_json::to_value(producer)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?,
            }],
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .ok_or_else(|| HandlerError::BadRequest("seat session is not running".into()))?;
    if persisted.len() != 1 {
        return Err(HandlerError::Internal(
            "seat producer append returned an invalid acknowledgement".into(),
        ));
    }
    Ok((producer_incarnation, next_engine_seq))
}

fn require_owned_seat(
    state: &AppState,
    thread_id: &str,
    owner: &str,
) -> Result<ryeos_app::state_store::ThreadDetail, HandlerError> {
    let detail = state
        .state_store
        .get_thread(thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    if detail.kind != SEAT_KIND {
        return Err(HandlerError::BadRequest(format!(
            "thread {thread_id} is not a seat session"
        )));
    }
    if detail.requested_by.as_deref() != Some(owner) {
        return Err(HandlerError::NotFound);
    }
    Ok(detail)
}

/// Settle every running seat owned by an exact predecessor UI session. This
/// runs at daemon-side session activation because the successor renderer must
/// never receive authority to close a predecessor-owned thread. Replays are
/// harmless and finish any cleanup whose first response was lost.
pub(crate) fn retire_session_seats(state: &AppState, session_id: &str) -> Result<()> {
    let owner = format!("session:{session_id}");
    loop {
        let running = state
            .state_store
            .list_threads_sorted(100, Some(&owner), ryeos_state::queries::ThreadSort::Watch)?
            .into_iter()
            .filter(|thread| thread.kind == SEAT_KIND && thread.status == "running")
            .collect::<Vec<_>>();
        if running.is_empty() {
            break;
        }
        for detail in running {
            state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: detail.thread_id.clone(),
                status: "completed".to_string(),
                outcome_code: None,
                result: None,
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })?;
            state.state_store.remove_seat_lease(&detail.thread_id)?;
        }
    }
    Ok(())
}

pub async fn handle_open(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let req: OpenRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let coordinate = req.coordinate();
    let attachment = ui_state
        .browser_sessions
        .resolve_attachment(&session.session_id, &coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != state.node_policy.generation_digest()
    {
        return Err(HandlerError::Structured {
            code: "ui_binding_stale".into(),
            status: 409,
            body: json!({
                "code": "ui_binding_stale",
                "error": "the node policy generation changed after attachment admission",
                "retryable": false,
            }),
        }
        .into());
    }
    let owner = seat_owner(&session);
    let surface_ref = attachment.surface_ref.clone();
    let client_ref = format!(
        "client:ryeos/ui-session/{}/{}:{}",
        attachment.binding_attachment_id,
        attachment.binding_generation,
        attachment.compiled_binding.binding_digest,
    );

    let existing = state
        .state_store
        .list_threads_filtered(100, Some(&owner))
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .into_iter()
        .filter(|thread| {
            thread.kind == SEAT_KIND
                && thread.status == "running"
                && thread.item_ref == surface_ref
                && thread.executor_ref == client_ref
        })
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
    if let Some(thread) = existing {
        let _admission = ui_state
            .browser_sessions
            .admit_attachment_dispatch(&session.session_id, &coordinate)
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        let detail = require_owned_seat(&state, &thread.thread_id, &owner)?;
        let (producer_incarnation, next_engine_seq) = issue_producer(&state, &detail, &owner)?;
        return Ok(json!({
            "thread_id": thread.thread_id,
            "chain_root_id": thread.chain_root_id,
            "reattached": true,
            "producer_incarnation": producer_incarnation,
            "next_engine_seq": next_engine_seq.to_string(),
        }));
    }

    let project_access = crate::seat_auth::attachment_project_access(&attachment)?;
    let project_root = match project_access.as_ref() {
        Some(access) => access.path().to_path_buf(),
        None => state.config.app_root.clone(),
    };
    let root_admission = ryeos_app::thread_lifecycle::admit_non_execution_root(
        &state.engine,
        state
            .node_history_policy()
            .map_err(|error| HandlerError::Internal(error.to_string()))?,
        &surface_ref,
        &project_root,
        &owner,
        session.granted_caps.clone(),
        state.threads.site_id(),
        state.threads.site_id(),
        SEAT_KIND.to_string(),
    )
    .map_err(|error| HandlerError::BadRequest(error.to_string()))?;

    // Linearize the durable seat creation after all descriptor-rooted
    // preparation. A concurrent revocation either wins here or happens after
    // this operation has crossed admission.
    let _admission = ui_state
        .browser_sessions
        .admit_attachment_dispatch(&session.session_id, &coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;

    let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let site_id = state.threads.site_id().to_string();
    let detail = state.threads.create_non_execution_root_thread(
        &ThreadCreateParams {
            thread_id: thread_id.clone(),
            chain_root_id: thread_id.clone(),
            kind: SEAT_KIND.to_string(),
            item_ref: surface_ref.clone(),
            executor_ref: client_ref.clone(),
            launch_mode: "wait".to_string(),
            current_site_id: site_id.clone(),
            origin_site_id: site_id,
            upstream_thread_id: None,
            requested_by: Some(owner.clone()),
            project_root: root_admission
                .project_root()
                .map(std::path::Path::to_path_buf),
            project_authority: root_admission.project_authority().clone(),
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: None,
        },
        &root_admission,
    )?;
    state.threads.mark_running(&thread_id)?;
    let (producer_incarnation, next_engine_seq) = issue_producer(&state, &detail, &owner)?;

    Ok(json!({
        "thread_id": detail.thread_id,
        "chain_root_id": detail.chain_root_id,
        "reattached": false,
        "producer_incarnation": producer_incarnation,
        "next_engine_seq": next_engine_seq.to_string(),
    }))
}

pub async fn handle_append(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: AppendRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    if req.events.is_empty() {
        return Err(HandlerError::BadRequest("no events to append".into()).into());
    }
    if req.events.len() > MAX_SEAT_APPEND_EVENTS {
        return Err(HandlerError::BadRequest(format!(
            "seat append exceeds the {MAX_SEAT_APPEND_EVENTS}-event limit"
        ))
        .into());
    }
    if req.event_count != req.events.len() {
        return Err(HandlerError::BadRequest(
            "event_count does not match the encoded event batch".into(),
        )
        .into());
    }
    if !canonical_digest(&req.producer_incarnation) {
        return Err(HandlerError::BadRequest(
            "producer_incarnation must be a canonical sha256 digest".into(),
        )
        .into());
    }
    if !canonical_operation_id(&req.operation_id) {
        return Err(HandlerError::BadRequest(
            "operation_id must be a canonical lowercase UUID".into(),
        )
        .into());
    }
    if !canonical_digest(&req.payload_digest) {
        return Err(HandlerError::BadRequest(
            "payload_digest must be a canonical sha256 digest".into(),
        )
        .into());
    }
    let expected_last = req
        .first_engine_seq
        .checked_add(req.events.len() as u64 - 1)
        .ok_or_else(|| HandlerError::BadRequest("engine sequence interval overflows".into()))?;
    if req.last_engine_seq != expected_last {
        return Err(HandlerError::BadRequest(
            "last_engine_seq does not match the exact event interval".into(),
        )
        .into());
    }
    for (offset, event) in req.events.iter().enumerate() {
        if !event.event_type.starts_with(SEAT_EVENT_PREFIX) {
            return Err(HandlerError::BadRequest(format!(
                "seat braids accept only `{SEAT_EVENT_PREFIX}*` events, got `{}`",
                event.event_type
            ))
            .into());
        }
        let expected_seq = req.first_engine_seq + offset as u64;
        if event.engine_seq != expected_seq {
            return Err(HandlerError::BadRequest(format!(
                "seat event engine sequence {} is not the expected {expected_seq}",
                event.engine_seq
            ))
            .into());
        }
    }
    let canonical_events = serde_json::to_value(&req.events)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let actual_digest = seat_payload_digest(&canonical_events)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    if req.payload_digest != actual_digest {
        return Err(HandlerError::BadRequest(
            "payload_digest does not match the typed event batch".into(),
        )
        .into());
    }

    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    let receipt = SeatAppendReceipt {
        schema_version: "ryeos.ui.seat.append-receipt.v1".to_string(),
        producer_incarnation: req.producer_incarnation.clone(),
        operation_id: req.operation_id.clone(),
        first_engine_seq: req.first_engine_seq,
        last_engine_seq: req.last_engine_seq,
        event_count: req.event_count,
        payload_digest: req.payload_digest.clone(),
    };
    let retained_receipts = state
        .state_store
        .thread_events_by_type_operation_id(
            &detail.thread_id,
            SEAT_APPEND_RECEIPT_EVENT,
            &req.operation_id,
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    if retained_receipts.len() > 1 {
        return Err(HandlerError::Internal(
            "seat append operation has multiple retained receipts".into(),
        )
        .into());
    }
    if let Some(retained_event) = retained_receipts.into_iter().next() {
        let chain_seq = retained_event.chain_seq;
        let retained: SeatAppendReceipt = decode_control(retained_event, "seat append receipt")?;
        if retained != receipt {
            return Err(HandlerError::BadRequest(
                "operation_id was already used for a different seat append".into(),
            )
            .into());
        }
        return Ok(append_acknowledgement(&retained, chain_seq));
    }

    // A retained receipt is the authoritative outcome of an operation and
    // remains replayable even if a later open rotated producer authority.
    // Only a genuinely new write must prove that it owns the current producer.
    let producer_event = state
        .state_store
        .latest_thread_event_by_type(&detail.thread_id, SEAT_PRODUCER_EVENT)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .ok_or_else(|| HandlerError::Internal("seat has no producer incarnation".into()))?;
    let producer: SeatProducer = decode_control(producer_event, "seat producer")?;
    if producer.schema_version != "ryeos.ui.seat.producer.v1" {
        return Err(HandlerError::Internal(
            "retained seat producer has an unsupported schema".into(),
        )
        .into());
    }
    if producer.producer_incarnation != req.producer_incarnation {
        return Err(HandlerError::BadRequest(
            "stale seat producer incarnation; reopen the seat before appending".into(),
        )
        .into());
    }

    if detail.status != "running" {
        return Err(HandlerError::BadRequest(format!(
            "seat session is {}; only running seats accept new events",
            detail.status
        ))
        .into());
    }

    let expected_first = match latest_receipt(&state, &detail.thread_id)? {
        Some((latest, _)) if latest.producer_incarnation == producer.producer_incarnation => latest
            .last_engine_seq
            .checked_add(1)
            .ok_or_else(|| HandlerError::BadRequest("seat engine sequence is exhausted".into()))?,
        Some(_) | None => producer.next_engine_seq,
    };
    if req.first_engine_seq != expected_first {
        return Err(HandlerError::BadRequest(format!(
            "seat append sequence gap or overlap: expected {expected_first}, got {}",
            req.first_engine_seq
        ))
        .into());
    }

    // Renew before appending so presence either defeats the expiry claim or
    // observes that the reaper already owns the transition.
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;

    let mut records: Vec<NewEventRecord> = req
        .events
        .into_iter()
        .map(|event| NewEventRecord {
            event_type: event.event_type,
            storage_class: "indexed".to_string(),
            payload: json!({
                "seq": event.engine_seq,
                "payload": event.payload,
            }),
        })
        .collect();
    records.push(NewEventRecord {
        event_type: SEAT_APPEND_RECEIPT_EVENT.to_string(),
        storage_class: "indexed".to_string(),
        payload: serde_json::to_value(&receipt)
            .map_err(|error| HandlerError::Internal(error.to_string()))?,
    });
    let persisted = state
        .threads
        .append_thread_events(&detail.chain_root_id, &detail.thread_id, &records)?
        .ok_or_else(|| {
            HandlerError::BadRequest(
                "seat session is no longer running; only running seats accept events".into(),
            )
        })?;
    if persisted.len() != receipt.event_count + 1 {
        return Err(HandlerError::Internal(
            "seat append returned an invalid acknowledgement".into(),
        )
        .into());
    }
    let receipt_chain_seq = persisted
        .last()
        .map(|record| record.chain_seq)
        .ok_or_else(|| HandlerError::Internal("seat append returned no records".into()))?;

    Ok(append_acknowledgement(&receipt, receipt_chain_seq))
}

pub async fn handle_replay(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: ReplayRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.chain_root_id, &owner)?;
    if detail.status != "running" {
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        return Err(HandlerError::BadRequest("seat session is not running".into()).into());
    }
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;
    let result = state
        .events
        .replay(&ryeos_app::event_store_service::EventReplayParams {
            thread_id: None,
            chain_root_id: Some(detail.chain_root_id),
            after_chain_seq: req.after_chain_seq,
            limit: req.limit,
        })?;
    let events: Vec<_> = result
        .events
        .into_iter()
        .filter(|event| event.event_type.starts_with(SEAT_EVENT_PREFIX))
        .collect();

    Ok(json!({
        "events": events,
        "next_cursor": result.next_cursor.map(|cursor| cursor.to_string()),
    }))
}

pub async fn handle_close(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: CloseRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    if detail.status != "running" {
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        return Ok(json!({ "thread_id": detail.thread_id, "status": detail.status }));
    }
    let finalized = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: req.thread_id,
        status: "completed".to_string(),
        outcome_code: None,
        result: None,
        error: None,
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    })?;
    state.state_store.remove_seat_lease(&finalized.thread_id)?;
    Ok(json!({ "thread_id": finalized.thread_id, "status": finalized.status }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchRequest {
    thread_id: String,
}

pub async fn handle_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let owner = seat_owner(&session);
    let req: TouchRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let detail = require_owned_seat(&state, &req.thread_id, &owner)?;
    if detail.status != "running" {
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        return Err(HandlerError::BadRequest("seat session is not running".into()).into());
    }
    state.state_store.touch_seat_lease(
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
    )?;
    Ok(json!({ "thread_id": detail.thread_id, "touched": true }))
}

pub static OPEN_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/open",
    endpoint: "ui.seat.open",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_open(params, ctx, state)),
};

pub static APPEND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/append",
    endpoint: "ui.seat.append",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_append(params, ctx, state)),
};

pub static REPLAY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/replay",
    endpoint: "ui.seat.replay",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_replay(params, ctx, state)),
};

pub static CLOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/close",
    endpoint: "ui.seat.close",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_close(params, ctx, state)),
};

pub static TOUCH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/touch",
    endpoint: "ui.seat.touch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_touch(params, ctx, state)),
};

#[cfg(test)]
mod digest_tests {
    use super::*;

    fn event_batch(payload: Value) -> Value {
        json!([{
            "engine_seq": 0_u64,
            "event_type": "seat.facet",
            "payload": payload,
        }])
    }

    #[test]
    fn typed_seat_digest_matches_browser_vectors() {
        let one_microsecond: Value = serde_json::from_str("0.000001").unwrap();
        let one_tenth_microsecond: Value = serde_json::from_str("1e-7").unwrap();
        let vectors = [
            (
                event_batch(json!({"key":"float", "value":one_microsecond})),
                "84550fb1cf3501cfdde9221974751fe5085b5bc1aed9ce7329e88467aaebc950",
            ),
            (
                event_batch(json!({
                    "key":"thresholds",
                    // The browser's wire encoder normalizes -0 to integer zero.
                    "value":[one_tenth_microsecond, 1.5, 0_u64],
                })),
                "6d0e2ab6e500346d7a21fcc6baef8401bfa2b1ce0a13d8816bc53883709bdd3f",
            ),
            (
                event_batch(json!({"key":"u64", "value":u64::MAX})),
                "b714cf712cbfc663f9eb93095596465167564a7d6bfa738c723376ff5609b209",
            ),
            (
                event_batch(json!({
                    "key":"unicode",
                    "value": {
                        "😀":"é",
                        "𐀀":[null, true],
                        "":{"__proto__":"inert"},
                    },
                })),
                "9f001b4f3bc1e8ccc924b29d7280435dbcefbc1777d5395a6ac080d4494d7835",
            ),
        ];

        for (value, expected) in vectors {
            assert_eq!(seat_payload_digest(&value).unwrap(), expected);
        }
    }
}
