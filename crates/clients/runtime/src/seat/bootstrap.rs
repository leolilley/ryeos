//! Claiming a seat and proving its history: fresh claim, resume, takeover,
//! same-producer reproof and in-place session replacement.
//!
//! The order never changes: observe the control predecessor, claim with one
//! frozen request (resent byte-identically, never re-derived), then read
//! `seat/history` to its completion fence and keep that same stream as the
//! live handoff. Only after the fence does a producer become current. A fixed
//! anchor can be overtaken while a proof is read; each of up to
//! [`MAX_COLD_REPLAY_ATTEMPTS`] attempts discards the provisional state,
//! re-reads the session and proves the same claimed seat again. A claim is
//! never repeated for that.

use std::rc::Rc;

use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::seat::RetainedSeatEvent;
use ryeos_client_base::ui::{
    AuthenticatedReproofGuard, BrowserSession, BrowserViewport, UiBindingAttachmentCoordinate,
    UiObservedPresentationProducer, UiSeatControlPredecessor, UiSeatControlRequest,
    UiSeatProducerClaim,
};
use serde_json::{Value, json};

use crate::admission::{
    AdmissionTarget, SeatBinding, check_admitted_session, decode_session, exact_membership,
    retained_seat_binding, root_seat_binding, same_session_record,
};
use crate::api::{MAX_COLD_REPLAY_ATTEMPTS, SEAT_OPEN_PATH, SESSION_CURRENT_PATH, safe_bound};
use crate::error::RuntimeError;
use crate::history::{
    HistoryClaim, HistoryCompletion, HistoryError, LiveHistory, SeatCoordinate, read_history,
};
use crate::platform::{Auth, Clock, Platform, Transport};
use crate::replay::{ColdReplayCandidate, CoreConfig, FreshSeatSink};

use super::ack::unwrap_result;
use super::control::{control_post, encode_control, observe_producer, relinquish_claimed_seat};
use super::{Frames, LiveSeatHandoff, SeatShared, claim_writer, is_lower_hex_64, random_uuid};

fn fatal(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Fatal(message.into())
}

/// How a retained presentation is claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainedClaim {
    /// A first claim of a prepared presentation nobody has claimed.
    Fresh,
    /// Resume the writer that relinquished, observed at claim time.
    ResumeRelinquished,
    /// Resume a relinquished writer the operator already saw.
    Resume {
        expected_producer_incarnation: String,
        predecessor: UiSeatControlPredecessor,
    },
    /// Replace the writer the operator already saw.
    Takeover {
        expected_producer_incarnation: String,
        predecessor: UiSeatControlPredecessor,
    },
}

/// What a start claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatStart {
    /// Prepare a new presentation under the session's authored root and claim
    /// it. The proof must be exactly the initialization and the claim.
    Fresh,
    /// Claim a presentation the session already holds, and replay it.
    Retained {
        presentation_id: String,
        claim: RetainedClaim,
    },
}

/// The identity of one claim attempt, frozen so a retry is exact: the same
/// preparation, the same nonce and, once built, the same request bytes.
#[derive(Debug, Clone)]
pub struct ClaimIntent {
    creation_request_id: String,
    claim_nonce: String,
    prepared: Option<SeatCoordinate>,
    request: Option<UiSeatControlRequest>,
}

impl ClaimIntent {
    /// A new intent with fresh randomness.
    pub fn new(platform: &impl Platform) -> Self {
        Self {
            creation_request_id: random_uuid(platform),
            claim_nonce: random_uuid(platform),
            prepared: None,
            request: None,
        }
    }

    /// The seat prepared or selected so far, if any.
    pub fn prepared(&self) -> Option<&SeatCoordinate> {
        self.prepared.as_ref()
    }
}

/// What replay produced.
pub enum ReplayOutput {
    /// A fresh claim's proof: the presentation's initialization row.
    Fresh {
        initialization: Box<RetainedSeatEvent>,
    },
    /// A retained presentation's private core, awaiting admission.
    Cold { candidate: Box<ColdReplayCandidate> },
}

/// An acknowledged claim and its completed proof.
pub struct Claimed<P: Platform> {
    pub coordinate: SeatCoordinate,
    pub producer: UiPresentationProducer,
    pub next_engine_seq: u64,
    pub root: UiBindingAttachmentCoordinate,
    pub max_seat_append_bytes: u64,
    pub claim_request: UiSeatControlRequest,
    pub completion: HistoryCompletion,
    /// The stream whose cold completion admitted this seat.
    pub history: LiveHistory<Frames<P>>,
    pub output: ReplayOutput,
}

/// Read and decode `session/current`.
pub async fn read_session<P: Platform>(
    platform: &P,
) -> Result<(Value, BrowserSession), RuntimeError> {
    let text = platform.transport().get(SESSION_CURRENT_PATH).await?;
    decode_session(&text)
}

fn history_error(error: HistoryError) -> RuntimeError {
    match error {
        HistoryError::Transport(error) => error,
        other => RuntimeError::Fatal(other.to_string()),
    }
}

async fn give_back<P: Platform>(
    platform: &P,
    producer: &UiPresentationProducer,
    coordinate: &SeatCoordinate,
    root: &UiBindingAttachmentCoordinate,
) {
    // Best effort: an uncertain network result is never taken as evidence
    // that the seat was released, and nothing here can report it.
    let _ = relinquish_claimed_seat(platform, producer, coordinate, root).await;
}

async fn prepare_fresh<P: Platform>(
    platform: &P,
    intent: &ClaimIntent,
    binding: &SeatBinding,
) -> Result<SeatCoordinate, RuntimeError> {
    let body = encode_control(&json!({
        "action": "prepare",
        "creation_request_id": intent.creation_request_id,
        "binding_attachment_id": binding.coordinate.binding_attachment_id,
        "binding_generation": binding.coordinate.binding_generation,
        "binding_digest": binding.coordinate.binding_digest,
    }))?;
    let response = control_post(platform, SEAT_OPEN_PATH, &body).await?;
    let prepared = unwrap_result(&response).map_err(|error| fatal(error.0))?;
    if prepared.get("root_binding") != Some(&serde_json::to_value(&binding.coordinate)?) {
        return Err(fatal(
            "prepared presentation root does not match requested binding",
        ));
    }
    let text = |field: &str| {
        prepared
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| fatal(format!("prepare omitted {field}")))
    };
    Ok(SeatCoordinate {
        presentation_id: text("presentation_id")?,
        thread_id: text("thread_id")?,
        chain_root_id: text("chain_root_id")?,
    })
}

async fn build_claim_request<P: Platform>(
    platform: &P,
    intent: &ClaimIntent,
    coordinate: &SeatCoordinate,
    binding: &SeatBinding,
    start: &SeatStart,
) -> Result<UiSeatControlRequest, RuntimeError> {
    let (predecessor, claim) = match start {
        SeatStart::Retained {
            claim:
                RetainedClaim::Resume {
                    expected_producer_incarnation,
                    predecessor,
                },
            ..
        } => (
            predecessor.clone(),
            UiSeatProducerClaim::Resume {
                expected_producer_incarnation: expected_producer_incarnation.clone(),
            },
        ),
        SeatStart::Retained {
            claim:
                RetainedClaim::Takeover {
                    expected_producer_incarnation,
                    predecessor,
                },
            ..
        } => (
            predecessor.clone(),
            UiSeatProducerClaim::Takeover {
                expected_producer_incarnation: expected_producer_incarnation.clone(),
            },
        ),
        other => {
            let observation = observe_producer(platform, coordinate, &binding.coordinate).await?;
            let claim = match other {
                SeatStart::Retained {
                    claim: RetainedClaim::ResumeRelinquished,
                    ..
                } => match &observation.producer {
                    UiObservedPresentationProducer::Relinquished {
                        producer_incarnation,
                    } if is_lower_hex_64(producer_incarnation) => UiSeatProducerClaim::Resume {
                        expected_producer_incarnation: producer_incarnation.clone(),
                    },
                    _ => {
                        return Err(fatal(
                            "native Resume requires an exact Relinquished predecessor",
                        ));
                    }
                },
                SeatStart::Fresh => {
                    if !matches!(
                        observation.producer,
                        UiObservedPresentationProducer::Unclaimed
                    ) {
                        return Err(fatal(
                            "fresh terminal claim requires its exact unclaimed initialization",
                        ));
                    }
                    UiSeatProducerClaim::Fresh
                }
                SeatStart::Retained { .. } => UiSeatProducerClaim::Fresh,
            };
            (observation.control_predecessor, claim)
        }
    };
    if matches!(
        claim,
        UiSeatProducerClaim::Resume { .. } | UiSeatProducerClaim::Takeover { .. }
    ) {
        let (UiSeatProducerClaim::Resume {
            expected_producer_incarnation,
        }
        | UiSeatProducerClaim::Takeover {
            expected_producer_incarnation,
        }) = &claim
        else {
            unreachable!("matched above");
        };
        if !is_lower_hex_64(expected_producer_incarnation) {
            return Err(fatal(
                "reconnect requires an exact predecessor producer incarnation",
            ));
        }
        if matches!(predecessor, UiSeatControlPredecessor::Initialization { .. }) {
            return Err(fatal(
                "replacement requires an observed control predecessor",
            ));
        }
    }
    let mut request = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: coordinate.presentation_id.clone(),
        thread_id: coordinate.thread_id.clone(),
        chain_root_id: coordinate.chain_root_id.clone(),
        predecessor,
        intent_nonce: intent.claim_nonce.clone(),
        claim,
    };
    request.assign_identity().map_err(fatal)?;
    Ok(request)
}

/// Claim a seat and prove its history to the completion fence.
///
/// `session` is the authenticated session the start was admitted under.
/// Nothing is published as current until the proof completes and the delivery
/// state is installed; a failed fresh claim hands the seat back, a failed
/// retained one leaves it to the daemon's lease lifecycle.
pub async fn claim_and_replay<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    intent: &mut ClaimIntent,
    session: &BrowserSession,
    viewport: BrowserViewport,
    start: &SeatStart,
    config: CoreConfig,
) -> Result<Claimed<P>, RuntimeError> {
    let platform = &*shared.platform;
    let binding: SeatBinding = match start {
        SeatStart::Fresh => root_seat_binding(session)?,
        SeatStart::Retained {
            presentation_id, ..
        } => retained_seat_binding(session, presentation_id)?,
    };
    safe_bound(binding.max_seat_append_bytes, "max_seat_append_bytes")
        .map_err(|_| fatal("signed seat append byte bound must be positive"))?;
    {
        let delivery = shared.delivery.borrow();
        if delivery.pending.is_some() || delivery.blocked {
            return Err(fatal(
                "unresolved append prevents preparing another presentation",
            ));
        }
    }

    // Which seat.
    let existing = intent.prepared.clone();
    let coordinate = match (existing, start) {
        (Some(coordinate), _) => coordinate,
        (None, SeatStart::Fresh) => {
            let coordinate = prepare_fresh(platform, intent, &binding).await?;
            intent.prepared = Some(coordinate.clone());
            coordinate
        }
        (
            None,
            SeatStart::Retained {
                presentation_id, ..
            },
        ) => {
            let membership = exact_membership(session, presentation_id)?;
            let (Some(thread_id), Some(chain_root_id)) =
                (&membership.thread_id, &membership.chain_root_id)
            else {
                return Err(fatal(
                    "retained membership has no seat thread or chain root",
                ));
            };
            let coordinate = SeatCoordinate {
                presentation_id: presentation_id.clone(),
                thread_id: thread_id.clone(),
                chain_root_id: chain_root_id.clone(),
            };
            intent.prepared = Some(coordinate.clone());
            coordinate
        }
    };

    // The one frozen claim request.
    let request = match &intent.request {
        Some(request) => request.clone(),
        None => {
            let request =
                build_claim_request(platform, intent, &coordinate, &binding, start).await?;
            intent.request = Some(request.clone());
            request
        }
    };
    let operation_id = request.operation_id().to_owned();
    let body = encode_control(&serde_json::to_value(&request)?)?;

    // Claim. A malformed 2xx can follow a committed claim: resend only the
    // same operation, never infer an owner from a presentation id.
    let mut acknowledged = None;
    for attempt in 0..3_u32 {
        let envelope = control_post(platform, SEAT_OPEN_PATH, &body).await?;
        let candidate = unwrap_result(&envelope).map_err(|error| fatal(error.0))?;
        let text = |field: &str| candidate.get(field).and_then(Value::as_str);
        if text("presentation_id") == Some(coordinate.presentation_id.as_str())
            && text("thread_id") == Some(coordinate.thread_id.as_str())
            && text("chain_root_id") == Some(coordinate.chain_root_id.as_str())
            && text("operation_id") == Some(operation_id.as_str())
            && text("state") == Some("active")
        {
            acknowledged = Some(candidate.clone());
            break;
        }
        if attempt < 2 {
            platform
                .clock()
                .sleep(std::time::Duration::from_millis(100 << attempt))
                .await;
        }
    }
    let claimed = acknowledged.ok_or_else(|| {
        fatal("claim outcome unresolved: no exact acknowledgement for the claim operation")
    })?;
    let (incarnation, next) = match claim_writer(&claimed) {
        Ok(writer) => writer,
        Err(message) => {
            // The exact operation was acknowledged. A canonical incarnation
            // still identifies the owner even with a malformed frontier;
            // without one, ownership is unknown and is not guessed.
            if let Some(incarnation) = claimed
                .get("producer_incarnation")
                .and_then(Value::as_str)
                .filter(|incarnation| is_lower_hex_64(incarnation))
            {
                let owner = UiPresentationProducer {
                    presentation_id: coordinate.presentation_id.clone(),
                    thread_id: coordinate.thread_id.clone(),
                    producer_incarnation: incarnation.to_owned(),
                };
                give_back(platform, &owner, &coordinate, &binding.coordinate).await;
            }
            return Err(fatal(message));
        }
    };
    let producer = UiPresentationProducer {
        presentation_id: coordinate.presentation_id.clone(),
        thread_id: coordinate.thread_id.clone(),
        producer_incarnation: incarnation,
    };
    let fresh = matches!(start, SeatStart::Fresh);
    if fresh && next != 0 {
        give_back(platform, &producer, &coordinate, &binding.coordinate).await;
        return Err(fatal(
            "fresh terminal claim unexpectedly contains retained history; use explicit retained-seat Resume instead",
        ));
    }

    // Prove. Each attempt starts from a re-read session and a fresh stream.
    let history_claim = HistoryClaim {
        operation_id: operation_id.clone(),
        producer_incarnation: producer.producer_incarnation.clone(),
        next_engine_seq: next,
        request: Some(request.clone()),
    };
    let proven = prove(
        shared,
        session,
        &coordinate,
        &binding,
        &history_claim,
        viewport,
        start,
        intent,
        config,
    )
    .await;
    let (history, output, completion) = match proven {
        Ok(proven) => proven,
        Err(error) => {
            if fresh {
                give_back(platform, &producer, &coordinate, &binding.coordinate).await;
            }
            return Err(error);
        }
    };

    let unresolved = {
        let delivery = shared.delivery.borrow();
        delivery.pending.is_some() || delivery.blocked
    };
    if unresolved {
        if fresh {
            give_back(platform, &producer, &coordinate, &binding.coordinate).await;
        }
        return Err(fatal("unresolved append prevents producer replacement"));
    }
    {
        let mut delivery = shared.delivery.borrow_mut();
        let key = shared.retention_key_for(&session.session_id, &coordinate.presentation_id);
        delivery.reset_for(
            coordinate.clone(),
            &session.session_id,
            binding.coordinate.clone(),
            key,
            next,
            binding.max_seat_append_bytes,
        );
    }
    *shared.relinquishing.borrow_mut() = None;
    *shared.producer.borrow_mut() = Some(producer.clone());
    Ok(Claimed {
        coordinate,
        producer,
        next_engine_seq: next,
        root: binding.coordinate,
        max_seat_append_bytes: binding.max_seat_append_bytes,
        claim_request: request,
        completion,
        history,
        output,
    })
}

#[allow(clippy::type_complexity)]
async fn prove<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    session: &BrowserSession,
    coordinate: &SeatCoordinate,
    binding: &SeatBinding,
    claim: &HistoryClaim,
    viewport: BrowserViewport,
    start: &SeatStart,
    intent: &ClaimIntent,
    config: CoreConfig,
) -> Result<(LiveHistory<Frames<P>>, ReplayOutput, HistoryCompletion), RuntimeError> {
    let platform = &*shared.platform;
    for attempt in 0..MAX_COLD_REPLAY_ATTEMPTS {
        let (_, observed) = read_session(platform).await?;
        if observed.session_id != session.session_id {
            return Err(fatal("session changed after the claim"));
        }
        let outcome = match start {
            SeatStart::Retained {
                presentation_id, ..
            } => {
                let before = exact_membership(session, presentation_id)?;
                let after = exact_membership(&observed, presentation_id)
                    .map_err(|_| fatal("membership changed after the claim"))?;
                if before != after {
                    return Err(fatal("membership changed after the claim"));
                }
                let mut candidate = ColdReplayCandidate::begin(
                    observed.clone(),
                    presentation_id,
                    viewport,
                    shared.platform.clock().now_ms(),
                    config,
                )
                .map_err(fatal)?;
                read_history(platform.transport(), coordinate, claim, &mut candidate)
                    .await
                    .map(|live| {
                        (
                            live,
                            ReplayOutput::Cold {
                                candidate: Box::new(candidate),
                            },
                        )
                    })
            }
            SeatStart::Fresh => {
                check_admitted_session(
                    &observed,
                    &AdmissionTarget {
                        session_id: &session.session_id,
                        seat: coordinate,
                        root: &binding.coordinate,
                    },
                )?;
                let mut sink = FreshSeatSink::new(
                    &coordinate.presentation_id,
                    &coordinate.chain_root_id,
                    &coordinate.thread_id,
                    &intent.creation_request_id,
                    &binding.coordinate,
                )
                .map_err(fatal)?;
                match read_history(platform.transport(), coordinate, claim, &mut sink).await {
                    Ok(live) => sink
                        .into_initialization()
                        .map(|initialization| {
                            (
                                live,
                                ReplayOutput::Fresh {
                                    initialization: Box::new(initialization),
                                },
                            )
                        })
                        .map_err(HistoryError::Refused),
                    Err(error) => Err(error),
                }
            }
        };
        match outcome {
            Ok((live, output)) => {
                let completion = live.completion().clone();
                return Ok((live, output, completion));
            }
            Err(HistoryError::Incomplete(_)) if attempt + 1 < MAX_COLD_REPLAY_ATTEMPTS => {}
            Err(HistoryError::Incomplete(_)) => {
                return Err(fatal(
                    "seat history ended before a completed proof after three attempts",
                ));
            }
            Err(error) => return Err(history_error(error)),
        }
    }
    Err(fatal("seat history proof loop ended without an outcome"))
}

/// A completed same-producer reproof, awaiting promotion.
pub struct ReproofOutput<P: Platform> {
    pub candidate: ColdReplayCandidate,
    pub completion: HistoryCompletion,
    pub history: LiveHistory<Frames<P>>,
}

/// Observe a new fixed cut under the same acknowledged claim. This never
/// claims, resumes or relinquishes, and never publishes its private core.
pub async fn reproof_seat<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    guard: AuthenticatedReproofGuard,
    producer: &UiPresentationProducer,
    claim: &UiSeatControlRequest,
    claim_next_engine_seq: u64,
    authenticated: &BrowserSession,
    viewport: BrowserViewport,
    config: CoreConfig,
) -> Result<ReproofOutput<P>, RuntimeError> {
    let platform = &*shared.platform;
    let membership = authenticated
        .presentations
        .iter()
        .find(|membership| {
            membership.presentation_id == producer.presentation_id
                && membership.thread_id.as_deref() == Some(producer.thread_id.as_str())
        })
        .ok_or_else(|| fatal("reproof exact current membership absent"))?;
    let chain_root_id = membership
        .chain_root_id
        .clone()
        .ok_or_else(|| fatal("reproof root absent"))?;
    {
        let delivery = shared.delivery.borrow();
        if delivery.pending.is_some() || delivery.blocked {
            return Err(fatal("unresolved append prevents reproof"));
        }
    }
    if !shared.is_current(producer) {
        return Err(fatal("reproof current producer changed"));
    }
    let coordinate = SeatCoordinate {
        presentation_id: producer.presentation_id.clone(),
        thread_id: producer.thread_id.clone(),
        chain_root_id,
    };
    let history_claim = HistoryClaim {
        operation_id: claim.operation_id().to_owned(),
        producer_incarnation: producer.producer_incarnation.clone(),
        next_engine_seq: claim_next_engine_seq,
        request: Some(claim.clone()),
    };
    let same_authority = |observed: &BrowserSession| {
        observed.session_id == authenticated.session_id
            && exact_membership(observed, &producer.presentation_id)
                .is_ok_and(|current| current == membership)
    };
    for attempt in 0..MAX_COLD_REPLAY_ATTEMPTS {
        let (_, observed) = read_session(platform).await?;
        if !same_authority(&observed) {
            return Err(fatal("reproof session membership changed"));
        }
        let mut candidate = ColdReplayCandidate::begin(
            observed,
            &producer.presentation_id,
            viewport,
            platform.clock().now_ms(),
            config,
        )
        .map_err(fatal)?;
        match read_history(
            platform.transport(),
            &coordinate,
            &history_claim,
            &mut candidate,
        )
        .await
        {
            Ok(history) => {
                let (_, after) = read_session(platform).await?;
                if !same_authority(&after) || !shared.is_current(producer) {
                    return Err(fatal("reproof authority changed before promotion"));
                }
                candidate.set_refresh(guard);
                let completion = history.completion().clone();
                return Ok(ReproofOutput {
                    candidate,
                    completion,
                    history,
                });
            }
            Err(HistoryError::Incomplete(_)) if attempt + 1 < MAX_COLD_REPLAY_ATTEMPTS => {}
            Err(error) => return Err(history_error(error)),
        }
    }
    Err(fatal(
        "same-producer reproof did not complete its fixed cut",
    ))
}

/// Replace the UI session in this process: redeem the successor's launch URL,
/// claim a fresh seat under it, prove its history, re-check admission and
/// leave the claimed stream in the handoff slot for the reducer's turnover.
///
/// The result is the effect's `data`: the successor session, its
/// presentation id and the replayed initialization row.
pub async fn replace_session_in_place<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    expected_session_id: &str,
    launch_url: &str,
    viewport: BrowserViewport,
    config: CoreConfig,
) -> Result<Value, RuntimeError> {
    let platform = &*shared.platform;
    {
        let delivery = shared.delivery.borrow();
        if delivery.pending.is_some() || delivery.blocked {
            return Err(fatal(
                "unresolved seat delivery prevents session replacement",
            ));
        }
    }
    *shared.producer.borrow_mut() = None;
    let established = platform.auth().redeem(launch_url).await?;
    if established.session_id != expected_session_id {
        return Err(fatal("replacement session id differs from the effect's"));
    }
    let redeemed: BrowserSession = serde_json::from_value(established.current)?;
    let mut intent = ClaimIntent::new(platform);
    let claimed = claim_and_replay(
        shared,
        &mut intent,
        &redeemed,
        viewport,
        &SeatStart::Fresh,
        config,
    )
    .await?;
    let producer = claimed.producer.clone();
    if !shared.is_current(&producer) {
        return Err(fatal("replacement producer is not current"));
    }
    let admitted = match read_session(platform).await {
        Ok((_, session)) => session,
        Err(error) => {
            shared.fence(&producer);
            return Err(error);
        }
    };
    let checked = check_admitted_session(
        &admitted,
        &AdmissionTarget {
            session_id: &redeemed.session_id,
            seat: &claimed.coordinate,
            root: &claimed.root,
        },
    );
    if let Err(error) = checked {
        shared.fence(&producer);
        return Err(error);
    }
    let ReplayOutput::Fresh { initialization } = claimed.output else {
        shared.fence(&producer);
        return Err(fatal("replacement proof is not a fresh claim"));
    };
    let handoff = LiveSeatHandoff {
        producer: producer.clone(),
        history: claimed.history,
        claim_request: claimed.claim_request,
        claim_next_engine_seq: claimed.next_engine_seq,
    };
    if shared.reserve_handoff(handoff).is_err() {
        shared.fence(&producer);
        return Err(fatal("prior replacement live history was not consumed"));
    }
    Ok(json!({
        "session": serde_json::to_value(&admitted)?,
        "presentation_id": producer.presentation_id,
        "replayed": [serde_json::to_value(&*initialization)?],
    }))
}

/// Structural check used by callers that compare an admitted session with the
/// one a private core was built from.
pub fn session_unchanged(candidate: &ColdReplayCandidate, admitted: &BrowserSession) -> bool {
    candidate
        .session()
        .is_some_and(|built| same_session_record(built, admitted))
}
