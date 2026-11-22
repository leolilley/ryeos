//! Cold replay and same-producer reproof over the shared core.
//!
//! Seat history is never folded into a visible core. A [`ColdReplayCandidate`]
//! owns a private core and a [`ColdSeatReplay`] accumulator; the history
//! consumer stages rows into it, and only an authenticated completion fence
//! turns it into a live core (`commit`) or promotes it over the running core
//! (`commit` with a reproof guard). A failed or truncated stream drops the
//! candidate and nothing is published.
//!
//! A fresh claim has a stricter shape: the stream may contain exactly the
//! presentation's initialization and the claimant's own producer control.
//! [`FreshSeatSink`] checks that and keeps the initialization row.

use ryeos_client_base::ui::binding::{SEAT_PRODUCER_EVENT, UiPresentationProducer, UiSeatProducer};
use ryeos_client_base::ui::cold_replay::ColdSeatReplay;
use ryeos_client_base::ui::seat::{RetainedSeatEvent, retained_seat_event_with_evidence};
use ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame;
use ryeos_client_base::ui::transfer_proof_frame::UiTransferProofFrame;
use ryeos_client_base::ui::{
    AuthenticatedReproofGuard, BrowserSession, BrowserViewport, RyeOsCore, RyeOsEffect, SeatEvent,
    SeatEventKind, UiBindingAttachmentCoordinate,
};
use serde_json::Value;

use crate::codec::exact_u64;
use crate::history::{HistoryCompletion, HistorySink};

/// Per-host switches applied to every core the runtime builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreConfig {
    /// Whether this host can open independent presentation documents.
    pub supports_independent_documents: bool,
}

/// Build a core for `presentation_id` from an authenticated session record.
pub fn new_core(
    session: BrowserSession,
    presentation_id: &str,
    viewport: BrowserViewport,
    now_ms: u64,
    config: CoreConfig,
) -> Result<RyeOsCore, String> {
    if session.ui_binding_contract_revision != ryeos_client_base::UI_BINDING_CONTRACT_REVISION {
        return Err("UI binding contract mismatch during replay".to_owned());
    }
    let mut core = RyeOsCore::new(session, presentation_id, viewport, now_ms);
    core.supports_independent_documents = config.supports_independent_documents;
    Ok(core)
}

/// What a committed candidate produced.
pub enum ReplayCommit {
    /// A new core, ready for producer readiness. `replay_effects` are the
    /// effects registered while finishing the replay; they are published only
    /// after the producer-ready fence, merged with the core's initial effects.
    Cold {
        core: Box<RyeOsCore>,
        replay_effects: Vec<RyeOsEffect>,
    },
    /// The candidate replaced the live core's replayed state; the live
    /// effect owner is preserved and `effects` are its follow-up effects.
    Promoted { effects: Vec<RyeOsEffect> },
}

/// A private core being rebuilt from authenticated history.
pub struct ColdReplayCandidate {
    core: RyeOsCore,
    staged: ColdSeatReplay,
    refresh: Option<AuthenticatedReproofGuard>,
}

impl ColdReplayCandidate {
    /// Begin a cold replay into a new core.
    pub fn begin(
        session: BrowserSession,
        presentation_id: &str,
        viewport: BrowserViewport,
        now_ms: u64,
        config: CoreConfig,
    ) -> Result<Self, String> {
        Ok(Self {
            core: new_core(session, presentation_id, viewport, now_ms, config)?,
            staged: ColdSeatReplay::new(),
            refresh: None,
        })
    }

    /// Turn this candidate into a same-producer refresh. `guard` comes from
    /// the running core's `begin_authenticated_reproof`; the live core is
    /// neither replaced nor reset before authenticated completion.
    pub fn set_refresh(&mut self, guard: AuthenticatedReproofGuard) {
        self.refresh = Some(guard);
    }

    /// The session record the candidate was built from.
    pub fn session(&self) -> Option<&BrowserSession> {
        self.core.data.session.as_ref()
    }

    /// Finish at the authenticated completion fence. For a refresh `live`
    /// must be the running core, which receives the promotion.
    pub fn commit(
        self,
        completion: &HistoryCompletion,
        live: Option<&mut RyeOsCore>,
    ) -> Result<ReplayCommit, String> {
        let Self {
            mut core,
            staged,
            refresh,
        } = self;
        let effects = staged.finish_at_authenticated_tip(
            &mut core,
            &completion.tip_hash,
            completion.thread_seq,
            completion.event_count,
        )?;
        core.require_replayed_initialization()?;
        match (refresh, live) {
            (Some(guard), Some(live)) => {
                let effects = live.promote_authenticated_reproof(core, &guard)?;
                Ok(ReplayCommit::Promoted { effects })
            }
            (Some(_), None) => Err("same-producer reproof has no live core".to_owned()),
            (None, _) => {
                core.bump_generation();
                Ok(ReplayCommit::Cold {
                    core: Box::new(core),
                    replay_effects: effects,
                })
            }
        }
    }
}

impl HistorySink for ColdReplayCandidate {
    fn stage_events(&mut self, records: &[Value]) -> Result<(), String> {
        self.staged
            .append_authenticated_records(&mut self.core, records)
    }

    fn stage_transfer_proof(&mut self, proof: &Value) -> Result<(), String> {
        let proof: UiTransferProofFrame = serde_json::from_value(proof.clone())
            .map_err(|error| format!("invalid paired transfer proof: {error}"))?;
        self.staged.stage_transfer_proof(&self.core, proof)
    }

    fn stage_cancellation_proof(&mut self, proof: &Value) -> Result<(), String> {
        let proof: UiMoveFreezeCancellationProofFrame = serde_json::from_value(proof.clone())
            .map_err(|error| format!("invalid move freeze cancellation proof: {error}"))?;
        self.staged
            .stage_move_freeze_cancellation_proof(&self.core, proof)
    }
}

/// The sink for a fresh claim's stream: exactly the presentation's
/// initialization followed by the claimant's own control, nothing else.
pub struct FreshSeatSink {
    presentation_id: String,
    chain_root_id: String,
    thread_id: String,
    creation_request_id: String,
    root_binding: Value,
    initialization: Option<RetainedSeatEvent>,
    control_seen: bool,
}

impl FreshSeatSink {
    pub fn new(
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        creation_request_id: &str,
        root_binding: &UiBindingAttachmentCoordinate,
    ) -> Result<Self, String> {
        Ok(Self {
            presentation_id: presentation_id.to_owned(),
            chain_root_id: chain_root_id.to_owned(),
            thread_id: thread_id.to_owned(),
            creation_request_id: creation_request_id.to_owned(),
            root_binding: serde_json::to_value(root_binding).map_err(|error| error.to_string())?,
            initialization: None,
            control_seen: false,
        })
    }

    /// The initialization row, once the stream has completed.
    pub fn into_initialization(self) -> Result<RetainedSeatEvent, String> {
        self.initialization
            .ok_or_else(|| "fresh terminal seat has no initialization".to_owned())
    }
}

impl HistorySink for FreshSeatSink {
    fn stage_events(&mut self, records: &[Value]) -> Result<(), String> {
        for event in records {
            if let Some(decoded) = retained_seat_event_with_evidence(event)? {
                let SeatEventKind::PresentationInitialized(record) = &decoded.event.kind else {
                    return Err(
                        "fresh terminal seat contains producer events before bootstrap".to_owned(),
                    );
                };
                record.validate()?;
                let foreign_root = serde_json::to_value(&record.initialization.root_binding)
                    .map_err(|error| error.to_string())?
                    != self.root_binding;
                if self.initialization.is_some()
                    || record.operation_id != self.creation_request_id
                    || record.initialization.presentation_id != self.presentation_id
                    || foreign_root
                {
                    return Err(
                        "fresh terminal initialization has duplicate or foreign ownership"
                            .to_owned(),
                    );
                }
                self.initialization = Some(decoded);
            }
            if event.get("event_type").and_then(Value::as_str) == Some(SEAT_PRODUCER_EVENT) {
                let payload = event
                    .get("payload")
                    .cloned()
                    .ok_or("seat control has no payload")?;
                let control: UiSeatProducer = serde_json::from_value(payload)
                    .map_err(|error| format!("invalid seat control in history: {error}"))?;
                control.validate().map_err(str::to_owned)?;
                control
                    .validate_subject(&self.chain_root_id, &self.thread_id)
                    .map_err(str::to_owned)?;
                if self.initialization.is_none() || self.control_seen {
                    return Err(
                        "fresh terminal seat has missing or changed producer control".to_owned(),
                    );
                }
                self.control_seen = true;
            }
        }
        Ok(())
    }

    fn stage_transfer_proof(&mut self, _proof: &Value) -> Result<(), String> {
        Err("fresh terminal claim cannot contain paired transfer testimony".to_owned())
    }

    fn stage_cancellation_proof(&mut self, _proof: &Value) -> Result<(), String> {
        Err("fresh terminal claim cannot contain paired transfer testimony".to_owned())
    }
}

/// Number of local seat events the core has seen acknowledged.
pub fn acknowledged_prefix_len(core: &RyeOsCore) -> usize {
    core.acknowledged_seat_prefix_len()
}

/// Positive readiness fact: the reducer holds exactly this producer at this
/// frontier. The absence of an error from the readiness event is not enough.
pub fn require_producer_ready(
    core: &RyeOsCore,
    producer: &UiPresentationProducer,
    next_engine_seq: u64,
) -> Result<(), String> {
    core.require_presentation_producer_ready(producer, next_engine_seq)
}

/// Rows of an append acknowledgement as retained events. The daemon sends
/// 64-bit positions as decimal strings in the acknowledgement; the core reads
/// numbers.
pub fn canonical_events_from_ack(rows: &[Value]) -> Result<Vec<RetainedSeatEvent>, String> {
    rows.iter()
        .map(|row| {
            let mut row = row.clone();
            for field in ["chain_seq", "thread_seq"] {
                let position = row
                    .get(field)
                    .and_then(exact_u64)
                    .ok_or_else(|| format!("invalid original seat position for {field}"))?;
                row[field] = Value::from(position);
            }
            let engine_seq = row
                .pointer("/payload/seq")
                .and_then(exact_u64)
                .ok_or("invalid original producer position")?;
            row["payload"]["seq"] = Value::from(engine_seq);
            retained_seat_event_with_evidence(&row)?
                .ok_or_else(|| "acknowledged interval contains a nonproducer row".to_owned())
        })
        .collect()
}

/// Replay an acknowledged interval into the running core, adopting the
/// canonical positions for the captured local prefix.
pub fn replay_acknowledged_events(
    core: &mut RyeOsCore,
    producer: &UiPresentationProducer,
    captured: Vec<SeatEvent>,
    rows: &[Value],
) -> Result<Vec<RyeOsEffect>, String> {
    let canonical = canonical_events_from_ack(rows)?;
    core.acknowledge_canonical_seat_prefix(producer, captured, canonical)
}
