//! The seat state machine: one presentation's durable writer.
//!
//! A seat is a thread. A client claims it (fresh, resume or takeover), replays
//! its authenticated history into a private core, and from then on mirrors
//! every local seat event into the thread with idempotent appends. This module
//! owns the pieces every client shares:
//!
//! * [`SeatDelivery`]: the one unresolved append, the receipts the daemon
//!   acknowledged, and the verified-append testimony that must agree with them;
//! * acknowledgement verification (`ack`) and wire event construction;
//! * append planning, exact retry under a [`RetryPolicy`] and reconciliation
//!   of a retained request after a restart (`append`);
//! * observe, touch and relinquish (`control`);
//! * claim, replay, reproof and in-place session replacement (`bootstrap`).
//!
//! Every network step is a method over the platform seams in
//! [`crate::platform`]; nothing here names a browser or an executor.

mod ack;
mod append;
mod bootstrap;
mod control;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use ryeos_client_base::ui::SeatEvent;
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{UiBindingAttachmentCoordinate, UiSeatControlRequest};
use serde_json::Value;

use crate::codec::exact_u64;
use crate::error::RuntimeError;
use crate::history::{LiveHistory, SeatCoordinate, VerifiedAppend};
use crate::platform::{Platform, Transport};
use crate::retention::RetainedPendingAppend;

pub use ack::{
    AckError, AckSummary, ExpectedAck, PlainSeatEvent, VerifiedAck, check_appendable,
    encoded_event_bytes, parse_exact_integer, require_append_acknowledgement,
    retained_append_events, unwrap_result, wire_seat_events,
};
pub use append::{
    AppendFailure, AppendResult, AuthContext, PendingTracker, ReconciledAppend, append_prefix_len,
    plan_append_len, post_append_exactly, reconcile_retained_append, send_append,
};
pub use bootstrap::{
    ClaimIntent, Claimed, ReplayOutput, ReproofOutput, RetainedClaim, SeatStart, claim_and_replay,
    read_session, replace_session_in_place, reproof_seat, session_unchanged,
};
pub use control::{observe_producer, relinquish_claimed_seat, relinquish_current, touch_seat};

/// Frames of a platform's history streams.
pub type Frames<P> = <<P as Platform>::Transport as Transport>::Frames;

/// Presence is renewed this often while the seat is idle.
pub const TOUCH_INTERVAL_MS: u64 = 60_000;
/// After a touch that could not be confirmed, the next try comes this soon.
pub const TOUCH_RETRY_AFTER_MS: u64 = 1_000;
/// Acknowledged receipts and verified operations remembered for cross-checks.
pub(crate) const REMEMBERED_RECEIPTS: usize = 128;

/// How long an unresolved append is retried before the outcome is reported
/// unknown. The schedule is `min(max_backoff, min_backoff * 2^(attempt-1))`
/// after attempt `attempt` (counting from 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// Retry until a definite answer or shutdown (the terminal has no
    /// recovery interface).
    Unbounded {
        min_backoff: Duration,
        max_backoff: Duration,
    },
    /// Give up after `attempts` tries; the caller then offers recovery (the
    /// browser).
    Bounded {
        attempts: u32,
        min_backoff: Duration,
        max_backoff: Duration,
    },
}

impl RetryPolicy {
    /// Five attempts, 100 ms doubling to 2 s.
    pub const WEB: Self = Self::Bounded {
        attempts: 5,
        min_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(2_000),
    };

    /// Unbounded, 100 ms doubling to 5 s.
    pub const TERMINAL: Self = Self::Unbounded {
        min_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(5_000),
    };

    /// Pause after failed attempt number `attempt` (from 1).
    pub fn delay(&self, attempt: u32) -> Duration {
        let (Self::Unbounded {
            min_backoff,
            max_backoff,
        }
        | Self::Bounded {
            min_backoff,
            max_backoff,
            ..
        }) = *self;
        let shift = attempt.saturating_sub(1).min(16);
        min_backoff.saturating_mul(1 << shift).min(max_backoff)
    }

    /// Whether failed attempt number `attempt` may be followed by another.
    pub fn allows_another(&self, attempt: u32) -> bool {
        match self {
            Self::Unbounded { .. } => true,
            Self::Bounded { attempts, .. } => attempt < *attempts,
        }
    }
}

/// A seat the client can no longer write to, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// The daemon definitely refused the writer (or the authenticated
    /// history was invalidated): choose a replacement explicitly.
    WriterRefused(String),
    /// An append's outcome is unknown after the retry budget. The writer is
    /// not declared dead; the retained request is reconciled first.
    AppendUnknown(String),
}

impl Unavailable {
    /// `writer_refused` or `append_unknown`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WriterRefused(_) => "writer_refused",
            Self::AppendUnknown(_) => "append_unknown",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::WriterRefused(message) | Self::AppendUnknown(message) => message,
        }
    }
}

/// Statuses that prove the daemon no longer accepts this writer.
pub fn is_seat_loss_status(status: u16) -> bool {
    matches!(status, 403 | 404 | 409 | 410)
}

/// True when a failed append may nevertheless have been applied: nothing
/// definite came back, or the daemon asked for a retry of the same request.
pub fn append_outcome_unknown(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Network { .. }
        | RuntimeError::UnreadableResponse { .. }
        | RuntimeError::Decode(_) => true,
        RuntimeError::Daemon { status, .. } => matches!(*status, 408 | 429 | 500..=599),
        _ => false,
    }
}

/// How a presence renewal went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchOutcome {
    /// Exactly the expected acknowledgement.
    Renewed,
    /// Unconfirmed; the producer is intact and the next interval retries.
    Retry,
    /// The daemon refused the writer.
    Refused,
}

/// Classify a touch response body (already decoded): only an object of
/// exactly `{presentation_id, thread_id, touched: true}` for this seat, after
/// unwrapping up to two `result` layers, renews.
pub fn classify_touch_response(
    response: &Value,
    presentation_id: &str,
    thread_id: &str,
) -> TouchOutcome {
    let Ok(acknowledgement) = unwrap_result(response) else {
        return TouchOutcome::Retry;
    };
    let exact = acknowledgement.as_object().is_some_and(|fields| {
        fields.len() == 3
            && fields.get("presentation_id").and_then(Value::as_str) == Some(presentation_id)
            && fields.get("thread_id").and_then(Value::as_str) == Some(thread_id)
            && fields.get("touched").and_then(Value::as_bool) == Some(true)
    });
    if exact {
        TouchOutcome::Renewed
    } else {
        TouchOutcome::Retry
    }
}

/// Classify a failed touch: a definite loss refuses; anything else retries.
pub fn classify_touch_error(error: &RuntimeError) -> TouchOutcome {
    match error.status() {
        Some(status) if is_seat_loss_status(status) => TouchOutcome::Refused,
        _ => TouchOutcome::Retry,
    }
}

/// A presence renewal may start only when nothing else is writing: no append
/// or touch in flight, no unresolved append and a writable seat.
pub fn may_start_seat_touch(
    append_inflight: bool,
    touch_inflight: bool,
    pending_append: bool,
    delivery_blocked: bool,
) -> bool {
    !append_inflight && !touch_inflight && !pending_append && !delivery_blocked
}

/// A clean close may not overtake a local event that has not reached the
/// acknowledged prefix. The host pumps the runtime for a bounded drain; this
/// is its final fence.
pub fn may_close_seat(
    local_events: usize,
    acknowledged_events: usize,
    append_inflight: bool,
    touch_inflight: bool,
    effects_pending: bool,
) -> bool {
    local_events == acknowledged_events && !append_inflight && !touch_inflight && !effects_pending
}

/// Read a daemon-acknowledged producer claim: a canonical 64-hex incarnation
/// and a frontier below `u64::MAX`.
pub fn claim_writer(claimed: &Value) -> Result<(String, u64), String> {
    let producer = claimed
        .get("producer_incarnation")
        .and_then(Value::as_str)
        .filter(|producer| is_lower_hex_64(producer))
        .ok_or("claim returned invalid producer incarnation")?;
    let frontier = claimed
        .get("next_engine_seq")
        .and_then(exact_u64)
        .ok_or("seat service returned no valid exact u64 for next_engine_seq")?;
    if frontier == u64::MAX {
        return Err("producer engine sequence is exhausted".to_owned());
    }
    Ok((producer.to_owned(), frontier))
}

pub(crate) fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A random version-4 UUID from the platform's entropy.
pub fn random_uuid(platform: &impl Platform) -> String {
    let mut bytes = [0_u8; 16];
    platform.random_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The unresolved append, pinned until a verified acknowledgement.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAppend {
    /// The exact request and its identity; this is also what is retained.
    pub record: RetainedPendingAppend,
    /// The captured local prefix the request carries. Empty for a request
    /// recovered from storage after a restart.
    pub events: Vec<SeatEvent>,
}

/// Delivery state of one seat, shared by the writer, the touch task and the
/// live-history reader. Never held across an await.
#[derive(Debug, Default)]
pub struct SeatDelivery {
    /// The producer frontier: the engine position of the next appended event.
    pub next_engine_seq: u64,
    /// The seat this delivery belongs to.
    pub coordinate: Option<SeatCoordinate>,
    /// The authenticated session id, for the retained record.
    pub session_id: String,
    /// The root binding the seat was created under.
    pub root: Option<UiBindingAttachmentCoordinate>,
    /// Storage key of the retained pending append.
    pub retention_key: Option<String>,
    /// The unresolved append, if any.
    pub pending: Option<PendingAppend>,
    /// Set when delivery can no longer be trusted; nothing more is sent.
    pub blocked: bool,
    /// The daemon's signed bound on one append request.
    pub max_seat_append_bytes: u64,
    recent_receipts: VecDeque<AckSummary>,
    verified_receipts: VecDeque<(String, u64, String)>,
}

impl SeatDelivery {
    /// Prepare for a freshly admitted producer.
    pub fn reset_for(
        &mut self,
        coordinate: SeatCoordinate,
        session_id: &str,
        root: UiBindingAttachmentCoordinate,
        retention_key: String,
        next_engine_seq: u64,
        max_seat_append_bytes: u64,
    ) {
        self.next_engine_seq = next_engine_seq;
        self.coordinate = Some(coordinate);
        self.session_id = session_id.to_owned();
        self.root = Some(root);
        self.retention_key = Some(retention_key);
        self.max_seat_append_bytes = max_seat_append_bytes;
        self.recent_receipts.clear();
        self.verified_receipts.clear();
    }

    /// Remember an acknowledged append.
    pub(crate) fn remember_receipt(&mut self, summary: AckSummary) {
        self.recent_receipts.push_back(summary);
        if self.recent_receipts.len() > REMEMBERED_RECEIPTS {
            self.recent_receipts.pop_front();
        }
    }

    /// A verified-append notification must name the exact pending operation
    /// or a recently acknowledged one, and agree with that receipt. It never
    /// acknowledges anything itself: only the HTTP acknowledgement does.
    pub fn accept_verified(&mut self, append: &VerifiedAppend) -> bool {
        let known = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.record_matches(append))
            || self.recent_receipts.iter().any(|ack| ack.matches(append));
        if !known {
            return false;
        }
        let contradicted = self.recent_receipts.iter().any(|ack| {
            ack.matches(append)
                && (ack.receipt_event_hash != append.event_hash
                    || ack.receipt_thread_seq != append.thread_seq)
        });
        if contradicted {
            return false;
        }
        if !self
            .verified_receipts
            .iter()
            .any(|(id, _, _)| *id == append.operation_id)
        {
            self.verified_receipts.push_back((
                append.operation_id.clone(),
                append.thread_seq,
                append.event_hash.clone(),
            ));
            if self.verified_receipts.len() > REMEMBERED_RECEIPTS {
                self.verified_receipts.pop_front();
            }
        }
        true
    }

    /// An acknowledgement must agree with any verified testimony already seen
    /// for the same operation.
    pub(crate) fn contradicts_verified(&self, summary: &AckSummary) -> bool {
        self.verified_receipts.iter().any(|(id, seq, hash)| {
            *id == summary.operation_id
                && (*seq != summary.receipt_thread_seq || *hash != summary.receipt_event_hash)
        })
    }
}

impl PendingAppend {
    fn record_matches(&self, append: &VerifiedAppend) -> bool {
        let record = &self.record;
        record.operation_id == append.operation_id
            && record.producer_incarnation == append.producer_incarnation
            && record.first_engine_seq.parse::<u64>().ok() == Some(append.first_engine_seq)
            && record.last_engine_seq.parse::<u64>().ok() == Some(append.last_engine_seq)
            && record.event_count == append.event_count
            && record.payload_digest == append.payload_digest
    }
}

/// A claimed stream handed from one owner to the next without losing the
/// interval between them: the live reader of the stream whose cold completion
/// admitted a seat.
pub struct LiveSeatHandoff<S: crate::platform::ByteSource> {
    pub producer: UiPresentationProducer,
    pub history: LiveHistory<S>,
    /// The acknowledged claim request, kept for same-producer reproof.
    pub claim_request: UiSeatControlRequest,
    pub claim_next_engine_seq: u64,
}

/// State shared by every task of one seat.
pub struct SeatShared<P: Platform> {
    pub platform: Rc<P>,
    /// The writer the platform may currently mutate under, if any. Effects
    /// read it when they run, so a fenced producer fails queued effects.
    pub producer: RefCell<Option<UiPresentationProducer>>,
    pub delivery: RefCell<SeatDelivery>,
    /// A claimed replacement stream waiting for the reducer to adopt it.
    pub handoff: RefCell<Option<LiveSeatHandoff<Frames<P>>>>,
}

impl<P: Platform> SeatShared<P> {
    pub fn new(platform: Rc<P>) -> Rc<Self> {
        Rc::new(Self {
            platform,
            producer: RefCell::new(None),
            delivery: RefCell::new(SeatDelivery::default()),
            handoff: RefCell::new(None),
        })
    }

    /// The current producer, cloned out of the cell.
    pub fn current_producer(&self) -> Option<UiPresentationProducer> {
        self.producer.borrow().clone()
    }

    /// True when `expected` is exactly the current producer.
    pub fn is_current(&self, expected: &UiPresentationProducer) -> bool {
        self.producer.borrow().as_ref() == Some(expected)
    }

    /// Fence delivery and drop the producer, but only if `expected` is still
    /// the current one: a late failure of a predecessor must not poison a
    /// successor.
    pub fn fence(&self, expected: &UiPresentationProducer) -> bool {
        if !self.is_current(expected) {
            return false;
        }
        self.delivery.borrow_mut().blocked = true;
        *self.producer.borrow_mut() = None;
        true
    }

    /// Reserve the handoff slot. A second reservation is refused and returned.
    pub fn reserve_handoff(
        &self,
        handoff: LiveSeatHandoff<Frames<P>>,
    ) -> Result<(), LiveSeatHandoff<Frames<P>>> {
        let mut slot = self.handoff.borrow_mut();
        if slot.is_some() {
            return Err(handoff);
        }
        *slot = Some(handoff);
        Ok(())
    }

    /// Take the handoff, if one is waiting.
    pub fn take_handoff(&self) -> Option<LiveSeatHandoff<Frames<P>>> {
        self.handoff.borrow_mut().take()
    }

    /// The pending-append key for the current delivery.
    pub(crate) fn retention_key_for(&self, session_id: &str, presentation_id: &str) -> String {
        use crate::platform::Store;
        if self.platform.store().hashes_session_keys() {
            crate::retention::storage_key_hashed(session_id, presentation_id)
        } else {
            crate::retention::storage_key(session_id, presentation_id)
        }
    }
}
