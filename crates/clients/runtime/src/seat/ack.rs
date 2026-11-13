//! Append requests and their acknowledgements, checked against each other.
//!
//! The daemon's acknowledgement is evidence for the staged local prefix only
//! when its durable interval, not merely its echoed summary, matches it. The
//! checks are the union of what the clients did separately:
//!
//! * the response has exactly the twelve acknowledgement keys, and the echoed
//!   operation, producer, positions, count and digest equal the request;
//! * every event row has exactly the eleven row keys, a distinct 64-hex hash,
//!   the seat's chain and thread, durable status, a non-empty timestamp, null
//!   or 64-hex predecessors, the staged type, the staged engine position and a
//!   payload with the staged digest framing;
//! * chain positions run `first + index`, consecutive rows are linked by hash
//!   and thread position, and the receipt follows the last row at
//!   `thread_seq + 1` with a hash different from the last row's.
//!
//! Positions are read as the daemon sends them in an acknowledgement (decimal
//! strings), and a JSON integer is accepted as well.

use std::collections::BTreeSet;
use std::fmt;

use ryeos_client_base::ui::SeatEvent;
use ryeos_client_base::ui::SeatEventKind;
use ryeos_client_base::ui::binding::UiPresentationProducer;
use serde_json::{Map, Value};

use crate::api::WireSeatEvent;
use crate::codec::{decode_exact, encode_exact, seat_payload_digest};
use crate::history::{SeatCoordinate, VerifiedAppend};

use super::is_lower_hex_64;

/// A refused acknowledgement. Every one leaves the append's outcome unknown:
/// a 2xx answer that cannot be validated proves nothing about what committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckError(pub String);

impl fmt::Display for AckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AckError {}

fn ack_error(message: &str) -> AckError {
    AckError(message.to_owned())
}

const ACK_KEYS: [&str; 12] = [
    "producer_incarnation",
    "operation_id",
    "first_engine_seq",
    "last_engine_seq",
    "event_count",
    "payload_digest",
    "appended",
    "chain_seq",
    "receipt_thread_seq",
    "receipt_event_hash",
    "events_authority",
    "events",
];

const ROW_KEYS: [&str; 11] = [
    "event_hash",
    "chain_root_id",
    "chain_seq",
    "thread_id",
    "thread_seq",
    "event_type",
    "durability",
    "ts",
    "prev_chain_event_hash",
    "prev_thread_event_hash",
    "payload",
];

/// Strip up to two `{ "result": ... }` layers from a seat service response.
pub fn unwrap_result(value: &Value) -> Result<&Value, AckError> {
    if !value.is_object() {
        return Err(ack_error("seat service returned a non-object response"));
    }
    let first = match value.get("result") {
        Some(inner) if inner.is_object() => inner,
        _ => value,
    };
    Ok(match first.get("result") {
        Some(inner) if inner.is_object() => inner,
        _ => first,
    })
}

/// Read a non-negative exact integer: a JSON integer or a canonical decimal
/// string (`0`, or digits without a leading zero).
pub fn parse_exact_integer(value: &Value, name: &str) -> Result<u64, AckError> {
    let invalid = || {
        AckError(format!(
            "seat service returned an invalid exact integer for {name}"
        ))
    };
    match value {
        Value::Number(number) => number.as_u64().ok_or_else(invalid),
        Value::String(text) => {
            let canonical = text == "0"
                || (!text.starts_with('0')
                    && !text.is_empty()
                    && text.bytes().all(|byte| byte.is_ascii_digit()));
            if canonical {
                text.parse().map_err(|_| invalid())
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}

fn integer_field(record: &Map<String, Value>, name: &str) -> Result<u64, AckError> {
    match record.get(name) {
        None | Some(Value::Null) => Err(AckError(format!("seat service returned no {name}"))),
        Some(value) => parse_exact_integer(value, name),
    }
}

fn required_text<'a>(record: &'a Map<String, Value>, name: &str) -> Result<&'a str, AckError> {
    match record.get(name).and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(AckError(format!("seat service returned no {name}"))),
    }
}

fn optional_hash(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) => true,
        Some(Value::String(text)) => is_lower_hex_64(text),
        _ => false,
    }
}

/// What a request promised an acknowledgement would echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedAck {
    pub producer: String,
    pub operation_id: String,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: u64,
    pub payload_digest: String,
}

/// The identity and receipt of one acknowledged append, kept to cross-check
/// later verified-append testimony.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckSummary {
    pub operation_id: String,
    pub producer_incarnation: String,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: u64,
    pub payload_digest: String,
    pub receipt_event_hash: String,
    pub receipt_thread_seq: u64,
}

impl AckSummary {
    /// True when `append` names the same operation, producer, interval and
    /// digest.
    pub(crate) fn matches(&self, append: &VerifiedAppend) -> bool {
        self.operation_id == append.operation_id
            && self.producer_incarnation == append.producer_incarnation
            && self.first_engine_seq == append.first_engine_seq
            && self.last_engine_seq == append.last_engine_seq
            && self.event_count == append.event_count
            && self.payload_digest == append.payload_digest
    }
}

/// A fully checked acknowledgement.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedAck {
    pub summary: AckSummary,
    /// The authoritative rows of the appended interval, as sent.
    pub rows: Vec<Value>,
}

fn digest_of(value: &Value) -> Option<String> {
    seat_payload_digest(value).ok()
}

/// Check `response` (already unwrapped) as the acknowledgement of the request
/// described by `expected` and `staged` on the seat `coordinate`.
pub fn require_append_acknowledgement(
    response: &Value,
    expected: &ExpectedAck,
    staged: &[WireSeatEvent],
    coordinate: &SeatCoordinate,
) -> Result<VerifiedAck, AckError> {
    let mismatch =
        || ack_error("seat append acknowledgement does not match the submitted operation");
    let record = response
        .as_object()
        .ok_or_else(|| ack_error("seat service returned a non-object response"))?;
    if record.keys().any(|key| !ACK_KEYS.contains(&key.as_str())) {
        return Err(mismatch());
    }
    let echoed_ok = required_text(record, "producer_incarnation")? == expected.producer
        && required_text(record, "operation_id")? == expected.operation_id
        && integer_field(record, "first_engine_seq")? == expected.first_engine_seq
        && integer_field(record, "last_engine_seq")? == expected.last_engine_seq
        && integer_field(record, "event_count")? == expected.event_count
        && integer_field(record, "appended")? == expected.event_count
        && required_text(record, "payload_digest")? == expected.payload_digest;
    if !echoed_ok {
        return Err(mismatch());
    }
    let events = record.get("events").and_then(Value::as_array);
    let count = usize::try_from(expected.event_count).unwrap_or(usize::MAX);
    let authoritative =
        record.get("events_authority").and_then(Value::as_str) == Some("authoritative");
    let events = match events {
        Some(events) if authoritative && events.len() == count && staged.len() == count => events,
        _ => {
            return Err(ack_error(
                "seat append acknowledgement omitted its authoritative event interval",
            ));
        }
    };
    let receipt_chain_seq = integer_field(record, "chain_seq")?;
    let receipt_thread_seq = integer_field(record, "receipt_thread_seq")?;
    let receipt_hash = required_text(record, "receipt_event_hash")?;
    if !is_lower_hex_64(receipt_hash) {
        return Err(ack_error(
            "seat append acknowledgement receipt hash is invalid",
        ));
    }
    let first_chain_seq = receipt_chain_seq
        .checked_sub(expected.event_count)
        .ok_or_else(|| ack_error("seat append acknowledgement event interval underflows"))?;

    let changed =
        || ack_error("seat append acknowledgement changed its authoritative event interval");
    let mut previous: Option<(&str, u64)> = None;
    let mut hashes = BTreeSet::new();
    for (index, (row, staged)) in events.iter().zip(staged).enumerate() {
        let Some(row) = row.as_object() else {
            return Err(changed());
        };
        if row.keys().any(|key| !ROW_KEYS.contains(&key.as_str())) {
            return Err(changed());
        }
        let hash = row.get("event_hash").and_then(Value::as_str);
        let Some(hash) = hash.filter(|hash| is_lower_hex_64(hash)) else {
            return Err(changed());
        };
        if hashes.contains(hash)
            || row.get("chain_root_id").and_then(Value::as_str)
                != Some(coordinate.chain_root_id.as_str())
            || row.get("thread_id").and_then(Value::as_str) != Some(coordinate.thread_id.as_str())
        {
            return Err(changed());
        }
        let offset = u64::try_from(index).unwrap_or(u64::MAX);
        if Some(integer_field(row, "chain_seq")?) != first_chain_seq.checked_add(offset) {
            return Err(changed());
        }
        let ts_ok = row
            .get("ts")
            .and_then(Value::as_str)
            .is_some_and(|ts| !ts.is_empty());
        if row.get("event_type").and_then(Value::as_str) != Some(staged.event_type.as_str())
            || row.get("durability").and_then(Value::as_str) != Some("durable")
            || !ts_ok
            || !optional_hash(row.get("prev_chain_event_hash"))
            || !optional_hash(row.get("prev_thread_event_hash"))
        {
            return Err(changed());
        }
        let Some(payload) = row.get("payload").and_then(Value::as_object) else {
            return Err(changed());
        };
        if integer_field(payload, "seq")? != staged.engine_seq {
            return Err(changed());
        }
        let acknowledged_digest = payload.get("payload").and_then(digest_of);
        if acknowledged_digest.is_none() || acknowledged_digest != digest_of(&staged.payload) {
            return Err(changed());
        }
        hashes.insert(hash);
        let thread_seq = integer_field(row, "thread_seq")?;
        if let Some((previous_hash, previous_seq)) = previous
            && (previous_seq.checked_add(1) != Some(thread_seq)
                || row.get("prev_chain_event_hash").and_then(Value::as_str) != Some(previous_hash)
                || row.get("prev_thread_event_hash").and_then(Value::as_str) != Some(previous_hash))
        {
            return Err(ack_error(
                "seat append acknowledgement event links are not contiguous",
            ));
        }
        previous = Some((hash, thread_seq));
    }
    let Some((last_hash, last_thread_seq)) = previous else {
        return Err(ack_error(
            "seat append acknowledgement receipt is not adjacent to its event interval",
        ));
    };
    if last_thread_seq.checked_add(1) != Some(receipt_thread_seq) || receipt_hash == last_hash {
        return Err(ack_error(
            "seat append acknowledgement receipt is not adjacent to its event interval",
        ));
    }
    Ok(VerifiedAck {
        summary: AckSummary {
            operation_id: expected.operation_id.clone(),
            producer_incarnation: expected.producer.clone(),
            first_engine_seq: expected.first_engine_seq,
            last_engine_seq: expected.last_engine_seq,
            event_count: expected.event_count,
            payload_digest: expected.payload_digest.clone(),
            receipt_event_hash: receipt_hash.to_owned(),
            receipt_thread_seq,
        },
        rows: events.clone(),
    })
}

/// Parse the events of a retained request and check the request against what
/// it promised: closed keys, the seat, the producer, the operation, the
/// interval, the count and the digest.
pub fn retained_append_events(
    encoded_request: &str,
    expected: &ExpectedAck,
    coordinate: &SeatCoordinate,
) -> Result<Vec<WireSeatEvent>, AckError> {
    const KEYS: [&str; 9] = [
        "presentation_id",
        "thread_id",
        "producer_incarnation",
        "operation_id",
        "first_engine_seq",
        "last_engine_seq",
        "event_count",
        "payload_digest",
        "events",
    ];
    let request = decode_exact(encoded_request).map_err(|error| AckError(error.to_string()))?;
    let differs = || ack_error("retained append request differs from its exact staged interval");
    let record = request.as_object().ok_or_else(differs)?;
    if record.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(differs());
    }
    let text = |name: &str| record.get(name).and_then(Value::as_str);
    if text("presentation_id") != Some(coordinate.presentation_id.as_str())
        || text("thread_id") != Some(coordinate.thread_id.as_str())
        || text("producer_incarnation") != Some(expected.producer.as_str())
        || text("operation_id") != Some(expected.operation_id.as_str())
        || integer_field(record, "first_engine_seq")? != expected.first_engine_seq
        || integer_field(record, "last_engine_seq")? != expected.last_engine_seq
        || integer_field(record, "event_count")? != expected.event_count
        || text("payload_digest") != Some(expected.payload_digest.as_str())
    {
        return Err(differs());
    }
    let events = record
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(differs)?;
    if u64::try_from(events.len()).ok() != Some(expected.event_count) {
        return Err(differs());
    }
    let invalid = || ack_error("retained append contains an invalid staged event interval");
    let mut out = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let event = event.as_object().ok_or_else(invalid)?;
        let offset = u64::try_from(index).unwrap_or(u64::MAX);
        let engine_seq = integer_field(event, "engine_seq")?;
        let event_type = event.get("event_type").and_then(Value::as_str);
        if Some(engine_seq) != expected.first_engine_seq.checked_add(offset)
            || event_type.is_none_or(str::is_empty)
            || !event.contains_key("payload")
        {
            return Err(invalid());
        }
        out.push(WireSeatEvent {
            engine_seq,
            event_type: event_type.unwrap_or_default().to_owned(),
            payload: event.get("payload").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(out)
}

/// A seat event in the neutral form the wire carries: a local sequence, the
/// event's type and its payload. The wire builder works on this so batch
/// planning is independent of the core's event enum.
#[derive(Debug, Clone, PartialEq)]
pub struct PlainSeatEvent {
    /// The core's local sequence (not the producer position).
    pub seq: u64,
    /// The producer position the event was reserved at, if any.
    pub producer_engine_seq: Option<u64>,
    pub event_type: String,
    pub payload: Value,
}

impl TryFrom<&SeatEvent> for PlainSeatEvent {
    type Error = AckError;

    fn try_from(event: &SeatEvent) -> Result<Self, AckError> {
        let kind =
            serde_json::to_value(&event.kind).map_err(|error| AckError(error.to_string()))?;
        let event_type = kind
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or_else(|| ack_error("seat event has no type"))?
            .to_owned();
        Ok(Self {
            seq: event.seq,
            producer_engine_seq: event.producer_engine_seq,
            event_type,
            payload: kind.get("payload").cloned().unwrap_or(Value::Null),
        })
    }
}

/// Events the core must never send: initialization is written by the daemon,
/// and a prepared operation is bound to its own producer and position.
pub fn check_appendable(
    events: &[SeatEvent],
    producer: &UiPresentationProducer,
    first_engine_seq: u64,
) -> Result<(), AckError> {
    for (offset, event) in events.iter().enumerate() {
        let expected = first_engine_seq.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let bad = match &event.kind {
            SeatEventKind::PresentationInitialized(_) => true,
            SeatEventKind::PresentationOperationPrepared(operation) => {
                operation.prepared_engine_seq != expected
                    || &operation.producer != producer
                    || operation.validate().is_err()
            }
            _ => false,
        };
        if bad {
            return Err(ack_error(
                "captured event differs from its original producer interval",
            ));
        }
    }
    Ok(())
}

/// Bind events to the reserved producer positions starting at
/// `first_engine_seq`. Local sequences are replaced by the reserved
/// positions; an event may not be rebased to another position, and the last
/// usable position is `u64::MAX - 1`.
pub fn wire_seat_events(
    events: &[PlainSeatEvent],
    first_engine_seq: u64,
) -> Result<Vec<WireSeatEvent>, AckError> {
    let exhausted = || ack_error("producer engine interval is exhausted");
    events
        .iter()
        .enumerate()
        .map(|(offset, event)| {
            let offset = u64::try_from(offset).map_err(|_| exhausted())?;
            let engine_seq = first_engine_seq.checked_add(offset).ok_or_else(exhausted)?;
            if engine_seq == u64::MAX {
                return Err(exhausted());
            }
            if event
                .producer_engine_seq
                .is_some_and(|original| original != engine_seq)
            {
                return Err(ack_error(
                    "captured event cannot be rebased to another producer interval",
                ));
            }
            if event.event_type == "seat.presentation_operation.prepared" {
                let payload = event.payload.as_object();
                let schema_ok = payload
                    .and_then(|p| p.get("schema"))
                    .and_then(Value::as_u64)
                    == Some(2);
                let position = payload
                    .and_then(|p| p.get("prepared_engine_seq"))
                    .and_then(|value| parse_exact_integer(value, "prepared_engine_seq").ok());
                if !schema_ok || position != Some(engine_seq) {
                    return Err(ack_error(
                        "prepared operation differs from its reserved producer position",
                    ));
                }
            }
            Ok(WireSeatEvent {
                engine_seq,
                event_type: event.event_type.clone(),
                payload: event.payload.clone(),
            })
        })
        .collect()
}

/// UTF-8 length of one event as it would appear in a request, using its local
/// sequence: what the pending-queue bound counts.
pub fn encoded_event_bytes(event: &PlainSeatEvent) -> Result<usize, AckError> {
    let encoded_type =
        serde_json::to_string(&event.event_type).map_err(|e| AckError(e.to_string()))?;
    let payload = encode_exact(&event.payload).map_err(|error| AckError(error.to_string()))?;
    Ok(format!(
        "{{\"engine_seq\":{},\"event_type\":{},\"payload\":{}}}",
        event.seq, encoded_type, payload
    )
    .len())
}
