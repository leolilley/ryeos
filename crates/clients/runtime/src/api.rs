//! The daemon's UI API, written once for every client: endpoint paths,
//! request bodies and the size bounds clients enforce before sending.
//!
//! Bodies are built as `serde_json::Value` or, where the retained bytes must
//! be reproducible, as an encoded string. Every encoded body goes through
//! [`crate::codec::encode_exact`].

use std::fmt::Write as _;

use ryeos_client_base::ui::binding::{
    UiBindingPayload, UiBindingRequest, UiBindingRequestBounds, UiInvocationRequest,
    UiPresentationProducer,
};
use serde_json::{Value, json};

use crate::codec::{MAX_SAFE_INTEGER, encode_exact};
use crate::error::RuntimeError;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub const SESSION_CURRENT_PATH: &str = "/ui/api/session/current";
pub const LAUNCH_MINT_PATH: &str = "/ui/api/launch/mint";
pub const INVOCATION_DISPATCH_PATH: &str = "/ui/api/invocations/dispatch";
pub const ATTACHMENT_DETACH_PATH: &str = "/ui/api/session/attachments/detach";

pub const SEAT_OPEN_PATH: &str = "/ui/api/session/seat/open";
pub const SEAT_APPEND_PATH: &str = "/ui/api/session/seat/append";
pub const SEAT_TOUCH_PATH: &str = "/ui/api/session/seat/touch";
pub const SEAT_CLOSE_PATH: &str = "/ui/api/session/seat/close";
pub const SEAT_TRANSFER_PATH: &str = "/ui/api/session/seat/transfer";
pub const SEAT_HISTORY_PATH: &str = "/ui/api/session/seat/history";
pub const SEAT_REPLAY_PATH: &str = "/ui/api/session/seat/replay";

const SESSION_EVENTS_PREFIX: &str = "/ui/events/session/";

/// The seat operations that take a JSON body under `/ui/api/session/seat/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatOperation {
    Open,
    Append,
    Touch,
    Close,
    Transfer,
    History,
    Replay,
}

impl SeatOperation {
    pub fn path(self) -> &'static str {
        match self {
            Self::Open => SEAT_OPEN_PATH,
            Self::Append => SEAT_APPEND_PATH,
            Self::Touch => SEAT_TOUCH_PATH,
            Self::Close => SEAT_CLOSE_PATH,
            Self::Transfer => SEAT_TRANSFER_PATH,
            Self::History => SEAT_HISTORY_PATH,
            Self::Replay => SEAT_REPLAY_PATH,
        }
    }
}

/// Session id as the daemon mints it: non-empty, at most 64 characters of
/// ASCII letters, digits and `-`.
fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// A single path segment: no separator, query, fragment, escape or control.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '?' | '#' | '%'))
}

/// Transient hint stream for one UI session. The browser follows the
/// `events_url` the daemon returns instead; both name this route.
pub fn session_events_path(session_id: &str) -> Result<String, RuntimeError> {
    if !valid_session_id(session_id) {
        return Err(RuntimeError::BindingRequest(
            "UI session id is invalid".to_owned(),
        ));
    }
    Ok(format!("{SESSION_EVENTS_PREFIX}{session_id}"))
}

/// Live tail of one chain root and its child threads. Frames are
/// `{ "event_type": ..., "payload": <persisted event record> }`; the route is
/// authenticated by the browser session, like the session hint stream.
pub fn chain_events_stream_path(chain_root_id: &str) -> Result<String, RuntimeError> {
    if !valid_segment(chain_root_id) {
        return Err(RuntimeError::BindingRequest(
            "chain root id is invalid".to_owned(),
        ));
    }
    Ok(format!("/ui/events/chain/{chain_root_id}"))
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Events in one append request.
pub const MAX_SEAT_BATCH_EVENTS: usize = 64;
/// Delivery attempts of one append before the outcome is reported unknown.
pub const MAX_SEAT_APPEND_ATTEMPTS: u32 = 5;
/// Unsaved seat events held before the seat is fenced.
pub const MAX_PENDING_SEAT_EVENTS: usize = 512;
/// Encoded bytes of unsaved seat events held before the seat is fenced.
pub const MAX_PENDING_SEAT_BYTES: usize = 512 * 1024;
/// Acknowledged operations remembered for duplicate and ordering checks.
pub const MAX_REMEMBERED_APPENDS: usize = 128;
/// One seat history frame, before decoding.
pub const MAX_SEAT_HISTORY_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Events in one seat history `events` frame.
pub const MAX_SEAT_HISTORY_BATCH_EVENTS: usize = 64;
/// Cold replay attempts before bootstrap gives up.
pub const MAX_COLD_REPLAY_ATTEMPTS: u32 = 3;
/// Coalescing window for session hints.
pub const HINT_BATCH_MS: u64 = 500;
const APPEND_RETRY_BASE_MS: u64 = 100;
const APPEND_RETRY_CAP_MS: u64 = 2000;

/// Pause before append attempt `attempt + 1` (`attempt` counts from 1):
/// 100, 200, 400, 800 ms, capped at 2000 ms.
pub fn append_retry_delay_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(16);
    (APPEND_RETRY_BASE_MS << shift).min(APPEND_RETRY_CAP_MS)
}

/// A daemon-signed byte bound must be positive and exactly representable by a
/// JavaScript number so every client reads the same limit.
pub fn safe_bound(value: u64, name: &str) -> Result<u64, RuntimeError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(RuntimeError::BindingRequest(format!(
            "invalid UI binding {name}"
        )));
    }
    Ok(value)
}

/// Refuse `encoded` when its UTF-8 length exceeds `maximum`.
pub fn require_within(encoded: &str, maximum: u64, message: &str) -> Result<(), RuntimeError> {
    let length = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    if length > maximum {
        return Err(RuntimeError::BindingRequest(message.to_owned()));
    }
    Ok(())
}

fn encode_within(
    body: &Value,
    bounds: UiBindingRequestBounds,
    message: &str,
) -> Result<String, RuntimeError> {
    let encoded = encode_exact(body)?;
    let maximum = safe_bound(bounds.max_request_bytes, "max_request_bytes")?;
    require_within(&encoded, maximum, message)?;
    Ok(encoded)
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// Body of `seat/history`.
pub fn history_body(presentation_id: &str, chain_root_id: &str) -> Value {
    json!({ "presentation_id": presentation_id, "chain_root_id": chain_root_id })
}

/// Encoded read-only observation of a destination seat, bounded by the
/// effect's admitted request size.
pub fn observe_destination_request(
    presentation_id: &str,
    thread_id: &str,
    chain_root_id: &str,
    bounds: UiBindingRequestBounds,
) -> Result<String, RuntimeError> {
    let body = json!({
        "action": "observe",
        "presentation_id": presentation_id,
        "thread_id": thread_id,
        "chain_root_id": chain_root_id,
    });
    encode_within(
        &body,
        bounds,
        "transfer observation exceeds admitted request bound",
    )
}

/// Encoded transfer preparation contact.
pub fn transfer_prepare_request(
    transfer: &impl serde::Serialize,
    export: &impl serde::Serialize,
    freeze: &impl serde::Serialize,
    bounds: UiBindingRequestBounds,
) -> Result<String, RuntimeError> {
    let body = json!({
        "action": "prepare",
        "transfer": serde_json::to_value(transfer)?,
        "export": serde_json::to_value(export)?,
        "freeze": serde_json::to_value(freeze)?,
    });
    encode_within(
        &body,
        bounds,
        "transfer preparation exceeds admitted request bound",
    )
}

/// Encoded transfer contact (accept, commit, abort, activate, ...). The
/// request already carries its `action` tag.
pub fn transfer_contact_request(
    request: &impl serde::Serialize,
    bounds: UiBindingRequestBounds,
) -> Result<String, RuntimeError> {
    let body = serde_json::to_value(request)?;
    encode_within(
        &body,
        bounds,
        "transfer contact exceeds admitted request bound",
    )
}

/// Encoded binding dispatch for the current producer, with the session
/// bounds applied: total request size and, for free-text input, input size.
pub fn dispatch_request(
    producer: &UiPresentationProducer,
    binding: &UiBindingRequest,
    presentation_operation_id: Option<&str>,
    bounds: UiBindingRequestBounds,
) -> Result<String, RuntimeError> {
    let maximum = safe_bound(bounds.max_request_bytes, "max_request_bytes")?;
    let input_maximum = safe_bound(bounds.max_input_bytes, "max_input_bytes")?;
    let invocation = UiInvocationRequest {
        producer: producer.clone(),
        binding: binding.clone(),
        presentation_operation_id: presentation_operation_id.map(str::to_owned),
    };
    let encoded = encode_exact(&serde_json::to_value(&invocation)?)?;
    require_within(
        &encoded,
        maximum,
        "UI binding request exceeds the session bound",
    )?;
    if let UiBindingPayload::Input { value, .. } = &binding.payload {
        require_within(
            value,
            input_maximum,
            "UI binding input exceeds the session bound",
        )?;
    }
    Ok(encoded)
}

/// Encoded release of one binding attachment, authenticated by the producer.
pub fn detach_request(
    producer: &UiPresentationProducer,
    binding_attachment_id: &str,
    binding_generation: u64,
    binding_digest: &str,
) -> Result<String, RuntimeError> {
    let body = json!({
        "producer": serde_json::to_value(producer)?,
        "binding_attachment_id": binding_attachment_id,
        "binding_generation": binding_generation,
        "binding_digest": binding_digest,
    });
    encode_exact(&body)
}

/// Add the current producer to a body bound for a producer-authenticated
/// route: `invocations/dispatch` takes a binding request and wraps it in an
/// invocation; `attachments/detach` takes a record and gains a `producer`
/// field. Any other path is returned unchanged.
pub fn wrap_with_producer(
    path: &str,
    body: &Value,
    producer: Option<&UiPresentationProducer>,
) -> Result<Value, RuntimeError> {
    if path != INVOCATION_DISPATCH_PATH && path != ATTACHMENT_DETACH_PATH {
        return Ok(body.clone());
    }
    let producer = producer.ok_or_else(|| {
        RuntimeError::BindingRequest("presentation producer is not ready".to_owned())
    })?;
    if path == INVOCATION_DISPATCH_PATH {
        let binding: UiBindingRequest = serde_json::from_value(body.clone())?;
        return Ok(serde_json::to_value(UiInvocationRequest {
            producer: producer.clone(),
            binding,
            presentation_operation_id: None,
        })?);
    }
    let mut request = body.clone();
    let Some(record) = request.as_object_mut() else {
        return Err(RuntimeError::BindingRequest(
            "attachment detach request must be a record".to_owned(),
        ));
    };
    record.insert("producer".to_owned(), serde_json::to_value(producer)?);
    Ok(request)
}

// ---------------------------------------------------------------------------
// Append request
// ---------------------------------------------------------------------------

/// One event as sent to `seat/append`.
#[derive(Debug, Clone, PartialEq)]
pub struct WireSeatEvent {
    pub engine_seq: u64,
    pub event_type: String,
    pub payload: Value,
}

impl WireSeatEvent {
    /// The value committed by the payload digest.
    pub fn to_value(&self) -> Value {
        json!({
            "engine_seq": self.engine_seq,
            "event_type": self.event_type,
            "payload": self.payload,
        })
    }
}

/// Digest of a batch of wire events: the commitment is over the array of
/// events exactly as sent.
pub fn wire_events_digest(events: &[WireSeatEvent]) -> Result<String, RuntimeError> {
    let batch = Value::Array(events.iter().map(WireSeatEvent::to_value).collect());
    Ok(crate::codec::seat_payload_digest(&batch)?)
}

/// Everything one `seat/append` body is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendRequest<'a> {
    pub presentation_id: &'a str,
    pub thread_id: &'a str,
    pub producer_incarnation: &'a str,
    pub operation_id: &'a str,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: u64,
    pub payload_digest: &'a str,
    pub events: &'a [WireSeatEvent],
}

/// Encode an append body with keys in the order every client writes them.
/// The exact string is what is retained and resent, so the order is part of
/// the retention contract.
pub fn encode_append_request(request: &AppendRequest<'_>) -> Result<String, RuntimeError> {
    let text = |value: &str| serde_json::to_string(value).map_err(RuntimeError::from);
    let mut out = String::new();
    write!(
        out,
        "{{\"presentation_id\":{},\"thread_id\":{},\"producer_incarnation\":{},\"operation_id\":{},\
         \"first_engine_seq\":{},\"last_engine_seq\":{},\"event_count\":{},\"payload_digest\":{},\"events\":[",
        text(request.presentation_id)?,
        text(request.thread_id)?,
        text(request.producer_incarnation)?,
        text(request.operation_id)?,
        request.first_engine_seq,
        request.last_engine_seq,
        request.event_count,
        text(request.payload_digest)?,
    )
    .expect("String write");
    for (index, event) in request.events.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"engine_seq\":{},\"event_type\":{},\"payload\":{}}}",
            event.engine_seq,
            text(&event.event_type)?,
            encode_exact(&event.payload)?,
        )
        .expect("String write");
    }
    out.push_str("]}");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays_double_then_cap() {
        let delays: Vec<u64> = (1..=7).map(append_retry_delay_ms).collect();
        assert_eq!(delays, [100, 200, 400, 800, 1600, 2000, 2000]);
    }

    #[test]
    fn bounds_must_be_safe_positive_integers() {
        assert!(safe_bound(0, "x").is_err());
        assert!(safe_bound(MAX_SAFE_INTEGER, "x").is_ok());
        assert!(safe_bound(MAX_SAFE_INTEGER + 1, "x").is_err());
    }

    #[test]
    fn stream_paths_refuse_separators() {
        assert_eq!(
            session_events_path("abc-1").unwrap(),
            "/ui/events/session/abc-1"
        );
        assert!(session_events_path("a/b").is_err());
        assert_eq!(
            chain_events_stream_path("T-1").unwrap(),
            "/ui/events/chain/T-1"
        );
        assert!(chain_events_stream_path("a/../b").is_err());
    }

    #[test]
    fn append_request_keeps_wire_key_order() {
        let events = [WireSeatEvent {
            engine_seq: 0,
            event_type: "seat.facet".into(),
            payload: json!({"draft": "retained"}),
        }];
        let encoded = encode_append_request(&AppendRequest {
            presentation_id: "A",
            thread_id: "thread-A",
            producer_incarnation: "p",
            operation_id: "o",
            first_engine_seq: 0,
            last_engine_seq: 0,
            event_count: 1,
            payload_digest: "d",
            events: &events,
        })
        .unwrap();
        assert_eq!(
            encoded,
            "{\"presentation_id\":\"A\",\"thread_id\":\"thread-A\",\"producer_incarnation\":\"p\",\
             \"operation_id\":\"o\",\"first_engine_seq\":0,\"last_engine_seq\":0,\"event_count\":1,\
             \"payload_digest\":\"d\",\"events\":[{\"engine_seq\":0,\"event_type\":\"seat.facet\",\
             \"payload\":{\"draft\":\"retained\"}}]}"
        );
    }
}
