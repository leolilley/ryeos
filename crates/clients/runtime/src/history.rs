//! Consuming one seat history stream.
//!
//! `POST seat/history` answers with an event stream: a `begin` frame, batches
//! of canonical rows (each at most [`MAX_SEAT_HISTORY_BATCH_EVENTS`]),
//! optional paired transfer proofs, and a `complete` fence. Everything before
//! the fence is provisional. This module checks the stream's own continuity
//! and the completion fence, hands each batch to a [`HistorySink`], and on
//! completion returns a [`LiveHistory`] that keeps reading the same stream for
//! `append_verified` testimony, reproof wakes and invalidation.
//!
//! One accumulator serves every consumer (fresh claim, resume, replacement,
//! same-producer reproof); what a consumer additionally requires of the rows
//! lives in its sink.
//!
//! Continuity rules, which follow the daemon's fixed-anchor reader:
//!
//! * `chain_seq` (when present) and `thread_seq` are strictly increasing;
//! * the predecessor hash must match only when `thread_seq` is exactly the
//!   previous one plus one, because the provisional stream omits non-fold
//!   events and so legitimately skips positions;
//! * the last `ui_seat.producer.v3` control seen must be the acknowledged
//!   claim: schema, operation, incarnation, state `active`, `next_engine_seq`
//!   and the request echo;
//! * the completion fence must name the last staged row and the exact count.
//!
//! Failure texts are the browser client's, which the shared vectors pin.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use ryeos_client_base::ui::UiSeatControlRequest;
use serde_json::{Map, Value};

use crate::api::{MAX_SEAT_HISTORY_BATCH_EVENTS, SEAT_HISTORY_PATH, history_body};
use crate::codec::{decode_strict, encode_exact, exact_u64};
use crate::error::RuntimeError;
use crate::platform::{ByteSource, Transport};
use crate::sse::{SseFrame, SseMode, SseReader, StreamError};

/// Event types of the history stream.
pub const BEGIN: &str = "ui_seat.replay.begin.v1";
pub const EVENTS: &str = "ui_seat.replay.events.v1";
pub const TRANSFER_PROOF: &str = "ui_seat.replay.transfer_proof.v1";
pub const CANCELLATION_PROOF: &str = "ui_seat.replay.move_freeze_cancellation_proof.v1";
pub const COMPLETE: &str = "ui_seat.replay.complete.v1";
pub const APPEND_VERIFIED: &str = "ui_seat.replay.append_verified.v1";
pub const REPROOF_REQUIRED: &str = "ui_seat.replay.reproof_required.v1";
pub const INVALIDATED: &str = "ui_seat.replay.invalidated.v1";

/// Event type of a seat producer control row.
pub const PRODUCER_EVENT: &str = "ui_seat.producer.v3";
const PRODUCER_SCHEMA: &str = "ryeos.ui.seat.producer.v3";

/// Verified operations remembered for duplicate checks.
const REMEMBERED_VERIFIED: usize = 128;

/// The presentation seat a stream belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeatCoordinate {
    pub presentation_id: String,
    pub chain_root_id: String,
    pub thread_id: String,
}

/// The acknowledged producer claim a completed stream must retain.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryClaim {
    pub operation_id: String,
    pub producer_incarnation: String,
    pub next_engine_seq: u64,
    /// When present, the control row's `request` must decode to exactly this
    /// request (a resume or takeover names its predecessor there).
    pub request: Option<UiSeatControlRequest>,
}

/// The fence a stream completed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCompletion {
    pub tip_hash: String,
    pub thread_seq: u64,
    pub event_count: u64,
}

/// One `append_verified` testimony.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAppend {
    pub operation_id: String,
    pub producer_incarnation: String,
    pub first_engine_seq: u64,
    pub last_engine_seq: u64,
    pub event_count: u64,
    pub payload_digest: String,
    pub event_hash: String,
    pub thread_seq: u64,
}

/// Where provisional rows go while a stream is read. A failure discards the
/// whole candidate; nothing staged is authoritative until completion.
pub trait HistorySink {
    /// A batch of 1..=64 canonical rows.
    fn stage_events(&mut self, records: &[Value]) -> Result<(), String>;
    /// A paired transfer proof frame's `proof`.
    fn stage_transfer_proof(&mut self, proof: &Value) -> Result<(), String>;
    /// A move-freeze cancellation proof frame's `proof`.
    fn stage_cancellation_proof(&mut self, proof: &Value) -> Result<(), String>;
}

/// Why a stream did not yield a completed proof.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryError {
    /// The stream ended or was invalidated before its fence. The staged rows
    /// are provisional; a fresh stream may be tried.
    Incomplete(String),
    /// A completed cut needs replacement; carries no phase authority.
    ReproofRequired(String),
    /// The stream broke a protocol rule or a sink refused a row.
    Refused(String),
    /// The request itself failed (status, delivery).
    Transport(RuntimeError),
}

impl HistoryError {
    /// True when a fresh attempt of the same proof may succeed.
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Self::Incomplete(_))
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete(message) | Self::ReproofRequired(message) | Self::Refused(message) => {
                formatter.write_str(message)
            }
            Self::Transport(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HistoryError {}

impl From<RuntimeError> for HistoryError {
    fn from(error: RuntimeError) -> Self {
        Self::Transport(error)
    }
}

fn refused(message: &str) -> HistoryError {
    HistoryError::Refused(message.to_owned())
}

fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn exact_u64_field(value: Option<&Value>) -> Result<u64, HistoryError> {
    value
        .and_then(exact_u64)
        .ok_or_else(|| refused("seat history integer is invalid"))
}

fn schema_is_one(data: &Map<String, Value>) -> bool {
    data.get("schema").and_then(Value::as_u64) == Some(1)
}

/// Primitive testimony copied out of a producer control row, so nothing a
/// sink does can change what the completion fence compares.
#[derive(Debug, Clone)]
struct ControlTip {
    schema_version: Option<String>,
    operation_id: Option<String>,
    producer_incarnation: Option<String>,
    state: Option<String>,
    next_engine_seq: u64,
    request: Value,
}

fn control_tip(payload: &Value) -> Result<ControlTip, HistoryError> {
    let payload = payload
        .as_object()
        .ok_or_else(|| refused("seat history producer control has no payload"))?;
    let request = payload
        .get("request")
        .filter(|request| request.is_object())
        .ok_or_else(|| refused("seat history producer control has no request"))?;
    let text = |name: &str| payload.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(ControlTip {
        schema_version: text("schema_version"),
        operation_id: text("operation_id"),
        producer_incarnation: text("producer_incarnation"),
        state: text("state"),
        next_engine_seq: exact_u64_field(payload.get("next_engine_seq"))?,
        request: request.clone(),
    })
}

fn decode_frame_data(frame: &SseFrame) -> Result<Map<String, Value>, HistoryError> {
    match decode_strict(&frame.data) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(refused("seat history frame payload is not an object")),
        Err(error) => Err(HistoryError::Refused(error.to_string())),
    }
}

fn frame_error(error: StreamError) -> HistoryError {
    match error {
        StreamError::Parse(crate::sse::SseError::FrameTooLarge) => {
            refused("seat history frame exceeds bound")
        }
        StreamError::Parse(crate::sse::SseError::UnsupportedField) => {
            refused("seat history has duplicate or unsupported SSE field")
        }
        StreamError::Parse(crate::sse::SseError::MissingField) => {
            refused("seat history frame omitted event or payload")
        }
        StreamError::Parse(error) => HistoryError::Refused(error.to_string()),
        StreamError::Transport(error) => HistoryError::Incomplete(error.to_string()),
    }
}

/// Open `seat/history` for `coordinate` and consume it to its completion
/// fence. A fresh request and a fresh proof on every call.
pub async fn read_history<T: Transport>(
    transport: &T,
    coordinate: &SeatCoordinate,
    claim: &HistoryClaim,
    sink: &mut impl HistorySink,
) -> Result<LiveHistory<T::Frames>, HistoryError> {
    let body = encode_exact(&history_body(
        &coordinate.presentation_id,
        &coordinate.chain_root_id,
    ))?;
    let source = transport.open_history(SEAT_HISTORY_PATH, body).await?;
    consume_history(source, coordinate, claim, sink).await
}

/// Consume an already-open history stream to its completion fence.
pub async fn consume_history<S: ByteSource>(
    source: S,
    coordinate: &SeatCoordinate,
    claim: &HistoryClaim,
    sink: &mut impl HistorySink,
) -> Result<LiveHistory<S>, HistoryError> {
    let mut reader = SseReader::new(source, SseMode::Strict);
    let mut state = Accumulator::default();
    loop {
        let frame = match reader.next_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return Err(HistoryError::Incomplete(
                    if reader.ended_mid_frame() {
                        "seat history ended with an incomplete frame"
                    } else {
                        "seat history stream ended before completion"
                    }
                    .to_owned(),
                ));
            }
            Err(error) => return Err(frame_error(error)),
        };
        let data = decode_frame_data(&frame)?;
        if let Some(completion) = state.accept(&frame.event, data, coordinate, claim, sink)? {
            return Ok(LiveHistory {
                reader,
                completion: completion.clone(),
                last_thread_seq: completion.thread_seq,
                last_event_hash: Some(completion.tip_hash),
                recent: HashMap::new(),
                order: VecDeque::new(),
                finished: false,
            });
        }
    }
}

#[derive(Default)]
struct Accumulator {
    began: bool,
    event_count: u64,
    last_chain_seq: Option<u64>,
    last_thread_seq: u64,
    last_hash: Option<String>,
    last_control: Option<ControlTip>,
}

impl Accumulator {
    /// Fold one pre-completion frame. `Some` is the completion fence.
    fn accept(
        &mut self,
        event: &str,
        data: Map<String, Value>,
        seat: &SeatCoordinate,
        claim: &HistoryClaim,
        sink: &mut impl HistorySink,
    ) -> Result<Option<HistoryCompletion>, HistoryError> {
        match event {
            BEGIN => {
                let text = |name: &str| data.get(name).and_then(Value::as_str);
                if self.began
                    || !schema_is_one(&data)
                    || text("presentation_id") != Some(seat.presentation_id.as_str())
                    || text("chain_root_id") != Some(seat.chain_root_id.as_str())
                    || text("thread_id") != Some(seat.thread_id.as_str())
                {
                    return Err(refused(
                        "seat history begin does not match the claimed seat",
                    ));
                }
                self.began = true;
            }
            EVENTS => self.accept_events(&data, seat, sink)?,
            TRANSFER_PROOF | CANCELLATION_PROOF => {
                let message = if event == TRANSFER_PROOF {
                    "seat history transfer proof has invalid shape or order"
                } else {
                    "seat history move freeze cancellation proof has invalid shape or order"
                };
                let proof = data.get("proof").filter(|proof| proof.is_object());
                let (true, true, 2, Some(proof)) =
                    (self.began, schema_is_one(&data), data.len(), proof)
                else {
                    return Err(refused(message));
                };
                let staged = if event == TRANSFER_PROOF {
                    sink.stage_transfer_proof(proof)
                } else {
                    sink.stage_cancellation_proof(proof)
                };
                staged.map_err(HistoryError::Refused)?;
            }
            COMPLETE => return self.complete(&data, seat, claim).map(Some),
            INVALIDATED if self.began && schema_is_one(&data) && data.len() == 1 => {
                return Err(HistoryError::Incomplete(
                    "seat history was invalidated before completion".to_owned(),
                ));
            }
            other => {
                return Err(HistoryError::Refused(format!(
                    "seat history has unknown frame {other}"
                )));
            }
        }
        Ok(None)
    }

    fn accept_events(
        &mut self,
        data: &Map<String, Value>,
        seat: &SeatCoordinate,
        sink: &mut impl HistorySink,
    ) -> Result<(), HistoryError> {
        let batch_error = || refused("seat history batch has invalid shape or order");
        let events = data
            .get("events")
            .and_then(Value::as_array)
            .filter(|events| !events.is_empty() && events.len() <= MAX_SEAT_HISTORY_BATCH_EVENTS)
            .ok_or_else(batch_error)?;
        if !self.began || !schema_is_one(data) {
            return Err(batch_error());
        }
        let mut next = Accumulator {
            began: true,
            event_count: self.event_count,
            last_chain_seq: self.last_chain_seq,
            last_thread_seq: self.last_thread_seq,
            last_hash: self.last_hash.clone(),
            last_control: self.last_control.clone(),
        };
        for entry in events {
            let Some(entry) = entry.as_object() else {
                return Err(refused("seat history event has invalid retained identity"));
            };
            let hash = entry.get("event_hash").and_then(Value::as_str);
            if entry.get("thread_id").and_then(Value::as_str) != Some(seat.thread_id.as_str())
                || entry.get("chain_root_id").and_then(Value::as_str)
                    != Some(seat.chain_root_id.as_str())
                || !hash.is_some_and(is_lower_hex_64)
            {
                return Err(refused("seat history event has invalid retained identity"));
            }
            let hash = hash.unwrap_or_default();
            let thread_seq = exact_u64_field(entry.get("thread_seq"))?;
            // The server proves the complete signed interval. Its provisional
            // stream omits non-fold events (notably thread_created), so only
            // monotone emitted order is checked, not adjacency.
            if thread_seq <= next.last_thread_seq {
                return Err(refused("seat history event order moved backwards"));
            }
            if let Some(chain_seq) = entry.get("chain_seq") {
                let chain_seq = exact_u64_field(Some(chain_seq))?;
                if next
                    .last_chain_seq
                    .is_some_and(|previous| chain_seq <= previous)
                {
                    return Err(refused("seat history event order moved backwards"));
                }
                next.last_chain_seq = Some(chain_seq);
            }
            if next.last_hash.is_some()
                && next.last_thread_seq.checked_add(1) == Some(thread_seq)
                && entry.get("prev_thread_event_hash").and_then(Value::as_str)
                    != next.last_hash.as_deref()
            {
                return Err(refused("seat history predecessor is discontinuous"));
            }
            next.last_thread_seq = thread_seq;
            next.last_hash = Some(hash.to_owned());
            if entry.get("event_type").and_then(Value::as_str) == Some(PRODUCER_EVENT) {
                next.last_control =
                    Some(control_tip(entry.get("payload").unwrap_or(&Value::Null))?);
            }
            next.event_count += 1;
        }
        sink.stage_events(events).map_err(HistoryError::Refused)?;
        *self = next;
        Ok(())
    }

    fn complete(
        &self,
        data: &Map<String, Value>,
        seat: &SeatCoordinate,
        claim: &HistoryClaim,
    ) -> Result<HistoryCompletion, HistoryError> {
        let tip_hash = data
            .get("tip_hash")
            .and_then(Value::as_str)
            .filter(|hash| is_lower_hex_64(hash));
        let (true, true, Some(tip_hash)) = (self.began, schema_is_one(data), tip_hash) else {
            return Err(refused("seat history completion is invalid"));
        };
        let count = exact_u64_field(data.get("event_count"))?;
        let tip_seq = exact_u64_field(data.get("thread_seq"))?;
        if count != self.event_count
            || tip_seq != self.last_thread_seq
            || self
                .last_hash
                .as_deref()
                .is_some_and(|last| last != tip_hash)
        {
            return Err(refused(
                "seat history completion does not match staged events",
            ));
        }
        let retains_claim = self.last_control.as_ref().is_some_and(|control| {
            let request = control.request.as_object();
            let request_text = |name: &str| {
                request
                    .and_then(|request| request.get(name))
                    .and_then(Value::as_str)
            };
            control.schema_version.as_deref() == Some(PRODUCER_SCHEMA)
                && control.operation_id.as_deref() == Some(claim.operation_id.as_str())
                && control.producer_incarnation.as_deref()
                    == Some(claim.producer_incarnation.as_str())
                && control.state.as_deref() == Some("active")
                && control.next_engine_seq == claim.next_engine_seq
                && request_text("action") == Some("claim")
                && request_text("operation_id") == Some(claim.operation_id.as_str())
                && request_text("presentation_id") == Some(seat.presentation_id.as_str())
                && request_text("chain_root_id") == Some(seat.chain_root_id.as_str())
                && request_text("thread_id") == Some(seat.thread_id.as_str())
                && claim.request.as_ref().is_none_or(|expected| {
                    serde_json::from_value::<UiSeatControlRequest>(control.request.clone())
                        .ok()
                        .as_ref()
                        == Some(expected)
                })
        });
        if !retains_claim {
            return Err(refused(
                "seat history completion does not retain the acknowledged producer claim",
            ));
        }
        Ok(HistoryCompletion {
            tip_hash: tip_hash.to_owned(),
            thread_seq: tip_seq,
            event_count: count,
        })
    }
}

/// What the live part of a completed stream reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// Verified testimony for an append.
    Verified(VerifiedAppend),
    /// The completed cut needs replacement (exactly `{"schema":1}`).
    ReproofRequired,
    /// The handoff is over; carries the reason. Nothing more is delivered.
    Invalidated(String),
}

/// A completed stream, still being read.
pub struct LiveHistory<S: ByteSource> {
    reader: SseReader<S>,
    completion: HistoryCompletion,
    last_thread_seq: u64,
    last_event_hash: Option<String>,
    recent: HashMap<String, (u64, String)>,
    order: VecDeque<String>,
    finished: bool,
}

impl<S: ByteSource> LiveHistory<S> {
    /// The fence this stream completed at.
    pub fn completion(&self) -> &HistoryCompletion {
        &self.completion
    }

    /// The next live event. After [`LiveEvent::ReproofRequired`] or
    /// [`LiveEvent::Invalidated`] every call answers `Invalidated`.
    pub async fn next(&mut self) -> LiveEvent {
        if self.finished {
            return LiveEvent::Invalidated("seat history live handoff ended".to_owned());
        }
        let event = self.read_next().await;
        if !matches!(event, LiveEvent::Verified(_)) {
            self.finished = true;
        }
        event
    }

    async fn read_next(&mut self) -> LiveEvent {
        let frame = match self.reader.next_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return LiveEvent::Invalidated("seat history live handoff ended".to_owned());
            }
            Err(StreamError::Transport(error)) => {
                return LiveEvent::Invalidated(error.to_string());
            }
            Err(error) => return LiveEvent::Invalidated(frame_error(error).to_string()),
        };
        let data = match decode_frame_data(&frame) {
            Ok(data) => data,
            Err(error) => return LiveEvent::Invalidated(error.to_string()),
        };
        match frame.event.as_str() {
            APPEND_VERIFIED => match self.verify(&data) {
                Ok(append) => LiveEvent::Verified(append),
                Err(error) => LiveEvent::Invalidated(error.to_string()),
            },
            REPROOF_REQUIRED if schema_is_one(&data) && data.len() == 1 => {
                LiveEvent::ReproofRequired
            }
            INVALIDATED if schema_is_one(&data) && data.len() == 1 => {
                LiveEvent::Invalidated("seat history was invalidated".to_owned())
            }
            _ => LiveEvent::Invalidated(
                "seat history emitted unsupported post-completion frame".to_owned(),
            ),
        }
    }

    /// Check one verified append against this stream's own high-water mark and
    /// its memory of recent operations, then remember it.
    fn verify(&mut self, data: &Map<String, Value>) -> Result<VerifiedAppend, HistoryError> {
        let append = parse_verified_append(data)?;
        let duplicate = self.recent.get(&append.operation_id);
        if duplicate
            .is_some_and(|(seq, hash)| *seq != append.thread_seq || *hash != append.event_hash)
        {
            return Err(refused(
                "seat history verified append reused an operation with different testimony",
            ));
        }
        let exact_repeat = duplicate.is_some();
        if append.thread_seq < self.last_thread_seq && !exact_repeat {
            return Err(refused("seat history verified append moved backwards"));
        }
        if append.thread_seq == self.last_thread_seq
            && self.last_event_hash.as_deref() != Some(append.event_hash.as_str())
        {
            return Err(refused(
                "seat history verified append changed the retained tip",
            ));
        }
        if append.thread_seq > self.last_thread_seq {
            self.last_thread_seq = append.thread_seq;
            self.last_event_hash = Some(append.event_hash.clone());
        }
        if !exact_repeat {
            self.recent.insert(
                append.operation_id.clone(),
                (append.thread_seq, append.event_hash.clone()),
            );
            self.order.push_back(append.operation_id.clone());
            if self.order.len() > REMEMBERED_VERIFIED
                && let Some(oldest) = self.order.pop_front()
            {
                self.recent.remove(&oldest);
            }
        }
        Ok(append)
    }
}

fn is_uuid_v8(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => *byte == b'8',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn parse_verified_append(data: &Map<String, Value>) -> Result<VerifiedAppend, HistoryError> {
    const FIELDS: [&str; 9] = [
        "schema",
        "operation_id",
        "producer_incarnation",
        "first_engine_seq",
        "last_engine_seq",
        "event_count",
        "payload_digest",
        "event_hash",
        "thread_seq",
    ];
    let text = |name: &str| data.get(name).and_then(Value::as_str);
    let shape_ok = data.len() == FIELDS.len()
        && FIELDS.iter().all(|field| data.contains_key(*field))
        && schema_is_one(data)
        && text("operation_id").is_some_and(is_uuid_v8)
        && text("producer_incarnation").is_some_and(is_lower_hex_64)
        && text("payload_digest").is_some_and(is_lower_hex_64)
        && text("event_hash").is_some_and(is_lower_hex_64);
    if !shape_ok {
        return Err(refused("seat history verified append has invalid shape"));
    }
    let first = exact_u64_field(data.get("first_engine_seq"))?;
    let last = exact_u64_field(data.get("last_engine_seq"))?;
    let count = exact_u64_field(data.get("event_count"))?;
    let thread_seq = exact_u64_field(data.get("thread_seq"))?;
    if count == 0 || first > last || last - first + 1 != count || thread_seq == 0 {
        return Err(refused("seat history verified append has invalid interval"));
    }
    let owned = |name: &str| text(name).unwrap_or_default().to_owned();
    Ok(VerifiedAppend {
        operation_id: owned("operation_id"),
        producer_incarnation: owned("producer_incarnation"),
        first_engine_seq: first,
        last_engine_seq: last,
        event_count: count,
        payload_digest: owned("payload_digest"),
        event_hash: owned("event_hash"),
        thread_seq,
    })
}
