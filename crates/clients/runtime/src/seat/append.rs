//! Sending seat events: planning a batch, the exact-retry loop, the pending
//! queue bound and reconciliation of a retained request.
//!
//! An append is written to durable storage before it is sent and cleared only
//! after a verified acknowledgement. Every retry sends the identical bytes, so
//! the daemon treats them as one idempotent operation. A 2xx answer that does
//! not validate as the acknowledgement proves nothing and is retried like a
//! lost reply, a few times at most; 401 and 403 first re-read the session once
//! (a rotated credential is not a lost writer).

use std::collections::VecDeque;
use std::rc::Rc;

use ryeos_client_base::ui::SeatEvent;
use ryeos_client_base::ui::UiBindingAttachmentCoordinate;
use ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity;
use ryeos_client_base::ui::binding::UiPresentationProducer;
use serde_json::{Value, json};

use crate::admission::require_current_session;
use crate::api::{
    AppendRequest, MAX_PENDING_SEAT_BYTES, MAX_PENDING_SEAT_EVENTS, MAX_SEAT_BATCH_EVENTS,
    SEAT_APPEND_PATH, SEAT_REPLAY_PATH, SESSION_CURRENT_PATH, WireSeatEvent, encode_append_request,
    wire_events_digest,
};
use crate::codec::{decode_strict, encode_exact};
use crate::history::SeatCoordinate;
use crate::platform::{Platform, Transport};
use crate::retention::{self, PENDING_APPEND_SCHEMA, RetainedPendingAppend};

use super::ack::{
    AckError, ExpectedAck, PlainSeatEvent, VerifiedAck, check_appendable, encoded_event_bytes,
    require_append_acknowledgement, retained_append_events, unwrap_result, wire_seat_events,
};
use super::{PendingAppend, RetryPolicy, SeatShared, append_outcome_unknown};

/// Replies that were 2xx but not a valid acknowledgement, tolerated before
/// the outcome is reported unknown (identical answers will not improve).
const MALFORMED_REPLY_LIMIT: u32 = 5;

/// Why an append did not end in a verified acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendFailure {
    /// The outcome is unknown; the request stays retained and pinned.
    Unknown(String),
    /// The daemon definitely refused, or the producer is no longer current.
    Refused(String),
    /// A local rule stopped the append before anything was sent.
    Fatal(String),
}

impl AppendFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Unknown(message) | Self::Refused(message) | Self::Fatal(message) => message,
        }
    }
}

/// The result of one append.
pub type AppendResult = Result<VerifiedAck, AppendFailure>;

fn engine_exhausted() -> AckError {
    AckError("producer engine interval is exhausted".to_owned())
}

fn probe_request<'a>(
    producer: &'a UiPresentationProducer,
    events: &'a [WireSeatEvent],
) -> AppendRequest<'a> {
    AppendRequest {
        presentation_id: &producer.presentation_id,
        thread_id: &producer.thread_id,
        producer_incarnation: &producer.producer_incarnation,
        operation_id: "00000000-0000-8000-8000-000000000000",
        first_engine_seq: u64::MAX,
        last_engine_seq: u64::MAX,
        event_count: u64::try_from(events.len()).unwrap_or(u64::MAX),
        payload_digest: "0000000000000000000000000000000000000000000000000000000000000000",
        events,
    }
}

/// How many leading events fit one request when every engine position is
/// assumed to be as wide as it can be (20 digits). The real request can
/// therefore never exceed the bound. At most [`MAX_SEAT_BATCH_EVENTS`].
pub fn append_prefix_len(
    events: &[PlainSeatEvent],
    producer: &UiPresentationProducer,
    limit: u64,
) -> usize {
    let mut wire: Vec<WireSeatEvent> = Vec::new();
    let mut count = 0;
    for event in events.iter().take(MAX_SEAT_BATCH_EVENTS) {
        wire.push(WireSeatEvent {
            engine_seq: u64::MAX,
            event_type: event.event_type.clone(),
            payload: event.payload.clone(),
        });
        let Ok(encoded) = encode_append_request(&probe_request(producer, &wire)) else {
            break;
        };
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > limit {
            break;
        }
        count += 1;
    }
    count
}

/// The size of the next batch: what fits the byte bound, cut to the
/// remaining engine positions. A single event that exceeds the bound, or an
/// exhausted producer interval, is an error rather than a silent drop.
pub fn plan_append_len(
    events: &[PlainSeatEvent],
    producer: &UiPresentationProducer,
    first_engine_seq: u64,
    limit: u64,
) -> Result<usize, AckError> {
    let remaining = u64::MAX - first_engine_seq;
    if remaining == 0 || events.is_empty() {
        return Err(engine_exhausted());
    }
    let fitting = append_prefix_len(events, producer, limit);
    let cut = usize::try_from(remaining).unwrap_or(usize::MAX);
    if fitting == 0 {
        return Err(AckError(format!(
            "one seat event exceeds the {limit}-byte append route limit"
        )));
    }
    Ok(fitting.min(cut))
}

/// Counts the local events not yet acknowledged and refuses to let more than
/// [`MAX_PENDING_SEAT_EVENTS`] events or [`MAX_PENDING_SEAT_BYTES`] encoded
/// bytes pile up behind a stuck writer.
#[derive(Debug, Default)]
pub struct PendingTracker {
    base: usize,
    sizes: VecDeque<usize>,
    bytes: usize,
}

impl PendingTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything: a new producer starts from a clean log.
    pub fn reset(&mut self, acknowledged: usize) {
        self.base = acknowledged;
        self.sizes.clear();
        self.bytes = 0;
    }

    /// Account for events appended to the log since the last call and drop
    /// the acknowledged prefix. An error is final: the seat is blocked.
    pub fn observe(&mut self, events: &[SeatEvent], acknowledged: usize) -> Result<(), String> {
        let observed = self.base + self.sizes.len();
        if events.len() < observed {
            return Err("RyeOS seat event log moved backwards".to_owned());
        }
        while self.base < acknowledged
            && let Some(size) = self.sizes.pop_front()
        {
            self.bytes -= size;
            self.base += 1;
        }
        if self.base < acknowledged {
            self.base = acknowledged;
        }
        let start = self.base + self.sizes.len();
        for event in events.get(start..).unwrap_or(&[]) {
            let size = PlainSeatEvent::try_from(event)
                .and_then(|plain| encoded_event_bytes(&plain))
                .map_err(|error| error.to_string())?;
            if self.sizes.len() >= MAX_PENDING_SEAT_EVENTS
                || self.bytes + size > MAX_PENDING_SEAT_BYTES
            {
                return Err(format!(
                    "unsaved seat history exceeds the pending limit ({MAX_PENDING_SEAT_EVENTS} events / {MAX_PENDING_SEAT_BYTES} bytes)"
                ));
            }
            self.sizes.push_back(size);
            self.bytes += size;
        }
        Ok(())
    }

    /// Unacknowledged events being held.
    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }
}

/// What the exact-retry loop needs to re-read the session after an
/// authentication response.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The session the append was admitted under.
    pub session_id: String,
    /// The root binding the seat was created under.
    pub root: UiBindingAttachmentCoordinate,
    /// The seat being written.
    pub coordinate: SeatCoordinate,
}

async fn reconcile_authentication<P: Platform>(
    platform: &P,
    context: &AuthContext,
) -> Result<(), String> {
    let text = platform
        .transport()
        .get(SESSION_CURRENT_PATH)
        .await
        .map_err(|error| error.to_string())?;
    let value = decode_session_value(&text).map_err(|error| error.to_string())?;
    require_current_session(
        &value,
        &context.session_id,
        &context.root,
        &context.coordinate,
    )
    .map_err(|error| error.to_string())
}

fn decode_session_value(text: &str) -> Result<Value, crate::error::RuntimeError> {
    let value = decode_strict(text)?;
    Ok(value.get("result").cloned().unwrap_or(value))
}

fn parse_ack(
    text: &str,
    expected: &ExpectedAck,
    staged: &[WireSeatEvent],
    coordinate: &SeatCoordinate,
) -> Result<VerifiedAck, String> {
    let decoded = decode_strict(text)
        .map_err(|error| format!("seat append acknowledgement is unreadable: {error}"))?;
    let record = unwrap_result(&decoded)
        .map_err(|error| format!("seat append acknowledgement is unreadable: {error}"))?;
    require_append_acknowledgement(record, expected, staged, coordinate)
        .map_err(|error| format!("seat append acknowledgement is invalid: {error}"))
}

/// Send `encoded` to `seat/append` until a valid acknowledgement, a definite
/// refusal or the policy's budget. Every attempt sends the same bytes.
///
/// `still_current` is asked after every unsuccessful reply and again after
/// each backoff: once it is false (the producer was replaced or fenced, or the
/// delivery no longer holds this append) the retry stops with an unknown
/// outcome instead of resending bytes nobody is waiting for.
pub async fn post_append_exactly<P: Platform>(
    platform: &P,
    context: &AuthContext,
    encoded: &str,
    expected: &ExpectedAck,
    staged: &[WireSeatEvent],
    policy: RetryPolicy,
    still_current: &dyn Fn() -> bool,
) -> AppendResult {
    let superseded = || {
        AppendFailure::Unknown(
            "seat append retry stopped: its delivery is no longer current".to_owned(),
        )
    };
    let mut authentication_reconciled = false;
    let mut malformed = 0_u32;
    let mut attempt = 1_u32;
    loop {
        let unknown = match platform
            .transport()
            .post_encoded(SEAT_APPEND_PATH, encoded.to_owned())
            .await
        {
            Ok(text) => match parse_ack(&text, expected, staged, &context.coordinate) {
                Ok(ack) => return Ok(ack),
                Err(message) => {
                    malformed += 1;
                    if malformed >= MALFORMED_REPLY_LIMIT {
                        return Err(AppendFailure::Unknown(message));
                    }
                    message
                }
            },
            Err(error) => {
                if matches!(error.status(), Some(401 | 403)) && !authentication_reconciled {
                    authentication_reconciled = true;
                    if let Err(message) = reconcile_authentication(platform, context).await {
                        return Err(AppendFailure::Refused(message));
                    }
                    if !still_current() {
                        return Err(superseded());
                    }
                    continue;
                }
                if !append_outcome_unknown(&error) {
                    return Err(AppendFailure::Refused(error.to_string()));
                }
                match error.status() {
                    Some(_) => format!("seat append response leaves delivery unknown: {error}"),
                    None => error.to_string(),
                }
            }
        };
        if !still_current() {
            return Err(superseded());
        }
        if !policy.allows_another(attempt) {
            return Err(AppendFailure::Unknown(unknown));
        }
        crate::platform::Clock::sleep(platform.clock(), policy.delay(attempt)).await;
        if !still_current() {
            return Err(superseded());
        }
        attempt += 1;
    }
}

fn expected_ack(
    producer: &str,
    operation_id: &str,
    record: &RetainedPendingAppend,
) -> Option<ExpectedAck> {
    Some(ExpectedAck {
        producer: producer.to_owned(),
        operation_id: operation_id.to_owned(),
        first_engine_seq: record.first_engine_seq.parse().ok()?,
        last_engine_seq: record.last_engine_seq.parse().ok()?,
        event_count: record.event_count,
        payload_digest: record.payload_digest.clone(),
    })
}

/// Send one captured prefix of the local seat log as an idempotent append and
/// settle the delivery state on a verified acknowledgement.
///
/// The request is retained before it is sent and cleared only after the
/// acknowledgement. Any failure leaves the delivery blocked: a later writer
/// must reconcile the retained request first.
pub async fn send_append<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    captured: &UiPresentationProducer,
    events: &[SeatEvent],
    policy: RetryPolicy,
) -> AppendResult {
    let platform = &*shared.platform;
    let (coordinate, first, limit, session_id, root, key) = {
        let delivery = shared.delivery.borrow();
        if !shared.is_current(captured) || events.is_empty() {
            return Err(AppendFailure::Refused(
                "seat append names a producer that is no longer current".to_owned(),
            ));
        }
        if delivery.blocked {
            let message = "seat delivery is blocked".to_owned();
            return Err(if delivery.pending.is_some() {
                AppendFailure::Unknown(message)
            } else {
                AppendFailure::Refused(message)
            });
        }
        let (Some(coordinate), Some(root), Some(key)) = (
            delivery.coordinate.clone(),
            delivery.root.clone(),
            delivery.retention_key.clone(),
        ) else {
            return Err(AppendFailure::Refused(
                "seat delivery is not initialised".to_owned(),
            ));
        };
        (
            coordinate,
            delivery.next_engine_seq,
            delivery.max_seat_append_bytes,
            delivery.session_id.clone(),
            root,
            key,
        )
    };
    // Only the producer that failed is fenced: a late failure of a
    // predecessor must not block the successor's delivery.
    let block = |failure: AppendFailure| {
        if shared.is_current(captured) {
            shared.delivery.borrow_mut().blocked = true;
        }
        failure
    };

    // Freeze the exact request from the captured prefix.
    let prepared = (|| -> Result<_, AckError> {
        check_appendable(events, captured, first)?;
        let plain: Vec<PlainSeatEvent> = events
            .iter()
            .map(PlainSeatEvent::try_from)
            .collect::<Result<_, _>>()?;
        let wire = wire_seat_events(&plain, first)?;
        let digest = wire_events_digest(&wire).map_err(|e| AckError(e.to_string()))?;
        let count = u64::try_from(wire.len()).map_err(|_| engine_exhausted())?;
        let last = first.checked_add(count - 1).ok_or_else(engine_exhausted)?;
        let operation_id = UiSeatAppendIdentity {
            presentation_id: captured.presentation_id.clone(),
            chain_root_id: coordinate.chain_root_id.clone(),
            thread_id: captured.thread_id.clone(),
            producer_incarnation: captured.producer_incarnation.clone(),
            first_engine_seq: first,
            last_engine_seq: last,
            event_count: count,
            payload_digest: digest.clone(),
        }
        .derive_identity()
        .map_err(|e| AckError(e.to_owned()))?;
        let encoded = encode_append_request(&AppendRequest {
            presentation_id: &captured.presentation_id,
            thread_id: &captured.thread_id,
            producer_incarnation: &captured.producer_incarnation,
            operation_id: &operation_id,
            first_engine_seq: first,
            last_engine_seq: last,
            event_count: count,
            payload_digest: &digest,
            events: &wire,
        })
        .map_err(|e| AckError(e.to_string()))?;
        Ok((wire, digest, count, last, operation_id, encoded))
    })();
    let (wire, digest, count, last, operation_id, encoded) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(block(AppendFailure::Fatal(error.0))),
    };
    if limit == 0 || u64::try_from(encoded.len()).unwrap_or(u64::MAX) > limit {
        return Err(block(AppendFailure::Fatal(
            "seat append exceeds its signed byte bound".to_owned(),
        )));
    }
    let record = RetainedPendingAppend {
        schema_version: PENDING_APPEND_SCHEMA.to_owned(),
        session_id: session_id.clone(),
        presentation_id: captured.presentation_id.clone(),
        chain_root_id: coordinate.chain_root_id.clone(),
        thread_id: captured.thread_id.clone(),
        producer_incarnation: captured.producer_incarnation.clone(),
        operation_id: operation_id.clone(),
        first_engine_seq: first.to_string(),
        last_engine_seq: last.to_string(),
        event_count: count,
        payload_digest: digest.clone(),
        encoded_request: encoded.clone(),
    };

    // Retain before sending. An existing pending append must be this one.
    {
        let mut delivery = shared.delivery.borrow_mut();
        match &delivery.pending {
            Some(existing) if existing.record != record || existing.events != events => {
                delivery.blocked = true;
                return Err(AppendFailure::Unknown(
                    "captured prefix differs from the retained append retry".to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                // Chosen behaviour: the retention write is synchronous and a
                // refused write is fatal, so an unwritable store ends the session.
                if let Err(error) = retention::retain(platform.store(), &key, &record) {
                    delivery.blocked = true;
                    return Err(AppendFailure::Fatal(error.to_string()));
                }
                delivery.pending = Some(PendingAppend {
                    record: record.clone(),
                    events: events.to_vec(),
                });
            }
        }
    }

    let expected = ExpectedAck {
        producer: captured.producer_incarnation.clone(),
        operation_id: operation_id.clone(),
        first_engine_seq: first,
        last_engine_seq: last,
        event_count: count,
        payload_digest: digest,
    };
    let context = AuthContext {
        session_id,
        root,
        coordinate,
    };
    let still_current = || {
        let delivery = shared.delivery.borrow();
        !delivery.blocked
            && shared.is_current(captured)
            && delivery
                .pending
                .as_ref()
                .is_some_and(|pending| pending.record == record && pending.events == events)
    };
    let result = post_append_exactly(
        platform,
        &context,
        &encoded,
        &expected,
        &wire,
        policy,
        &still_current,
    )
    .await;
    let ack = match result {
        Ok(ack) => ack,
        Err(failure) => return Err(block(failure)),
    };

    // The delivery must still be this append, unblocked, under this producer.
    let mut delivery = shared.delivery.borrow_mut();
    let still_ours = delivery
        .pending
        .as_ref()
        .is_some_and(|pending| pending.record == record && pending.events == events)
        && !delivery.blocked
        && shared.is_current(captured);
    if !still_ours {
        if shared.is_current(captured) {
            delivery.blocked = true;
        }
        return Err(AppendFailure::Unknown(
            "seat append lost its retained identity before acknowledgement".to_owned(),
        ));
    }
    if delivery.contradicts_verified(&ack.summary) {
        delivery.blocked = true;
        return Err(AppendFailure::Unknown(
            "verified seat append differs from its exact acknowledgement".to_owned(),
        ));
    }
    delivery.next_engine_seq = last + 1;
    delivery.remember_receipt(ack.summary.clone());
    delivery.pending = None;
    if let Err(error) = retention::clear_matching(
        platform.store(),
        &key,
        &operation_id,
        &captured.producer_incarnation,
        Some(encoded.as_str()),
    ) {
        delivery.blocked = true;
        return Err(AppendFailure::Unknown(error.to_string()));
    }
    Ok(ack)
}

/// A retained request from an earlier life of this seat, settled against the
/// daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciledAppend {
    pub record: RetainedPendingAppend,
    pub ack: VerifiedAck,
    /// The events the retained request carried, in wire form.
    pub events: Vec<WireSeatEvent>,
}

/// Settle the retained append of `coordinate`, if any: ask the daemon for its
/// receipt by operation identity (`seat/replay`), and when it has none resend
/// exactly the retained bytes. The request, never the local log, is the
/// evidence. `Ok(None)` means nothing was retained.
pub async fn reconcile_retained_append<P: Platform>(
    platform: &P,
    session_id: &str,
    root: &UiBindingAttachmentCoordinate,
    coordinate: &SeatCoordinate,
    key: &str,
    policy: RetryPolicy,
) -> Result<Option<ReconciledAppend>, AppendFailure> {
    let fatal = |message: &str| AppendFailure::Fatal(message.to_owned());
    let unknown = |message: &str| AppendFailure::Unknown(message.to_owned());
    let Some(record) = retention::load(platform.store(), key)
        .map_err(|error| AppendFailure::Fatal(error.to_string()))?
    else {
        return Ok(None);
    };
    if record.session_id != session_id
        || record.presentation_id != coordinate.presentation_id
        || record.thread_id != coordinate.thread_id
        || record.chain_root_id != coordinate.chain_root_id
    {
        return Err(fatal("retained seat append belongs to a different session"));
    }
    let expected = expected_ack(&record.producer_incarnation, &record.operation_id, &record)
        .ok_or_else(|| fatal("retained append request differs from its exact staged interval"))?;
    let identity = UiSeatAppendIdentity {
        presentation_id: coordinate.presentation_id.clone(),
        chain_root_id: coordinate.chain_root_id.clone(),
        thread_id: coordinate.thread_id.clone(),
        producer_incarnation: expected.producer.clone(),
        first_engine_seq: expected.first_engine_seq,
        last_engine_seq: expected.last_engine_seq,
        event_count: expected.event_count,
        payload_digest: expected.payload_digest.clone(),
    }
    .derive_identity();
    if identity.ok().as_deref() != Some(record.operation_id.as_str()) {
        return Err(fatal(
            "retained append uses an unsupported or conflicting transport identity",
        ));
    }
    let events = retained_append_events(&record.encoded_request, &expected, coordinate)
        .map_err(|error| AppendFailure::Fatal(error.0))?;
    let digest =
        wire_events_digest(&events).map_err(|error| AppendFailure::Fatal(error.to_string()))?;
    if digest != expected.payload_digest {
        return Err(fatal(
            "retained append payload differs from its exact commitment",
        ));
    }

    let lookup = encode_exact(&json!({
        "presentation_id": coordinate.presentation_id,
        "chain_root_id": coordinate.chain_root_id,
        "receipt": {
            "thread_id": record.thread_id,
            "producer_incarnation": expected.producer,
            "operation_id": expected.operation_id,
            "first_engine_seq": expected.first_engine_seq,
            "last_engine_seq": expected.last_engine_seq,
            "event_count": expected.event_count,
            "payload_digest": expected.payload_digest,
        },
    }))
    .map_err(|error| AppendFailure::Fatal(error.to_string()))?;
    let text = platform
        .transport()
        .post_encoded(SEAT_REPLAY_PATH, lookup)
        .await
        .map_err(|error| {
            if append_outcome_unknown(&error) {
                AppendFailure::Unknown(error.to_string())
            } else {
                AppendFailure::Refused(error.to_string())
            }
        })?;
    let decoded =
        decode_strict(&text).map_err(|error| AppendFailure::Unknown(error.to_string()))?;
    let observed = unwrap_result(&decoded).map_err(|error| AppendFailure::Unknown(error.0))?;
    let fields = observed.as_object();
    let well_formed = fields.is_some_and(|fields| {
        fields.len() == 2 && fields.contains_key("receipt_status") && fields.contains_key("receipt")
    });
    if !well_formed {
        return Err(unknown(
            "prior seat append receipt lookup has an unsupported response",
        ));
    }
    let status = observed.get("receipt_status").and_then(Value::as_str);
    let receipt = observed.get("receipt").unwrap_or(&Value::Null);
    let context = AuthContext {
        session_id: session_id.to_owned(),
        root: root.clone(),
        coordinate: coordinate.clone(),
    };
    let ack = match (status, receipt) {
        (Some("found"), receipt) if receipt.is_object() => {
            require_append_acknowledgement(receipt, &expected, &events, coordinate)
                .map_err(|error| AppendFailure::Unknown(error.0))?
        }
        (Some("not_found"), Value::Null) => {
            // The authenticated history has no receipt yet, but the exact
            // request survived. Resend only those bytes: a late original
            // delivery is the daemon's ordinary idempotent retry.
            post_append_exactly(
                platform,
                &context,
                &record.encoded_request,
                &expected,
                &events,
                policy,
                &|| true,
            )
            .await?
        }
        _ => {
            return Err(unknown(
                "prior seat append receipt is not confirmed; replacement claim blocked",
            ));
        }
    };
    Ok(Some(ReconciledAppend {
        record,
        ack,
        events,
    }))
}
