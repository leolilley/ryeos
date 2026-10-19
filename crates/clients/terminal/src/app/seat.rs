//! Seat-thread lifecycle: the seat is itself a thread — braided, owned,
//! replayable. Prepare and freshly claim on start, mirror local seat events into
//! the braid as they append, relinquish only acknowledged delivery on exit.
//! and replay the same fold.

use ryeos_client_base::ui::SeatEvent;
use ryeos_client_base::ui::SeatEventKind;
use ryeos_client_base::ui::binding::{SEAT_PRODUCER_EVENT, UiSeatProducer, UiSeatProducerState};
use ryeos_client_base::ui::{
    UiObservedPresentationProducer, UiPresentationProducerObservation, UiSeatControlPredecessor,
    UiSeatControlRequest, UiSeatProducerClaim,
};

use crate::transport::daemon::DaemonClient;

// Match the browser adapter's selected batch size; both stay below the
// daemon's 128-event route bound without relying only on encoded byte size.
const MAX_SEAT_BATCH_EVENTS: usize = 64;
const MAX_SEAT_REPLAY_EVENTS: usize = 64;
const MAX_COLD_REPLAY_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SeatBindingCoordinate {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    #[serde(skip)]
    pub max_seat_append_bytes: u64,
}

impl From<&ryeos_client_base::ui::binding::UiBindingAttachment> for SeatBindingCoordinate {
    fn from(attachment: &ryeos_client_base::ui::binding::UiBindingAttachment) -> Self {
        Self {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
            max_seat_append_bytes: attachment.binding_request_bounds.max_seat_append_bytes,
        }
    }
}

/// Acknowledged seat admission. The core is published only after a completed
/// authenticated history proof.
pub struct SeatBootstrap {
    pub thread_id: String,
    pub chain_root_id: String,
    pub producer: ryeos_client_base::ui::binding::UiPresentationProducer,
    pub next_engine_seq: u64,
    /// Original acknowledged claim, not the mutable append frontier.
    pub claim_request: UiSeatControlRequest,
    pub claim_next_engine_seq: u64,
    pub replayed: Vec<ryeos_client_base::ui::seat::RetainedSeatEvent>,
    /// Effects registered while finalizing a private retained-seat replay.
    /// Fresh admission has none; the app publishes these only after its
    /// postclaim membership check and producer readiness fence.
    pub replay_effects: Vec<ryeos_client_base::ui::effect::RyeOsEffect>,
    /// Continue the same authenticated stream after its exact cold fence;
    /// opening another stream would leave an unobserved interval.
    pub live_history: crate::transport::daemon::SseStream,
    pub replay_tip_seq: u64,
    pub replay_tip_hash: String,
}

pub struct LiveSeatHandoff {
    pub producer: ryeos_client_base::ui::binding::UiPresentationProducer,
    pub stream: crate::transport::daemon::SseStream,
    pub tip_seq: u64,
    pub tip_hash: String,
    pub claim_request: UiSeatControlRequest,
    pub claim_next_engine_seq: u64,
}

pub(crate) fn reserve_live_handoff<T>(slot: &mut Option<T>, handoff: T) -> Result<(), T> {
    if slot.is_some() {
        Err(handoff)
    } else {
        *slot = Some(handoff);
        Ok(())
    }
}

fn current_live_producer(
    current: Option<&ryeos_client_base::ui::binding::UiPresentationProducer>,
    expected: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> bool {
    current == Some(expected)
}

/// A post-claim admission failure may not leave a native writer without its
/// authenticated history lane. Fence only the failed incarnation; a late
/// predecessor failure must not poison a successor's delivery state.
pub async fn fence_failed_bootstrap(
    client: &DaemonClient,
    expected: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> bool {
    let mut delivery = client.seat_delivery.lock().await;
    let mut current = client.ui_producer.write().expect("producer lock poisoned");
    fence_failed_bootstrap_state(&mut delivery, &mut current, expected)
}

fn fence_failed_bootstrap_state(
    delivery: &mut SeatDelivery,
    current: &mut Option<ryeos_client_base::ui::binding::UiPresentationProducer>,
    expected: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> bool {
    if !current_live_producer(current.as_ref(), expected) {
        return false;
    }
    delivery.blocked = true;
    *current = None;
    true
}

#[derive(Default)]
pub(crate) struct SeatDelivery {
    pub next_engine_seq: u64,
    pub chain_root_id: Option<String>,
    pub pending: Option<serde_json::Value>,
    // One bounded captured prefix, not a second history log. Transport equality
    // alone cannot identify local events whose non-wire sequence differs.
    pending_events: Option<Vec<SeatEvent>>,
    pub blocked: bool,
    pub max_seat_append_bytes: u64,
    recent_receipts: std::collections::VecDeque<serde_json::Value>,
    verified_receipts: std::collections::VecDeque<(String, u64, String)>,
}

impl SeatDelivery {
    fn retain_exact_append(
        &mut self,
        candidate: serde_json::Value,
        captured: &[SeatEvent],
    ) -> Result<&serde_json::Value, &'static str> {
        if self.blocked {
            return Err("seat delivery is blocked");
        }
        if captured.is_empty()
            || captured.len() > MAX_SEAT_BATCH_EVENTS
            || self.pending.is_some() != self.pending_events.is_some()
            || self.pending.as_ref().is_some_and(|pending| {
                pending != &candidate || self.pending_events.as_deref() != Some(captured)
            })
        {
            self.blocked = true;
            return Err("captured prefix differs from the retained append retry");
        }
        if self.pending.is_none() {
            self.pending_events = Some(captured.to_vec());
        }
        Ok(self.pending.get_or_insert(candidate))
    }
}

fn same_append_identity(observed: &serde_json::Value, retained: &serde_json::Value) -> bool {
    const FIELDS: [&str; 6] = [
        "operation_id",
        "producer_incarnation",
        "first_engine_seq",
        "last_engine_seq",
        "event_count",
        "payload_digest",
    ];
    FIELDS.iter().all(|field| {
        let left = observed.get(*field);
        let right = retained.get(*field);
        if matches!(
            *field,
            "first_engine_seq" | "last_engine_seq" | "event_count"
        ) {
            exact_u64(left, field).ok() == exact_u64(right, field).ok()
                && exact_u64(left, field).is_ok()
        } else {
            left.and_then(serde_json::Value::as_str)
                .is_some_and(|value| Some(value) == right.and_then(serde_json::Value::as_str))
        }
    })
}

/// The server proves a signed suffix; the terminal only recognizes receipts
/// for its exact retained or recently acknowledged operation. It never folds
/// a live frame into local history or treats it as an HTTP append ACK.
fn accept_live_receipt(
    delivery: &mut SeatDelivery,
    payload: &serde_json::Value,
    producer_incarnation: &str,
    tip: &mut (u64, String),
    seen: &mut std::collections::VecDeque<(String, u64, String)>,
) -> bool {
    if payload.as_object().is_none_or(|fields| fields.len() != 9)
        || exact_u64(payload.get("schema"), "schema").ok() != Some(1)
        || payload
            .get("producer_incarnation")
            .and_then(serde_json::Value::as_str)
            != Some(producer_incarnation)
    {
        return false;
    }
    let Some(operation_id) = payload
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let Some(hash) = payload
        .get("event_hash")
        .and_then(serde_json::Value::as_str)
        .filter(|hash| canonical_event_hash(hash))
    else {
        return false;
    };
    let Ok(seq) = exact_u64(payload.get("thread_seq"), "thread_seq") else {
        return false;
    };
    if seen.iter().any(|(id, prior_seq, prior_hash)| {
        id == operation_id && (*prior_seq != seq || prior_hash != hash)
    }) {
        return false;
    }
    if seq < tip.0
        && !seen.iter().any(|(id, prior_seq, prior_hash)| {
            id == operation_id && *prior_seq == seq && prior_hash == hash
        })
    {
        return false;
    }
    if seq == tip.0 && tip.1 != hash {
        return false;
    }
    if !delivery
        .pending
        .as_ref()
        .is_some_and(|body| same_append_identity(payload, body))
        && !delivery
            .recent_receipts
            .iter()
            .any(|ack| same_append_identity(payload, ack))
    {
        return false;
    }
    if delivery.recent_receipts.iter().any(|ack| {
        same_append_identity(payload, ack)
            && (ack
                .get("receipt_event_hash")
                .and_then(serde_json::Value::as_str)
                != Some(hash)
                || exact_u64(ack.get("receipt_thread_seq"), "receipt_thread_seq").ok() != Some(seq))
    }) {
        return false;
    }
    if seq > tip.0 {
        *tip = (seq, hash.to_owned());
    }
    if !seen.iter().any(|(id, _, _)| id == operation_id) {
        seen.push_back((operation_id.to_owned(), seq, hash.to_owned()));
        if seen.len() > 128 {
            seen.pop_front();
        }
    }
    if !delivery
        .verified_receipts
        .iter()
        .any(|(id, _, _)| id == operation_id)
    {
        delivery
            .verified_receipts
            .push_back((operation_id.to_owned(), seq, hash.to_owned()));
        if delivery.verified_receipts.len() > 128 {
            delivery.verified_receipts.pop_front();
        }
    }
    true
}

fn exact_live_reproof_wake(event_type: &str, data: &str) -> bool {
    if event_type != "ui_seat.replay.reproof_required.v1" || data.len() > 16 * 1024 * 1024 {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(data).is_ok_and(|payload| {
        payload.as_object().is_some_and(|object| object.len() == 1)
            && payload.get("schema").and_then(serde_json::Value::as_u64) == Some(1)
    })
}

pub async fn watch_live_history(
    client: &DaemonClient,
    mut stream: crate::transport::daemon::SseStream,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    tip_seq: u64,
    tip_hash: String,
) -> bool {
    let mut tip = (tip_seq, tip_hash);
    let mut seen = std::collections::VecDeque::new();
    while let Some(frame) = stream.next_event().await {
        let mut delivery = client.seat_delivery.lock().await;
        if !current_live_producer(
            client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .as_ref(),
            producer,
        ) {
            return false;
        }
        if frame.event_type == "ui_seat.replay.reproof_required.v1" {
            if exact_live_reproof_wake(&frame.event_type, &frame.data) {
                return true;
            }
            break;
        }
        if frame.event_type != "ui_seat.replay.append_verified.v1" {
            break;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            break;
        };
        if !accept_live_receipt(
            &mut delivery,
            &payload,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen,
        ) {
            break;
        }
    }
    // EOF, authentication loss, foreign control and uncertain suffix all
    // have the same local consequence. Never create replacement authority.
    let mut delivery = client.seat_delivery.lock().await;
    if current_live_producer(
        client
            .ui_producer
            .read()
            .expect("producer lock poisoned")
            .as_ref(),
        producer,
    ) {
        delivery.blocked = true;
    }
    false
}

/// Observe a new fixed cut under the same acknowledged claim. This function
/// never claims/resumes/relinquishes and never publishes its private Core.
pub async fn reproof_seat_thread(
    client: &DaemonClient,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    claim: &UiSeatControlRequest,
    claim_next_engine_seq: u64,
    authenticated: &ryeos_client_base::ui::BrowserSession,
    viewport: ryeos_client_base::ui::BrowserViewport,
    now_ms: u64,
) -> Result<(ryeos_client_base::ui::RyeOsCore, LiveSeatHandoff), String> {
    let membership = authenticated
        .presentations
        .iter()
        .find(|membership| {
            membership.presentation_id == producer.presentation_id
                && membership.thread_id.as_deref() == Some(producer.thread_id.as_str())
        })
        .ok_or("reproof exact current membership absent")?;
    let root = membership
        .chain_root_id
        .as_deref()
        .ok_or("reproof root absent")?;
    let same_membership = |current: &ryeos_client_base::ui::binding::UiPresentationMembership| {
        current.presentation_id == membership.presentation_id
            && current.root_binding == membership.root_binding
            && current.thread_id == membership.thread_id
            && current.chain_root_id == membership.chain_root_id
            && current.initialization_digest == membership.initialization_digest
    };
    {
        let delivery = client.seat_delivery.lock().await;
        if delivery.pending.is_some() || delivery.blocked {
            return Err("unresolved append prevents reproof".into());
        }
        if !current_live_producer(
            client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .as_ref(),
            producer,
        ) {
            return Err("reproof current producer changed".into());
        }
    }
    for _ in 0..MAX_COLD_REPLAY_ATTEMPTS {
        let observed = client
            .current_ui_session()
            .await
            .map_err(|error| error.to_string())?;
        if observed.session_id != authenticated.session_id
            || observed
                .presentations
                .iter()
                .find(|current| current.presentation_id == producer.presentation_id)
                .is_none_or(|current| !same_membership(current))
        {
            return Err("reproof session membership changed".into());
        }
        let mut stream = client
            .open_seat_history(&producer.presentation_id, root)
            .await
            .map_err(|error| error.to_string())?;
        let mut proof = ResumeSeatReplay::new();
        let mut staged = ryeos_client_base::ui::cold_replay::ColdSeatReplay::new();
        let mut candidate = ryeos_client_base::ui::RyeOsCore::new(
            observed,
            &producer.presentation_id,
            viewport,
            now_ms,
        );
        while let Some(frame) = stream.next_event().await {
            if precompletion_replay_invalidation(&frame, proof.begun)? {
                break;
            }
            if proof.accept(
                &frame,
                &producer.presentation_id,
                root,
                &producer.thread_id,
                claim,
                &producer.producer_incarnation,
                claim_next_engine_seq,
                &mut staged,
                &mut candidate,
            )? {
                let tip_seq = proof.last_thread_seq.ok_or("reproof tip absent")?;
                let tip_hash = proof.last_hash.ok_or("reproof hash absent")?;
                // These candidate effects are not dispatched. Shared promotion
                // preserves the live registered effect owner and avoids startup.
                staged.finish_at_authenticated_tip(
                    &mut candidate,
                    &tip_hash,
                    tip_seq,
                    proof.event_count,
                )?;
                let observed = client
                    .current_ui_session()
                    .await
                    .map_err(|error| error.to_string())?;
                if observed.session_id != authenticated.session_id
                    || observed
                        .presentations
                        .iter()
                        .find(|current| current.presentation_id == producer.presentation_id)
                        .is_none_or(|current| !same_membership(current))
                    || !current_live_producer(
                        client
                            .ui_producer
                            .read()
                            .expect("producer lock poisoned")
                            .as_ref(),
                        producer,
                    )
                {
                    return Err("reproof authority changed before promotion".into());
                }
                return Ok((
                    candidate,
                    LiveSeatHandoff {
                        producer: producer.clone(),
                        stream,
                        tip_seq,
                        tip_hash,
                        claim_request: claim.clone(),
                        claim_next_engine_seq,
                    },
                ));
            }
        }
    }
    Err("same-producer reproof did not complete its fixed cut".into())
}

fn operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn result(envelope: &serde_json::Value) -> &serde_json::Value {
    envelope.get("result").unwrap_or(envelope)
}

fn exact_u64(value: Option<&serde_json::Value>, name: &str) -> Result<u64, String> {
    match value {
        Some(serde_json::Value::String(value))
            if value == "0"
                || (!value.starts_with('0')
                    && !value.is_empty()
                    && value.bytes().all(|byte| byte.is_ascii_digit())) =>
        {
            value
                .parse::<u64>()
                .map_err(|_| format!("seat service returned an out-of-range u64 for {name}"))
        }
        Some(serde_json::Value::Number(value)) => value
            .as_u64()
            .ok_or_else(|| format!("seat service returned an invalid exact u64 for {name}")),
        _ => Err(format!(
            "seat service returned no valid exact u64 for {name}"
        )),
    }
}

fn claim_writer(claimed: &serde_json::Value) -> Result<(String, u64), String> {
    let producer = claimed
        .get("producer_incarnation")
        .and_then(serde_json::Value::as_str)
        .filter(|producer| {
            producer.len() == 64
                && producer
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or("claim returned invalid producer incarnation")?;
    let frontier = exact_u64(claimed.get("next_engine_seq"), "next_engine_seq")?;
    if frontier == u64::MAX {
        return Err("producer engine sequence is exhausted".into());
    }
    Ok((producer.into(), frontier))
}

/// The caller has already checked the exact claim operation and active state.
/// A malformed frontier may still leave an acknowledged owner to relinquish;
/// a missing/noncanonical incarnation supplies no safe cleanup target.
fn acknowledged_claim_cleanup_owner(
    claimed: &serde_json::Value,
    presentation_id: &str,
    thread_id: &str,
) -> Option<ryeos_client_base::ui::binding::UiPresentationProducer> {
    let incarnation = claimed
        .get("producer_incarnation")
        .and_then(serde_json::Value::as_str)
        .filter(|value| canonical_event_hash(value))?;
    Some(ryeos_client_base::ui::binding::UiPresentationProducer {
        presentation_id: presentation_id.to_owned(),
        thread_id: thread_id.to_owned(),
        producer_incarnation: incarnation.to_owned(),
    })
}

struct FreshSeatReplay {
    begun: bool,
    completed: bool,
    event_count: u64,
    last_chain_seq: Option<u64>,
    last_thread_seq: Option<u64>,
    last_hash: Option<String>,
    initialization: Option<ryeos_client_base::ui::seat::RetainedSeatEvent>,
    expected_claim_operation_id: String,
    expected_producer_incarnation: String,
    final_control: Option<UiSeatProducer>,
}

impl FreshSeatReplay {
    fn new(expected_claim_operation_id: &str, expected_producer_incarnation: &str) -> Self {
        Self {
            begun: false,
            completed: false,
            event_count: 0,
            last_chain_seq: None,
            last_thread_seq: None,
            last_hash: None,
            initialization: None,
            expected_claim_operation_id: expected_claim_operation_id.into(),
            expected_producer_incarnation: expected_producer_incarnation.into(),
            final_control: None,
        }
    }

    fn accept(
        &mut self,
        frame: &crate::transport::sse::SseFrame,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        creation_request_id: &str,
        binding: &SeatBindingCoordinate,
    ) -> Result<bool, String> {
        let payload: serde_json::Value = serde_json::from_str(&frame.data)
            .map_err(|error| format!("invalid seat history frame: {error}"))?;
        if exact_u64(payload.get("schema"), "schema")? != 1 {
            return Err("unsupported seat history stream schema".into());
        }
        match frame.event_type.as_str() {
            "ui_seat.replay.begin.v1" if !self.begun => {
                if self.event_count != 0
                    || payload
                        .get("presentation_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(presentation_id)
                    || payload
                        .get("chain_root_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(chain_root_id)
                    || payload.get("thread_id").and_then(serde_json::Value::as_str)
                        != Some(thread_id)
                {
                    return Err("seat history begin belongs to another seat".into());
                }
                self.begun = true;
                Ok(false)
            }
            "ui_seat.replay.events.v1" if self.begun && !self.completed => {
                let events = payload
                    .get("events")
                    .and_then(serde_json::Value::as_array)
                    .filter(|events| !events.is_empty() && events.len() <= MAX_SEAT_REPLAY_EVENTS)
                    .ok_or("seat history frame omitted a bounded event batch")?;
                for event in events {
                    if event
                        .get("chain_root_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(chain_root_id)
                        || event.get("thread_id").and_then(serde_json::Value::as_str)
                            != Some(thread_id)
                    {
                        return Err("seat history event belongs to another seat".into());
                    }
                    let chain_seq = exact_u64(event.get("chain_seq"), "chain_seq")?;
                    let thread_seq = exact_u64(event.get("thread_seq"), "thread_seq")?;
                    let hash = event
                        .get("event_hash")
                        .and_then(serde_json::Value::as_str)
                        .filter(|hash| {
                            hash.len() == 64
                                && hash.bytes().all(|byte| {
                                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                })
                        })
                        .ok_or("seat history event has no canonical hash")?;
                    if self
                        .last_chain_seq
                        .is_some_and(|previous| chain_seq <= previous)
                        || self
                            .last_thread_seq
                            .is_some_and(|previous| thread_seq <= previous)
                    {
                        return Err("seat history events are not strictly ordered".into());
                    }
                    if let Some(decoded) =
                        ryeos_client_base::ui::seat::retained_seat_event_with_evidence(event)?
                    {
                        let SeatEventKind::PresentationInitialized(record) = &decoded.event.kind
                        else {
                            return Err(
                                "fresh terminal seat contains producer events before bootstrap"
                                    .into(),
                            );
                        };
                        record.validate()?;
                        if self.initialization.is_some()
                            || record.operation_id != creation_request_id
                            || record.initialization.presentation_id != presentation_id
                            || serde_json::to_value(&record.initialization.root_binding)
                                .map_err(|error| error.to_string())?
                                != serde_json::to_value(binding)
                                    .map_err(|error| error.to_string())?
                        {
                            return Err(
                                "fresh terminal initialization has duplicate or foreign ownership"
                                    .into(),
                            );
                        }
                        self.initialization = Some(decoded);
                    }
                    if event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some(SEAT_PRODUCER_EVENT)
                    {
                        let control: UiSeatProducer = serde_json::from_value(
                            event
                                .get("payload")
                                .cloned()
                                .ok_or("seat control has no payload")?,
                        )
                        .map_err(|error| format!("invalid seat control in history: {error}"))?;
                        control.validate().map_err(str::to_owned)?;
                        control
                            .validate_subject(chain_root_id, thread_id)
                            .map_err(str::to_owned)?;
                        if self.initialization.is_none() || self.final_control.is_some() {
                            return Err(
                                "fresh terminal seat has missing or changed producer control"
                                    .into(),
                            );
                        }
                        self.final_control = Some(control);
                    }
                    self.event_count = self
                        .event_count
                        .checked_add(1)
                        .ok_or("seat history count exhausted")?;
                    self.last_chain_seq = Some(chain_seq);
                    self.last_thread_seq = Some(thread_seq);
                    self.last_hash = Some(hash.to_owned());
                }
                Ok(false)
            }
            "ui_seat.replay.transfer_proof.v1"
            | "ui_seat.replay.move_freeze_cancellation_proof.v1"
                if self.begun && !self.completed =>
            {
                Err("fresh terminal claim cannot contain paired transfer testimony".into())
            }
            "ui_seat.replay.complete.v1" if self.begun && !self.completed => {
                let current = self
                    .final_control
                    .as_ref()
                    .ok_or("fresh terminal seat has no authenticated producer control")?;
                if self.initialization.is_none()
                    || current.operation_id != self.expected_claim_operation_id
                    || current.producer_incarnation != self.expected_producer_incarnation
                    || current.state != UiSeatProducerState::Active
                    || current.next_engine_seq != 0
                    || !matches!(
                        &current.request,
                        UiSeatControlRequest::Claim {
                            presentation_id: claim_presentation,
                            claim: UiSeatProducerClaim::Fresh,
                            ..
                        } if claim_presentation == presentation_id
                    )
                    || exact_u64(payload.get("event_count"), "event_count")? != self.event_count
                    || Some(exact_u64(payload.get("thread_seq"), "thread_seq")?)
                        != self.last_thread_seq
                    || payload.get("tip_hash").and_then(serde_json::Value::as_str)
                        != self.last_hash.as_deref()
                {
                    return Err("seat history completion does not match the received proof".into());
                }
                self.completed = true;
                Ok(true)
            }
            _ => Err("unexpected or duplicate seat history frame".into()),
        }
    }
}

struct ResumeSeatReplay {
    begun: bool,
    completed: bool,
    event_count: u64,
    last_chain_seq: Option<u64>,
    last_thread_seq: Option<u64>,
    last_hash: Option<String>,
    final_control: Option<UiSeatProducer>,
}

#[cfg(test)]
mod resume_replay_tests {
    use super::*;

    #[test]
    fn live_reproof_wake_is_closed_and_distinct_from_invalidation() {
        assert!(exact_live_reproof_wake(
            "ui_seat.replay.reproof_required.v1",
            r#"{"schema":1}"#
        ));
        for payload in [
            r#"{"schema":1,"proof":{}}"#,
            r#"{"schema":"1"}"#,
            r#"{"schema":2}"#,
            "null",
        ] {
            assert!(!exact_live_reproof_wake(
                "ui_seat.replay.reproof_required.v1",
                payload
            ));
        }
        assert!(!exact_live_reproof_wake(
            "ui_seat.replay.invalidated.v1",
            r#"{"schema":1}"#
        ));
    }

    #[test]
    fn resume_replay_starts_without_a_published_control_or_history() {
        let candidate = ResumeSeatReplay::new();
        assert!(!candidate.begun);
        assert_eq!(candidate.event_count, 0);
        assert!(candidate.final_control.is_none());
        assert!(candidate.last_thread_seq.is_none());
    }

    #[test]
    fn only_a_canonical_relinquished_incarnation_can_be_a_resume_predecessor() {
        assert!(canonical_event_hash(&"a".repeat(64)));
        assert!(!canonical_event_hash(&"g".repeat(64)));
        assert!(!canonical_event_hash("short"));
    }
}

impl ResumeSeatReplay {
    fn new() -> Self {
        Self {
            begun: false,
            completed: false,
            event_count: 0,
            last_chain_seq: None,
            last_thread_seq: None,
            last_hash: None,
            final_control: None,
        }
    }

    fn accept(
        &mut self,
        frame: &crate::transport::sse::SseFrame,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        claim: &UiSeatControlRequest,
        producer_incarnation: &str,
        next_engine_seq: u64,
        staged: &mut ryeos_client_base::ui::cold_replay::ColdSeatReplay,
        core: &mut ryeos_client_base::ui::RyeOsCore,
    ) -> Result<bool, String> {
        if matches!(
            frame.event_type.as_str(),
            "ui_seat.replay.transfer_proof.v1" | "ui_seat.replay.move_freeze_cancellation_proof.v1"
        ) && frame.data.len() > 16 * 1024 * 1024
        {
            return Err("paired transfer proof exceeds its transport byte bound".into());
        }
        let payload: serde_json::Value = serde_json::from_str(&frame.data)
            .map_err(|error| format!("invalid seat history frame: {error}"))?;
        if exact_u64(payload.get("schema"), "schema")? != 1 {
            return Err("unsupported seat history stream schema".into());
        }
        match frame.event_type.as_str() {
            "ui_seat.replay.begin.v1" if !self.begun => {
                if payload
                    .get("presentation_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(presentation_id)
                    || payload
                        .get("chain_root_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(chain_root_id)
                    || payload.get("thread_id").and_then(serde_json::Value::as_str)
                        != Some(thread_id)
                {
                    return Err("seat history begin belongs to another seat".into());
                }
                self.begun = true;
                Ok(false)
            }
            "ui_seat.replay.transfer_proof.v1" if self.begun && !self.completed => {
                if payload.as_object().is_none_or(|object| {
                    object.len() != 2
                        || !object.contains_key("schema")
                        || !object.contains_key("proof")
                }) {
                    return Err("paired transfer proof envelope is not closed".into());
                }
                let proof = payload
                    .get("proof")
                    .cloned()
                    .ok_or("paired transfer proof frame has no proof")?;
                let proof: ryeos_client_base::ui::transfer_proof_frame::UiTransferProofFrame =
                    serde_json::from_value(proof)
                        .map_err(|error| format!("invalid paired transfer proof: {error}"))?;
                staged.stage_transfer_proof(core, proof)?;
                Ok(false)
            }
            "ui_seat.replay.move_freeze_cancellation_proof.v1" if self.begun && !self.completed => {
                if payload.as_object().is_none_or(|object| {
                    object.len() != 2
                        || !object.contains_key("schema")
                        || !object.contains_key("proof")
                }) {
                    return Err("move freeze cancellation proof envelope is not closed".into());
                }
                let proof: ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame =
                    serde_json::from_value(payload.get("proof").cloned().ok_or("move freeze cancellation frame has no proof")?)
                    .map_err(|error| format!("invalid move freeze cancellation proof: {error}"))?;
                staged.stage_move_freeze_cancellation_proof(core, proof)?;
                Ok(false)
            }
            "ui_seat.replay.events.v1" if self.begun && !self.completed => {
                let events = payload
                    .get("events")
                    .and_then(serde_json::Value::as_array)
                    .filter(|events| !events.is_empty() && events.len() <= MAX_SEAT_REPLAY_EVENTS)
                    .ok_or("seat history frame omitted a bounded event batch")?;
                for event in events {
                    if event
                        .get("chain_root_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(chain_root_id)
                        || event.get("thread_id").and_then(serde_json::Value::as_str)
                            != Some(thread_id)
                    {
                        return Err("seat history event belongs to another seat".into());
                    }
                    let chain_seq = exact_u64(event.get("chain_seq"), "chain_seq")?;
                    let thread_seq = exact_u64(event.get("thread_seq"), "thread_seq")?;
                    let hash = event
                        .get("event_hash")
                        .and_then(serde_json::Value::as_str)
                        .filter(|hash| canonical_event_hash(hash))
                        .ok_or("seat history event has no canonical hash")?;
                    if self
                        .last_chain_seq
                        .is_some_and(|previous| chain_seq <= previous)
                        || self
                            .last_thread_seq
                            .is_some_and(|previous| thread_seq <= previous)
                    {
                        return Err("seat history events are not strictly ordered".into());
                    }
                    if self
                        .last_thread_seq
                        .is_some_and(|previous| previous.checked_add(1) == Some(thread_seq))
                        && event
                            .get("prev_thread_event_hash")
                            .and_then(serde_json::Value::as_str)
                            != self.last_hash.as_deref()
                    {
                        return Err("seat history predecessor is discontinuous".into());
                    }
                    if event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some(SEAT_PRODUCER_EVENT)
                    {
                        let control: UiSeatProducer = serde_json::from_value(
                            event
                                .get("payload")
                                .cloned()
                                .ok_or("seat control has no payload")?,
                        )
                        .map_err(|error| format!("invalid seat control in history: {error}"))?;
                        control.validate().map_err(str::to_owned)?;
                        control
                            .validate_subject(chain_root_id, thread_id)
                            .map_err(str::to_owned)?;
                        self.final_control = Some(control);
                    }
                    self.event_count = self
                        .event_count
                        .checked_add(1)
                        .ok_or("seat history count exhausted")?;
                    self.last_chain_seq = Some(chain_seq);
                    self.last_thread_seq = Some(thread_seq);
                    self.last_hash = Some(hash.to_owned());
                }
                staged.append_authenticated_records(core, events)?;
                Ok(false)
            }
            "ui_seat.replay.complete.v1" if self.begun && !self.completed => {
                let control = self
                    .final_control
                    .as_ref()
                    .ok_or("seat history has no final producer control")?;
                if &control.request != claim
                    || control.operation_id != claim.operation_id()
                    || control.producer_incarnation != producer_incarnation
                    || control.state != UiSeatProducerState::Active
                    || control.next_engine_seq != next_engine_seq
                    || exact_u64(payload.get("event_count"), "event_count")? != self.event_count
                    || Some(exact_u64(payload.get("thread_seq"), "thread_seq")?)
                        != self.last_thread_seq
                    || payload.get("tip_hash").and_then(serde_json::Value::as_str)
                        != self.last_hash.as_deref()
                {
                    return Err(
                        "seat history completion does not match the exact resumed claim".into(),
                    );
                }
                self.completed = true;
                Ok(true)
            }
            _ => Err("unexpected or duplicate seat history frame".into()),
        }
    }
}

async fn exact_control(
    client: &DaemonClient,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    exact_post(client, "/ui/api/session/seat/open", body).await
}

async fn observe_control(
    client: &DaemonClient,
    presentation_id: &str,
    thread_id: &str,
    chain_root_id: &str,
) -> Result<UiPresentationProducerObservation, String> {
    let response = exact_control(
        client,
        &serde_json::json!({
            "action":"observe", "presentation_id":presentation_id,
            "thread_id":thread_id, "chain_root_id":chain_root_id,
        }),
    )
    .await?;
    let observation: UiPresentationProducerObservation =
        serde_json::from_value(result(&response).clone())
            .map_err(|error| format!("invalid exact producer observation: {error}"))?;
    observation
        .control_predecessor
        .validate()
        .map_err(str::to_owned)?;
    if observation.presentation_id != presentation_id
        || observation.thread_id != thread_id
        || observation.chain_root_id != chain_root_id
        || matches!(
            observation.producer,
            UiObservedPresentationProducer::Unclaimed
        ) != matches!(
            observation.control_predecessor,
            UiSeatControlPredecessor::Initialization { .. }
        )
    {
        return Err("producer observation differs from its exact seat or control state".into());
    }
    Ok(observation)
}

async fn exact_post(
    client: &DaemonClient,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    for attempt in 0..5 {
        match client.signed_post(path, body).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let retry = match &error {
                    crate::transport::daemon::ClientError::DaemonError { status, .. } => {
                        *status >= 500
                    }
                    crate::transport::daemon::ClientError::Transport(_) => true,
                    _ => false,
                };
                if !retry || attempt == 4 {
                    return Err(format!("seat control outcome unresolved: {error}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
            }
        }
    }
    unreachable!()
}

fn append_outcome_unknown(error: &crate::transport::daemon::ClientError) -> bool {
    matches!(
        error,
        crate::transport::daemon::ClientError::Transport(_)
            | crate::transport::daemon::ClientError::Json(_)
            | crate::transport::daemon::ClientError::DaemonDown { .. }
            | crate::transport::daemon::ClientError::DaemonError {
                status: 408 | 429 | 500..=599,
                ..
            }
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeatTouchOutcome {
    Renewed,
    Retry,
    Refused,
}

fn touch_outcome(
    response: Result<serde_json::Value, crate::transport::daemon::ClientError>,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> SeatTouchOutcome {
    match response {
        Ok(envelope) => {
            let acknowledgement = result(&envelope);
            if acknowledgement
                .as_object()
                .is_some_and(|value| value.len() == 3)
                && acknowledgement
                    .get("presentation_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(producer.presentation_id.as_str())
                && acknowledgement
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(producer.thread_id.as_str())
                && acknowledgement
                    .get("touched")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
                SeatTouchOutcome::Renewed
            } else {
                SeatTouchOutcome::Retry
            }
        }
        Err(crate::transport::daemon::ClientError::DaemonError { status, .. })
            if matches!(status, 403 | 404 | 409 | 410) =>
        {
            SeatTouchOutcome::Refused
        }
        Err(_) => SeatTouchOutcome::Retry,
    }
}

fn canonical_event_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The daemon's exact receipt is evidence for the staged local prefix only
/// when its durable interval, not merely its echoed summary, matches it.
fn acknowledged_append_interval(
    acknowledgement: &serde_json::Value,
    body: &serde_json::Value,
    chain_root_id: &str,
) -> bool {
    use serde_json::Value;

    if acknowledgement
        .get("events_authority")
        .and_then(Value::as_str)
        != Some("authoritative")
    {
        return false;
    }
    let Some(thread_id) = body.get("thread_id").and_then(Value::as_str) else {
        return false;
    };
    let Some(staged) = body.get("events").and_then(Value::as_array) else {
        return false;
    };
    let Some(acknowledged) = acknowledgement.get("events").and_then(Value::as_array) else {
        return false;
    };
    if staged.is_empty() || staged.len() != acknowledged.len() {
        return false;
    }
    let Some(receipt_chain_seq) = exact_u64(acknowledgement.get("chain_seq"), "chain_seq").ok()
    else {
        return false;
    };
    let Some(first_chain_seq) = receipt_chain_seq.checked_sub(staged.len() as u64) else {
        return false;
    };
    let mut previous_hash: Option<&str> = None;
    let mut previous_thread_seq: Option<u64> = None;
    let mut hashes = std::collections::BTreeSet::new();
    for (offset, (expected, actual)) in staged.iter().zip(acknowledged).enumerate() {
        let Some(hash) = actual.get("event_hash").and_then(Value::as_str) else {
            return false;
        };
        if !canonical_event_hash(hash) || !hashes.insert(hash) {
            return false;
        }
        let Some(chain_seq) = first_chain_seq.checked_add(offset as u64) else {
            return false;
        };
        let Some(thread_seq) = exact_u64(actual.get("thread_seq"), "thread_seq").ok() else {
            return false;
        };
        let previous_chain_hash = actual.get("prev_chain_event_hash");
        let previous_thread_hash = actual.get("prev_thread_event_hash");
        let valid_predecessor = |value: Option<&Value>| match value {
            Some(Value::Null) => true,
            Some(Value::String(hash)) => canonical_event_hash(hash),
            _ => false,
        };
        if !valid_predecessor(previous_chain_hash)
            || !valid_predecessor(previous_thread_hash)
            || previous_thread_seq
                .is_some_and(|previous| previous.checked_add(1) != Some(thread_seq))
        {
            return false;
        }
        if exact_u64(actual.get("chain_seq"), "chain_seq").ok() != Some(chain_seq)
            || actual.get("chain_root_id").and_then(Value::as_str) != Some(chain_root_id)
            || actual.get("thread_id").and_then(Value::as_str) != Some(thread_id)
            || actual.get("durability").and_then(Value::as_str) != Some("durable")
            || actual
                .get("ts")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || actual.get("event_type") != expected.get("event_type")
            || actual.pointer("/payload/seq") != expected.get("engine_seq")
            || actual.pointer("/payload/payload") != expected.get("payload")
        {
            return false;
        }
        if let Some(previous) = previous_hash
            && (actual.get("prev_chain_event_hash").and_then(Value::as_str) != Some(previous)
                || actual.get("prev_thread_event_hash").and_then(Value::as_str) != Some(previous))
        {
            return false;
        }
        previous_hash = Some(hash);
        previous_thread_seq = Some(thread_seq);
    }
    let Some(last_thread_seq) = previous_thread_seq else {
        return false;
    };
    let Some(receipt_hash) = acknowledgement
        .get("receipt_event_hash")
        .and_then(Value::as_str)
        .filter(|hash| canonical_event_hash(hash))
    else {
        return false;
    };
    let Ok(receipt_thread_seq) = exact_u64(
        acknowledgement.get("receipt_thread_seq"),
        "receipt_thread_seq",
    ) else {
        return false;
    };
    previous_hash.is_some()
        && last_thread_seq.checked_add(1) == Some(receipt_thread_seq)
        && receipt_hash != previous_hash.expect("nonempty interval")
}

/// Only the closed durable acknowledgement for this captured batch may clear
/// the native writer's retained request and advance its producer frontier.
fn accept_append_acknowledgement(
    delivery: &mut SeatDelivery,
    acknowledgement: &serde_json::Value,
    body: &serde_json::Value,
    chain_root_id: &str,
) -> bool {
    if !append_acknowledgement_matches(delivery, acknowledgement, body, chain_root_id) {
        delivery.blocked = true;
        return false;
    }
    let Some(next) = body["last_engine_seq"]
        .as_u64()
        .and_then(|last| last.checked_add(1))
    else {
        delivery.blocked = true;
        return false;
    };
    delivery.next_engine_seq = next;
    delivery.recent_receipts.push_back(acknowledgement.clone());
    if delivery.recent_receipts.len() > 128 {
        delivery.recent_receipts.pop_front();
    }
    delivery.pending = None;
    delivery.pending_events = None;
    true
}

fn append_acknowledgement_matches(
    delivery: &SeatDelivery,
    acknowledgement: &serde_json::Value,
    body: &serde_json::Value,
    chain_root_id: &str,
) -> bool {
    const FIELDS: [&str; 12] = [
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
    let closed = acknowledgement.as_object().is_some_and(|fields| {
        fields.len() == FIELDS.len() && FIELDS.iter().all(|key| fields.contains_key(*key))
    });
    if !closed
        || [
            "producer_incarnation",
            "operation_id",
            "event_count",
            "payload_digest",
        ]
        .iter()
        .any(|key| acknowledgement.get(*key) != body.get(*key))
        || ["first_engine_seq", "last_engine_seq"].iter().any(|key| {
            acknowledgement
                .get(*key)
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<u64>().ok())
                != body.get(*key).and_then(|value| value.as_u64())
        })
        || acknowledgement.get("appended") != body.get("event_count")
        || !acknowledged_append_interval(acknowledgement, body, chain_root_id)
    {
        return false;
    }
    if delivery.verified_receipts.iter().any(|(id, seq, hash)| {
        Some(id.as_str())
            == acknowledgement
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
            && (Some(*seq)
                != exact_u64(
                    acknowledgement.get("receipt_thread_seq"),
                    "receipt_thread_seq",
                )
                .ok()
                || Some(hash.as_str())
                    != acknowledgement
                        .get("receipt_event_hash")
                        .and_then(serde_json::Value::as_str))
    }) {
        return false;
    }
    body["last_engine_seq"]
        .as_u64()
        .and_then(|last| last.checked_add(1))
        .is_some()
}

/// A transport failure or server error cannot distinguish an uncommitted
/// append from a lost reply. Keep the captured body and local prefix pinned
/// while this process is alive, retrying the same idempotent operation until
/// the daemon returns a definite answer. The writer task is separate from the
/// terminal input/render loop, and runtime shutdown cancels its wait.
async fn exact_append_post(
    client: &DaemonClient,
    body: &serde_json::Value,
) -> Result<serde_json::Value, crate::transport::daemon::ClientError> {
    let mut delay_ms = 100u64;
    loop {
        match client
            .signed_post("/ui/api/session/seat/append", body)
            .await
        {
            Ok(response) => return Ok(response),
            Err(error) if append_outcome_unknown(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = delay_ms.saturating_mul(2).min(5_000);
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeatAppendOutcome {
    Acknowledged,
    Unknown,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppendReplyAction {
    Acknowledge,
    RetryExact,
    Refused,
}

fn append_reply_action(
    delivery: &SeatDelivery,
    response: &Result<serde_json::Value, crate::transport::daemon::ClientError>,
    body: &serde_json::Value,
    chain_root_id: &str,
) -> AppendReplyAction {
    match response {
        Ok(envelope)
            if append_acknowledgement_matches(delivery, result(envelope), body, chain_root_id) =>
        {
            AppendReplyAction::Acknowledge
        }
        Ok(_) => AppendReplyAction::RetryExact,
        Err(_) => AppendReplyAction::Refused,
    }
}

/// A completed proof may be replaced only by a new complete proof. This
/// classifier applies before completion; other frames go to the replay fold.
fn precompletion_replay_invalidation(
    frame: &crate::transport::sse::SseFrame,
    began: bool,
) -> Result<bool, String> {
    if frame.event_type != "ui_seat.replay.invalidated.v1" {
        return Ok(false);
    }
    let payload: serde_json::Value = serde_json::from_str(&frame.data)
        .map_err(|error| format!("invalid seat history invalidation: {error}"))?;
    if payload != serde_json::json!({"schema": 1}) || !began {
        return Err("seat history invalidation has an invalid shape or order".into());
    }
    Ok(true)
}

/// Prepare and freshly claim one exact presentation. A daemon-backed UI
/// never degrades to an engine-local seat: doing so
/// would silently discard the durable session authority the user requested.
pub async fn bootstrap_seat(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
) -> Result<SeatBootstrap, String> {
    open_seat_thread(client, binding).await
}

pub async fn resume_seat_thread(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
    presentation_id: &str,
    authenticated: &ryeos_client_base::ui::BrowserSession,
    viewport: ryeos_client_base::ui::BrowserViewport,
    now_ms: u64,
) -> Result<(SeatBootstrap, ryeos_client_base::ui::RyeOsCore), String> {
    if binding.max_seat_append_bytes == 0 {
        return Err("signed seat append byte bound must be positive".into());
    }
    {
        let delivery = client.seat_delivery.lock().await;
        if delivery.pending.is_some() || delivery.blocked {
            return Err("unresolved append prevents resuming another presentation".into());
        }
    }
    let memberships = authenticated
        .presentations
        .iter()
        .filter(|membership| membership.presentation_id == presentation_id)
        .collect::<Vec<_>>();
    if memberships.len() != 1 {
        return Err("resume requires one exact authenticated presentation membership".into());
    }
    let membership = memberships[0];
    let expected_binding = serde_json::to_value(binding).map_err(|error| error.to_string())?;
    if serde_json::to_value(&membership.root_binding).map_err(|error| error.to_string())?
        != expected_binding
    {
        return Err("resume root binding differs from authenticated membership".into());
    }
    let thread_id = membership
        .thread_id
        .as_deref()
        .ok_or("resume membership has no seat thread")?;
    let chain_root_id = membership
        .chain_root_id
        .as_deref()
        .ok_or("resume membership has no chain root")?;
    let observation = observe_control(client, presentation_id, thread_id, chain_root_id).await?;
    if serde_json::to_value(&observation.root_binding).map_err(|error| error.to_string())?
        != expected_binding
    {
        return Err("resume observation differs from authenticated root binding".into());
    }
    let old_incarnation = match &observation.producer {
        UiObservedPresentationProducer::Relinquished {
            producer_incarnation,
        } if canonical_event_hash(producer_incarnation) => producer_incarnation.clone(),
        _ => return Err("native Resume requires an exact Relinquished predecessor".into()),
    };
    let mut request = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: presentation_id.into(),
        thread_id: thread_id.into(),
        chain_root_id: chain_root_id.into(),
        predecessor: observation.control_predecessor,
        intent_nonce: operation_id(),
        claim: UiSeatProducerClaim::Resume {
            expected_producer_incarnation: old_incarnation,
        },
    };
    request.assign_identity().map_err(str::to_owned)?;
    let operation = request.operation_id().to_owned();
    let claim_body = serde_json::to_value(&request).map_err(|error| error.to_string())?;
    // A malformed 200 can follow a committed claim. Re-send only the same
    // operation; do not infer an owner from a presentation ID or substitute a
    // new nonce. If no exact ACK arrives, the outcome remains unresolved.
    let mut acknowledged = None;
    for attempt in 0..3 {
        let envelope = exact_control(client, &claim_body).await?;
        let candidate = result(&envelope);
        if candidate
            .get("presentation_id")
            .and_then(serde_json::Value::as_str)
            == Some(presentation_id)
            && candidate
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(thread_id)
            && candidate
                .get("chain_root_id")
                .and_then(serde_json::Value::as_str)
                == Some(chain_root_id)
            && candidate
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
                == Some(operation.as_str())
            && candidate.get("state").and_then(serde_json::Value::as_str) == Some("active")
        {
            acknowledged = Some(candidate.clone());
            break;
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
        }
    }
    let claimed = acknowledged.ok_or(
        "resume claim outcome unresolved: no exact acknowledgement for the retained operation",
    )?;
    let (new_incarnation, next) = match claim_writer(&claimed) {
        Ok(writer) => writer,
        Err(error) => {
            if let Some(owner) =
                acknowledged_claim_cleanup_owner(&claimed, presentation_id, thread_id)
            {
                relinquish_claimed_seat(client, &owner, chain_root_id).await;
            }
            return Err(error);
        }
    };
    let producer = ryeos_client_base::ui::binding::UiPresentationProducer {
        presentation_id: presentation_id.into(),
        thread_id: thread_id.into(),
        producer_incarnation: new_incarnation,
    };
    let replay_result: Result<_, String> = async {
        let fresh_session = client
            .current_ui_session()
            .await
            .map_err(|error| error.to_string())?;
        if fresh_session.session_id != authenticated.session_id {
            return Err("resume session changed after the claim".into());
        }
        let fresh_memberships = fresh_session
            .presentations
            .iter()
            .filter(|item| item.presentation_id == presentation_id)
            .collect::<Vec<_>>();
        if fresh_memberships.len() != 1 || fresh_memberships[0] != membership {
            return Err("resume membership changed after the claim".into());
        }
        for attempt in 0..MAX_COLD_REPLAY_ATTEMPTS {
            let mut stream = client
                .open_seat_history(presentation_id, chain_root_id)
                .await
                .map_err(|error| error.to_string())?;
            let mut proof = ResumeSeatReplay::new();
            let mut staged = ryeos_client_base::ui::cold_replay::ColdSeatReplay::new();
            let mut core = ryeos_client_base::ui::RyeOsCore::new(
                fresh_session.clone(),
                presentation_id,
                viewport,
                now_ms,
            );
            while let Some(frame) = stream.next_event().await {
                if precompletion_replay_invalidation(&frame, proof.begun)? {
                    break;
                }
                if proof.accept(
                    &frame,
                    presentation_id,
                    chain_root_id,
                    thread_id,
                    &request,
                    &producer.producer_incarnation,
                    next,
                    &mut staged,
                    &mut core,
                )? {
                    let tip_sequence = proof
                        .last_thread_seq
                        .ok_or("completed seat has no tip sequence")?;
                    let tip_hash = proof
                        .last_hash
                        .as_deref()
                        .ok_or("completed seat has no tip hash")?;
                    let effects = staged.finish_at_authenticated_tip(
                        &mut core,
                        tip_hash,
                        tip_sequence,
                        proof.event_count,
                    )?;
                    return Ok((
                        core,
                        effects,
                        stream,
                        proof
                            .last_thread_seq
                            .ok_or("completed seat has no tip sequence")?,
                        proof.last_hash.ok_or("completed seat has no tip hash")?,
                    ));
                }
            }
            if attempt + 1 == MAX_COLD_REPLAY_ATTEMPTS {
                return Err(
                    "seat history ended before a completed Resume proof after three attempts"
                        .into(),
                );
            }
        }
        unreachable!("bounded Resume proof loop always returns")
    }
    .await;
    let (core, replay_effects, live_history, replay_tip_seq, replay_tip_hash) = match replay_result
    {
        Ok(complete) => complete,
        Err(error) => {
            // The claim is exact, but the retained prefix was not accepted.
            // Do not publish a producer or assert clean relinquishment from a
            // core that never passed causal replay. The node's existing lease
            // lifecycle settles this stranded incarnation.
            eprintln!(
                "terminal Resume replay refused for seat {}; claimed producer remains lifecycle-owned: {error}",
                producer.thread_id
            );
            return Err(error);
        }
    };
    let mut delivery = client.seat_delivery.lock().await;
    if delivery.pending.is_some() || delivery.blocked {
        return Err("unresolved append prevents resumed producer publication".into());
    }
    delivery.next_engine_seq = next;
    delivery.chain_root_id = Some(chain_root_id.into());
    delivery.recent_receipts.clear();
    delivery.verified_receipts.clear();
    delivery.max_seat_append_bytes = binding.max_seat_append_bytes;
    *client.ui_producer.write().expect("producer lock poisoned") = Some(producer.clone());
    Ok((
        SeatBootstrap {
            thread_id: thread_id.into(),
            chain_root_id: chain_root_id.into(),
            producer,
            next_engine_seq: next,
            claim_request: request,
            claim_next_engine_seq: next,
            replayed: Vec::new(),
            replay_effects,
            live_history,
            replay_tip_seq,
            replay_tip_hash,
        },
        core,
    ))
}

/// Open the seat session thread.
pub async fn open_seat_thread(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
) -> Result<SeatBootstrap, String> {
    if binding.max_seat_append_bytes == 0 {
        return Err("signed seat append byte bound must be positive".into());
    }
    {
        let delivery = client.seat_delivery.lock().await;
        if delivery.pending.is_some() || delivery.blocked {
            return Err("unresolved append prevents preparing another presentation".into());
        }
    }
    let mut body = serde_json::to_value(binding)
        .map_err(|error| format!("encode durable UI seat binding: {error}"))?;
    body["action"] = serde_json::json!("prepare");
    body["creation_request_id"] = serde_json::json!(operation_id());
    let prepared = exact_control(client, &body).await?;
    let prepared = result(&prepared);
    if prepared.get("root_binding")
        != Some(&serde_json::to_value(binding).map_err(|error| error.to_string())?)
    {
        return Err("prepared presentation root does not match requested binding".into());
    }
    let presentation_id = prepared
        .get("presentation_id")
        .and_then(|v| v.as_str())
        .ok_or("prepare omitted presentation_id")?;
    let thread_id = prepared
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or("prepare omitted thread_id")?;
    let chain_root_id = prepared
        .get("chain_root_id")
        .and_then(|v| v.as_str())
        .ok_or("prepare omitted chain_root_id")?;
    let observation = observe_control(client, presentation_id, thread_id, chain_root_id).await?;
    if !matches!(
        observation.producer,
        UiObservedPresentationProducer::Unclaimed
    ) || serde_json::to_value(&observation.root_binding).map_err(|error| error.to_string())?
        != serde_json::to_value(binding).map_err(|error| error.to_string())?
    {
        return Err("fresh terminal claim requires its exact unclaimed initialization".into());
    }
    let mut request = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: presentation_id.into(),
        thread_id: thread_id.into(),
        chain_root_id: chain_root_id.into(),
        predecessor: observation.control_predecessor,
        intent_nonce: operation_id(),
        claim: UiSeatProducerClaim::Fresh,
    };
    request.assign_identity().map_err(str::to_owned)?;
    let operation = request.operation_id().to_owned();
    let claim_request = request.clone();
    let request = serde_json::to_value(&request).map_err(|error| error.to_string())?;
    let claimed = exact_control(client, &request).await?;
    let claimed = result(&claimed);
    if claimed.get("presentation_id").and_then(|v| v.as_str()) != Some(presentation_id)
        || claimed.get("thread_id").and_then(|v| v.as_str()) != Some(thread_id)
        || claimed.get("chain_root_id").and_then(|v| v.as_str()) != Some(chain_root_id)
        || claimed.get("operation_id").and_then(|v| v.as_str()) != Some(operation.as_str())
        || claimed.get("state").and_then(|v| v.as_str()) != Some("active")
    {
        return Err("claim did not acknowledge the exact presentation operation".into());
    }
    let (producer_incarnation, next) = match claim_writer(claimed) {
        Ok(writer) => writer,
        Err(error) => {
            // The exact claim operation was acknowledged above. A valid
            // incarnation still identifies the owner even if its frontier
            // field is malformed; observe that owner before relinquishing.
            // Without an incarnation, ownership is unknown and must not be
            // guessed from the presentation ID alone.
            if let Some(producer) =
                acknowledged_claim_cleanup_owner(claimed, presentation_id, thread_id)
            {
                relinquish_claimed_seat(client, &producer, chain_root_id).await;
            }
            return Err(error);
        }
    };
    let producer = ryeos_client_base::ui::binding::UiPresentationProducer {
        presentation_id: presentation_id.to_owned(),
        thread_id: thread_id.to_owned(),
        producer_incarnation,
    };
    let replay_result: Result<(Vec<ryeos_client_base::ui::seat::RetainedSeatEvent>, crate::transport::daemon::SseStream, u64, String), String> = async {
        if next != 0 {
            return Err("fresh terminal claim unexpectedly contains retained history; use explicit retained-seat Resume instead".into());
        }
        let creation_request_id = body
            .get("creation_request_id")
            .and_then(serde_json::Value::as_str)
            .ok_or("fresh terminal seat has no creation request ID")?;
        // A publication can advance the signed tip during a fixed-anchor
        // read. Discard every provisional prefix and request a fresh proof of
        // this same claimed seat; never issue another producer claim here.
        for attempt in 0..MAX_COLD_REPLAY_ATTEMPTS {
            let mut stream = client
                .open_seat_history(presentation_id, chain_root_id)
                .await
                .map_err(|error| error.to_string())?;
            let mut replay = FreshSeatReplay::new(&operation, &producer.producer_incarnation);
            while let Some(frame) = stream.next_event().await {
                if precompletion_replay_invalidation(&frame, replay.begun)? {
                    break;
                }
                if replay.accept(
                    &frame,
                    presentation_id,
                    chain_root_id,
                    thread_id,
                    creation_request_id,
                    binding,
                )? {
                    return Ok((vec![replay.initialization.expect("completion requires initialization")], stream,
                        replay.last_thread_seq.ok_or("completed seat has no tip sequence")?,
                        replay.last_hash.ok_or("completed seat has no tip hash")?));
                }
            }
            if attempt + 1 == MAX_COLD_REPLAY_ATTEMPTS {
                return Err("seat history stream ended before its completion fence after three fresh proofs".into());
            }
        }
        unreachable!("bounded history proof loop always returns")
    }
    .await;
    let (replayed, live_history, replay_tip_seq, replay_tip_hash) = match replay_result {
        Ok(replayed) => replayed,
        Err(error) => {
            relinquish_claimed_seat(client, &producer, chain_root_id).await;
            return Err(error);
        }
    };
    let mut delivery = client.seat_delivery.lock().await;
    if delivery.pending.is_some() || delivery.blocked {
        drop(delivery);
        relinquish_claimed_seat(client, &producer, chain_root_id).await;
        return Err("unresolved append prevents producer replacement".into());
    }
    delivery.next_engine_seq = next;
    delivery.chain_root_id = Some(chain_root_id.to_owned());
    delivery.recent_receipts.clear();
    delivery.verified_receipts.clear();
    delivery.max_seat_append_bytes = binding.max_seat_append_bytes;
    *client.ui_producer.write().expect("producer lock poisoned") = Some(producer.clone());
    Ok(SeatBootstrap {
        thread_id: thread_id.to_owned(),
        chain_root_id: chain_root_id.to_owned(),
        producer,
        next_engine_seq: next,
        claim_request,
        claim_next_engine_seq: next,
        replayed,
        replay_effects: Vec::new(),
        live_history,
        replay_tip_seq,
        replay_tip_hash,
    })
}

#[cfg(test)]
fn seat_event_from_replay(event: &serde_json::Value) -> Option<SeatEvent> {
    ryeos_client_base::ui::seat::retained_seat_event(event)
        .ok()
        .flatten()
}

/// Serialize newly-appended seat events for the braid mirror. The local
/// log is the write-ahead view; the braid is the durable truth.
pub fn braid_batch(events: &[SeatEvent]) -> Result<Vec<serde_json::Value>, String> {
    events
        .iter()
        .map(|event| serde_json::to_value(&event.kind).map_err(|error| error.to_string()))
        .collect()
}

/// Conservatively size the complete append wrapper before handing a captured
/// local prefix to the asynchronous writer. Maximum-width engine sequences
/// ensure the real contiguous interval cannot grow beyond this estimate.
pub fn append_prefix_len(
    events: &[SeatEvent],
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    limit: u64,
) -> usize {
    let mut wire = Vec::new();
    let mut count = 0;
    for event in events.iter().take(MAX_SEAT_BATCH_EVENTS) {
        let Some(mut encoded) = braid_batch(std::slice::from_ref(event))
            .ok()
            .and_then(|mut batch| batch.pop())
        else {
            break;
        };
        encoded["engine_seq"] = serde_json::json!(u64::MAX);
        wire.push(encoded);
        let body = serde_json::json!({"presentation_id":producer.presentation_id,"thread_id":producer.thread_id,
            "producer_incarnation":producer.producer_incarnation,"operation_id":"00000000-0000-4000-8000-000000000000",
            "first_engine_seq":u64::MAX,"last_engine_seq":u64::MAX,"event_count":wire.len(),
            "payload_digest":"0".repeat(64),"events":wire});
        if serde_json::to_vec(&body).map_or(true, |bytes| bytes.len() as u64 > limit) {
            break;
        }
        count += 1;
    }
    count
}

/// Build one bounded immutable transport request from the original captured
/// producer interval. This pure seam is shared by first delivery and exact
/// retry; a retained request must never acknowledge another local prefix.
fn append_body(
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    chain_root_id: &str,
    first: u64,
    captured: &[SeatEvent],
    max_bytes: u64,
) -> Result<serde_json::Value, String> {
    if captured.is_empty() || captured.len() > MAX_SEAT_BATCH_EVENTS {
        return Err("seat append event count exceeds the adapter batch bound".into());
    }
    let last = first
        .checked_add(captured.len() as u64 - 1)
        .filter(|last| last.checked_add(1).is_some())
        .ok_or("producer engine interval is exhausted")?;
    for (offset, event) in captured.iter().enumerate() {
        let expected = first + offset as u64;
        if event
            .producer_engine_seq
            .is_some_and(|original| original != expected)
            || matches!(&event.kind, SeatEventKind::PresentationInitialized(_))
            || matches!(&event.kind, SeatEventKind::PresentationOperationPrepared(operation)
                if operation.prepared_engine_seq != expected
                    || &operation.producer != producer || operation.validate().is_err())
        {
            return Err("captured event differs from its original producer interval".into());
        }
    }
    let events = braid_batch(captured)?
        .into_iter()
        .enumerate()
        .map(|(offset, mut event)| {
            event["engine_seq"] = serde_json::json!(first + offset as u64);
            event
        })
        .collect::<Vec<_>>();
    let digest = ryeos_client_base::ui::seat_payload_digest(&serde_json::json!(events))
        .map_err(|error| error.to_string())?;
    let operation_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: producer.presentation_id.clone(),
        chain_root_id: chain_root_id.into(),
        thread_id: producer.thread_id.clone(),
        producer_incarnation: producer.producer_incarnation.clone(),
        first_engine_seq: first,
        last_engine_seq: last,
        event_count: events.len() as u64,
        payload_digest: digest.clone(),
    }
    .derive_identity()
    .map_err(str::to_owned)?;
    let body = serde_json::json!({"presentation_id":producer.presentation_id,"thread_id":producer.thread_id,
        "producer_incarnation":producer.producer_incarnation,"operation_id":operation_id,
        "first_engine_seq":first,"last_engine_seq":last,"event_count":events.len(),"payload_digest":digest,"events":events});
    if max_bytes == 0
        || serde_json::to_vec(&body).map_or(true, |bytes| bytes.len() as u64 > max_bytes)
    {
        return Err("seat append exceeds its signed byte bound".into());
    }
    Ok(body)
}

/// Append one mirrored batch to the seat thread's braid. A single writer
/// task calls this with at most one batch in flight, so braid order
/// matches local append order without the loop ever waiting on it.
pub async fn append_braid(
    client: &DaemonClient,
    captured_producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    events: &[SeatEvent],
    canonical: &mut Vec<ryeos_client_base::ui::seat::RetainedSeatEvent>,
) -> SeatAppendOutcome {
    let Some(producer) = client
        .ui_producer
        .read()
        .expect("producer lock poisoned")
        .clone()
    else {
        return SeatAppendOutcome::Refused;
    };
    if &producer != captured_producer || events.is_empty() {
        return SeatAppendOutcome::Refused;
    }
    let mut delivery = client.seat_delivery.lock().await;
    if delivery.blocked
        || client
            .ui_producer
            .read()
            .expect("producer lock poisoned")
            .as_ref()
            != Some(captured_producer)
    {
        return if delivery.pending.is_some() {
            SeatAppendOutcome::Unknown
        } else {
            SeatAppendOutcome::Refused
        };
    }
    let Some(chain_root_id) = delivery.chain_root_id.as_deref() else {
        return SeatAppendOutcome::Refused;
    };
    let chain_root_id = chain_root_id.to_owned();
    let candidate = match append_body(
        captured_producer,
        &chain_root_id,
        delivery.next_engine_seq,
        events,
        delivery.max_seat_append_bytes,
    ) {
        Ok(candidate) => candidate,
        Err(_) => {
            delivery.blocked = true;
            return SeatAppendOutcome::Refused;
        }
    };
    let body = match delivery.retain_exact_append(candidate, events) {
        Ok(retained) => retained.clone(),
        Err(_) => return SeatAppendOutcome::Unknown,
    };
    // The pending body and captured local prefix stay pinned, but the UI must
    // not hold its delivery mutex while an unknown network outcome is retried.
    drop(delivery);
    let mut malformed_replies = 0u8;
    loop {
        let response = exact_append_post(client, &body).await;
        let mut delivery = client.seat_delivery.lock().await;
        if delivery.pending.as_ref() != Some(&body)
            || delivery.pending_events.as_deref() != Some(events)
            || delivery.blocked
            || client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .as_ref()
                != Some(captured_producer)
        {
            delivery.blocked = true;
            return SeatAppendOutcome::Unknown;
        }
        match append_reply_action(&delivery, &response, &body, &chain_root_id) {
            AppendReplyAction::Acknowledge => {
                let Ok(envelope) = response else {
                    unreachable!()
                };
                let retained = match acknowledged_canonical_events(result(&envelope)) {
                    Ok(events) => events,
                    Err(_) => {
                        delivery.blocked = true;
                        return SeatAppendOutcome::Unknown;
                    }
                };
                return if accept_append_acknowledgement(
                    &mut delivery,
                    result(&envelope),
                    &body,
                    &chain_root_id,
                ) {
                    *canonical = retained;
                    SeatAppendOutcome::Acknowledged
                } else {
                    SeatAppendOutcome::Unknown
                };
            }
            AppendReplyAction::Refused => {
                delivery.blocked = true;
                return SeatAppendOutcome::Refused;
            }
            AppendReplyAction::RetryExact => {
                malformed_replies += 1;
                if malformed_replies == 5 {
                    delivery.blocked = true;
                    return SeatAppendOutcome::Unknown;
                }
                drop(delivery);
                tokio::time::sleep(std::time::Duration::from_millis(
                    100u64 << (malformed_replies - 1),
                ))
                .await;
            }
        }
    }
}

fn acknowledged_canonical_events(
    acknowledgement: &serde_json::Value,
) -> Result<Vec<ryeos_client_base::ui::seat::RetainedSeatEvent>, String> {
    if acknowledgement
        .get("events_authority")
        .and_then(serde_json::Value::as_str)
        != Some("authoritative")
    {
        return Err("append evidence has no canonical authority marker".into());
    }
    acknowledgement
        .get("events")
        .and_then(serde_json::Value::as_array)
        .ok_or("append evidence interval absent")?
        .iter()
        .map(|row| {
            let mut row = row.clone();
            for field in ["chain_seq", "thread_seq"] {
                let position = exact_u64(row.get(field), field)?;
                row[field] = serde_json::json!(position);
            }
            let engine_seq = exact_u64(
                row.get("payload").and_then(|payload| payload.get("seq")),
                "seq",
            )?;
            row["payload"]["seq"] = serde_json::json!(engine_seq);
            ryeos_client_base::ui::seat::retained_seat_event_with_evidence(&row)?
                .ok_or_else(|| "append evidence row is not an ordinary producer event".into())
        })
        .collect()
}

/// Fence the exact native writer before network observation. This transition
/// holds both local owners only briefly: an append rechecks the blocked delivery
/// under its lock, and a replacement producer is never cleared by an old close.
fn prepare_close_delivery(
    delivery: &mut SeatDelivery,
    current: &mut Option<ryeos_client_base::ui::binding::UiPresentationProducer>,
    expected: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> Option<(
    ryeos_client_base::ui::binding::UiPresentationProducer,
    String,
)> {
    if current.as_ref() != Some(expected) || delivery.pending.is_some() || delivery.blocked {
        return None;
    }
    let chain_root_id = delivery.chain_root_id.clone()?;
    if chain_root_id != expected.thread_id {
        return None;
    }
    delivery.blocked = true;
    *current = None;
    Some((expected.clone(), chain_root_id))
}

/// Relinquish the exact native producer on clean exit; best effort. This is
/// deliberately not `Close`: a verified Relinquished predecessor is eligible
/// for an explicit Resume while a Closed seat is terminal.
pub async fn relinquish_seat_thread(client: &DaemonClient, thread_id: &str) {
    let Some(expected) = client
        .ui_producer
        .read()
        .expect("producer lock poisoned")
        .clone()
        .filter(|producer| producer.thread_id == thread_id)
    else {
        return;
    };
    let Some((producer, chain_root_id)) = ({
        let mut delivery = client.seat_delivery.lock().await;
        let mut current = client.ui_producer.write().expect("producer lock poisoned");
        prepare_close_delivery(&mut delivery, &mut current, &expected)
    }) else {
        return;
    };
    relinquish_claimed_seat(client, &producer, &chain_root_id).await;
}

/// Only an acknowledged exact claim may enter here. Observe the current
/// control predecessor before relinquishing; an uncertain network result is
/// logged, never converted into evidence that this seat was released.
async fn relinquish_claimed_seat(
    client: &DaemonClient,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    chain_root_id: &str,
) {
    let observation = match observe_control(
        client,
        &producer.presentation_id,
        &producer.thread_id,
        chain_root_id,
    )
    .await
    {
        Ok(observation) => observation,
        Err(error) => {
            eprintln!("terminal relinquishment observation refused: {error}");
            return;
        }
    };
    if !matches!(&observation.producer, UiObservedPresentationProducer::Active { producer_incarnation }
        if producer_incarnation == &producer.producer_incarnation)
    {
        return;
    }
    let mut request = UiSeatControlRequest::Relinquish {
        schema: 2,
        operation_id: String::new(),
        presentation_id: producer.presentation_id.clone(),
        thread_id: producer.thread_id.clone(),
        chain_root_id: chain_root_id.to_owned(),
        predecessor: observation.control_predecessor,
        intent_nonce: operation_id(),
        producer_incarnation: producer.producer_incarnation.clone(),
    };
    if let Err(error) = request.assign_identity() {
        eprintln!("terminal relinquishment identity refused: {error}");
        return;
    }
    let body = match serde_json::to_value(&request) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("terminal relinquishment encoding refused: {error}");
            return;
        }
    };
    match exact_post(client, "/ui/api/session/seat/close", &body).await {
        Ok(envelope) => {
            let acknowledgement = result(&envelope);
            if [
                "presentation_id",
                "thread_id",
                "producer_incarnation",
                "operation_id",
            ]
            .iter()
            .any(|key| acknowledgement.get(*key) != body.get(*key))
                || acknowledgement
                    .get("state")
                    .and_then(|value| value.as_str())
                    != Some("relinquished")
            {
                eprintln!(
                    "terminal relinquishment acknowledgement did not match; outcome unresolved"
                );
            }
        }
        Err(error) => eprintln!("terminal relinquishment outcome unresolved: {error}"),
    }
}

/// Refresh the runtime-only seat presence lease; best effort and deliberately
/// independent of durable seat events.
pub async fn touch_seat_thread(
    client: &DaemonClient,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> SeatTouchOutcome {
    if client
        .ui_producer
        .read()
        .expect("producer lock poisoned")
        .as_ref()
        != Some(producer)
    {
        return SeatTouchOutcome::Refused;
    }
    touch_outcome(
        client
            .signed_post(
                "/ui/api/session/seat/touch",
                &serde_json::to_value(producer).expect("producer coordinate serializes"),
            )
            .await,
        &producer,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_begun_precompletion_invalidation_retries_replay() {
        let valid = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.invalidated.v1".into(),
            data: r#"{"schema":1}"#.into(),
        };
        assert_eq!(precompletion_replay_invalidation(&valid, true), Ok(true));
        assert!(precompletion_replay_invalidation(&valid, false).is_err());
        let mut malformed = valid.clone();
        malformed.data = r#"{"schema":1,"extra":true}"#.into();
        assert!(precompletion_replay_invalidation(&malformed, true).is_err());
        let other = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.events.v1".into(),
            data: "not parsed by this classifier".into(),
        };
        assert_eq!(precompletion_replay_invalidation(&other, true), Ok(false));
    }

    #[test]
    fn malformed_claim_frontier_only_relinquishes_an_exact_incarnation() {
        let malformed_frontier = serde_json::json!({
            "producer_incarnation": "ab".repeat(32),
            "next_engine_seq": "not-a-sequence",
        });
        assert!(claim_writer(&malformed_frontier).is_err());
        let owner = acknowledged_claim_cleanup_owner(&malformed_frontier, "presentation", "seat")
            .expect("exact acknowledged incarnation remains available for cleanup");
        assert_eq!(owner.presentation_id, "presentation");
        assert_eq!(owner.thread_id, "seat");
        assert_eq!(owner.producer_incarnation, "ab".repeat(32));
        for invalid in [
            serde_json::json!({"next_engine_seq":0}),
            serde_json::json!({
                "producer_incarnation":"AB".repeat(32),"next_engine_seq":0
            }),
        ] {
            assert!(acknowledged_claim_cleanup_owner(&invalid, "presentation", "seat").is_none());
        }
    }

    fn captured_producer() -> ryeos_client_base::ui::UiPresentationProducer {
        ryeos_client_base::ui::UiPresentationProducer {
            presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            thread_id: "seat".into(),
            producer_incarnation: "ab".repeat(32),
        }
    }

    #[test]
    fn native_close_fences_exact_delivery_before_network_and_preserves_other_writers() {
        let producer = captured_producer();
        let mut delivery = SeatDelivery {
            chain_root_id: Some("seat".into()),
            ..Default::default()
        };
        let mut current = Some(producer.clone());
        let other = ryeos_client_base::ui::UiPresentationProducer {
            thread_id: "other".into(),
            ..producer.clone()
        };
        assert!(prepare_close_delivery(&mut delivery, &mut current, &other).is_none());
        assert!(!delivery.blocked);
        assert_eq!(current, Some(producer.clone()));

        delivery.pending = Some(serde_json::json!({"retained":"append"}));
        assert!(prepare_close_delivery(&mut delivery, &mut current, &producer).is_none());
        assert!(!delivery.blocked);
        assert_eq!(current, Some(producer.clone()));
        delivery.pending = None;

        delivery.chain_root_id = Some("other-root".into());
        assert!(prepare_close_delivery(&mut delivery, &mut current, &producer).is_none());
        assert!(!delivery.blocked);
        delivery.chain_root_id = Some("seat".into());

        let mut already_blocked = SeatDelivery {
            chain_root_id: Some("seat".into()),
            blocked: true,
            ..Default::default()
        };
        let mut blocked_current = Some(producer.clone());
        assert!(
            prepare_close_delivery(&mut already_blocked, &mut blocked_current, &producer).is_none()
        );
        assert_eq!(blocked_current, Some(producer.clone()));

        assert_eq!(
            prepare_close_delivery(&mut delivery, &mut current, &producer),
            Some((producer.clone(), "seat".into()))
        );
        assert!(delivery.blocked);
        assert!(current.is_none());
        assert!(prepare_close_delivery(&mut delivery, &mut current, &producer).is_none());

        let replacement = ryeos_client_base::ui::UiPresentationProducer {
            producer_incarnation: "cd".repeat(32),
            ..producer.clone()
        };
        let mut replacement_current = Some(replacement.clone());
        let mut replacement_delivery = SeatDelivery {
            chain_root_id: Some("seat".into()),
            ..Default::default()
        };
        assert!(
            prepare_close_delivery(
                &mut replacement_delivery,
                &mut replacement_current,
                &producer
            )
            .is_none()
        );
        assert!(!replacement_delivery.blocked);
        assert_eq!(replacement_current, Some(replacement));
    }

    fn facets(count: usize) -> Vec<SeatEvent> {
        (0..count)
            .map(|index| SeatEvent {
                seq: 100 + index as u64,
                producer_engine_seq: None,
                kind: SeatEventKind::Facet {
                    key: "selection".into(),
                    value: serde_json::json!(index),
                },
            })
            .collect()
    }

    #[test]
    fn native_claim_writer_requires_canonical_producer_and_live_exact_u64_frontier() {
        let baseline = serde_json::json!({
            "producer_incarnation":"ab".repeat(32), "next_engine_seq":"0",
        });
        assert_eq!(claim_writer(&baseline).unwrap(), ("ab".repeat(32), 0));
        for frontier in [
            serde_json::Value::Null,
            serde_json::json!(-1),
            serde_json::json!("-1"),
            serde_json::json!("00"),
            serde_json::json!("+0"),
            serde_json::json!(u64::MAX),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut invalid = baseline.clone();
            invalid["next_engine_seq"] = frontier;
            assert!(claim_writer(&invalid).is_err());
        }
        for producer in ["".into(), "a".repeat(63), "A".repeat(64), "g".repeat(64)] {
            let mut invalid = baseline.clone();
            invalid["producer_incarnation"] = serde_json::json!(producer);
            assert!(claim_writer(&invalid).is_err());
        }
        for field in ["producer_incarnation", "next_engine_seq"] {
            let mut missing = baseline.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(claim_writer(&missing).is_err());
        }
        let mut last = baseline;
        last["next_engine_seq"] = serde_json::json!((u64::MAX - 1).to_string());
        assert_eq!(claim_writer(&last).unwrap().1, u64::MAX - 1);
    }

    #[test]
    fn native_replay_stream_rejects_duplicate_begin_and_unbounded_or_unproved_completion() {
        let binding = SeatBindingCoordinate {
            binding_attachment_id: "root".into(),
            binding_generation: 1,
            binding_digest: "a".repeat(64),
            max_seat_append_bytes: 65_536,
        };
        let frame = |event_type: &str, data: serde_json::Value| crate::transport::sse::SseFrame {
            event_type: event_type.into(),
            data: data.to_string(),
        };
        let begin = frame(
            "ui_seat.replay.begin.v1",
            serde_json::json!({"schema":1,"presentation_id":"p","chain_root_id":"r","thread_id":"t"}),
        );
        let mut replay = FreshSeatReplay::new("claim", &"a".repeat(64));
        assert!(
            !replay
                .accept(&begin, "p", "r", "t", "creation", &binding)
                .unwrap()
        );
        assert!(
            replay
                .accept(&begin, "p", "r", "t", "creation", &binding)
                .is_err()
        );
        let unbounded = frame(
            "ui_seat.replay.events.v1",
            serde_json::json!({"schema":1,"events":vec![serde_json::json!({});MAX_SEAT_REPLAY_EVENTS+1]}),
        );
        assert!(
            replay
                .accept(&unbounded, "p", "r", "t", "creation", &binding)
                .is_err()
        );
        let complete = frame(
            "ui_seat.replay.complete.v1",
            serde_json::json!({"schema":1,"tip_hash":"a".repeat(64),"thread_seq":2,"event_count":1}),
        );
        assert!(
            replay
                .accept(&complete, "p", "r", "t", "creation", &binding)
                .is_err()
        );
    }

    #[test]
    fn native_replay_rejects_foreign_or_missing_event_seat_identity() {
        let binding = SeatBindingCoordinate {
            binding_attachment_id: "root".into(),
            binding_generation: 1,
            binding_digest: "a".repeat(64),
            max_seat_append_bytes: 65_536,
        };
        let frame = |event_type: &str, data: serde_json::Value| crate::transport::sse::SseFrame {
            event_type: event_type.into(),
            data: data.to_string(),
        };
        let begin = frame(
            "ui_seat.replay.begin.v1",
            serde_json::json!({"schema":1,"presentation_id":"p","chain_root_id":"r","thread_id":"t"}),
        );
        for event in [
            serde_json::json!({"chain_root_id":"foreign","thread_id":"t"}),
            serde_json::json!({"chain_root_id":"r","thread_id":"foreign"}),
            serde_json::json!({"thread_id":"t"}),
            serde_json::json!({"chain_root_id":"r"}),
        ] {
            let mut replay = FreshSeatReplay::new("claim", &"a".repeat(64));
            assert!(
                !replay
                    .accept(&begin, "p", "r", "t", "creation", &binding)
                    .unwrap()
            );
            let events = frame(
                "ui_seat.replay.events.v1",
                serde_json::json!({"schema":1,"events":[event]}),
            );
            assert_eq!(
                replay
                    .accept(&events, "p", "r", "t", "creation", &binding)
                    .unwrap_err(),
                "seat history event belongs to another seat"
            );
        }
    }

    #[test]
    fn native_fresh_replay_requires_the_acknowledged_claim_at_its_signed_tip() {
        let presentation_id = "11111111-1111-4111-8111-111111111111";
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: presentation_id.into(),
            thread_id: "t".into(),
            chain_root_id: "r".into(),
            predecessor: UiSeatControlPredecessor::Initialization {
                event_hash: "a".repeat(64),
            },
            intent_nonce: "22222222-2222-4222-8222-222222222222".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        request.assign_identity().unwrap();
        let claim_operation = request.operation_id().to_owned();
        let incarnation = "b".repeat(64);
        let control = UiSeatProducer {
            schema_version: ryeos_client_base::ui::binding::SEAT_PRODUCER_SCHEMA.into(),
            request,
            operation_id: claim_operation.clone(),
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: incarnation.clone(),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        control.validate().unwrap();
        let binding = SeatBindingCoordinate {
            binding_attachment_id: "root".into(),
            binding_generation: 1,
            binding_digest: "a".repeat(64),
            max_seat_append_bytes: 65_536,
        };
        let event = serde_json::json!({
            "chain_root_id":"r","thread_id":"t","chain_seq":1,"thread_seq":1,
            "event_hash":"c".repeat(64),"event_type":SEAT_PRODUCER_EVENT,
            "payload":control,
        });
        let events = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.events.v1".into(),
            data: serde_json::json!({"schema":1,"events":[event]}).to_string(),
        };
        let complete = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.complete.v1".into(),
            data: serde_json::json!({
                "schema":1,"tip_hash":"c".repeat(64),"thread_seq":1,"event_count":1
            })
            .to_string(),
        };
        let replay = |expected_incarnation: &str| {
            let mut replay = FreshSeatReplay::new(&claim_operation, expected_incarnation);
            // The separate initialization regression covers its full shape;
            // isolate the tip-control decision after a validated bootstrap.
            replay.begun = true;
            replay.initialization = Some(ryeos_client_base::ui::seat::RetainedSeatEvent {
                event: SeatEvent {
                    seq: 0,
                    producer_engine_seq: None,
                    kind: SeatEventKind::Facet {
                        key: "fixture".into(),
                        value: serde_json::Value::Null,
                    },
                },
                evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence {
                    event_hash: "d".repeat(64),
                    chain_seq: 0,
                },
                thread_id: "t".into(),
                chain_root_id: "r".into(),
            });
            replay
        };
        let mut current = replay(&incarnation);
        assert!(
            !current
                .accept(&events, presentation_id, "r", "t", "creation", &binding)
                .unwrap()
        );
        assert!(
            current
                .accept(&complete, presentation_id, "r", "t", "creation", &binding)
                .unwrap()
        );
        let late_proof = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.transfer_proof.v1".into(),
            data: serde_json::json!({"schema":1,"proof":{}}).to_string(),
        };
        let cancellation_proof = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.move_freeze_cancellation_proof.v1".into(),
            data: serde_json::json!({"schema":1,"proof":{}}).to_string(),
        };
        assert!(
            replay(&incarnation)
                .accept(
                    &cancellation_proof,
                    presentation_id,
                    "r",
                    "t",
                    "creation",
                    &binding
                )
                .unwrap_err()
                .contains("fresh terminal claim")
        );
        assert!(
            current
                .accept(
                    &cancellation_proof,
                    presentation_id,
                    "r",
                    "t",
                    "creation",
                    &binding
                )
                .is_err()
        );
        assert!(
            current
                .accept(&late_proof, presentation_id, "r", "t", "creation", &binding)
                .is_err()
        );
        assert!(
            current
                .accept(&complete, presentation_id, "r", "t", "creation", &binding)
                .is_err()
        );

        assert!(
            replay(&incarnation)
                .accept(&complete, presentation_id, "r", "t", "creation", &binding)
                .unwrap_err()
                .contains("no authenticated producer control")
        );

        let mut stale = replay(&"d".repeat(64));
        stale
            .accept(&events, presentation_id, "r", "t", "creation", &binding)
            .unwrap();
        assert!(
            stale
                .accept(&complete, presentation_id, "r", "t", "creation", &binding)
                .is_err()
        );

        let mut changed = replay(&incarnation);
        changed
            .accept(&events, presentation_id, "r", "t", "creation", &binding)
            .unwrap();
        let mut later: serde_json::Value = serde_json::from_str(&events.data).unwrap();
        later["events"][0]["chain_seq"] = serde_json::json!(2);
        later["events"][0]["thread_seq"] = serde_json::json!(2);
        later["events"][0]["event_hash"] = serde_json::json!("d".repeat(64));
        let later = crate::transport::sse::SseFrame {
            event_type: "ui_seat.replay.events.v1".into(),
            data: later.to_string(),
        };
        assert!(
            changed
                .accept(&later, presentation_id, "r", "t", "creation", &binding)
                .is_err()
        );
    }

    #[test]
    fn native_live_receipt_requires_exact_current_or_recent_append() {
        let producer = captured_producer();
        let body = serde_json::json!({
            "operation_id":"op-1", "producer_incarnation":producer.producer_incarnation,
            "first_engine_seq":1, "last_engine_seq":2, "event_count":2,
            "payload_digest":"digest-1"
        });
        let receipt = serde_json::json!({
            "schema":1, "operation_id":"op-1", "producer_incarnation":producer.producer_incarnation,
            "first_engine_seq":"1", "last_engine_seq":"2", "event_count":"2",
            "payload_digest":"digest-1", "event_hash":"b".repeat(64), "thread_seq":"7"
        });
        let mut delivery = SeatDelivery {
            pending: Some(body.clone()),
            ..Default::default()
        };
        let mut tip = (3, "a".repeat(64));
        let mut seen = std::collections::VecDeque::new();
        assert!(accept_live_receipt(
            &mut delivery,
            &receipt,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
        assert_eq!(tip.0, 7);
        delivery.pending = None;
        delivery.recent_receipts.push_back(serde_json::json!({
            "operation_id":"op-1", "producer_incarnation":producer.producer_incarnation,
            "first_engine_seq":"1", "last_engine_seq":"2", "event_count":2,
            "payload_digest":"digest-1", "receipt_event_hash":"b".repeat(64),
            "receipt_thread_seq":"7"
        }));
        assert!(accept_live_receipt(
            &mut delivery,
            &receipt,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
        let mut mismatched_receipt = receipt.clone();
        mismatched_receipt["event_hash"] = serde_json::json!("d".repeat(64));
        assert!(!accept_live_receipt(
            &mut delivery,
            &mismatched_receipt,
            &producer.producer_incarnation,
            &mut (3, "a".repeat(64)),
            &mut std::collections::VecDeque::new()
        ));
        let mut foreign = receipt.clone();
        foreign["producer_incarnation"] = serde_json::json!("c".repeat(64));
        assert!(!accept_live_receipt(
            &mut delivery,
            &foreign,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
        let mut changed = receipt.clone();
        changed["payload_digest"] = serde_json::json!("other");
        assert!(!accept_live_receipt(
            &mut delivery,
            &changed,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
        let mut advanced_duplicate = receipt.clone();
        advanced_duplicate["thread_seq"] = serde_json::json!("8");
        advanced_duplicate["event_hash"] = serde_json::json!("d".repeat(64));
        assert!(!accept_live_receipt(
            &mut delivery,
            &advanced_duplicate,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
        assert_eq!(tip.0, 7);
        let mut contradictory = receipt;
        contradictory["event_hash"] = serde_json::json!("d".repeat(64));
        assert!(!accept_live_receipt(
            &mut delivery,
            &contradictory,
            &producer.producer_incarnation,
            &mut tip,
            &mut seen
        ));
    }

    #[test]
    fn late_live_watcher_cannot_block_replacement_producer() {
        let old = captured_producer();
        let replacement = ryeos_client_base::ui::UiPresentationProducer {
            producer_incarnation: "cd".repeat(32),
            ..old.clone()
        };
        assert!(current_live_producer(Some(&old), &old));
        assert!(!current_live_producer(Some(&replacement), &old));
        assert!(!current_live_producer(None, &old));
    }

    #[test]
    fn replacement_live_handoff_has_one_unconsumed_slot() {
        let mut slot = None;
        assert_eq!(reserve_live_handoff(&mut slot, "first"), Ok(()));
        assert_eq!(reserve_live_handoff(&mut slot, "second"), Err("second"));
        assert_eq!(slot.take(), Some("first"));
        assert_eq!(reserve_live_handoff(&mut slot, "successor"), Ok(()));
        assert_eq!(slot, Some("successor"));
    }

    #[test]
    fn failed_replacement_admission_fences_only_exact_claimed_incarnation() {
        let failed = captured_producer();
        let successor = ryeos_client_base::ui::UiPresentationProducer {
            producer_incarnation: "cd".repeat(32),
            ..failed.clone()
        };
        let mut delivery = SeatDelivery::default();
        let mut current = Some(successor.clone());
        assert!(!fence_failed_bootstrap_state(
            &mut delivery,
            &mut current,
            &failed
        ));
        assert!(!delivery.blocked);
        assert_eq!(current, Some(successor.clone()));
        assert!(fence_failed_bootstrap_state(
            &mut delivery,
            &mut current,
            &successor
        ));
        assert!(delivery.blocked);
        assert!(current.is_none());
    }

    #[test]
    fn native_append_has_an_event_bound_even_when_small_facets_fit_the_byte_bound() {
        let producer = captured_producer();
        let events = facets(256);
        let before = events.clone();
        let count = append_prefix_len(&events, &producer, 1_000_000);
        assert_eq!(count, MAX_SEAT_BATCH_EVENTS);
        let request = append_body(&producer, "root", 7, &events[..count], 1_000_000).unwrap();
        assert_eq!(request["event_count"], MAX_SEAT_BATCH_EVENTS);
        assert_eq!(request["first_engine_seq"], 7);
        assert_eq!(
            request["last_engine_seq"],
            7 + MAX_SEAT_BATCH_EVENTS as u64 - 1
        );
        assert!(append_body(&producer, "root", 7, &events, 1_000_000).is_err());
        assert!(append_body(&producer, "root", 7, &[], 1_000_000).is_err());
        assert_eq!(events, before);
    }

    #[test]
    fn native_request_keeps_engine_positions_distinct_from_local_and_chain_sequences() {
        let producer = captured_producer();
        let mut events = facets(2);
        events[0].producer_engine_seq = Some(41);
        events[1].producer_engine_seq = Some(42);
        let request = append_body(&producer, "root", 41, &events, 65_536).unwrap();
        assert_eq!(request["events"][0]["engine_seq"], 41);
        assert_eq!(request["events"][1]["engine_seq"], 42);
        assert_eq!(
            request["operation_id"].as_str().unwrap().as_bytes()[14],
            b'8'
        );
        events[0].seq = 700;
        events[1].seq = 900;
        assert_eq!(
            append_body(&producer, "root", 41, &events, 65_536).unwrap(),
            request
        );
        assert!(append_body(&producer, "root", 42, &events, 65_536).is_err());
    }

    #[test]
    fn native_exact_retry_cannot_acknowledge_a_different_captured_prefix() {
        let producer = captured_producer();
        let captured = facets(1);
        let original = append_body(&producer, "root", 0, &captured, 65_536).unwrap();
        let mut delivery = SeatDelivery::default();
        assert_eq!(
            delivery
                .retain_exact_append(original.clone(), &captured)
                .unwrap(),
            &original
        );
        assert_eq!(
            delivery
                .retain_exact_append(original.clone(), &captured)
                .unwrap(),
            &original
        );
        let mut changed = facets(1);
        changed[0].kind = SeatEventKind::Facet {
            key: "selection".into(),
            value: serde_json::json!("different local work"),
        };
        let conflicting = append_body(&producer, "root", 0, &changed, 65_536).unwrap();
        assert_ne!(conflicting["operation_id"], original["operation_id"]);
        assert!(delivery.retain_exact_append(conflicting, &changed).is_err());
        assert!(delivery.blocked);
        assert_eq!(delivery.pending.as_ref(), Some(&original));
        assert_eq!(delivery.next_engine_seq, 0);
    }

    #[test]
    fn native_unknown_append_outcome_keeps_the_exact_request_retryable() {
        use crate::transport::daemon::ClientError;

        let producer = captured_producer();
        let captured = facets(1);
        let original = append_body(&producer, "root", 0, &captured, 65_536).unwrap();
        let mut delivery = SeatDelivery::default();
        delivery
            .retain_exact_append(original.clone(), &captured)
            .unwrap();
        assert!(append_outcome_unknown(&ClientError::DaemonError {
            path: "/ui/api/session/seat/append".into(),
            status: 500,
            message: "outcome unknown".into(),
        }));
        assert!(append_outcome_unknown(&ClientError::DaemonError {
            path: "/ui/api/session/seat/append".into(),
            status: 408,
            message: "request timed out after possible commit".into(),
        }));
        assert!(append_outcome_unknown(&ClientError::DaemonError {
            path: "/ui/api/session/seat/append".into(),
            status: 429,
            message: "retry later without a commit witness".into(),
        }));
        assert!(append_outcome_unknown(&ClientError::DaemonDown {
            url: "http://127.0.0.1:7400".into(),
        }));
        assert!(append_outcome_unknown(&ClientError::Json(
            serde_json::from_slice::<serde_json::Value>(b"{").unwrap_err(),
        )));
        assert!(!append_outcome_unknown(&ClientError::DaemonError {
            path: "/ui/api/session/seat/append".into(),
            status: 403,
            message: "definite refusal".into(),
        }));
        assert!(!delivery.blocked);
        assert_eq!(delivery.pending.as_ref(), Some(&original));
        assert_eq!(
            delivery.pending_events.as_deref(),
            Some(captured.as_slice())
        );
        assert_eq!(delivery.next_engine_seq, 0);
    }

    #[test]
    fn native_touch_distinguishes_unknown_delivery_from_authenticated_refusal() {
        use crate::transport::daemon::ClientError;

        let producer = captured_producer();

        assert_eq!(
            touch_outcome(
                Err(ClientError::DaemonDown {
                    url: "http://127.0.0.1:7400".into(),
                }),
                &producer
            ),
            SeatTouchOutcome::Retry
        );
        assert_eq!(
            touch_outcome(
                Ok(serde_json::json!({"result":{
                    "presentation_id":producer.presentation_id,
                    "thread_id":producer.thread_id,
                    "touched":true,
                    "unexpected":"not part of the closed acknowledgement"
                }})),
                &producer
            ),
            SeatTouchOutcome::Retry
        );
        assert_eq!(
            touch_outcome(
                Err(ClientError::DaemonError {
                    path: "/ui/api/session/seat/touch".into(),
                    status: 503,
                    message: "outcome unknown".into(),
                }),
                &producer
            ),
            SeatTouchOutcome::Retry
        );
        for status in [408, 429] {
            assert_eq!(
                touch_outcome(
                    Err(ClientError::DaemonError {
                        path: "/ui/api/session/seat/touch".into(),
                        status,
                        message: "retry later".into(),
                    }),
                    &producer,
                ),
                SeatTouchOutcome::Retry
            );
        }
        assert_eq!(
            touch_outcome(
                Err(ClientError::DaemonError {
                    path: "/ui/api/session/seat/touch".into(),
                    status: 409,
                    message: "producer fenced".into(),
                }),
                &producer
            ),
            SeatTouchOutcome::Refused
        );
        assert_eq!(
            touch_outcome(
                Ok(serde_json::json!({"result":{
                    "presentation_id":producer.presentation_id,
                    "thread_id":producer.thread_id,
                    "touched":true
                }})),
                &producer
            ),
            SeatTouchOutcome::Renewed
        );
        assert_eq!(
            touch_outcome(
                Ok(serde_json::json!({"result":{
                    "presentation_id":producer.presentation_id,
                    "thread_id":"another-seat",
                    "touched":true
                }})),
                &producer
            ),
            SeatTouchOutcome::Retry
        );
        assert_eq!(
            touch_outcome(
                Ok(serde_json::json!({"result":{
                    "presentation_id":producer.presentation_id,
                    "thread_id":producer.thread_id
                }})),
                &producer
            ),
            SeatTouchOutcome::Retry
        );
    }

    #[test]
    fn native_ack_requires_the_exact_authoritative_durable_interval() {
        let producer = captured_producer();
        let captured = facets(1);
        let body = append_body(&producer, "root", 0, &captured, 65_536).unwrap();
        let mut acknowledgement = serde_json::json!({
            "events_authority":"authoritative",
            "chain_seq":"4",
            "receipt_thread_seq":"4",
            "receipt_event_hash":"cd".repeat(32),
            "events":[{
                "event_hash":"ab".repeat(32),
                "chain_root_id":"root",
                "chain_seq":3,
                "thread_id":producer.thread_id,
                "thread_seq":3,
                "event_type":body["events"][0]["event_type"],
                "durability":"durable",
                "ts":"2026-10-06T00:00:00Z",
                "prev_chain_event_hash":null,
                "prev_thread_event_hash":null,
                "payload":{"seq":0,"payload":body["events"][0]["payload"]}
            }]
        });
        assert!(acknowledged_append_interval(
            &acknowledgement,
            &body,
            "root"
        ));
        acknowledgement["events"][0]["chain_seq"] = serde_json::json!("3");
        acknowledgement["events"][0]["thread_seq"] = serde_json::json!("3");
        let original = acknowledged_canonical_events(&acknowledgement).unwrap();
        assert_eq!(original.len(), 1);
        assert_eq!(original[0].event.seq, 3);
        assert_eq!(original[0].event.producer_engine_seq, Some(0));
        assert_eq!(original[0].evidence.chain_seq, 3);
        assert_eq!(original[0].evidence.event_hash, "ab".repeat(32));
        assert_eq!(original[0].thread_id, producer.thread_id);
        acknowledgement["events"][0]["prev_chain_event_hash"] = serde_json::json!("cd".repeat(32));
        acknowledgement["events"][0]["prev_thread_event_hash"] = serde_json::json!("ef".repeat(32));
        assert!(acknowledged_append_interval(
            &acknowledgement,
            &body,
            "root"
        ));
        acknowledgement["events"][0]["prev_chain_event_hash"] = serde_json::json!("bad");
        assert!(!acknowledged_append_interval(
            &acknowledgement,
            &body,
            "root"
        ));
        acknowledgement["events"][0]["prev_chain_event_hash"] = serde_json::json!("cd".repeat(32));
        acknowledgement["events"][0]["chain_root_id"] = serde_json::json!("foreign");
        assert!(!acknowledged_append_interval(
            &acknowledgement,
            &body,
            "root"
        ));
        acknowledgement["events"][0]["chain_root_id"] = serde_json::json!("root");
        acknowledgement["events"][0]["payload"]["payload"] = serde_json::json!({"changed":true});
        assert!(!acknowledged_append_interval(
            &acknowledgement,
            &body,
            "root"
        ));
    }

    #[test]
    fn native_append_refuses_extra_ack_fields_without_clearing_the_captured_prefix() {
        let producer = captured_producer();
        let captured = facets(1);
        let body = append_body(&producer, "root", 0, &captured, 65_536).unwrap();
        let acknowledgement = serde_json::json!({
            "producer_incarnation":body["producer_incarnation"],
            "operation_id":body["operation_id"],
            "first_engine_seq":"0",
            "last_engine_seq":"0",
            "event_count":body["event_count"],
            "payload_digest":body["payload_digest"],
            "appended":body["event_count"],
            "events_authority":"authoritative",
            "chain_seq":"4",
            "receipt_thread_seq":"4",
            "receipt_event_hash":"cd".repeat(32),
            "events":[{
                "event_hash":"ab".repeat(32),
                "chain_root_id":"root",
                "chain_seq":3,
                "thread_id":producer.thread_id,
                "thread_seq":3,
                "event_type":body["events"][0]["event_type"],
                "durability":"durable",
                "ts":"2026-10-06T00:00:00Z",
                "prev_chain_event_hash":null,
                "prev_thread_event_hash":null,
                "payload":{"seq":0,"payload":body["events"][0]["payload"]}
            }]
        });
        let mut delivery = SeatDelivery::default();
        delivery
            .retain_exact_append(body.clone(), &captured)
            .unwrap();
        let mut expanded = acknowledgement.clone();
        expanded["unexpected"] = serde_json::json!("ambiguous extension");
        assert_eq!(
            append_reply_action(
                &delivery,
                &Ok(serde_json::json!({"result":expanded.clone()})),
                &body,
                "root",
            ),
            AppendReplyAction::RetryExact
        );
        assert_eq!(delivery.pending.as_ref(), Some(&body));
        assert!(!delivery.blocked);
        assert_eq!(
            append_reply_action(
                &delivery,
                &Ok(serde_json::json!({"result":acknowledgement.clone()})),
                &body,
                "root",
            ),
            AppendReplyAction::Acknowledge
        );
        assert_eq!(
            append_reply_action(
                &delivery,
                &Err(crate::transport::daemon::ClientError::DaemonError {
                    path: "/ui/api/session/seat/append".into(),
                    status: 403,
                    message: "definite refusal".into(),
                }),
                &body,
                "root",
            ),
            AppendReplyAction::Refused
        );
        assert!(!accept_append_acknowledgement(
            &mut delivery,
            &expanded,
            &body,
            "root"
        ));
        assert!(delivery.blocked);
        assert_eq!(delivery.pending.as_ref(), Some(&body));
        assert_eq!(
            delivery.pending_events.as_deref(),
            Some(captured.as_slice())
        );
        assert_eq!(delivery.next_engine_seq, 0);

        let mut valid = SeatDelivery::default();
        valid.retain_exact_append(body.clone(), &captured).unwrap();
        assert!(accept_append_acknowledgement(
            &mut valid,
            &acknowledgement,
            &body,
            "root"
        ));
        assert_eq!(valid.next_engine_seq, 1);
        assert!(valid.pending.is_none());

        let mut contradicted = SeatDelivery::default();
        contradicted
            .retain_exact_append(body.clone(), &captured)
            .unwrap();
        contradicted.verified_receipts.push_back((
            body["operation_id"].as_str().unwrap().to_owned(),
            4,
            "ef".repeat(32),
        ));
        assert!(!accept_append_acknowledgement(
            &mut contradicted,
            &acknowledgement,
            &body,
            "root"
        ));
        assert!(contradicted.blocked);
        assert_eq!(contradicted.pending.as_ref(), Some(&body));
    }

    #[test]
    fn native_retry_keeps_local_prefix_identity_even_when_wire_payload_matches() {
        let producer = captured_producer();
        let captured = facets(1);
        let original = append_body(&producer, "root", 0, &captured, 65_536).unwrap();
        let mut delivery = SeatDelivery::default();
        delivery
            .retain_exact_append(original.clone(), &captured)
            .unwrap();
        let mut changed = captured.clone();
        changed[0].seq += 1;
        assert_eq!(
            append_body(&producer, "root", 0, &changed, 65_536).unwrap(),
            original
        );
        assert!(
            delivery
                .retain_exact_append(original.clone(), &changed)
                .is_err()
        );
        assert_eq!(delivery.pending, Some(original));
        assert_eq!(delivery.pending_events, Some(captured));
        assert!(delivery.blocked);
    }

    #[test]
    fn native_bounded_request_refuses_exhaustion_and_oversize_without_rewriting_events() {
        let producer = captured_producer();
        let events = facets(2);
        let before = events.clone();
        assert!(append_body(&producer, "root", u64::MAX, &events[..1], 65_536).is_err());
        assert!(append_body(&producer, "root", u64::MAX - 1, &events, 65_536).is_err());
        let last = append_body(&producer, "root", u64::MAX - 1, &events[..1], 65_536).unwrap();
        assert_eq!(last["last_engine_seq"].as_u64(), Some(u64::MAX - 1));
        assert!(append_body(&producer, "root", 0, &events, 1).is_err());
        assert_eq!(events, before);
    }

    #[test]
    fn signed_append_bound_selects_only_a_captured_local_prefix() {
        let producer = ryeos_client_base::ui::binding::UiPresentationProducer {
            presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            thread_id: "T-seat".into(),
            producer_incarnation: "a".repeat(64),
        };
        let events = (0..2)
            .map(|seq| SeatEvent {
                producer_engine_seq: None,
                seq: 91 + seq,
                kind: SeatEventKind::Facet {
                    key: "draft".into(),
                    value: serde_json::json!("x".repeat(1100)),
                },
            })
            .collect::<Vec<_>>();
        let before = serde_json::to_value(&events).unwrap();
        assert_eq!(append_prefix_len(&events, &producer, 2000), 1);
        assert_eq!(append_prefix_len(&events, &producer, 65536), 2);
        assert_eq!(append_prefix_len(&events, &producer, 1), 0);
        assert_eq!(serde_json::to_value(&events).unwrap(), before);
    }
    use serde_json::json;

    #[test]
    fn replay_parser_accepts_persisted_seat_facet_shape() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "seat.facet",
            "payload": {
                "seq": 2,
                "payload": {
                    "key": "selection",
                    "value": { "item": "thread-1" }
                }
            }
        });

        let seat_event = seat_event_from_replay(&event).expect("seat event");

        assert_eq!(seat_event.seq, 4);
        assert_eq!(seat_event.producer_engine_seq, Some(2));
        assert_eq!(
            seat_event.kind,
            SeatEventKind::Facet {
                key: "selection".to_string(),
                value: json!({ "item": "thread-1" }),
            }
        );
    }

    #[test]
    fn replay_parser_ignores_non_seat_events() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "thread.started",
            "payload": {}
        });

        assert!(seat_event_from_replay(&event).is_none());
    }

    #[test]
    fn replay_parser_refuses_missing_chain_position_and_unwrapped_producer_event() {
        let mut event = json!({"event_type":"seat.facet","payload":{"seq":2,"payload":{"key":"selection","value":null}}});
        assert!(ryeos_client_base::ui::seat::retained_seat_event(&event).is_err());
        event["chain_seq"] = json!(4);
        event["payload"] = json!({"key":"selection","value":null});
        assert!(ryeos_client_base::ui::seat::retained_seat_event(&event).is_err());
    }

    #[test]
    fn replay_parser_preserves_completed_operation_and_unique_braid_position() {
        let completed =
            ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation {
                operation_id: "12345678-1234-8123-8123-123456789abc".into(),
                result: json!({"surface_instance_id":"retained-surface"}),
            };
        let kind = SeatEventKind::PresentationOperationCompleted(completed);
        let wire = serde_json::to_value(&kind).unwrap();
        let event = json!({
            "chain_seq": 91,
            "event_type": wire["event_type"],
            "payload": {"seq": 2, "payload": wire["payload"]},
        });
        let decoded = seat_event_from_replay(&event).unwrap();
        assert_eq!(decoded.seq, 91);
        assert_eq!(decoded.producer_engine_seq, Some(2));
        assert_eq!(decoded.kind, kind);
    }

    #[test]
    fn braid_serialization_does_not_rewrite_captured_local_prefix() {
        let captured = vec![SeatEvent {
            producer_engine_seq: None,
            seq: 7,
            kind: SeatEventKind::Facet {
                key: "selection".into(),
                value: json!({"thread":"T-a"}),
            },
        }];
        let before = captured.clone();
        let wire = braid_batch(&captured).unwrap();
        assert_eq!(captured, before);
        assert_eq!(wire[0]["event_type"], "seat.facet");
        assert!(wire[0].get("engine_seq").is_none());
    }

    #[test]
    fn seat_binding_coordinate_serializes_as_flat_exact_triple() {
        let coordinate = SeatBindingCoordinate {
            binding_attachment_id: "attachment-7".to_string(),
            binding_generation: 4,
            binding_digest: "digest-7".to_string(),
            max_seat_append_bytes: 2000,
        };

        assert_eq!(
            serde_json::to_value(coordinate).expect("serialize seat coordinate"),
            json!({
                "binding_attachment_id": "attachment-7",
                "binding_generation": 4,
                "binding_digest": "digest-7",
            })
        );
    }
}
