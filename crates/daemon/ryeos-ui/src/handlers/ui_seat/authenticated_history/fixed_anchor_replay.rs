//! Fixed-anchor, bounded forward proof for provisional seat replay.
//!
//! Projection rows are locators only. Each row is reloaded from canonical CAS,
//! linked to the previously verified event, and checked against one signed tip.
//! There is deliberately no client-supplied cursor or omitted-event allowlist.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_client_base::ui::binding::UiPresentationInitialization;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT;
use ryeos_client_base::ui::seat::{SeatEvent, retained_seat_event};
use ryeos_state::AuthoritativeThreadEventReadAnchor;
use ryeos_ui_presentation_contract::seat_control::{
    UiSeatControlPredecessor, UiSeatProducer, UiSeatProducerState, authenticate_control_transition,
};
use ryeos_ui_presentation_contract::transfer::TransferEvidence;

use super::super::{
    MAX_SEAT_APPEND_EVENTS, SEAT_APPEND_RECEIPT_EVENT, SEAT_EVENT_PREFIX, SEAT_PRODUCER_EVENT,
    SeatAppendReceipt,
};
use super::{invalid, persisted, validate_replay_initialization};

const LOCATOR_PAGE_EVENTS: usize = 64;
const LOCATOR_PAGE_BYTES: usize = 8 * 1024 * 1024;
// An acceleration budget, never a retained-history or transfer-count limit.
// A requested older witness absent from this ring requires a fresh bounded
// read; required witnesses are pinned for the duration of that read.
const TRANSFER_PREFIX_WITNESSES: usize = 128;
const MAX_REQUESTED_FREEZE_EVENTS: usize = 128;

/// One exact ordinary event published only after its producer interval and
/// receipt have been authenticated at the requested fixed signed anchor.
#[derive(Clone)]
pub(crate) struct VerifiedFreezeSeatEvent {
    pub(crate) evidence: TransferEvidence,
    pub(crate) event: SeatEvent,
    pub(crate) receipt_evidence: TransferEvidence,
    pub(crate) receipt_operation_id: String,
    pub(crate) receipt_producer_incarnation: String,
    pub(crate) receipt_first_engine_seq: u64,
    pub(crate) receipt_last_engine_seq: u64,
}

/// Only `Complete` proves the full interval. Batches are provisional and must
/// not be applied to a live core, used as absence evidence or mutation authority.
pub(crate) enum FixedAnchorReplayStep {
    Batch {
        events: Vec<PersistedEventRecord>,
    },
    Complete {
        tip_hash: String,
        thread_seq: u64,
    },
    /// Read-only handoff, never a completion or a validated transfer phase.
    RequiresPairedTransfer {
        anchor: AuthoritativeThreadEventReadAnchor,
    },
}

pub(crate) enum LiveSeatReplayAdvance {
    Current,
    Stopped,
    ReproofRequired,
}

fn validate_live_transfer_wake(record: &PersistedEventRecord) -> Result<(), HandlerError> {
    let value = serde_json::to_value(record).map_err(|error| invalid(error.to_string()))?;
    let result = match record.event_type.as_str() {
        ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT => {
            ryeos_ui_presentation_contract::transfer::from_event_value(&value).map(|_| ())
        }
        ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT => {
            ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value).map(|_| ())
        }
        ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT => {
            ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(&value)
                .map(|_| ())
        }
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT => {
            ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(&value)
                .map(|_| ())
        }
        _ => Err("live suffix has unsupported transfer testimony".into()),
    };
    result.map_err(invalid)
}

#[derive(Debug, Clone)]
enum ReplayEndpoint {
    Current(AuthoritativeThreadEventReadAnchor),
    Retired(ryeos_state::RetiredSeatPositivePrefix),
}

impl PartialEq for ReplayEndpoint {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Current(a), Self::Current(b)) => a == b,
            (Self::Retired(a), Self::Retired(b)) => a.same_cut(b),
            _ => false,
        }
    }
}

impl From<AuthoritativeThreadEventReadAnchor> for ReplayEndpoint {
    fn from(anchor: AuthoritativeThreadEventReadAnchor) -> Self {
        Self::Current(anchor)
    }
}

impl ReplayEndpoint {
    fn chain_root_id(&self) -> &str {
        match self {
            Self::Current(a) => a.chain_root_id(),
            Self::Retired(p) => p.chain_root_id(),
        }
    }
    fn thread_id(&self) -> &str {
        match self {
            Self::Current(a) => a.thread_id(),
            Self::Retired(p) => p.chain_root_id(),
        }
    }
    fn tip(&self) -> Option<(&str, u64)> {
        match self {
            Self::Current(a) => a.tip(),
            Self::Retired(p) => p.tip().map(|(hash, seq, _)| (hash, seq)),
        }
    }
    fn current(&self) -> Result<&AuthoritativeThreadEventReadAnchor, HandlerError> {
        match self {
            Self::Current(a) => Ok(a),
            Self::Retired(_) => Err(invalid("retired replay is not a current producer anchor")),
        }
    }
}

/// Scheduling data only: O(history hashes/native positions), not event payloads
/// or authority. Every reverse/forward turn has the ordinary replay page bound.
#[derive(Clone)]
struct RetiredForwardRead {
    progress: Option<ryeos_state::AuthoritativeThreadEventReadProgress>,
    indexed: bool,
    positions: Vec<(String, u64, u64)>,
    remaining: usize,
}

#[derive(Clone)]
pub(crate) struct FixedAnchorReplay {
    anchor: ReplayEndpoint,
    retired_read: Option<RetiredForwardRead>,
    presentation_id: String,
    chain_root_id: String,
    thread_id: String,
    tip_hash: String,
    tip_thread_seq: u64,
    tip_chain_seq: i64,
    after_chain_seq: Option<i64>,
    previous_hash: Option<String>,
    previous_thread_seq: u64,
    initialization_seen: bool,
    control_predecessor: Option<UiSeatControlPredecessor>,
    control: Option<UiSeatProducer>,
    next_engine_seq: u64,
    initialization: Option<UiPresentationInitialization>,
    composition: Option<UiCompositionReplay>,
    pending_producer_events: Vec<PersistedEventRecord>,
    pending_handoff_events: Vec<PersistedEventRecord>,
    awaiting_paired_handoff: bool,
    reached_tip: bool,
    transfer_admission: bool,
    live_suffix: bool,
    admission_initialization_digest: Option<String>,
    control_evidence: Option<TransferEvidence>,
    transfer_testimony_seen: bool,
    transfer_witnesses: VecDeque<TransferEvidence>,
    required_transfer_witnesses: Vec<TransferEvidence>,
    verified_required_witnesses: Vec<TransferEvidence>,
    requested_freeze_events: BTreeMap<String, u64>,
    verified_freeze_events: BTreeMap<String, VerifiedFreezeSeatEvent>,
    cancellation_scope: Option<
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
    >,
    cancellation_seen: bool,
    paired_cut: Option<Arc<super::super::transfer_ingress::FixedTransferCut>>,
    paired_seen: BTreeMap<String, u64>,
}

/// Constructed only by a complete private admission read. This immutable
/// prefix authenticates historical witnesses; it carries no current writer.
pub(crate) struct VerifiedTransferSeatPrefix {
    replay: FixedAnchorReplay,
}

impl VerifiedTransferSeatPrefix {
    pub(in crate::handlers::ui_seat) fn anchor(&self) -> &AuthoritativeThreadEventReadAnchor {
        self.replay
            .anchor
            .current()
            .expect("private admission prefix is current")
    }

    pub(in crate::handlers::ui_seat) fn freeze_event(
        &self,
        evidence: &TransferEvidence,
    ) -> Option<&VerifiedFreezeSeatEvent> {
        (self
            .replay
            .requested_freeze_events
            .get(&evidence.event_hash)
            == Some(&evidence.chain_seq))
        .then(|| self.replay.verified_freeze_events.get(&evidence.event_hash))
        .flatten()
    }

    pub(in crate::handlers::ui_seat) fn history(
        &self,
        initial: &UiPresentationInitialization,
    ) -> Result<super::super::composition::VerifiedHistory, HandlerError> {
        let digest = ryeos_client_base::ui::seat_payload_digest(
            &serde_json::to_value(initial).map_err(|error| invalid(error.to_string()))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        if self.replay.admission_initialization_digest.as_deref() != Some(digest.as_str()) {
            return Err(invalid(
                "transfer proof initialization differs from current membership",
            ));
        }
        self.replay.transfer_admission_history()
    }
}

fn paired_scope_covers_prefix(prefix: &[TransferEvidence], requested: &[TransferEvidence]) -> bool {
    prefix.iter().all(|evidence| requested.contains(evidence))
}

impl FixedAnchorReplay {
    /// The immutable dependency proof actually consumed by this replay, never
    /// a caller-supplied substitute. Publication still checks its full cut.
    pub(crate) fn completed_paired_cut(
        &self,
    ) -> Option<&super::super::transfer_ingress::FixedTransferCut> {
        if !self.reached_tip
            || self.awaiting_paired_handoff
            || !self.pending_producer_events.is_empty()
            || !self.pending_handoff_events.is_empty()
        {
            return None;
        }
        self.paired_cut.as_deref().filter(|cut| {
            cut.completed_root_positions(
                &self.presentation_id,
                &self.chain_root_id,
                &self.thread_id,
                &self.paired_seen,
            )
        })
    }
    pub(crate) fn matches_completed_retired_prefix(
        &self,
        prefix: &ryeos_state::RetiredSeatPositivePrefix,
    ) -> bool {
        matches!(&self.anchor,ReplayEndpoint::Retired(expected) if expected.same_cut(prefix))
            && self.reached_tip
            && !self.awaiting_paired_handoff
            && self.pending_producer_events.is_empty()
            && self.pending_handoff_events.is_empty()
            && self.previous_hash.as_deref() == Some(self.tip_hash.as_str())
            && self.previous_thread_seq == self.tip_thread_seq
            && self.after_chain_seq == Some(self.tip_chain_seq)
            && self
                .retired_read
                .as_ref()
                .is_some_and(|read| read.indexed && read.remaining == 0)
            && self.paired_cut.as_ref().is_none_or(|cut| {
                cut.completed_root_positions(
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.thread_id,
                    &self.paired_seen,
                )
            })
            && self
                .initialization
                .as_ref()
                .and_then(|initial| serde_json::to_value(initial).ok())
                .and_then(|value| ryeos_client_base::ui::seat_payload_digest(&value).ok())
                .as_deref()
                == self.admission_initialization_digest.as_deref()
    }
    /// Bind a completed paired cut to this original signed root anchor. The
    /// cut is private and read-only; individual CAS rows still have to match
    /// its scheduled evidence at their exact forward positions.
    pub(crate) fn attach_paired_cut(
        &mut self,
        cut: Arc<super::super::transfer_ingress::FixedTransferCut>,
    ) -> Result<(), HandlerError> {
        if self.transfer_admission
            || self.paired_cut.is_some()
            || !self.awaiting_paired_handoff
            || !self.pending_producer_events.is_empty()
            || !cut.covers_seat_anchor(
                &self.presentation_id,
                &self.chain_root_id,
                &self.thread_id,
                self.anchor.current()?,
            )
        {
            return Err(invalid("paired replay cut differs from original root seat"));
        }
        self.paired_cut = Some(cut);
        self.awaiting_paired_handoff = false;
        Ok(())
    }

    /// Capture one signed head. A signed head without an event is not a valid
    /// initialized seat and cannot silently become an empty replay.
    pub(crate) fn begin(
        state: &AppState,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Self, HandlerError> {
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(chain_root_id, thread_id)
            .map_err(|error| invalid(error.to_string()))?
            .ok_or_else(|| invalid("seat replay has no signed thread anchor"))?;
        Self::begin_at_anchor(state, presentation_id, chain_root_id, thread_id, anchor)
    }

    pub(crate) fn begin_at_anchor(
        state: &AppState,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        anchor: AuthoritativeThreadEventReadAnchor,
    ) -> Result<Self, HandlerError> {
        Self::begin_at_endpoint(
            state,
            presentation_id,
            chain_root_id,
            thread_id,
            anchor.into(),
        )
    }

    pub(crate) fn begin_at_retired_prefix(
        state: &AppState,
        presentation_id: &str,
        prefix: ryeos_state::RetiredSeatPositivePrefix,
        initialization: &UiPresentationInitialization,
        cut: Option<Arc<super::super::transfer_ingress::FixedTransferCut>>,
    ) -> Result<Self, HandlerError> {
        let root = prefix.chain_root_id().to_owned();
        if let Some(cut) = &cut {
            if !cut.covers_retired_seat_prefix(presentation_id, &root, &prefix) {
                return Err(invalid(
                    "retired replay dependency cut differs from original prefix",
                ));
            }
        }
        let mut replay = Self::begin_at_endpoint(
            state,
            presentation_id,
            &root,
            &root,
            ReplayEndpoint::Retired(prefix),
        )?;
        replay.paired_cut = cut;
        replay.admission_initialization_digest = Some(
            ryeos_client_base::ui::seat_payload_digest(
                &serde_json::to_value(initialization).map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|error| invalid(error.to_string()))?,
        );
        Ok(replay)
    }

    fn begin_at_endpoint(
        state: &AppState,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        anchor: ReplayEndpoint,
    ) -> Result<Self, HandlerError> {
        if anchor.chain_root_id() != chain_root_id || anchor.thread_id() != thread_id {
            return Err(invalid("seat replay anchor belongs to another seat"));
        }
        let (tip_hash, tip_thread_seq) = anchor
            .tip()
            .map(|(hash, sequence)| (hash.to_owned(), sequence))
            .ok_or_else(|| invalid("seat replay has no signed event tip"))?;
        let tip = state
            .state_store
            .retained_thread_event_by_hash(&tip_hash)
            .map_err(|error| invalid(error.to_string()))?
            .ok_or_else(|| invalid("seat replay signed tip is absent from CAS"))?;
        if tip.chain_root_id != chain_root_id
            || tip.thread_id != thread_id
            || tip.thread_seq != tip_thread_seq
        {
            return Err(invalid("seat replay tip differs from signed thread anchor"));
        }
        let retired_read = match &anchor {
            ReplayEndpoint::Current(_) => None,
            ReplayEndpoint::Retired(prefix) => {
                if prefix.tip().map(|(_, _, seq)| seq) != Some(tip.chain_seq) {
                    return Err(invalid(
                        "retired replay native tip differs from marker prefix",
                    ));
                }
                Some(RetiredForwardRead {
                    progress: None,
                    indexed: false,
                    positions: Vec::new(),
                    remaining: 0,
                })
            }
        };
        let tip_chain_seq = i64::try_from(tip.chain_seq)
            .map_err(|_| invalid("seat replay tip chain sequence overflows"))?;
        Ok(Self {
            anchor,
            retired_read,
            presentation_id: presentation_id.to_owned(),
            chain_root_id: chain_root_id.to_owned(),
            thread_id: thread_id.to_owned(),
            tip_hash,
            tip_thread_seq,
            tip_chain_seq,
            after_chain_seq: None,
            previous_hash: None,
            previous_thread_seq: 0,
            initialization_seen: false,
            control_predecessor: None,
            control: None,
            next_engine_seq: 0,
            initialization: None,
            composition: None,
            pending_producer_events: Vec::new(),
            pending_handoff_events: Vec::new(),
            awaiting_paired_handoff: false,
            reached_tip: false,
            transfer_admission: false,
            live_suffix: false,
            admission_initialization_digest: None,
            control_evidence: None,
            transfer_testimony_seen: false,
            transfer_witnesses: VecDeque::new(),
            required_transfer_witnesses: Vec::new(),
            verified_required_witnesses: Vec::new(),
            requested_freeze_events: BTreeMap::new(),
            verified_freeze_events: BTreeMap::new(),
            cancellation_scope: None,
            cancellation_seen: false,
            paired_cut: None,
            paired_seen: BTreeMap::new(),
        })
    }

    /// Private transfer admission uses the app reader's exact supplied anchor.
    /// A cached prefix is usable only when every requested older witness was
    /// already checked at its original event position. Otherwise read genesis
    /// again in bounded slices; final state cannot re-prove an old witness.
    #[cfg(test)]
    pub(crate) fn begin_transfer_admission(
        state: &AppState,
        seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
        anchor: AuthoritativeThreadEventReadAnchor,
        initialization_digest: &str,
        required: &[TransferEvidence],
        seed: Option<Arc<VerifiedTransferSeatPrefix>>,
    ) -> Result<Self, HandlerError> {
        Self::begin_transfer_admission_with_freeze(
            state,
            seat,
            anchor,
            initialization_digest,
            required,
            &[],
            seed,
        )
    }

    /// Cold private replay of one exact retained cancellation. Only the app
    /// owner's completed paired proof can establish this scope; no omitted
    /// event allowlist or aggregate prefix is accepted.
    pub(crate) fn begin_move_freeze_cancellation_admission(
        state: &AppState,
        seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
        initialization_digest: &str,
        proof: Arc<
            ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof,
        >,
    ) -> Result<Self, HandlerError> {
        if !proof.belongs_to(state.state_store.as_ref())
            || seat != &proof.transfer().source
            || proof.retained_cancellation().is_none()
        {
            return Err(invalid(
                "cancellation scope lacks exact completed paired source proof",
            ));
        }
        let links = proof.freeze();
        let mut read = Self::begin_transfer_admission_with_freeze(
            state,
            seat,
            proof.source_anchor().clone(),
            initialization_digest,
            &[],
            &std::iter::once(links.preparation.clone())
                .chain(links.completion.clone())
                .collect::<Vec<_>>(),
            None,
        )?;
        read.cancellation_scope = Some(proof);
        Ok(read)
    }

    /// The freeze coordinates are caller-frozen exact ordinary source events.
    /// A cached transfer-only prefix cannot prove their receipt attribution;
    /// requested freeze reads start from genesis at this same signed anchor.
    pub(crate) fn begin_transfer_admission_with_freeze(
        state: &AppState,
        seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
        anchor: AuthoritativeThreadEventReadAnchor,
        initialization_digest: &str,
        required: &[TransferEvidence],
        freeze_events: &[TransferEvidence],
        seed: Option<Arc<VerifiedTransferSeatPrefix>>,
    ) -> Result<Self, HandlerError> {
        if freeze_events.len() > MAX_REQUESTED_FREEZE_EVENTS
            || freeze_events.iter().any(|evidence| {
                evidence.event_hash.len() != 64
                    || !evidence
                        .event_hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(invalid(
                "requested freeze event inventory is invalid or unbounded",
            ));
        }
        let requested_freeze_events = freeze_events
            .iter()
            .map(|evidence| (evidence.event_hash.clone(), evidence.chain_seq))
            .collect::<BTreeMap<_, _>>();
        if requested_freeze_events.len() != freeze_events.len() {
            return Err(invalid("requested freeze event evidence is repeated"));
        }
        let presentation_id = seat.presentation_id.as_str();
        let chain_root_id = seat.chain_root_id.as_str();
        let thread_id = seat.thread_id.as_str();
        let fresh =
            Self::begin_at_anchor(state, presentation_id, chain_root_id, thread_id, anchor)?;
        let mut read = match seed
            .as_deref()
            .filter(|_| freeze_events.is_empty())
            .map(|prefix| &prefix.replay)
        {
            Some(seed)
                if seed.transfer_admission
                    && seed.cancellation_scope.is_none()
                    && seed.reached_tip
                    && seed.pending_producer_events.is_empty()
                    && seed.presentation_id == presentation_id
                    && seed.chain_root_id == chain_root_id
                    && seed.thread_id == thread_id
                    && seed.admission_initialization_digest.as_deref()
                        == Some(initialization_digest)
                    && paired_scope_covers_prefix(&seed.required_transfer_witnesses, required)
                    && seed.tip_thread_seq <= fresh.tip_thread_seq
                    && (seed.tip_thread_seq != fresh.tip_thread_seq
                        || seed.tip_hash == fresh.tip_hash)
                    && required.iter().all(|evidence| {
                        seed.after_chain_seq
                            .is_none_or(|sequence| evidence.chain_seq > sequence as u64)
                            || seed.transfer_witnesses.contains(evidence)
                    }) =>
            {
                let mut next = seed.clone();
                next.anchor = fresh.anchor;
                next.tip_hash = fresh.tip_hash;
                next.tip_thread_seq = fresh.tip_thread_seq;
                next.tip_chain_seq = fresh.tip_chain_seq;
                next.reached_tip = next.previous_thread_seq == next.tip_thread_seq;
                next
            }
            _ => fresh,
        };
        read.transfer_admission = true;
        read.admission_initialization_digest = Some(initialization_digest.to_owned());
        read.required_transfer_witnesses = required.to_vec();
        read.verified_required_witnesses = required
            .iter()
            .filter(|evidence| read.transfer_witnesses.contains(evidence))
            .cloned()
            .collect();
        read.requested_freeze_events = requested_freeze_events;
        read.verified_freeze_events.clear();
        Ok(read)
    }

    /// Completion grants no session or writer authority. The caller still
    /// rechecks both exact heads and memberships under publication admission.
    pub(crate) fn finish_transfer_admission(
        mut self,
    ) -> Result<Arc<VerifiedTransferSeatPrefix>, HandlerError> {
        if !self.transfer_admission
            || !self.reached_tip
            || !self.pending_producer_events.is_empty()
            || self
                .required_transfer_witnesses
                .iter()
                .any(|evidence| !self.verified_required_witnesses.contains(evidence))
            || self.requested_freeze_events.len() != self.verified_freeze_events.len()
            || self.cancellation_scope.is_some() && !self.cancellation_seen
        {
            return Err(invalid(
                "transfer admission lacks complete event-position proof",
            ));
        }
        let initial = self
            .initialization
            .as_ref()
            .ok_or_else(|| invalid("transfer admission lacks initialization"))?;
        let digest = ryeos_client_base::ui::seat_payload_digest(
            &serde_json::to_value(initial).map_err(|error| invalid(error.to_string()))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        if self.admission_initialization_digest.as_deref() != Some(digest.as_str()) {
            return Err(invalid(
                "transfer admission initialization differs from membership",
            ));
        }
        // Retain the paired-proof scope with the completed prefix. A later
        // request cannot reuse a prefix that admitted another operation's
        // testimony without reconstructing that operation's layout effects.
        self.verified_required_witnesses.clear();
        Ok(Arc::new(VerifiedTransferSeatPrefix { replay: self }))
    }

    /// Receipt-selected witnesses on a complete root composition cut. Unlike
    /// selected-operation admission, every older phase is folded by the
    /// completed paired dependency owner, never ignored as an unrelated row.
    pub(crate) fn begin_preparation_freeze(
        state: &AppState,
        seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
        anchor: AuthoritativeThreadEventReadAnchor,
        initialization_digest: &str,
        freeze_events: &[TransferEvidence],
        cut: Option<Arc<super::super::transfer_ingress::FixedTransferCut>>,
    ) -> Result<Self, HandlerError> {
        let mut read = Self::begin_transfer_admission_with_freeze(
            state,
            seat,
            anchor,
            initialization_digest,
            &[],
            freeze_events,
            None,
        )?;
        if cut.as_ref().is_some_and(|cut| {
            !cut.covers_seat_anchor(
                &seat.presentation_id,
                &seat.chain_root_id,
                &seat.thread_id,
                read.anchor.current().expect("preparation is current"),
            )
        }) {
            return Err(invalid("preparation freeze cut differs from root"));
        }
        read.transfer_admission = false;
        read.paired_cut = cut;
        Ok(read)
    }

    pub(crate) fn finish_preparation_freeze(
        mut self,
        initial: &UiPresentationInitialization,
    ) -> Result<Arc<VerifiedTransferSeatPrefix>, HandlerError> {
        self.completed_composition_frontier(initial)?;
        self.transfer_admission = true;
        self.finish_transfer_admission()
    }

    fn transfer_admission_history(
        &self,
    ) -> Result<super::super::composition::VerifiedHistory, HandlerError> {
        if !self.transfer_admission || !self.reached_tip {
            return Err(invalid(
                "transfer composition has no complete admission proof",
            ));
        }
        Ok(super::super::composition::VerifiedHistory {
            replay: self
                .composition
                .clone()
                .ok_or_else(|| invalid("transfer composition is absent"))?,
            chain_seq: u64::try_from(self.after_chain_seq.unwrap_or(-1))
                .map_err(|_| invalid("transfer composition has no chain position"))?,
            tip_hash: self.tip_hash.clone(),
            thread_seq: self.tip_thread_seq,
            next_engine_seq: self.next_engine_seq,
            control: self.control.clone(),
            control_predecessor: self
                .control_predecessor
                .clone()
                .ok_or_else(|| invalid("transfer composition lacks control ancestry"))?,
            transfer_testimony_seen: self.transfer_testimony_seen,
        })
    }

    /// One bounded projection/CAS work slice. An empty Batch means this slice
    /// contained only unpublishable events (for example, a pending producer
    /// interval); the caller should yield and request the next slice.
    pub(crate) fn next_step(
        &mut self,
        state: &AppState,
    ) -> Result<FixedAnchorReplayStep, HandlerError> {
        if self.awaiting_paired_handoff {
            return Ok(FixedAnchorReplayStep::RequiresPairedTransfer {
                anchor: self.anchor.current()?.clone(),
            });
        }
        if !self.pending_handoff_events.is_empty() {
            if self.paired_cut.is_none() {
                return Err(invalid("paired handoff prefix has no authenticated cut"));
            }
            return Ok(FixedAnchorReplayStep::Batch {
                events: std::mem::take(&mut self.pending_handoff_events),
            });
        }
        if self.reached_tip {
            if self.retired_read.is_some() {
                let actual = self
                    .initialization
                    .as_ref()
                    .ok_or_else(|| invalid("retired completion lacks initialization"))?;
                let digest = ryeos_client_base::ui::seat_payload_digest(
                    &serde_json::to_value(actual).map_err(|e| invalid(e.to_string()))?,
                )
                .map_err(|error| invalid(error.to_string()))?;
                if self.admission_initialization_digest.as_deref() != Some(digest.as_str()) {
                    return Err(invalid(
                        "retired replay initialization differs from captured membership",
                    ));
                }
            }
            if self.paired_cut.as_ref().is_some_and(|cut| {
                !cut.completed_root_positions(
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.thread_id,
                    &self.paired_seen,
                )
            }) {
                return Err(invalid(
                    "paired replay did not consume its exact root testimony",
                ));
            }
            if self.anchor.chain_root_id() != self.chain_root_id
                || self.anchor.thread_id() != self.thread_id
                || self.anchor.tip() != Some((self.tip_hash.as_str(), self.tip_thread_seq))
            {
                return Err(invalid(
                    "seat replay completion differs from fixed signed anchor",
                ));
            }
            if let ReplayEndpoint::Retired(prefix) = &self.anchor {
                if !self.matches_completed_retired_prefix(prefix) {
                    return Err(invalid(
                        "retired forward index did not complete its exact fold",
                    ));
                }
                // The completed endpoint, native cursor, receipt state and
                // paired coverage retain the proof. Consumed hash strings and
                // Vec capacity are scheduling data and can now be dropped.
                if let Some(read) = &mut self.retired_read {
                    read.positions = Vec::new();
                }
            }
            return Ok(FixedAnchorReplayStep::Complete {
                tip_hash: self.tip_hash.clone(),
                thread_seq: self.tip_thread_seq,
            });
        }
        let (locators, has_more) = if let Some(read) = &mut self.retired_read {
            if !read.indexed {
                let ReplayEndpoint::Retired(prefix) = &self.anchor else {
                    return Err(invalid("retired replay scheduling endpoint changed"));
                };
                let authority = state
                    .state_store
                    .pinned_state_authority()
                    .map_err(|e| invalid(e.to_string()))?;
                let guard = authority
                    .acquire_shared_guard()
                    .map_err(|e| invalid(e.to_string()))?;
                let page = authority
                    .read_retired_seat_positive_prefix_page(
                        prefix,
                        read.progress.as_ref(),
                        LOCATOR_PAGE_EVENTS as u64,
                        LOCATOR_PAGE_BYTES as u64,
                        &guard,
                    )
                    .map_err(|e| invalid(e.to_string()))?;
                for (hash, event) in page.events {
                    if event.chain_root_id != self.chain_root_id
                        || event.thread_id != self.thread_id
                    {
                        return Err(invalid("retired reverse index changed exact seat"));
                    }
                    read.positions
                        .push((hash, event.thread_seq, event.chain_seq));
                }
                read.progress = page.progress;
                read.indexed = read.progress.is_none();
                if read.indexed {
                    read.remaining = read.positions.len();
                }
                // Authenticate the entire reverse ancestry before any forward
                // payload is released, including ordinary-only histories.
                return Ok(FixedAnchorReplayStep::Batch { events: Vec::new() });
            }
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            for (hash, thread_seq, chain_seq) in read.positions[..read.remaining]
                .iter()
                .rev()
                .take(LOCATOR_PAGE_EVENTS)
            {
                let event = state
                    .state_store
                    .retained_thread_event_by_hash(hash)
                    .map_err(|e| invalid(e.to_string()))?
                    .ok_or_else(|| invalid("retired forward index CAS object is absent"))?;
                if event.thread_seq != *thread_seq || event.chain_seq != *chain_seq {
                    return Err(invalid("retired forward index native coordinates changed"));
                }
                let record = persisted(hash.clone(), event)?;
                let size = serde_json::to_vec(&record)
                    .map_err(|e| invalid(e.to_string()))?
                    .len();
                if size > LOCATOR_PAGE_BYTES {
                    return Err(invalid("retired forward event exceeds replay byte bound"));
                }
                if bytes + size > LOCATOR_PAGE_BYTES {
                    break;
                }
                bytes += size;
                rows.push(record);
            }
            let more = rows.len() < read.remaining;
            (rows, more)
        } else {
            let page = state
                .state_store
                .replay_events_through(
                    &self.chain_root_id,
                    Some(&self.thread_id),
                    self.after_chain_seq,
                    self.tip_chain_seq,
                    LOCATOR_PAGE_EVENTS,
                    LOCATOR_PAGE_BYTES,
                )
                .map_err(|error| invalid(error.to_string()))?;
            (page.events, page.has_more)
        };
        if locators.is_empty() {
            return Err(invalid(
                "seat replay projection made no progress before signed tip",
            ));
        }
        let mut publishable = Vec::new();
        for locator in locators {
            let hash = locator
                .event_hash
                .as_deref()
                .ok_or_else(|| invalid("seat replay locator lacks a CAS hash"))?;
            let event = state
                .state_store
                .retained_thread_event_by_hash(hash)
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| invalid("seat replay locator CAS object is absent"))?;
            let record = persisted(hash.to_owned(), event)?;
            let expected_seq = self
                .previous_thread_seq
                .checked_add(1)
                .ok_or_else(|| invalid("seat replay thread sequence exhausted"))?;
            if !fixed_link_matches(
                &record,
                &locator,
                &self.chain_root_id,
                &self.thread_id,
                self.previous_hash.as_deref(),
                expected_seq,
                self.after_chain_seq,
            ) {
                // Empty no-op allowlist: even an apparently harmless omitted
                // event is not silently accepted as a signed-history gap.
                return Err(invalid(
                    "seat replay locator omits or changes signed thread history",
                ));
            }
            if let Some(handoff) = self.paired_handoff(&record.event_type)? {
                if self.live_suffix {
                    validate_live_transfer_wake(&record)?;
                }
                // Preserve the original cut. Do not consume this testimony or
                // publish this slice yet. Earlier ordinary rows on this page
                // are already folded and the cursor has advanced past them;
                // keep their bounded transport batch until the paired owner
                // proves and attaches the original cut.
                self.pending_handoff_events = publishable;
                self.awaiting_paired_handoff = true;
                return Ok(handoff);
            }
            self.apply_record(record, &mut publishable)?;
            self.previous_hash = Some(hash.to_owned());
            self.previous_thread_seq = expected_seq;
            self.after_chain_seq = Some(locator.chain_seq);
            if let Some(read) = &mut self.retired_read {
                read.remaining = read
                    .remaining
                    .checked_sub(1)
                    .ok_or_else(|| invalid("retired forward index exhausted before exact tip"))?;
            }
            if hash == self.tip_hash && expected_seq == self.tip_thread_seq {
                self.reached_tip = true;
                break;
            }
        }
        if self.reached_tip {
            if !self.initialization_seen || !self.pending_producer_events.is_empty() {
                return Err(invalid(
                    "seat replay ends without initialized, receipted history",
                ));
            }
        } else if !has_more {
            return Err(invalid("seat replay locators end before the signed tip"));
        }
        Ok(FixedAnchorReplayStep::Batch {
            events: publishable,
        })
    }

    /// Read-only projection after this exact root consumed every scheduled
    /// paired position and its complete receipted interval. A one-seat replay
    /// or partial handoff cannot be used as negative ownership testimony.
    pub(crate) fn completed_paired_projection(&self) -> Result<&UiCompositionReplay, HandlerError> {
        if !self.reached_tip
            || self.awaiting_paired_handoff
            || !self.pending_producer_events.is_empty()
            || !self.pending_handoff_events.is_empty()
            || !self.paired_cut.as_ref().is_some_and(|cut| {
                cut.completed_root_positions(
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.thread_id,
                    &self.paired_seen,
                )
            })
            || self.anchor.tip() != Some((self.tip_hash.as_str(), self.tip_thread_seq))
        {
            return Err(invalid(
                "paired projection did not finish its original signed cut",
            ));
        }
        self.composition
            .as_ref()
            .ok_or_else(|| invalid("paired projection has no composition"))
    }

    /// Read-only observation after the entire captured root was consumed.
    /// Paired roots additionally require complete dependency-position coverage.
    pub(crate) fn completed_revision_and_control(
        &self,
    ) -> Result<(u64, UiSeatControlPredecessor, Option<UiSeatProducer>), HandlerError> {
        if !self.reached_tip
            || matches!(&self.anchor, ReplayEndpoint::Retired(prefix) if !self.matches_completed_retired_prefix(prefix))
            || self.awaiting_paired_handoff
            || !self.pending_producer_events.is_empty()
            || !self.pending_handoff_events.is_empty()
            || self.anchor.tip() != Some((self.tip_hash.as_str(), self.tip_thread_seq))
        {
            return Err(invalid(
                "producer observation lacks completed exact root history",
            ));
        }
        let replay = if self.paired_cut.is_some() {
            self.completed_paired_projection()?
        } else {
            self.composition
                .as_ref()
                .ok_or_else(|| invalid("producer observation has no composition"))?
        };
        Ok((
            replay.revision,
            self.control_predecessor.clone().ok_or_else(|| {
                invalid("producer observation lacks initialization/control predecessor")
            })?,
            self.control.clone(),
        ))
    }

    /// Completed original-cut fold for prospective append validation. This
    /// carries historical composition, not permission to publish: the caller
    /// must retain the paired cut and condition every dependency anchor and
    /// current session membership at the actual append owner boundary.
    pub(crate) fn completed_composition_frontier(
        &self,
        initial: &UiPresentationInitialization,
    ) -> Result<super::super::composition::VerifiedHistory, HandlerError> {
        let (_, predecessor, control) = self.completed_revision_and_control()?;
        if self.initialization.as_ref() != Some(initial)
            || self.after_chain_seq != Some(self.tip_chain_seq)
            || self.previous_hash.as_deref() != Some(self.tip_hash.as_str())
            || self.previous_thread_seq != self.tip_thread_seq
            || (self.transfer_testimony_seen && self.paired_cut.is_none())
        {
            return Err(invalid(
                "append frontier lacks exact completed initialization and signed cut",
            ));
        }
        Ok(super::super::composition::VerifiedHistory {
            replay: self
                .composition
                .clone()
                .ok_or_else(|| invalid("append frontier has no composition"))?,
            chain_seq: u64::try_from(self.tip_chain_seq)
                .map_err(|_| invalid("append frontier chain position is invalid"))?,
            tip_hash: self.tip_hash.clone(),
            thread_seq: self.tip_thread_seq,
            next_engine_seq: self.next_engine_seq,
            control,
            control_predecessor: predecessor,
            transfer_testimony_seen: self.transfer_testimony_seen,
        })
    }

    /// Prove only the suffix after an already completed signed cut. Hub
    /// notifications merely request this read; neither their payload nor their
    /// delivery order is part of the proof. Do not disclose any receipt until
    /// the whole new cut has linked back to our previously verified frontier.
    /// A second bounded-page pass emits receipts one by one under backpressure.
    pub(crate) fn advance_receipts(
        &mut self,
        state: &AppState,
        mut continue_read: impl FnMut() -> bool,
        mut emit: impl FnMut(PersistedEventRecord) -> bool,
    ) -> Result<LiveSeatReplayAdvance, HandlerError> {
        if !self.reached_tip || !self.pending_producer_events.is_empty() {
            return Err(invalid("seat live replay has no completed predecessor"));
        }
        if !continue_read() {
            return Ok(LiveSeatReplayAdvance::Stopped);
        }
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&self.chain_root_id, &self.thread_id)
            .map_err(|error| invalid(error.to_string()))?
            .ok_or_else(|| invalid("seat live replay lost its signed anchor"))?;
        let (hash, sequence) = anchor
            .tip()
            .ok_or_else(|| invalid("seat live replay lost its signed tip"))?;
        if sequence < self.tip_thread_seq
            || (sequence == self.tip_thread_seq && hash != self.tip_hash)
        {
            return Err(invalid("seat live replay signed tip moved backwards"));
        }
        if sequence == self.tip_thread_seq {
            return Ok(LiveSeatReplayAdvance::Current);
        }
        let tip = state
            .state_store
            .retained_thread_event_by_hash(hash)
            .map_err(|error| invalid(error.to_string()))?
            .ok_or_else(|| invalid("seat live replay signed tip is absent from CAS"))?;
        if tip.chain_root_id != self.chain_root_id
            || tip.thread_id != self.thread_id
            || tip.thread_seq != sequence
        {
            return Err(invalid("seat live replay tip differs from signed anchor"));
        }
        self.tip_chain_seq = i64::try_from(tip.chain_seq)
            .map_err(|_| invalid("seat live replay tip chain sequence overflows"))?;
        if self
            .after_chain_seq
            .is_none_or(|previous| self.tip_chain_seq <= previous)
        {
            return Err(invalid(
                "seat live replay tip does not advance chain sequence",
            ));
        }
        self.tip_hash = hash.to_owned();
        self.tip_thread_seq = sequence;
        self.anchor = anchor.into();
        self.reached_tip = false;
        // A previous paired proof authenticates only its old exact cut. The
        // first new transfer row requires a fresh full fixed-cut proof.
        self.paired_cut = None;
        self.paired_seen.clear();
        self.live_suffix = true;
        let mut emission_proof = self.clone();
        loop {
            if !continue_read() {
                return Ok(LiveSeatReplayAdvance::Stopped);
            }
            match self.next_step(state)? {
                FixedAnchorReplayStep::Batch { events } => {
                    for event in events {
                        live_receipt(&event)?;
                    }
                }
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    return Ok(LiveSeatReplayAdvance::ReproofRequired);
                }
            }
        }
        loop {
            if !continue_read() {
                return Ok(LiveSeatReplayAdvance::Stopped);
            }
            match emission_proof.next_step(state)? {
                FixedAnchorReplayStep::Batch { events } => {
                    for event in events {
                        if live_receipt(&event)? && !emit(event) {
                            return Ok(LiveSeatReplayAdvance::Stopped);
                        }
                    }
                }
                FixedAnchorReplayStep::Complete { .. } => {
                    return Ok(LiveSeatReplayAdvance::Current);
                }
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    return Ok(LiveSeatReplayAdvance::ReproofRequired);
                }
            }
        }
    }

    fn paired_handoff(
        &self,
        event_type: &str,
    ) -> Result<Option<FixedAnchorReplayStep>, HandlerError> {
        if self.transfer_admission
            || self.paired_cut.is_some()
            || (!ryeos_ui_presentation_contract::transfer::is_transfer_testimony(event_type)
                && event_type != ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT)
        {
            return Ok(None);
        }
        if !self.pending_producer_events.is_empty() {
            return Err(invalid(
                "transfer interrupts an unreceipted producer interval",
            ));
        }
        Ok(Some(FixedAnchorReplayStep::RequiresPairedTransfer {
            anchor: self.anchor.current()?.clone(),
        }))
    }

    fn apply_record(
        &mut self,
        record: PersistedEventRecord,
        publishable: &mut Vec<PersistedEventRecord>,
    ) -> Result<(), HandlerError> {
        if matches!(
            record.event_type.as_str(),
            "thread_completed"
                | "thread_failed"
                | "thread_cancelled"
                | "thread_killed"
                | "thread_timed_out"
                | "thread_continued"
        ) {
            let status: ryeos_state::objects::thread_snapshot::ThreadStatus =
                serde_json::from_value(
                    record
                        .payload
                        .get("status")
                        .cloned()
                        .ok_or_else(|| invalid("retired terminal testimony lacks typed status"))?,
                )
                .map_err(|e| invalid(e.to_string()))?;
            if !matches!(
                status,
                ryeos_state::objects::thread_snapshot::ThreadStatus::Completed
                    | ryeos_state::objects::thread_snapshot::ThreadStatus::Cancelled
                    | ryeos_state::objects::thread_snapshot::ThreadStatus::Killed
            ) || record.event_type != format!("thread_{}", status.as_str())
                || record
                    .payload
                    .as_object()
                    .is_none_or(|payload| payload.len() != 2)
                || record
                    .payload
                    .get("outcome_code")
                    .and_then(serde_json::Value::as_str)
                    != Some("seat_lease_expired")
            {
                return Err(invalid(
                    "retired terminal event type and typed status differ",
                ));
            }
            // A terminal marker-bound tip is lifecycle testimony, not an
            // ordinary seat operation or a new producer capability. No generic
            // decoder is widened; current/live replay still refuses it.
            if !matches!(&self.anchor, ReplayEndpoint::Retired(prefix) if prefix.terminal_status()==status)
                || !self.initialization_seen
                || self.composition.is_none()
                || !self.pending_producer_events.is_empty()
                || self.awaiting_paired_handoff
                || !self.pending_handoff_events.is_empty()
                || record.event_hash.as_deref() != Some(self.tip_hash.as_str())
                || record.thread_seq != i64::try_from(self.tip_thread_seq).unwrap_or(-1)
                || record.chain_seq != self.tip_chain_seq
            {
                return Err(invalid(
                    "terminal testimony is not the exact retired seat tip",
                ));
            }
            return Ok(());
        }
        match record.event_type.as_str() {
            ryeos_state::event_types::THREAD_CREATED
                if !self.initialization_seen && self.previous_thread_seq == 0 =>
            {
                Ok(())
            }
            ryeos_state::event_types::THREAD_STARTED
                if !self.initialization_seen
                    && self.previous_thread_seq == 1
                    && self.pending_producer_events.is_empty() =>
            {
                // Signed root lifecycle position, not seat testimony. The
                // fixed locator/CAS link above has already authenticated it.
                Ok(())
            }
            PRESENTATION_INITIALIZED_EVENT
                if !self.initialization_seen && self.pending_producer_events.is_empty() =>
            {
                validate_replay_initialization(&record, &self.presentation_id)?;
                let initial: ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord =
                    serde_json::from_value(record.payload.clone()).map_err(|error| {
                        invalid(format!("invalid authenticated replay initialization: {error}"))
                    })?;
                let event =
                    retained_seat_event(&serde_json::to_value(&record).map_err(|error| {
                        invalid(format!(
                            "encode authenticated replay initialization: {error}"
                        ))
                    })?)
                    .map_err(invalid)?
                    .ok_or_else(|| invalid("authenticated initialization is not a seat event"))?;
                super::super::composition::fold_private_composition_event(
                    &mut self.composition,
                    &initial.initialization,
                    &event,
                )
                .map_err(invalid)?;
                self.initialization = Some(initial.initialization);
                self.initialization_seen = true;
                self.control_predecessor = Some(UiSeatControlPredecessor::Initialization {
                    event_hash: record
                        .event_hash
                        .clone()
                        .ok_or_else(|| invalid("initialization has no CAS hash"))?,
                });
                publishable.push(record);
                Ok(())
            }
            SEAT_PRODUCER_EVENT
                if self.initialization_seen && self.pending_producer_events.is_empty() =>
            {
                let control: UiSeatProducer = serde_json::from_value(record.payload.clone())
                    .map_err(|error| {
                        invalid(format!("invalid authenticated replay control: {error}"))
                    })?;
                let predecessor = self
                    .control_predecessor
                    .as_ref()
                    .ok_or_else(|| invalid("seat replay control precedes initialization"))?;
                authenticate_control_transition(
                    &control,
                    self.control.as_ref(),
                    predecessor,
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.thread_id,
                    self.next_engine_seq,
                )
                .map_err(invalid)?;
                self.control_predecessor = Some(UiSeatControlPredecessor::Control {
                    event_hash: record
                        .event_hash
                        .clone()
                        .ok_or_else(|| invalid("control has no CAS hash"))?,
                });
                self.control_evidence = Some(TransferEvidence {
                    event_hash: record
                        .event_hash
                        .clone()
                        .ok_or_else(|| invalid("control has no CAS hash"))?,
                    chain_seq: u64::try_from(record.chain_seq)
                        .map_err(|_| invalid("control has no canonical chain position"))?,
                });
                self.control = Some(control);
                publishable.push(record);
                Ok(())
            }
            SEAT_APPEND_RECEIPT_EVENT if self.initialization_seen => {
                let receipt: SeatAppendReceipt = serde_json::from_value(record.payload.clone())
                    .map_err(|error| {
                        invalid(format!("invalid authenticated replay receipt: {error}"))
                    })?;
                let expected_id =
                    ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
                        presentation_id: self.presentation_id.clone(),
                        chain_root_id: self.chain_root_id.clone(),
                        thread_id: self.thread_id.clone(),
                        producer_incarnation: receipt.producer_incarnation.clone(),
                        first_engine_seq: receipt.first_engine_seq,
                        last_engine_seq: receipt.last_engine_seq,
                        event_count: receipt.event_count as u64,
                        payload_digest: receipt.payload_digest.clone(),
                    }
                    .derive_identity()
                    .map_err(invalid)?;
                if receipt.schema_version
                    != ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA
                    || receipt.operation_id != expected_id
                    || receipt.first_engine_seq != self.next_engine_seq
                    || !self.control.as_ref().is_some_and(|control| {
                        control.state == UiSeatProducerState::Active
                            && control.producer_incarnation == receipt.producer_incarnation
                    })
                {
                    return Err(invalid(
                        "authenticated replay receipt breaks producer frontier",
                    ));
                }
                super::super::append_ack::validate_projected_interval(
                    &self.chain_root_id,
                    &self.thread_id,
                    &receipt,
                    record.chain_seq,
                    &self.pending_producer_events,
                )?;
                let initial = self
                    .initialization
                    .as_ref()
                    .ok_or_else(|| invalid("seat replay has no composition initialization"))?;
                let mut witnessed = Vec::new();
                for producer_record in &self.pending_producer_events {
                    let event =
                        retained_seat_event(&serde_json::to_value(producer_record).map_err(
                            |error| invalid(format!("encode authenticated replay event: {error}")),
                        )?)
                        .map_err(invalid)?
                        .ok_or_else(|| invalid("receipted producer record is not a seat event"))?;
                    super::super::composition::fold_private_receipted_composition_event(
                        &mut self.composition,
                        initial,
                        &event,
                        &self.thread_id,
                        &receipt.producer_incarnation,
                    )
                    .map_err(invalid)?;
                    if let Some(hash) = producer_record.event_hash.as_ref()
                        && let Some(expected_seq) = self.requested_freeze_events.get(hash)
                    {
                        let chain_seq = u64::try_from(producer_record.chain_seq)
                            .map_err(|_| invalid("requested freeze event position is invalid"))?;
                        if *expected_seq != chain_seq {
                            return Err(invalid("requested freeze event position differs"));
                        }
                        witnessed.push(VerifiedFreezeSeatEvent {
                            evidence: TransferEvidence {
                                event_hash: hash.clone(),
                                chain_seq,
                            },
                            event,
                            receipt_evidence: TransferEvidence {
                                event_hash: record
                                    .event_hash
                                    .clone()
                                    .ok_or_else(|| invalid("receipt has no CAS hash"))?,
                                chain_seq: u64::try_from(record.chain_seq)
                                    .map_err(|_| invalid("receipt position is invalid"))?,
                            },
                            receipt_operation_id: receipt.operation_id.clone(),
                            receipt_producer_incarnation: receipt.producer_incarnation.clone(),
                            receipt_first_engine_seq: receipt.first_engine_seq,
                            receipt_last_engine_seq: receipt.last_engine_seq,
                        });
                    }
                }
                for witness in witnessed {
                    if self
                        .verified_freeze_events
                        .insert(witness.evidence.event_hash.clone(), witness)
                        .is_some()
                    {
                        return Err(invalid("requested freeze event repeats in signed history"));
                    }
                }
                self.next_engine_seq = receipt
                    .last_engine_seq
                    .checked_add(1)
                    .ok_or_else(|| invalid("seat replay engine frontier is exhausted"))?;
                publishable.append(&mut self.pending_producer_events);
                publishable.push(record);
                Ok(())
            }
            kind if kind.starts_with(SEAT_EVENT_PREFIX) && self.initialization_seen => {
                if self.pending_producer_events.len() >= MAX_SEAT_APPEND_EVENTS {
                    return Err(invalid(
                        "seat replay producer interval exceeds append bound",
                    ));
                }
                self.pending_producer_events.push(record);
                Ok(())
            }
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT => {
                if !self.pending_producer_events.is_empty()
                    || (self.paired_cut.is_none() && self.cancellation_seen)
                {
                    return Err(invalid(
                        "cancellation interrupts a receipt or repeats its outcome",
                    ));
                }
                if let Some(cut) = self.paired_cut.as_ref() {
                    let evidence = TransferEvidence {
                        event_hash: record
                            .event_hash
                            .clone()
                            .ok_or_else(|| invalid("paired cancellation has no CAS hash"))?,
                        chain_seq: u64::try_from(record.chain_seq)
                            .map_err(|_| invalid("paired cancellation position is invalid"))?,
                    };
                    let frame = cut
                        .authenticated_cancellation_frame_at(&evidence)
                        .ok_or_else(|| invalid("paired cancellation lacks exact negative proof"))?;
                    let value = serde_json::to_value(&record)
                        .map_err(|error| invalid(error.to_string()))?;
                    let cancellation =
                        ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                            &value,
                        )
                        .map_err(invalid)?;
                    if frame.source_anchor.chain_root_id != self.chain_root_id
                        || frame.source_anchor.thread_id != self.thread_id
                        || self.anchor.tip()
                            != Some((
                                frame.source_anchor.event_hash.as_str(),
                                frame.source_anchor.thread_seq,
                            ))
                        || frame.cancellation_evidence != evidence
                        || frame.cancellation != cancellation
                        || self.paired_seen.contains_key(&evidence.event_hash)
                    {
                        return Err(invalid(
                            "paired cancellation differs from exact retained source outcome",
                        ));
                    }
                    self.composition
                        .as_mut()
                        .ok_or_else(|| invalid("paired cancellation composition absent"))?
                        .apply_authenticated_move_freeze_cancellation(
                            &cancellation,
                            &frame.preparation.event,
                            frame.completion.as_ref().map(|witness| &witness.event),
                            evidence.chain_seq,
                        )
                        .map_err(invalid)?;
                    self.paired_seen
                        .insert(evidence.event_hash.clone(), evidence.chain_seq);
                    self.transfer_testimony_seen = true;
                    publishable.push(record);
                    return Ok(());
                }
                let scope = self.cancellation_scope.as_ref().ok_or_else(|| {
                    invalid("MOVE freeze cancellation requires authenticated settlement replay")
                })?;
                let (expected, retained) = scope
                    .retained_cancellation()
                    .ok_or_else(|| invalid("cancellation scope lacks retained testimony"))?;
                let value =
                    serde_json::to_value(&record).map_err(|error| invalid(error.to_string()))?;
                let cancellation =
                    ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(&value)
                        .map_err(invalid)?;
                let seq = u64::try_from(record.chain_seq)
                    .map_err(|_| invalid("invalid cancellation position"))?;
                if scope.source_anchor() != self.anchor.current()?
                    || record.event_hash.as_deref() != Some(expected.event_hash.as_str())
                    || seq != expected.chain_seq
                    || &cancellation != retained
                    || &cancellation.transfer != scope.transfer()
                    || &cancellation.freeze != scope.freeze()
                {
                    return Err(invalid(
                        "cancellation differs from exact paired retained outcome",
                    ));
                }
                let preparation = self
                    .verified_freeze_events
                    .get(&cancellation.freeze.preparation.event_hash)
                    .filter(|event| event.evidence == cancellation.freeze.preparation)
                    .ok_or_else(|| {
                        invalid("cancellation preparation lacks exact receipted witness")
                    })?;
                let completion = cancellation
                    .freeze
                    .completion
                    .as_ref()
                    .map(|evidence| {
                        self.verified_freeze_events
                            .get(&evidence.event_hash)
                            .filter(|event| &event.evidence == evidence)
                            .ok_or_else(|| {
                                invalid("cancellation completion lacks exact receipted witness")
                            })
                    })
                    .transpose()?;
                self.composition
                    .as_mut()
                    .ok_or_else(|| invalid("cancellation composition absent"))?
                    .apply_authenticated_move_freeze_cancellation(
                        &cancellation,
                        &preparation.event,
                        completion.map(|event| &event.event),
                        seq,
                    )
                    .map_err(invalid)?;
                self.cancellation_seen = true;
                Ok(())
            }
            kind if ryeos_ui_presentation_contract::transfer::is_transfer_testimony(kind) => {
                if !self.pending_producer_events.is_empty() {
                    return Err(invalid("transfer interrupts an unreceipted producer batch"));
                }
                let value = serde_json::to_value(&record).map_err(|error| {
                    invalid(format!("encode authenticated transfer event: {error}"))
                })?;
                if kind == ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT {
                    let transfer =
                        ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                            .map_err(invalid)?;
                    let control = self
                        .control
                        .as_ref()
                        .filter(|control| control.state == UiSeatProducerState::Active)
                        .ok_or_else(|| invalid("transfer has no active producer"))?;
                    let revision = self
                        .composition
                        .as_ref()
                        .ok_or_else(|| invalid("transfer has no composition frontier"))?
                        .revision;
                    transfer
                        .validate_current_witness(
                            self.next_engine_seq,
                            revision,
                            &self.presentation_id,
                            &control.producer_incarnation,
                        )
                        .map_err(invalid)?;
                } else if self.transfer_admission {
                    let (control_evidence, control) = self
                        .control_evidence
                        .as_ref()
                        .zip(self.control.as_ref())
                        .ok_or_else(|| {
                            invalid("lifecycle transfer has no authenticated control")
                        })?;
                    if record.chain_seq
                        <= i64::try_from(control_evidence.chain_seq).unwrap_or(i64::MAX)
                    {
                        return Err(invalid("lifecycle transfer precedes its control fence"));
                    }
                    match kind {
                        ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT => {
                            let lifecycle = ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value)
                                .map_err(invalid)?;
                            ryeos_ui_presentation_contract::transfer_lifecycle::verify_closed_producer_fence(
                                &lifecycle, control_evidence, control,
                            ).map_err(invalid)?;
                        }
                        ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT => {
                            let lifecycle = ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(&value)
                                .map_err(invalid)?;
                            ryeos_ui_presentation_contract::transfer_lifecycle_v2::verify_latest_control(
                                &lifecycle, control_evidence, control,
                            ).map_err(invalid)?;
                        }
                        _ => return Err(invalid("unsupported transfer testimony version")),
                    }
                }
                if self.transfer_admission {
                    let evidence = TransferEvidence {
                        event_hash: record
                            .event_hash
                            .clone()
                            .ok_or_else(|| invalid("transfer has no CAS hash"))?,
                        chain_seq: u64::try_from(record.chain_seq)
                            .map_err(|_| invalid("transfer has no canonical chain position"))?,
                    };
                    if !self.required_transfer_witnesses.contains(&evidence) {
                        return Err(invalid(
                            "transfer admission encountered testimony outside its paired proof",
                        ));
                    }
                    if self.required_transfer_witnesses.contains(&evidence)
                        && !self.verified_required_witnesses.contains(&evidence)
                    {
                        self.verified_required_witnesses.push(evidence.clone());
                    }
                    self.transfer_witnesses.push_back(evidence);
                    while self.transfer_witnesses.len() > TRANSFER_PREFIX_WITNESSES {
                        self.transfer_witnesses.pop_front();
                    }
                    self.transfer_testimony_seen = true;
                    publishable.push(record);
                    return Ok(());
                }
                if let Some(cut) = self.paired_cut.as_ref() {
                    let evidence = TransferEvidence {
                        event_hash: record
                            .event_hash
                            .clone()
                            .ok_or_else(|| invalid("paired transfer has no CAS hash"))?,
                        chain_seq: u64::try_from(record.chain_seq)
                            .map_err(|_| invalid("paired transfer position is invalid"))?,
                    };
                    let decoded = match kind {
                        ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT => {
                            ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                                .map_err(invalid)?
                                .payload
                        }
                        ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT => {
                            ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value)
                                .map_err(invalid)?
                                .payload
                        }
                        ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT => {
                            ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(&value)
                                .map_err(invalid)?
                                .payload
                        }
                        _ => return Err(invalid("unsupported paired transfer family")),
                    };
                    let (authenticated, proof) = cut
                        .authenticated_phase_at(&evidence)
                        .filter(|(authenticated, _)| *authenticated == &decoded)
                        .ok_or_else(|| {
                            invalid("paired transfer differs from exact scheduled proof")
                        })?;
                    if self
                        .paired_seen
                        .insert(evidence.event_hash.clone(), evidence.chain_seq)
                        .is_some()
                    {
                        return Err(invalid("paired transfer repeats an exact testimony"));
                    }
                    let composition = self
                        .composition
                        .as_mut()
                        .ok_or_else(|| invalid("paired transfer has no composition projection"))?;
                    let bound = composition
                        .binding_descriptors()
                        .get(
                            &self
                                .initialization
                                .as_ref()
                                .ok_or_else(|| invalid("paired transfer has no initialization"))?
                                .root_binding
                                .binding_attachment_id,
                        )
                        .ok_or_else(|| invalid("paired transfer has no root descriptor"))?
                        .binding_request_bounds
                        .max_request_bytes;
                    composition
                        .apply_authenticated_transfer_phase(proof, authenticated, &evidence, bound)
                        .map_err(invalid)?;
                    self.transfer_testimony_seen = true;
                    // Keep the exact canonical row in the provisional batch.
                    // The caller still withholds all such batches and Complete
                    // until separate bounded paired-proof frames are staged.
                    publishable.push(record);
                    return Ok(());
                }
                // The exact witness is checked above, but a one-seat stream
                // cannot prove the counterpart or lifecycle phases. Until the
                // paired lane is connected it must not publish Complete.
                Err(invalid(
                    "transfer requires a complete two-seat replay proof",
                ))
            }
            _ => Err(invalid(
                "seat replay includes an unsupported or misplaced event",
            )),
        }
    }
}

fn live_receipt(event: &PersistedEventRecord) -> Result<bool, HandlerError> {
    if event.event_type == SEAT_APPEND_RECEIPT_EVENT {
        Ok(true)
    } else if event.event_type.starts_with(SEAT_EVENT_PREFIX) {
        Ok(false)
    } else {
        // A producer-control transition is authenticated, but this browser
        // must make a new admission choice before its core can continue.
        Err(invalid("seat live replay encountered a control change"))
    }
}

fn fixed_link_matches(
    record: &PersistedEventRecord,
    locator: &PersistedEventRecord,
    root: &str,
    thread: &str,
    previous_hash: Option<&str>,
    expected_thread_seq: u64,
    previous_chain_seq: Option<i64>,
) -> bool {
    record.chain_root_id == root
        && record.thread_id == thread
        && record.thread_seq == i64::try_from(expected_thread_seq).unwrap_or(-1)
        && record.prev_thread_event_hash.as_deref() == previous_hash
        && record.chain_seq == locator.chain_seq
        && record.thread_seq == locator.thread_seq
        && locator.chain_root_id == root
        && locator.thread_id == thread
        && previous_chain_seq.is_none_or(|previous| record.chain_seq > previous)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::handlers::ui_seat::transfer_ingress;
    use ryeos_client_base::ui::seat_digest::seat_payload_digest;
    use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};
    use tempfile::TempDir;

    const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";

    // Mint the opaque anchor through the real signed store. These fold tests
    // deliberately do not claim to exercise projection paging or completion.
    fn replay_fixture() -> (tempfile::TempDir, FixedAnchorReplay) {
        use ryeos_app::state_store::{NodeIdentitySigner, StateStore};
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &directory.path().join("node-key.pem"),
        )
        .unwrap();
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let store = StateStore::new_with_head_trust(
            directory.path().to_owned(),
            directory.path().join(".ai/state"),
            directory.path().join(".ai/state/runtime.sqlite3"),
            Arc::new(NodeIdentitySigner::from_identity(&identity)),
            ryeos_app::write_barrier::WriteBarrier::new(),
            Arc::new(trust),
        )
        .unwrap();
        create_replay_thread(&store);
        let anchor = store
            .capture_thread_event_read_anchor("T-seat", "T-seat")
            .unwrap()
            .unwrap();
        let (hash, sequence) = anchor.tip().unwrap();
        let tip_chain_seq = i64::try_from(
            store
                .retained_thread_event_by_hash(hash)
                .unwrap()
                .unwrap()
                .chain_seq,
        )
        .unwrap();
        let replay = FixedAnchorReplay {
            presentation_id: PRESENTATION.into(),
            chain_root_id: "T-seat".into(),
            thread_id: "T-seat".into(),
            tip_hash: hash.into(),
            tip_thread_seq: sequence,
            tip_chain_seq,
            after_chain_seq: None,
            previous_hash: None,
            previous_thread_seq: 0,
            initialization_seen: false,
            control_predecessor: None,
            control: None,
            next_engine_seq: 0,
            initialization: None,
            composition: None,
            pending_producer_events: Vec::new(),
            pending_handoff_events: Vec::new(),
            awaiting_paired_handoff: false,
            reached_tip: false,
            anchor: anchor.into(),
            retired_read: None,
            transfer_admission: false,
            live_suffix: false,
            admission_initialization_digest: None,
            control_evidence: None,
            transfer_testimony_seen: false,
            transfer_witnesses: VecDeque::new(),
            required_transfer_witnesses: Vec::new(),
            verified_required_witnesses: Vec::new(),
            requested_freeze_events: BTreeMap::new(),
            verified_freeze_events: BTreeMap::new(),
            cancellation_scope: None,
            cancellation_seen: false,
            paired_cut: None,
            paired_seen: BTreeMap::new(),
        };
        (directory, replay)
    }

    fn create_replay_thread(store: &ryeos_app::state_store::StateStore) {
        create_named_replay_thread(store, "T-seat");
    }

    fn create_named_replay_thread(store: &ryeos_app::state_store::StateStore, thread: &str) {
        create_named_replay_thread_with_retention(
            store,
            thread,
            ryeos_state::objects::ThreadHistoryRetention::Durable,
        );
    }

    fn create_named_replay_thread_with_retention(
        store: &ryeos_app::state_store::StateStore,
        thread: &str,
        retention: ryeos_state::objects::ThreadHistoryRetention,
    ) {
        create_named_replay_thread_with_retention_and_owner(store, thread, retention, "fp:test");
    }

    fn create_named_replay_thread_with_retention_and_owner(
        store: &ryeos_app::state_store::StateStore,
        thread: &str,
        retention: ryeos_state::objects::ThreadHistoryRetention,
        owner: &str,
    ) {
        create_named_replay_thread_with_authority(
            store,
            thread,
            retention,
            owner,
            "directive:test",
            "native:test",
        );
    }

    fn create_named_replay_thread_with_authority(
        store: &ryeos_app::state_store::StateStore,
        thread: &str,
        retention: ryeos_state::objects::ThreadHistoryRetention,
        owner: &str,
        item_ref: &str,
        executor_ref: &str,
    ) {
        store.create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
            thread_id: thread.into(), chain_root_id: thread.into(),
            kind: "seat_session".into(), item_ref: item_ref.into(),
            executor_ref: executor_ref.into(), launch_mode: "wait".into(),
            current_site_id: "site:test".into(), origin_site_id: "site:test".into(),
            upstream_thread_id: None, requested_by: Some(owner.into()), project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None, usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention,
                canonical_item_ref:item_ref.into(), item_content_hash:"a".repeat(64),
                item_signer_fingerprint:Some("a".repeat(64)),
                item_trust_class:ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash:"a".repeat(64),
                resolved_from:ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        }).unwrap();
    }

    fn initialized_replay() -> (tempfile::TempDir, FixedAnchorReplay) {
        use ryeos_ui_presentation_contract::seat_control::{
            UiSeatControlRequest, UiSeatProducerClaim,
        };
        let (directory, mut replay) = replay_fixture();
        let initial: UiPresentationInitialization = serde_json::from_value(serde_json::json!({
            "schema":ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA,
            "selection":{"kind":"configured_defaults"},"presentation_id":PRESENTATION,
            "creation_request_id":"22222222-2222-4222-8222-222222222222",
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32)},
            "next_mount_id":1,"surfaces":[],"binding_attachments":[{
                "binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32),
                "root_ref":"config:ryeos/test/presentation","root_generation":"cd".repeat(32),
                "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only",
                "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
                "presentation":{"definition":{"kind":"shared_presentation","config":{
                    "category":"ui","shared_presentation":{"schema":1,"title":"Replay fixture","launcher":"launcher",
                        "dialogs":[{"id":"launcher","title":"Launcher","view":"view:test/launcher"}],
                        "docks":{},"capabilities":{"sources":[],"affordances":[]}}}},
                    "views":{"view:test/launcher":{"widget":"rows"}}}
            }]
        })).unwrap();
        initial.validate().unwrap();
        let mut initialization = record(&"a".repeat(64), 2, None);
        initialization.chain_root_id = "T-seat".into();
        initialization.event_type = PRESENTATION_INITIALIZED_EVENT.into();
        initialization.payload = serde_json::json!({"operation_id":initial.creation_request_id,"initialization":initial});
        replay
            .apply_record(initialization, &mut Vec::new())
            .unwrap();
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: PRESENTATION.into(),
            thread_id: "T-seat".into(),
            chain_root_id: "T-seat".into(),
            predecessor: replay.control_predecessor.clone().unwrap(),
            intent_nonce: "33333333-3333-4333-8333-333333333333".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        request.assign_identity().unwrap();
        let control = UiSeatProducer {
            schema_version: ryeos_ui_presentation_contract::seat_control::SEAT_PRODUCER_SCHEMA
                .into(),
            operation_id: request.operation_id().into(),
            request,
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: "b".repeat(64),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        let mut control_record = record(&"b".repeat(64), 3, Some(&"a".repeat(64)));
        control_record.chain_root_id = "T-seat".into();
        control_record.event_type = SEAT_PRODUCER_EVENT.into();
        control_record.payload = serde_json::to_value(control).unwrap();
        replay
            .apply_record(control_record, &mut Vec::new())
            .unwrap();
        (directory, replay)
    }

    fn transfer_record(replay: &FixedAnchorReplay) -> PersistedEventRecord {
        let mut event = record(&"e".repeat(64), 7, Some(&"d".repeat(64)));
        event.chain_root_id = "T-seat".into();
        event.event_type = ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT.into();
        event.payload = serde_json::json!({"seq":replay.next_engine_seq,
            "operation_id":"44444444-4444-4444-8444-444444444444",
            "payload":{"schema":1,"transfer":{
                "operation_id":"44444444-4444-4444-8444-444444444444",
                "source":{"presentation_id":"55555555-5555-4555-8555-555555555555","thread_id":"T-other","chain_root_id":"T-other","producer_incarnation":"c".repeat(64),"revision":0},
                "destination":{"presentation_id":PRESENTATION,"thread_id":"T-seat","chain_root_id":"T-seat","producer_incarnation":replay.control.as_ref().unwrap().producer_incarnation,"revision":replay.composition.as_ref().unwrap().revision},
                "export_digest":"f".repeat(64)},"phase":{"kind":"inbound_commitment"}}});
        event
    }

    // Uses the established empty-engine AppState fixture: no item resolution,
    // live bundles, admission grants or external signing invocation is needed.
    // Initialization/control come from the existing fold fixture, but all
    // events below acquire real CAS hashes, links and projection positions.
    #[test]
    fn completed_observation_preserves_unclaimed_signed_initialization_predecessor() {
        use ryeos_app::state_store::NewEventRecord;
        let (_directory, state) = crate::test_state::build_test_state();
        create_replay_thread(&state.state_store);
        let (_template_directory, template) = initialized_replay();
        let initial = template.initialization.as_ref().unwrap();
        let appended = state.state_store.append_events("T-seat", "T-seat", &[NewEventRecord {
            event_type: PRESENTATION_INITIALIZED_EVENT.into(),
            storage_class: "indexed".into(),
            payload: serde_json::json!({"operation_id":initial.creation_request_id,"initialization":initial}),
        }]).unwrap();
        let mut read = FixedAnchorReplay::begin(&state, PRESENTATION, "T-seat", "T-seat").unwrap();
        assert!(read.completed_revision_and_control().is_err());
        assert!(read.completed_composition_frontier(initial).is_err());
        loop {
            match read.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("initialization-only history has no transfer")
                }
            }
        }
        let (revision, predecessor, producer) = read.completed_revision_and_control().unwrap();
        assert_eq!(revision, 0);
        assert!(producer.is_none());
        let frontier = read.completed_composition_frontier(initial).unwrap();
        assert_eq!(frontier.replay.revision, revision);
        assert_eq!(frontier.next_engine_seq, 0);
        assert_eq!(frontier.tip_hash, appended[0].event_hash.clone().unwrap());
        assert_eq!(
            frontier.chain_seq,
            u64::try_from(appended[0].chain_seq).unwrap()
        );
        let mut changed_initial = initial.clone();
        changed_initial.creation_request_id = "99999999-9999-4999-8999-999999999999".into();
        assert!(
            read.completed_composition_frontier(&changed_initial)
                .is_err()
        );
        assert_eq!(
            predecessor,
            UiSeatControlPredecessor::Initialization {
                event_hash: appended[0].event_hash.clone().unwrap(),
            }
        );
    }

    #[test]
    fn completed_observation_exclusion_refuses_advanced_nonroot_dependency() {
        use ryeos_app::state_store::NewEventRecord;
        let (_directory, state, proof, _digest) = cancellation_fixture();
        let anchors = [
            proof.source_anchor().clone(),
            proof.destination_anchor().clone(),
        ];
        let mut published = false;
        assert_eq!(
            state
                .state_store
                .with_thread_event_read_anchors(&anchors, || {
                    published = true;
                    Ok(7)
                })
                .unwrap(),
            Some(7)
        );
        assert!(published);
        let destination = &proof.transfer().destination;
        state
            .state_store
            .append_events(
                &destination.chain_root_id,
                &destination.thread_id,
                &[NewEventRecord {
                    event_type: "notice".into(),
                    storage_class: "indexed".into(),
                    payload: serde_json::json!({"advanced":"counterpart"}),
                }],
            )
            .unwrap();
        published = false;
        assert_eq!(
            state
                .state_store
                .with_thread_event_read_anchors(&anchors, || {
                    published = true;
                    Ok(7)
                })
                .unwrap(),
            None
        );
        assert!(!published);
    }

    fn paged_transfer_fixture(
        unreceipted: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        FixedAnchorReplay,
        PersistedEventRecord,
    ) {
        use ryeos_app::state_store::NewEventRecord;
        use ryeos_ui_presentation_contract::seat_control::UiSeatControlRequest;
        let (directory, state) = crate::test_state::build_test_state();
        create_replay_thread(&state.state_store);
        let (_fold_directory, template) = initialized_replay();
        let initial = template.initialization.as_ref().unwrap();
        let appended = state.state_store.append_events("T-seat", "T-seat", &[NewEventRecord {
            event_type: PRESENTATION_INITIALIZED_EVENT.into(),
            storage_class: "indexed".into(),
            payload: serde_json::json!({"operation_id":initial.creation_request_id,"initialization":initial}),
        }]).unwrap();
        let mut control = template.control.as_ref().unwrap().clone();
        let UiSeatControlRequest::Claim { predecessor, .. } = &mut control.request else {
            panic!("fixture must retain its fresh claim");
        };
        *predecessor = UiSeatControlPredecessor::Initialization {
            event_hash: appended[0].event_hash.clone().unwrap(),
        };
        control.request.assign_identity().unwrap();
        control.operation_id = control.request.operation_id().into();
        let mut events = vec![NewEventRecord {
            event_type: SEAT_PRODUCER_EVENT.into(),
            storage_class: "indexed".into(),
            payload: serde_json::to_value(control).unwrap(),
        }];
        if unreceipted {
            events.push(NewEventRecord {
                event_type: "seat.facet".into(),
                storage_class: "indexed".into(),
                payload: serde_json::json!({"seq":0,"payload":{"key":"selection","value":null}}),
            });
        }
        events.push(NewEventRecord {
            // Generic append intentionally seals current transfer writers.
            // An unknown family member can still be retained as a storage
            // fixture and MUST trigger the same typed handoff, not absence.
            // This proves the family boundary, not a validated v1 phase.
            event_type: "contract.ryeos_ui.presentation_transfer.fixture_future".into(),
            storage_class: "indexed".into(),
            payload: transfer_record(&template).payload,
        });
        let appended = state
            .state_store
            .append_events("T-seat", "T-seat", &events)
            .unwrap();
        let transfer = appended.last().unwrap().clone();
        let replay = FixedAnchorReplay::begin(&state, PRESENTATION, "T-seat", "T-seat").unwrap();
        (directory, state, replay, transfer)
    }

    #[tokio::test]
    async fn next_step_hands_off_real_signed_transfer_family_at_original_cut_without_publication() {
        let (_directory, state, mut replay, transfer) = paged_transfer_fixture(false);
        let original_anchor = replay.anchor.clone();
        let step = replay.next_step(&state).unwrap();
        let FixedAnchorReplayStep::RequiresPairedTransfer { anchor } = step else {
            panic!("transfer-bearing slice must not yield Batch or Complete");
        };
        assert_eq!(&anchor, original_anchor.current().unwrap());
        assert_eq!(
            anchor.tip().unwrap().0,
            transfer.event_hash.as_deref().unwrap()
        );
        assert_eq!(replay.previous_hash, transfer.prev_thread_event_hash);
        assert_eq!(replay.previous_thread_seq + 1, transfer.thread_seq as u64);
        assert!(replay.after_chain_seq.unwrap() < transfer.chain_seq);
        assert!(replay.initialization_seen);
        assert!(replay.control.is_some());
        assert_eq!(replay.next_engine_seq, 0);
        assert!(replay.pending_producer_events.is_empty());
        assert!(!replay.reached_tip);
        assert!(replay.awaiting_paired_handoff);
        assert_eq!(replay.pending_handoff_events.len(), 2);
        assert_eq!(
            replay.pending_handoff_events[0].event_type,
            PRESENTATION_INITIALIZED_EVENT
        );
        assert_eq!(
            replay.pending_handoff_events[1].event_type,
            SEAT_PRODUCER_EVENT
        );
        let retained = replay
            .pending_handoff_events
            .iter()
            .map(|event| event.event_hash.clone())
            .collect::<Vec<_>>();
        let cursor = (replay.after_chain_seq, replay.previous_thread_seq);
        assert!(matches!(
            replay.next_step(&state).unwrap(),
            FixedAnchorReplayStep::RequiresPairedTransfer { .. }
        ));
        assert_eq!(
            replay
                .pending_handoff_events
                .iter()
                .map(|event| event.event_hash.clone())
                .collect::<Vec<_>>(),
            retained
        );
        assert_eq!((replay.after_chain_seq, replay.previous_thread_seq), cursor);
    }

    // Real receipt intervals and the dedicated cancellation writer mint the
    // opaque paired proof. This fixture is not an ingress or session grant.
    pub(crate) fn cancellation_fixture() -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
    ) {
        cancellation_fixture_with_completion(true)
    }

    pub(crate) fn cancellation_fixture_with_completion(
        completed: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
    ) {
        cancellation_fixture_options(completed, false)
    }

    fn cancellation_fixture_options(
        completed: bool,
        with_closed_surface: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
    ) {
        freeze_fixture_options(completed, with_closed_surface, true)
    }

    fn freeze_fixture_options(
        completed: bool,
        with_closed_surface: bool,
        publish_cancellation: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
    ) {
        freeze_fixture_options_with_retention(
            completed,
            with_closed_surface,
            publish_cancellation,
            ryeos_state::objects::ThreadHistoryRetention::Durable,
        )
    }

    fn freeze_fixture_options_with_retention(
        completed: bool,
        with_closed_surface: bool,
        publish_cancellation: bool,
        retention: ryeos_state::objects::ThreadHistoryRetention,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
    ) {
        let (directory, state, proof, digest, _) = freeze_fixture_options_with_session(
            completed,
            with_closed_surface,
            publish_cancellation,
            retention,
            false,
        );
        (directory, state, proof, digest)
    }

    /// Real authored closure for ingress qualification. The ephemeral author is
    /// trusted only by this temporary Engine; no development publisher key or
    /// installed trust policy is read or changed. Signed bundle dependencies
    /// must already be valid, exactly as for any live compiler test.
    fn compiled_transfer_fixture_context(
        state: &AppState,
        author: &lillux::crypto::SigningKey,
    ) -> crate::browser_session::LaunchContext {
        let project = state.config.app_root.canonicalize().unwrap();
        for (relative, body) in [
            (
                ".ai/config/test/transfer-presentation.yaml",
                "category: test\nversion: '1'\nshared_presentation:\n  schema: 1\n  title: Transfer fixture\n  launcher: launcher\n  dialogs:\n    - { id: launcher, title: Launcher, view: 'view:test/launcher' }\n  docks:\n    bottom: { content: 'view:test/launcher', open: true, size: 7 }\n  capabilities: { sources: [], affordances: [] }\n",
            ),
            (
                ".ai/views/test/launcher.yaml",
                "category: test\nversion: '1'\nwidget: text\nbody: '@facet:selection.work'\n",
            ),
        ] {
            let path = project.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                lillux::signature::sign_content(body, author, "#", None),
            )
            .unwrap();
        }
        let authority = Arc::new(lillux::PinnedDirectory::open(&project).unwrap().unwrap());
        let ctx = crate::test_state::local_operator_context(state, vec!["*".into()]);
        let request = crate::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
            composition:
                ryeos_client_base::presentation::UiCompositionSelection::SharedPresentation {
                    config_ref: "config:test/transfer-presentation".into(),
                },
            project_path: Some(project.display().to_string()),
            user_principal_id: Some(ctx.fingerprint.clone()),
        };
        let (compiled_binding, presentation) =
            crate::handlers::ui_launch_mint::compile_session_binding(
                &request,
                &ctx,
                state,
                Some(&authority),
            )
            .expect("compile temporary signed transfer Config and exact View closure");
        crate::browser_session::LaunchContext {
            compiled_binding: Arc::new(compiled_binding),
            presentation,
            granted_caps: ctx.scopes,
            user_principal_id: Some(ctx.fingerprint),
            project_authority: Some(authority),
            registered_project_id: None,
        }
    }

    fn freeze_fixture_options_with_session(
        completed: bool,
        with_closed_surface: bool,
        publish_cancellation: bool,
        retention: ryeos_state::objects::ThreadHistoryRetention,
        session_admission: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
        Option<String>,
    ) {
        freeze_fixture_options_with_compiler(
            completed,
            with_closed_surface,
            publish_cancellation,
            retention,
            session_admission,
            false,
        )
    }

    fn freeze_fixture_options_with_compiler(
        completed: bool,
        with_closed_surface: bool,
        publish_cancellation: bool,
        retention: ryeos_state::objects::ThreadHistoryRetention,
        session_admission: bool,
        live_compiler: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        String,
        Option<String>,
    ) {
        use ryeos_app::state_store::{NewEventRecord, UiTransferPairSeat};
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            assemble_move_freeze_cancellation_proof, begin_fixed_seat_read, finish_fixed_seat_read,
            read_fixed_seat_page,
        };
        use ryeos_client_base::ui::{
            admission_operation::{
                UiCompletedPresentationOperation, UiPreparedPresentationOperation,
            },
            transfer_freeze::UiMoveFreezeRequest,
        };
        use ryeos_ui_presentation_contract::{
            identity::UiSeatAppendIdentity,
            payload_digest::seat_payload_digest,
            seat_control::UiSeatControlRequest,
            transfer::{TransferIdentity, TransferSeat},
        };
        let fixture_author = live_compiler.then(lillux::crypto::generate_signing_key);
        let (directory, mut state) = match &fixture_author {
            Some(author) => {
                crate::test_state::build_test_state_with_live_bundles_and_fixture_author(
                    &author.verifying_key(),
                )
            }
            None => crate::test_state::build_test_state(),
        };
        let (_template_directory, template) = initialized_replay();
        let mut initial_value =
            serde_json::to_value(template.initialization.as_ref().unwrap()).unwrap();
        initial_value["binding_attachments"][0]["presentation"]["definition"]["config"]["shared_presentation"]
            ["docks"] =
            serde_json::json!({"bottom":{"content":"view:test/launcher","open":true,"size":7}});
        initial_value["binding_attachments"][0]["presentation"]["views"]["view:test/launcher"] =
            serde_json::json!({"widget":"text","body":"@facet:selection.work"});
        let mut initial: UiPresentationInitialization =
            serde_json::from_value(initial_value).unwrap();
        if session_admission {
            let mut route = state
                .node_config
                .routes
                .iter()
                .find(|route| {
                    route.response.source.as_deref()
                        == Some(crate::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref)
                })
                .expect("fixture dispatch route")
                .clone();
            route.id = "ui/seat/append".into();
            route.path = "/ui/api/seat/append".into();
            route.response.source = Some(
                crate::handlers::ui_seat::APPEND_DESCRIPTOR
                    .service_ref
                    .into(),
            );
            route.limits.body_bytes_max = 65536;
            Arc::make_mut(&mut state.node_config).routes.push(route);
            initial.binding_attachments[0].binding_request_bounds =
                crate::handlers::ui_launch_mint::binding_request_bounds(&state).unwrap();
        }
        let browser_session_id = if session_admission {
            assert!(!with_closed_surface && !publish_cancellation);
            let ui = crate::state::get_ui_state(&state).unwrap();
            let sessions = &ui.browser_sessions;
            let policy = state.node_policy.generation_digest();
            let context = if let Some(author) = &fixture_author {
                compiled_transfer_fixture_context(&state, author)
            } else {
                let mut context = crate::browser_session::tests::test_shared_context();
                context.presentation = initial.binding_attachments[0].presentation.clone();
                Arc::make_mut(&mut context.compiled_binding)
                    .binding
                    .node_policy_generation_digest = policy.to_owned();
                context
            };
            let (id, token, _) = sessions.mint_token(context, 16, policy).unwrap();
            sessions.consume_launch_token(&token).unwrap();
            let session = sessions.get_session(&id).unwrap();
            let root = session.attachments[&session.root_attachment_id].coordinate();
            for presentation in [PRESENTATION, "66666666-6666-4666-8666-666666666666"] {
                let member = sessions
                    .prepare_presentation(
                        &id,
                        presentation,
                        &root,
                        4,
                        policy,
                        initial.binding_attachments[0].binding_request_bounds,
                    )
                    .unwrap();
                crate::browser_session::tests::test_rekey_presentation(
                    sessions,
                    &id,
                    &member.presentation_id,
                    presentation,
                );
            }
            initial = sessions.get_session(&id).unwrap().presentations[PRESENTATION]
                .initialization
                .clone();
            Some(id)
        } else {
            None
        };
        let surface = if with_closed_surface {
            use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
            use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
            let attachment = crate::browser_session::tests::test_surface_attachment();
            let descriptor =
                attachment.public_descriptor(initial.binding_attachments[0].binding_request_bounds);
            let binding = ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                binding_generation: descriptor.binding_generation,
                binding_digest: descriptor.binding_digest.clone(),
            };
            let seeds = descriptor
                .presentation
                .definition
                .surface()
                .unwrap()
                .to_mount_seeds()
                .unwrap();
            let active = seeds.first().map(|seed| seed.id.clone());
            let surface_id = "99999999-9999-4999-8999-999999999999";
            let manifest = UiSurfaceMountManifest::allocate(
                PresentationId::try_new(PRESENTATION).unwrap(),
                SurfaceInstanceId::try_new(surface_id).unwrap(),
                initial.next_mount_id,
                seeds
                    .into_iter()
                    .map(|seed| (seed, binding.clone()))
                    .collect(),
                active.as_deref(),
            )
            .unwrap();
            initial.next_mount_id = manifest.next_mount_id;
            let surface = ryeos_client_base::ui::binding::UiSurfaceMembership {
                surface_instance_id: surface_id.into(),
                ordinal: 1,
                title: descriptor.presentation.definition.title().into(),
                binding,
                mount_manifest: manifest,
                saved_surface: None,
            };
            initial.surfaces.push(surface.clone());
            initial.binding_attachments.push(descriptor);
            initial
                .binding_attachments
                .sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
            Some(surface)
        } else {
            None
        };
        initial.validate().unwrap();
        let source_initial_digest =
            seat_payload_digest(&serde_json::to_value(&initial).unwrap()).unwrap();
        let source_incarnation = template
            .control
            .as_ref()
            .unwrap()
            .producer_incarnation
            .clone();
        let destination_presentation = "66666666-6666-4666-8666-666666666666";
        for (thread, presentation) in [
            ("T-seat", PRESENTATION),
            ("T-cancel-destination", destination_presentation),
        ] {
            let (item_ref, executor_ref) = if let Some(id) = &browser_session_id {
                let ui = crate::state::get_ui_state(&state).unwrap();
                let session = ui.browser_sessions.get_session(id).unwrap();
                let member = &session.presentations[presentation];
                (
                    session.attachments[&member.root_coordinate.binding_attachment_id]
                        .root_ref
                        .clone(),
                    crate::handlers::ui_seat::presentation_executor_ref(
                        presentation,
                        &member.root_coordinate,
                    ),
                )
            } else {
                ("directive:test".into(), "native:test".into())
            };
            create_named_replay_thread_with_authority(
                &state.state_store,
                thread,
                retention.clone(),
                &browser_session_id
                    .as_ref()
                    .map(|id| format!("session:{id}"))
                    .unwrap_or_else(|| "fp:test".into()),
                &item_ref,
                &executor_ref,
            );
            state.state_store.mark_thread_running(thread, None).unwrap();
            let mut member = initial.clone();
            if let Some(id) = &browser_session_id {
                let ui = crate::state::get_ui_state(&state).unwrap();
                let sessions = &ui.browser_sessions;
                let membership =
                    sessions.get_session(id).unwrap().presentations[presentation].clone();
                sessions
                    .bind_presentation_seat(
                        id,
                        presentation,
                        &membership.root_coordinate,
                        thread,
                        thread,
                        state.node_policy.generation_digest(),
                    )
                    .unwrap();
                member = sessions.get_session(id).unwrap().presentations[presentation]
                    .initialization
                    .clone();
            }
            member.presentation_id = presentation.into();
            if thread != "T-seat" {
                if browser_session_id.is_none() {
                    member.creation_request_id = presentation.into();
                }
                if surface.is_some() {
                    member.surfaces.clear();
                    let root_attachment_id = member.root_binding.binding_attachment_id.clone();
                    member.binding_attachments.retain(|attachment| {
                        attachment.binding_attachment_id == root_attachment_id
                    });
                    member.next_mount_id = 1;
                }
            }
            let initialized = state.state_store.append_events(thread, thread, &[NewEventRecord {
                event_type: PRESENTATION_INITIALIZED_EVENT.into(), storage_class: "indexed".into(),
                payload: serde_json::json!({"operation_id":member.creation_request_id,"initialization":member}),
            }]).unwrap();
            if let Some(id) = &browser_session_id {
                let signed:ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord =
                    serde_json::from_value(initialized[0].payload.clone()).unwrap();
                crate::browser_session::tests::test_install_signed_initialization(
                    &crate::state::get_ui_state(&state).unwrap().browser_sessions,
                    id,
                    presentation,
                    &signed,
                );
            }
            let mut control = template.control.as_ref().unwrap().clone();
            let UiSeatControlRequest::Claim {
                presentation_id,
                thread_id,
                chain_root_id,
                predecessor,
                ..
            } = &mut control.request
            else {
                unreachable!()
            };
            *presentation_id = presentation.into();
            *thread_id = thread.into();
            *chain_root_id = thread.into();
            *predecessor = UiSeatControlPredecessor::Initialization {
                event_hash: initialized[0].event_hash.clone().unwrap(),
            };
            control.request.assign_identity().unwrap();
            control.operation_id = control.request.operation_id().into();
            state
                .state_store
                .append_events(
                    thread,
                    thread,
                    &[NewEventRecord {
                        event_type: SEAT_PRODUCER_EVENT.into(),
                        storage_class: "indexed".into(),
                        payload: serde_json::to_value(control).unwrap(),
                    }],
                )
                .unwrap();
        }
        let mut replay = UiCompositionReplay::begin(&initial, &SeatEvent {
            producer_engine_seq:None, seq:1,
            kind:ryeos_client_base::ui::seat::SeatEventKind::PresentationInitialized(
                ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord {
                    operation_id:initial.creation_request_id.clone(), initialization:initial.clone(),
                }),
        }).unwrap();
        fn append_interval(
            state: &AppState,
            incarnation: &str,
            first: u64,
            semantic: Vec<(&str, serde_json::Value)>,
        ) -> Vec<PersistedEventRecord> {
            let last = first + semantic.len() as u64 - 1;
            let values = semantic.iter().enumerate().map(|(offset,(kind,payload))| serde_json::json!({"engine_seq":first+offset as u64,"event_type":kind,"payload":payload})).collect::<Vec<_>>();
            let digest = seat_payload_digest(&serde_json::to_value(values).unwrap()).unwrap();
            let receipt = SeatAppendReceipt {
                schema_version:
                    ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA.into(),
                producer_incarnation: incarnation.into(),
                operation_id: UiSeatAppendIdentity {
                    presentation_id: PRESENTATION.into(),
                    chain_root_id: "T-seat".into(),
                    thread_id: "T-seat".into(),
                    producer_incarnation: incarnation.into(),
                    first_engine_seq: first,
                    last_engine_seq: last,
                    event_count: semantic.len() as u64,
                    payload_digest: digest.clone(),
                }
                .derive_identity()
                .unwrap(),
                first_engine_seq: first,
                last_engine_seq: last,
                event_count: semantic.len(),
                payload_digest: digest,
            };
            let mut events = semantic
                .into_iter()
                .enumerate()
                .map(|(offset, (kind, payload))| NewEventRecord {
                    event_type: kind.into(),
                    storage_class: "indexed".into(),
                    payload: serde_json::json!({"seq":first+offset as u64,"payload":payload}),
                })
                .collect::<Vec<_>>();
            events.push(NewEventRecord {
                event_type: SEAT_APPEND_RECEIPT_EVENT.into(),
                storage_class: "indexed".into(),
                payload: serde_json::to_value(receipt).unwrap(),
            });
            state
                .state_store
                .append_events("T-seat", "T-seat", &events)
                .unwrap()
        }
        let dock_key = replay.composition.docks()["bottom"]
            .view()
            .instance_key()
            .clone();
        // Shared mounts have no implicit subject: establish real receipted
        // subject testimony before capturing the exact transferable dock.
        let mut subject: UiPreparedPresentationOperation = serde_json::from_value(serde_json::json!({
            "schema":2,"operation_id":"","producer":{"presentation_id":PRESENTATION,"thread_id":"T-seat","producer_incarnation":source_incarnation},
            "prepared_engine_seq":0,"root_binding":initial.root_binding,"request_bounds":initial.binding_attachments[0].binding_request_bounds,
            "body":{"kind":"composition","request":{"schema":1,"presentation_id":PRESENTATION,"operation_id":"","expected_revision":replay.revision,"expected_next_mount_id":replay.next_mount_id,
                "owner":{"kind":"shared","binding":initial.root_binding},"mutation":{"kind":"set_subject","mount":dock_key,"attachment":{"mode":"required_subject","input":"work","facets":["selection.work"]}}}}
        })).unwrap();
        subject.assign_identity().unwrap();
        subject.validate().unwrap();
        let subject_completed = replay
            .expected_composition_completion(&subject, 65536)
            .unwrap();
        let retained = append_interval(
            &state,
            &source_incarnation,
            0,
            vec![
                (
                    "seat.presentation_operation.prepared",
                    serde_json::to_value(&subject).unwrap(),
                ),
                (
                    "seat.presentation_operation.completed",
                    serde_json::to_value(&subject_completed).unwrap(),
                ),
            ],
        );
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: Some(0),
                seq: u64::try_from(retained[0].chain_seq).unwrap(),
                kind: ryeos_client_base::ui::seat::SeatEventKind::PresentationOperationPrepared(
                    subject,
                ),
            })
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: Some(1),
                seq: u64::try_from(retained[1].chain_seq).unwrap(),
                kind: ryeos_client_base::ui::seat::SeatEventKind::PresentationOperationCompleted(
                    subject_completed,
                ),
            })
            .unwrap();
        let dock = &replay.composition.docks()["bottom"];
        let mounted = replay
            .composition
            .mount(dock.view().instance_key())
            .unwrap();
        let export: ryeos_client_base::ui::transfer_export::UiTransferExport = serde_json::from_value(serde_json::json!({
            "schema":1,"mode":"move","content":{"kind":"shared_dock","edge":"bottom",
                "visible":dock.visible,"size":dock.size,"mount":{
                "source_instance":mounted.instance_key(),"view_ref":mounted.view_ref(),"binding":mounted.binding(),
                "local":mounted.local(),"subject":replay.subjects[mounted.instance_key()].attachment,
                "route":{"kind":"authored"},"current_label":null,"drafts":[],"lens_frames":[]
            }}
        })).unwrap();
        let transfer = TransferIdentity {
            operation_id: "77777777-7777-4777-8777-777777777777".into(),
            source: TransferSeat {
                presentation_id: PRESENTATION.into(),
                thread_id: "T-seat".into(),
                chain_root_id: "T-seat".into(),
                producer_incarnation: source_incarnation.clone(),
                revision: replay.revision,
            },
            destination: TransferSeat {
                presentation_id: destination_presentation.into(),
                thread_id: "T-cancel-destination".into(),
                chain_root_id: "T-cancel-destination".into(),
                producer_incarnation: source_incarnation.clone(),
                revision: 0,
            },
            export_digest: seat_payload_digest(&serde_json::to_value(&export).unwrap()).unwrap(),
        };
        export.validate_snapshot(&transfer, &replay, 65536).unwrap();
        let bounds = initial.binding_attachments[0].binding_request_bounds;
        let mut operation: UiPreparedPresentationOperation = serde_json::from_value(serde_json::json!({
            "schema":2,"operation_id":"", "producer":{"presentation_id":PRESENTATION,"thread_id":"T-seat","producer_incarnation":source_incarnation},
            "prepared_engine_seq":2,"root_binding":initial.root_binding,"request_bounds":bounds,
            "body":{"kind":"move_freeze","request":UiMoveFreezeRequest {
                schema:1,operation_id:String::new(), transfer:transfer.clone(),root_binding:initial.root_binding.clone(),
                instances:vec![mounted.instance_key().clone()], view_sets:Vec::new(), export,
            }}
        })).unwrap();
        operation.assign_identity().unwrap();
        operation.validate().unwrap();
        let completion = UiCompletedPresentationOperation {
            operation_id: operation.operation_id.clone(),
            result: serde_json::to_value(operation.move_freeze().unwrap().completion()).unwrap(),
        };
        let prepared_value = serde_json::to_value(&operation).unwrap();
        let completed_value = serde_json::to_value(&completion).unwrap();
        let mut semantics = serde_json::json!([
            {"engine_seq":2,"event_type":"seat.presentation_operation.prepared","payload":prepared_value},
            {"engine_seq":3,"event_type":"seat.presentation_operation.completed","payload":completed_value}
        ]);
        if !completed {
            semantics.as_array_mut().unwrap().pop();
        }
        let last_engine_seq = 2 + u64::from(completed);
        let event_count = 1 + u64::from(completed);
        let digest = seat_payload_digest(&semantics).unwrap();
        let receipt = SeatAppendReceipt {
            schema_version:
                ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: source_incarnation.clone(),
            operation_id: UiSeatAppendIdentity {
                presentation_id: PRESENTATION.into(),
                chain_root_id: "T-seat".into(),
                thread_id: "T-seat".into(),
                producer_incarnation: source_incarnation.clone(),
                first_engine_seq: 2,
                last_engine_seq,
                event_count,
                payload_digest: digest.clone(),
            }
            .derive_identity()
            .unwrap(),
            first_engine_seq: 2,
            last_engine_seq,
            event_count: usize::try_from(event_count).unwrap(),
            payload_digest: digest,
        };
        let mut events = vec![NewEventRecord {
            event_type: "seat.presentation_operation.prepared".into(),
            storage_class: "indexed".into(),
            payload: serde_json::json!({"seq":2,"payload":prepared_value}),
        }];
        if completed {
            events.push(NewEventRecord {
                event_type: "seat.presentation_operation.completed".into(),
                storage_class: "indexed".into(),
                payload: serde_json::json!({"seq":3,"payload":completed_value}),
            });
        }
        events.push(NewEventRecord {
            event_type: SEAT_APPEND_RECEIPT_EVENT.into(),
            storage_class: "indexed".into(),
            payload: serde_json::to_value(receipt).unwrap(),
        });
        state
            .state_store
            .append_events("T-seat", "T-seat", &events)
            .unwrap();
        let source_detail = state.state_store.get_thread("T-seat").unwrap().unwrap();
        state
            .state_store
            .touch_seat_lease(
                "T-seat",
                &browser_session_id
                    .as_ref()
                    .map(|id| format!("session:{id}"))
                    .unwrap_or_else(|| "fp:test".into()),
                &source_detail.item_ref,
                &source_detail.executor_ref,
            )
            .unwrap();
        let scan = |seat: &TransferSeat| {
            let anchor = state
                .state_store
                .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)
                .unwrap()
                .unwrap();
            let mut read = begin_fixed_seat_read(
                state.state_store.clone(),
                anchor,
                seat,
                std::slice::from_ref(&transfer),
            )
            .unwrap();
            while !read_fixed_seat_page(&mut read, 1, 1024 * 1024).unwrap() {}
            finish_fixed_seat_read(read).unwrap()
        };
        let destination = scan(&transfer.destination);
        let source = scan(&transfer.source);
        let proof =
            assemble_move_freeze_cancellation_proof(&source, &destination, &transfer).unwrap();
        if !publish_cancellation {
            return (
                directory,
                state,
                Arc::new(proof),
                source_initial_digest,
                browser_session_id,
            );
        }
        let cancellation_owner = browser_session_id
            .as_ref()
            .map(|id| format!("session:{id}"))
            .unwrap_or_else(|| "fp:test".into());
        let pair = UiTransferPairSeat {
            anchor: proof.source_anchor(),
            owner: &cancellation_owner,
            surface: &source_detail.item_ref,
            client_ref: &source_detail.executor_ref,
            presentation_id: PRESENTATION,
            producer_incarnation: &source_incarnation,
            next_engine_seq: last_engine_seq + 1,
            revision: transfer.source.revision,
            max_append_bytes: bounds.max_seat_append_bytes,
        };
        let published = state
            .state_store
            .append_ui_move_freeze_cancellation(&proof, &pair, || Ok(true))
            .unwrap();
        assert!(matches!(
            published,
            ryeos_app::state_store::UiMoveFreezeCancellationAppendResult::Committed(_)
        ));
        if let Some(surface) = &surface {
            let closure = ryeos_client_base::ui::seat::UiSurfaceClosure {
                presentation_id: PRESENTATION.into(),
                surface_instance_id: surface.surface_instance_id.clone(),
                binding: surface.binding.clone(),
            };
            append_interval(
                &state,
                &source_incarnation,
                last_engine_seq + 1,
                vec![(
                    ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT,
                    serde_json::to_value(closure).unwrap(),
                )],
            );
        }
        let source = scan(&transfer.source);
        let proof = Arc::new(
            assemble_move_freeze_cancellation_proof(&source, &destination, &transfer).unwrap(),
        );
        (
            directory,
            state,
            proof,
            source_initial_digest,
            browser_session_id,
        )
    }

    #[test]
    fn activated_pair_observation_includes_later_receipted_destination_revision() {
        activated_pair_observation_fixture(ryeos_state::objects::ThreadHistoryRetention::Durable);
    }

    fn activated_pair_observation_fixture(
        retention: ryeos_state::objects::ThreadHistoryRetention,
    ) -> (
        tempfile::TempDir,
        AppState,
        TransferIdentity,
        UiPresentationInitialization,
        UiPresentationInitialization,
    ) {
        let (directory, state, transfer, source, destination, _) =
            activated_pair_observation_fixture_with_session(retention, false);
        (directory, state, transfer, source, destination)
    }

    pub(crate) fn ordinary_session_replacement_fixture() -> (TempDir, AppState, String, TransferSeat)
    {
        let (directory, state, proof, _, id) = freeze_fixture_options_with_session(
            true,
            false,
            false,
            ryeos_state::objects::ThreadHistoryRetention::Durable,
            true,
        );
        (
            directory,
            state,
            id.unwrap(),
            proof.transfer().source.clone(),
        )
    }

    pub(crate) fn activated_pair_observation_fixture_with_session(
        retention: ryeos_state::objects::ThreadHistoryRetention,
        session_admission: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        TransferIdentity,
        UiPresentationInitialization,
        UiPresentationInitialization,
        Option<String>,
    ) {
        pair_observation_fixture_with_session(retention, session_admission, false, false)
    }

    pub(crate) fn activated_pair_with_compiled_authority() -> (
        tempfile::TempDir,
        AppState,
        TransferIdentity,
        UiPresentationInitialization,
        UiPresentationInitialization,
        Option<String>,
    ) {
        pair_observation_fixture_with_session(
            ryeos_state::objects::ThreadHistoryRetention::Durable,
            true,
            false,
            true,
        )
    }

    pub(crate) fn accepted_pair_observation_fixture_with_session() -> (
        tempfile::TempDir,
        AppState,
        TransferIdentity,
        UiPresentationInitialization,
        UiPresentationInitialization,
        Option<String>,
    ) {
        pair_observation_fixture_with_session(
            ryeos_state::objects::ThreadHistoryRetention::Durable,
            true,
            true,
            false,
        )
    }

    fn pair_observation_fixture_with_session(
        retention: ryeos_state::objects::ThreadHistoryRetention,
        session_admission: bool,
        stop_after_acceptance: bool,
        live_compiler: bool,
    ) -> (
        tempfile::TempDir,
        AppState,
        TransferIdentity,
        UiPresentationInitialization,
        UiPresentationInitialization,
        Option<String>,
    ) {
        use ryeos_app::state_store::{
            NewEventRecord, UiTransferPairSeat, UiTransferSourceDecision,
        };
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            begin_fixed_seat_read, finish_fixed_seat_read, read_complete_fixed_history,
            read_fixed_seat_page,
        };
        use ryeos_client_base::ui::admission_operation::{
            UiCompletedPresentationOperation, UiPreparedPresentationOperation,
        };
        use ryeos_client_base::ui::transfer_export::UiTransferExport;
        use ryeos_client_base::ui::transfer_reservation::UiTransferReservation;
        use ryeos_ui_presentation_contract::identity::UiSeatAppendIdentity;
        fn pair<'a>(
            seat: &'a TransferSeat,
            anchor: &'a AuthoritativeThreadEventReadAnchor,
            revision: u64,
            frontier: u64,
            owners: &'a BTreeMap<String, (String, String, String)>,
        ) -> UiTransferPairSeat<'a> {
            let (owner, surface, client_ref) = &owners[&seat.thread_id];
            UiTransferPairSeat {
                anchor,
                owner,
                surface,
                client_ref,
                presentation_id: &seat.presentation_id,
                producer_incarnation: &seat.producer_incarnation,
                revision,
                next_engine_seq: frontier,
                max_append_bytes: 65536,
            }
        }
        let (directory, state, negative, _digest, browser_session_id) =
            freeze_fixture_options_with_compiler(
                true,
                false,
                false,
                retention,
                session_admission,
                live_compiler,
            );
        assert!(negative.retained_cancellation().is_none());
        let transfer = negative.transfer().clone();
        let owner = browser_session_id
            .as_ref()
            .map(|id| format!("session:{id}"))
            .unwrap_or_else(|| "fp:test".into());
        let destination_detail = state
            .state_store
            .get_thread(&transfer.destination.thread_id)
            .unwrap()
            .unwrap();
        state
            .state_store
            .touch_seat_lease(
                &transfer.destination.thread_id,
                &owner,
                &destination_detail.item_ref,
                &destination_detail.executor_ref,
            )
            .unwrap();
        let owner = [&transfer.source, &transfer.destination]
            .into_iter()
            .map(|seat| {
                let detail = state
                    .state_store
                    .get_thread(&seat.thread_id)
                    .unwrap()
                    .unwrap();
                (
                    seat.thread_id.clone(),
                    (owner.clone(), detail.item_ref, detail.executor_ref),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let original = state
            .state_store
            .retained_thread_event_by_hash(&negative.freeze().preparation.event_hash)
            .unwrap()
            .unwrap();
        let prepared: UiPreparedPresentationOperation =
            serde_json::from_value(original.payload["payload"].clone()).unwrap();
        let export: UiTransferExport = prepared.move_freeze().unwrap().export.clone();
        let export_value = serde_json::to_value(&export).unwrap();
        let scan = |seat: &TransferSeat| {
            let anchor = state
                .state_store
                .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)
                .unwrap()
                .unwrap();
            let mut read = begin_fixed_seat_read(
                state.state_store.clone(),
                anchor,
                seat,
                std::slice::from_ref(&transfer),
            )
            .unwrap();
            while !read_fixed_seat_page(&mut read, 1, 1024 * 1024).unwrap() {}
            finish_fixed_seat_read(read).unwrap()
        };
        let ordinary = |seat: &TransferSeat| {
            let mut read = FixedAnchorReplay::begin(
                &state,
                &seat.presentation_id,
                &seat.chain_root_id,
                &seat.thread_id,
            )
            .unwrap();
            loop {
                match read.next_step(&state).unwrap() {
                    FixedAnchorReplayStep::Batch { .. } => {}
                    FixedAnchorReplayStep::Complete { .. } => return read,
                    FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        panic!("no positive phase written yet")
                    }
                }
            }
        };
        let source_replay = ordinary(&transfer.source);
        let source_initial = source_replay.initialization.clone().unwrap();
        let source_projection = source_replay.composition.unwrap();
        let destination_replay = ordinary(&transfer.destination);
        let destination_initial = destination_replay.initialization.unwrap();
        let destination_projection = destination_replay.composition.unwrap();
        let ui = crate::state::get_ui_state(&state).unwrap();
        let policy = state.node_policy.generation_digest();
        let admit_pair = || -> anyhow::Result<bool> {
            let Some(id) = &browser_session_id else {
                return Ok(true);
            };
            let session = ui
                .browser_sessions
                .get_session(id)
                .ok_or_else(|| anyhow::anyhow!("fixture session expired"))?;
            Ok(ui
                .browser_sessions
                .matches_transfer_preparation_memberships(
                    id,
                    &session.presentations[&transfer.source.presentation_id],
                    &session.presentations[&transfer.destination.presentation_id],
                    session.next_attachment_generation,
                    policy,
                ))
        };
        let source = scan(&transfer.source);
        let destination = scan(&transfer.destination);
        state
            .state_store
            .append_ui_transfer_preparation_pair(
                &transfer,
                &export_value,
                &pair(
                    &transfer.destination,
                    negative.destination_anchor(),
                    0,
                    0,
                    &owner,
                ),
                &pair(
                    &transfer.source,
                    negative.source_anchor(),
                    transfer.source.revision,
                    negative.next_engine_seq(),
                    &owner,
                ),
                &source,
                &destination,
                admit_pair,
            )
            .unwrap();
        let proof = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
        let inbound = proof.lifecycle().inbound();
        let preparation = proof.lifecycle().prepared().unwrap();
        let inbound_event = state
            .state_store
            .retained_thread_event_by_hash(&inbound.evidence.event_hash)
            .unwrap()
            .unwrap();
        let prepared_event = state
            .state_store
            .retained_thread_event_by_hash(&preparation.evidence.event_hash)
            .unwrap()
            .unwrap();
        let staged = browser_session_id.as_ref().map(|id| {
            ui.browser_sessions
                .stage_fresh_transfer_attachment_batch(id, &transfer, &export, 16, policy)
                .unwrap()
        });
        let mut descriptor = source_initial
            .binding_attachments
            .iter()
            .find(|descriptor| {
                descriptor.binding_attachment_id
                    == source_initial.root_binding.binding_attachment_id
            })
            .unwrap()
            .clone();
        if let Some(staged) = &staged {
            let descriptors = staged.destination_descriptors(&source_projection).unwrap();
            assert_eq!(descriptors.len(), 1);
            descriptor = descriptors[0].clone();
        } else {
            descriptor.binding_attachment_id = "77777777-7777-4777-8777-777777777777".into();
            descriptor.binding_generation += 1;
        }
        let coordinate = ryeos_client_base::ui::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        let reservation = UiTransferReservation::reserve_shared_dock(
            &transfer,
            &export,
            &destination_projection,
            "right",
            &std::collections::BTreeMap::from([(
                source_initial.root_binding.binding_attachment_id.clone(),
                coordinate,
            )]),
            vec![descriptor],
            65536,
        )
        .unwrap();
        // The legacy mode represents synthetic admission metadata only. The
        // session mode reserves the actual hidden pinned batch in this writer's
        // admission callback, then publishes it through the activation owner.
        state
            .state_store
            .append_ui_transfer_accepted_if_two_anchors(
                &transfer,
                &proof,
                preparation.evidence,
                &inbound_event,
                &prepared_event,
                &serde_json::to_value(&reservation).unwrap(),
                &pair(
                    &transfer.destination,
                    proof.destination_anchor(),
                    0,
                    0,
                    &owner,
                ),
                &pair(
                    &transfer.source,
                    proof.source_anchor(),
                    transfer.source.revision,
                    negative.next_engine_seq(),
                    &owner,
                ),
                || {
                    if let (Some(id), Some(staged)) = (&browser_session_id, &staged) {
                        ui.browser_sessions
                            .reserve_hidden_transfer_attachment_batch(
                                id,
                                staged,
                                &export,
                                &reservation,
                                16,
                                65536,
                                policy,
                            )?;
                        admit_pair()
                    } else {
                        Ok(true)
                    }
                },
            )
            .unwrap();
        let proof = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
        if stop_after_acceptance {
            assert!(proof.lifecycle().accepted().is_some());
            assert!(proof.lifecycle().released().is_none());
            return (
                directory,
                state,
                transfer,
                source_initial,
                destination_initial,
                browser_session_id,
            );
        }
        state
            .state_store
            .append_ui_transfer_source_decision_if_two_anchors(
                &proof,
                UiTransferSourceDecision::Commit,
                &pair(
                    &transfer.source,
                    proof.source_anchor(),
                    transfer.source.revision,
                    negative.next_engine_seq(),
                    &owner,
                ),
                &pair(
                    &transfer.destination,
                    proof.destination_anchor(),
                    0,
                    0,
                    &owner,
                ),
                admit_pair,
            )
            .unwrap();
        let proof = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
        state
            .state_store
            .append_ui_transfer_activated_if_two_anchors(
                &proof,
                &pair(
                    &transfer.destination,
                    proof.destination_anchor(),
                    0,
                    0,
                    &owner,
                ),
                &pair(
                    &transfer.source,
                    proof.source_anchor(),
                    transfer.source.revision,
                    negative.next_engine_seq(),
                    &owner,
                ),
                admit_pair,
            )
            .unwrap();
        let fold = |proof| {
            let cut = transfer_ingress::test_cut_from_completed_positive_proof(
                proof,
                source_initial.clone(),
                destination_initial.clone(),
            );
            let seat = &transfer.destination;
            let mut read = FixedAnchorReplay::begin(
                &state,
                &seat.presentation_id,
                &seat.chain_root_id,
                &seat.thread_id,
            )
            .unwrap();
            loop {
                match read.next_step(&state).unwrap() {
                    FixedAnchorReplayStep::Batch { .. } => {}
                    FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        read.attach_paired_cut(cut.clone()).unwrap()
                    }
                    FixedAnchorReplayStep::Complete { .. } => return read,
                }
            }
        };
        let activated_proof =
            read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
        assert!(activated_proof.lifecycle().activated().is_some());
        if let Some(id) = &browser_session_id {
            let staged = ui
                .browser_sessions
                .stage_activation_transfer_attachment_batch(
                    id,
                    &activated_proof,
                    &export,
                    &reservation,
                    65536,
                    policy,
                )
                .unwrap();
            let prevalidated = ui
                .browser_sessions
                .prevalidate_transfer_attachment_batch(
                    &staged,
                    &export,
                    &reservation,
                    65536,
                    policy,
                )
                .unwrap();
            let session = ui.browser_sessions.get_session(id).unwrap();
            let publication_cut = transfer_ingress::test_cut_from_completed_positive_proof(
                read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap(),
                source_initial.clone(),
                destination_initial.clone(),
            );
            let (anchors, retired) = publication_cut.publication_cut(&state, &session).unwrap();
            let expected = session.presentations.values().cloned().collect::<Vec<_>>();
            // A real durable activation is insufficient without its complete
            // cut and exact full presentation snapshot. Neither refusal may
            // consume the hidden reservation or publish descriptors.
            for (supplied_anchors, supplied_memberships) in [
                (&anchors[..1], expected.as_slice()),
                (anchors.as_slice(), &[][..]),
            ] {
                assert!(
                    ui.browser_sessions
                        .activate_transfer_attachment_batch_at_mixed_cut(
                            &state.state_store,
                            supplied_anchors,
                            &retired,
                            supplied_memberships,
                            &publication_cut,
                            id,
                            &session.presentations[&transfer.source.presentation_id],
                            &session.presentations[&transfer.destination.presentation_id],
                            &prevalidated,
                            &export,
                            &reservation,
                            65536,
                            16,
                            policy,
                        )
                        .is_err()
                );
                let unchanged = ui.browser_sessions.get_session(id).unwrap();
                assert_eq!(unchanged.presentations, session.presentations);
                assert_eq!(
                    unchanged.next_attachment_generation,
                    session.next_attachment_generation
                );
                assert_eq!(unchanged.attachments.len(), session.attachments.len());
                assert!(session.attachments.iter().all(|(key, attachment)| {
                    unchanged
                        .attachments
                        .get(key)
                        .is_some_and(|current| Arc::ptr_eq(current, attachment))
                }));
                assert_eq!(
                    format!("{:?}", unchanged.pending_transfer_attachments),
                    format!("{:?}", session.pending_transfer_attachments)
                );
            }
            assert!(
                ui.browser_sessions
                    .activate_transfer_attachment_batch_at_mixed_cut(
                        &state.state_store,
                        &anchors,
                        &retired,
                        &expected,
                        &publication_cut,
                        id,
                        &session.presentations[&transfer.source.presentation_id],
                        &session.presentations[&transfer.destination.presentation_id],
                        &prevalidated,
                        &export,
                        &reservation,
                        65536,
                        16,
                        policy
                    )
                    .unwrap()
                    .is_some()
            );
            let activated_session = ui.browser_sessions.get_session(id).unwrap();
            assert_eq!(
                activated_session.presentations[&transfer.destination.presentation_id]
                    .activated_transfer_reservations
                    .len(),
                1
            );
            assert!(activated_session.pending_transfer_attachments.is_empty());
        }
        let activated = fold(activated_proof);
        let (revision, _, _) = activated.completed_revision_and_control().unwrap();
        assert_eq!(revision, 1);
        let root_binding = destination_initial.root_binding.clone();
        // This unrelated edit belongs to the original Shared root, not the
        // freshly admitted imported dock's independent binding.
        let later_mount = activated.composition.as_ref().unwrap().composition.docks()["bottom"]
            .view()
            .instance_key()
            .clone();
        let mut later: UiPreparedPresentationOperation = serde_json::from_value(serde_json::json!({
            "schema":2,"operation_id":"","producer":{"presentation_id":transfer.destination.presentation_id,
                "thread_id":transfer.destination.thread_id,"producer_incarnation":transfer.destination.producer_incarnation},
            "prepared_engine_seq":0,"root_binding":root_binding,"request_bounds":destination_initial.binding_attachments[0].binding_request_bounds,
            "body":{"kind":"composition","request":{"schema":1,"presentation_id":transfer.destination.presentation_id,
                "operation_id":"","expected_revision":revision,"expected_next_mount_id":activated.composition.as_ref().unwrap().next_mount_id,
                "owner":{"kind":"shared","binding":root_binding},"mutation":{"kind":"set_subject",
                    "mount":later_mount,"attachment":{"mode":"required_subject","input":"later","facets":["selection.work"]}}}}
        })).unwrap();
        later.assign_identity().unwrap();
        later.validate().unwrap();
        let completion: UiCompletedPresentationOperation = activated
            .composition
            .as_ref()
            .unwrap()
            .expected_composition_completion(&later, 65536)
            .unwrap();
        let semantics = serde_json::json!([
            {"engine_seq":0,"event_type":"seat.presentation_operation.prepared","payload":later},
            {"engine_seq":1,"event_type":"seat.presentation_operation.completed","payload":completion}
        ]);
        let digest = seat_payload_digest(&semantics).unwrap();
        let identity = UiSeatAppendIdentity {
            presentation_id: transfer.destination.presentation_id.clone(),
            chain_root_id: transfer.destination.chain_root_id.clone(),
            thread_id: transfer.destination.thread_id.clone(),
            producer_incarnation: transfer.destination.producer_incarnation.clone(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest.clone(),
        };
        let receipt = SeatAppendReceipt {
            schema_version:
                ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA.into(),
            operation_id: identity.derive_identity().unwrap(),
            producer_incarnation: transfer.destination.producer_incarnation.clone(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest,
        };
        state
            .state_store
            .append_events(
                &transfer.destination.chain_root_id,
                &transfer.destination.thread_id,
                &[
                    NewEventRecord {
                        event_type: "seat.presentation_operation.prepared".into(),
                        storage_class: "indexed".into(),
                        payload: serde_json::json!({"seq":0,"payload":later}),
                    },
                    NewEventRecord {
                        event_type: "seat.presentation_operation.completed".into(),
                        storage_class: "indexed".into(),
                        payload: serde_json::json!({"seq":1,"payload":completion}),
                    },
                    NewEventRecord {
                        event_type: SEAT_APPEND_RECEIPT_EVENT.into(),
                        storage_class: "indexed".into(),
                        payload: serde_json::to_value(receipt).unwrap(),
                    },
                ],
            )
            .unwrap();
        let updated =
            fold(read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap());
        let (observed, predecessor, producer) = updated.completed_revision_and_control().unwrap();
        assert_eq!(observed, revision + 1);
        assert_eq!(
            predecessor,
            activated.completed_revision_and_control().unwrap().1
        );
        assert!(producer.is_some());
        let frontier = updated
            .completed_composition_frontier(&destination_initial)
            .unwrap();
        assert_eq!(frontier.replay.revision, observed);
        assert_eq!(frontier.next_engine_seq, updated.next_engine_seq);
        assert_eq!(
            serde_json::to_value(&frontier.control).unwrap(),
            serde_json::to_value(&updated.control).unwrap()
        );
        assert_eq!(frontier.control_predecessor, predecessor);
        assert!(frontier.transfer_testimony_seen);
        assert!(frontier.require_ordinary_mutation().is_err());
        let mut missing_paired = updated.clone();
        missing_paired.paired_cut = None;
        assert!(
            missing_paired
                .completed_composition_frontier(&destination_initial)
                .is_err()
        );
        (
            directory,
            state,
            transfer,
            source_initial,
            destination_initial,
            browser_session_id,
        )
    }

    #[tokio::test]
    async fn lifecycle_released_hidden_batch_requires_current_cut_and_successful_publication() {
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            TransferSettlementSide, UiTransferSettlementWriteOutcome, read_complete_fixed_history,
        };
        let (_directory, state, transfer, source_initial, destination_initial, session_id) =
            accepted_pair_observation_fixture_with_session();
        let session_id = session_id.unwrap();
        let ui = crate::state::get_ui_state(&state).unwrap();
        state.threads.set_scheduler_db(
            state.scheduler_db.clone(),
            state.scheduler_runtime_gate.clone(),
            state.config.app_root.clone(),
        );
        let mut config = state.node_config.as_ref().clone();
        let mut route = config.routes[0].clone();
        route.id = "ui/seat/append".into();
        route.path = "/ui/api/seat/append".into();
        route.methods = std::collections::HashSet::from(["POST".into()]);
        route.response.source = Some("service:ui/seat/append".into());
        route.response.source_config = serde_json::json!({});
        route.limits.body_bytes_max = 65536;
        config.routes.retain(|existing| {
            existing.response.source.as_deref() != Some("service:ui/seat/append")
        });
        config.routes.push(route);
        state
            .threads
            .set_ui_seat_append_route_from_node_config(&config)
            .unwrap();
        let initial_session = ui.browser_sessions.get_session(&session_id).unwrap();
        assert!(
            initial_session
                .pending_transfer_attachments
                .contains_key(&transfer.operation_id)
        );
        for (seat, side) in [
            (&transfer.source, TransferSettlementSide::Source),
            (&transfer.destination, TransferSettlementSide::Destination),
        ] {
            assert!(
                state
                    .state_store
                    .claim_expired_seat_lease(&seat.thread_id, i64::MAX)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .reserve_claimed_ui_seat_transfer_discovery(&seat.thread_id)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .queue_claimed_ui_seat_transfer(&seat.thread_id, &transfer, side)
                    .unwrap()
            );
            let mut settled = false;
            for _ in 0..512 {
                if matches!(
                    state
                        .threads
                        .advance_claimed_ui_transfer_settlement_once()
                        .await
                        .unwrap(),
                    Some(UiTransferSettlementWriteOutcome::Committed(_))
                ) {
                    settled = true;
                    break;
                }
            }
            assert!(settled);
            let proof = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
            assert!(proof.lifecycle().aborted().is_some());
            if side == TransferSettlementSide::Source {
                assert!(proof.lifecycle().released().is_none());
            } else {
                assert!(proof.lifecycle().released().is_some());
            }
            // Lifecycle testimony itself never mutates browser reservation state.
            assert!(
                ui.browser_sessions
                    .get_session(&session_id)
                    .unwrap()
                    .pending_transfer_attachments
                    .contains_key(&transfer.operation_id)
            );
        }
        let completed = || {
            let proof = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
            let cut = transfer_ingress::test_cut_from_completed_positive_proof(
                proof,
                source_initial.clone(),
                destination_initial.clone(),
            );
            let mut read = FixedAnchorReplay::begin(
                &state,
                &transfer.destination.presentation_id,
                &transfer.destination.chain_root_id,
                &transfer.destination.thread_id,
            )
            .unwrap();
            loop {
                match read.next_step(&state).unwrap() {
                    FixedAnchorReplayStep::Batch { .. } => {}
                    FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        read.attach_paired_cut(cut.clone()).unwrap()
                    }
                    FixedAnchorReplayStep::Complete { .. } => break,
                }
            }
            let frontier = read
                .completed_composition_frontier(&destination_initial)
                .unwrap();
            (cut, frontier.replay)
        };
        let (cut, replay) = completed();
        let membership = &initial_session.presentations[&transfer.destination.presentation_id];
        let expected = initial_session
            .presentations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let (anchors, retired) = cut.publication_cut(&state, &initial_session).unwrap();
        let roots = [(membership, &replay, cut.as_ref())];
        let policy = state.node_policy.generation_digest();
        assert!(
            ui.browser_sessions
                .with_reconciled_completed_roots_at_cut::<()>(
                    &state.state_store,
                    &anchors,
                    &retired,
                    &session_id,
                    &expected,
                    policy,
                    &roots,
                    &[],
                    |candidate| {
                        assert!(
                            !candidate
                                .pending_transfer_attachments
                                .contains_key(&transfer.operation_id)
                        );
                        Ok(None)
                    }
                )
                .unwrap()
                .is_none()
        );
        assert!(
            ui.browser_sessions
                .with_reconciled_completed_roots_at_cut::<()>(
                    &state.state_store,
                    &anchors,
                    &retired,
                    &session_id,
                    &expected,
                    policy,
                    &roots,
                    &[],
                    |_| Err(anyhow::anyhow!("publication failed"))
                )
                .is_err()
        );
        assert!(
            ui.browser_sessions
                .get_session(&session_id)
                .unwrap()
                .pending_transfer_attachments
                .contains_key(&transfer.operation_id)
        );
        // Advance the cut through real source settlement observation. An
        // arbitrary notice would deliberately poison the closed seat grammar.
        assert!(
            state
                .threads
                .reserve_claimed_ui_seat_transfer_discovery(&transfer.source.thread_id)
                .unwrap()
        );
        assert!(
            state
                .threads
                .queue_claimed_ui_seat_transfer(
                    &transfer.source.thread_id,
                    &transfer,
                    TransferSettlementSide::Source,
                )
                .unwrap()
        );
        let mut observed = false;
        for _ in 0..512 {
            if matches!(
                state
                    .threads
                    .advance_claimed_ui_transfer_settlement_once()
                    .await
                    .unwrap(),
                Some(UiTransferSettlementWriteOutcome::Committed(_))
            ) {
                observed = true;
                break;
            }
        }
        assert!(observed);
        assert!(
            read_complete_fixed_history(state.state_store.clone(), &transfer)
                .unwrap()
                .lifecycle()
                .settlement_observed()
                .is_some()
        );
        assert!(
            ui.browser_sessions
                .with_reconciled_completed_roots_at_cut::<()>(
                    &state.state_store,
                    &anchors,
                    &retired,
                    &session_id,
                    &expected,
                    policy,
                    &roots,
                    &[],
                    |_| panic!("stale head must not enter publication")
                )
                .unwrap()
                .is_none()
        );
        assert!(
            ui.browser_sessions
                .get_session(&session_id)
                .unwrap()
                .pending_transfer_attachments
                .contains_key(&transfer.operation_id)
        );
        let (cut, replay) = completed();
        let (anchors, retired) = cut.publication_cut(&state, &initial_session).unwrap();
        for _ in 0..2 {
            assert_eq!(
                ui.browser_sessions
                    .with_reconciled_completed_roots_at_cut(
                        &state.state_store,
                        &anchors,
                        &retired,
                        &session_id,
                        &expected,
                        policy,
                        &[(membership, &replay, cut.as_ref())],
                        &[],
                        |_| Ok(Some(()))
                    )
                    .unwrap(),
                Some(())
            );
        }
        let published = ui.browser_sessions.get_session(&session_id).unwrap();
        assert!(
            !published
                .pending_transfer_attachments
                .contains_key(&transfer.operation_id)
        );
        assert_eq!(published.presentations, initial_session.presentations);
        assert_eq!(
            published.next_attachment_generation,
            initial_session.next_attachment_generation
        );
        assert_eq!(
            published.next_surface_ordinal,
            initial_session.next_surface_ordinal
        );
        assert_eq!(
            published.attachments.len(),
            initial_session.attachments.len()
        );
        for (id, attachment) in &initial_session.attachments {
            assert!(std::sync::Arc::ptr_eq(
                attachment,
                &published.attachments[id]
            ));
        }
    }

    #[tokio::test]
    async fn both_retired_positive_endpoints_fold_after_real_maintenance_retirement() {
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            TransferSettlementSide, UiTransferHistoryEndpoint, UiTransferSettlementWriteOutcome,
            begin_historical_transfer_cut_read_at_endpoint, finish_historical_transfer_cut_read,
            read_complete_fixed_history, read_historical_transfer_cut_page,
        };
        let (_directory, mut state, transfer, source_initial, destination_initial, session_id) =
            activated_pair_observation_fixture_with_session(
                ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 1 },
                true,
            );
        let session_id = session_id.unwrap();
        state.threads.set_scheduler_db(
            state.scheduler_db.clone(),
            state.scheduler_runtime_gate.clone(),
            state.config.app_root.clone(),
        );
        let mut config = state.node_config.as_ref().clone();
        let mut route = config.routes[0].clone();
        route.id = "ui/seat/append".into();
        route.path = "/ui/api/seat/append".into();
        route.methods = std::collections::HashSet::from(["POST".into()]);
        route.response.source = Some("service:ui/seat/append".into());
        route.response.source_config = serde_json::json!({});
        route.limits.body_bytes_max = 65536;
        config.routes.retain(|existing| {
            existing.response.source.as_deref() != Some("service:ui/seat/append")
        });
        config.routes.push(route);
        state
            .threads
            .set_ui_seat_append_route_from_node_config(&config)
            .unwrap();
        state.node_config = Arc::new(config);
        for (seat, side) in [
            (&transfer.destination, TransferSettlementSide::Destination),
            (&transfer.source, TransferSettlementSide::Source),
        ] {
            assert!(
                state
                    .state_store
                    .claim_expired_seat_lease(&seat.thread_id, i64::MAX)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .reserve_claimed_ui_seat_transfer_discovery(&seat.thread_id)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .queue_claimed_ui_seat_transfer(&seat.thread_id, &transfer, side)
                    .unwrap()
            );
            let mut settled = false;
            for _ in 0..512 {
                if matches!(
                    state
                        .threads
                        .advance_claimed_ui_transfer_settlement_once()
                        .await
                        .unwrap(),
                    Some(
                        UiTransferSettlementWriteOutcome::Committed(_)
                            | UiTransferSettlementWriteOutcome::AlreadyRecorded(_)
                    )
                ) {
                    settled = true;
                    break;
                }
            }
            assert!(settled);
        }
        assert!(
            read_complete_fixed_history(state.state_store.clone(), &transfer)
                .unwrap()
                .lifecycle()
                .settlement_observed()
                .is_some()
        );
        // Both terminal transitions occur through the existing claimed/pin
        // owner, before either head disappears. Removal uses the real scheduler
        // inspection callback inside bounded maintenance, not a test pin sink.
        for seat in [&transfer.source, &transfer.destination] {
            let mut finalized = false;
            for _ in 0..512 {
                if state
                    .threads
                    .finalize_claimed_ui_seat_if_cleared(&seat.thread_id)
                    .await
                    .unwrap()
                {
                    finalized = true;
                    break;
                }
            }
            assert!(finalized);
        }
        let mut prefixes = BTreeMap::new();
        for _ in 0..512 {
            state
                .threads
                .advance_terminal_ui_seat_retirement("2100-01-01T00:00:00Z", false)
                .unwrap();
            let authority = state.state_store.pinned_state_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            for seat in [&transfer.source, &transfer.destination] {
                if let Some(prefix) = authority
                    .capture_retired_seat_positive_prefix(&seat.chain_root_id, &guard)
                    .unwrap()
                {
                    prefixes.insert(seat.chain_root_id.clone(), prefix);
                }
            }
            if prefixes.len() == 2 {
                break;
            }
        }
        assert_eq!(prefixes.len(), 2);
        for seat in [&transfer.source, &transfer.destination] {
            assert!(
                state
                    .state_store
                    .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)
                    .unwrap()
                    .is_none()
            );
        }
        let endpoints = prefixes
            .iter()
            .map(|(root, prefix)| {
                (
                    root.clone(),
                    UiTransferHistoryEndpoint::Retired(prefix.clone()),
                )
            })
            .collect();
        let local =
            UiTransferHistoryEndpoint::Retired(prefixes[&transfer.source.chain_root_id].clone());
        let mut reader = begin_historical_transfer_cut_read_at_endpoint(
            state.state_store.clone(),
            &local,
            std::slice::from_ref(&transfer),
            endpoints,
        )
        .unwrap();
        while !read_historical_transfer_cut_page(&mut reader).unwrap() {}
        let proof = Arc::new(finish_historical_transfer_cut_read(reader).unwrap());
        assert!(proof.current_anchors().is_empty());
        assert_eq!(proof.retired_prefixes().len(), 2);
        assert!(
            proof
                .lifecycle(&transfer.operation_id)
                .unwrap()
                .activated()
                .is_some()
        );
        let ui = crate::state::get_ui_state(&state).unwrap();
        let session = ui.browser_sessions.get_session(&session_id).unwrap();
        assert_eq!(
            session.presentations[&transfer.destination.presentation_id]
                .activated_transfer_reservations
                .len(),
            1
        );
        let imported = session
            .attachments
            .values()
            .filter(|attachment| attachment.binding_attachment_id != session.root_attachment_id)
            .map(|attachment| attachment.coordinate())
            .collect::<Vec<_>>();
        assert_eq!(imported.len(), 1);
        let cut = transfer_ingress::test_cut_from_completed_historical_proof(
            &state.state_store,
            proof.clone(),
            vec![
                session.presentations[&transfer.source.presentation_id].clone(),
                session.presentations[&transfer.destination.presentation_id].clone(),
            ],
        )
        .unwrap();
        let frame = cut
            .proof_frame(&state.state_store, &transfer.operation_id, 65536)
            .unwrap();
        assert_eq!(
            frame.source_anchor.event_hash,
            prefixes[&transfer.source.chain_root_id].tip().unwrap().0
        );
        assert_eq!(
            frame.destination_anchor.event_hash,
            prefixes[&transfer.destination.chain_root_id]
                .tip()
                .unwrap()
                .0
        );
        for (seat, initial) in [
            (&transfer.source, &source_initial),
            (&transfer.destination, &destination_initial),
        ] {
            let prefix = prefixes[&seat.chain_root_id].clone();
            let mut replay = FixedAnchorReplay::begin_at_retired_prefix(
                &state,
                &seat.presentation_id,
                prefix.clone(),
                initial,
                Some(cut.clone()),
            )
            .unwrap();
            loop {
                match replay.next_step(&state).unwrap() {
                    FixedAnchorReplayStep::Batch { .. } => {}
                    FixedAnchorReplayStep::Complete { .. } => break,
                    FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        panic!("completed all-retired proof lost phase coverage")
                    }
                }
            }
            assert!(replay.matches_completed_retired_prefix(&prefix));
            assert!(replay.retired_read.as_ref().unwrap().positions.is_empty());
            let frontier = replay.completed_composition_frontier(initial).unwrap();
            assert!(frontier.transfer_testimony_seen);
        }
        assert_eq!(
            state
                .state_store
                .with_retired_seat_read_prefixes(&[], proof.retired_prefixes(), || Ok(17))
                .unwrap(),
            Some(17)
        );
        let state = Arc::new(state);
        let value = crate::handlers::ui_session_current::handle(
            serde_json::json!({}),
            ryeos_app::handler_context::HandlerContext::new(
                format!("session:{session_id}"),
                Vec::new(),
                false,
            ),
            state.clone(),
        )
        .await
        .unwrap();
        let projected: ryeos_client_base::ui::BrowserSession =
            serde_json::from_value(value).unwrap();
        assert_eq!(projected.presentations.len(), 2);
        for coordinate in &imported {
            assert!(projected.binding_attachments.iter().any(|descriptor| {
                descriptor.binding_attachment_id == coordinate.binding_attachment_id
                    && descriptor.binding_generation == coordinate.binding_generation
                    && descriptor.binding_digest == coordinate.binding_digest
            }));
        }
        let current = crate::state::get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .get_session(&session_id)
            .unwrap();
        assert_eq!(
            current.presentations[&transfer.destination.presentation_id]
                .activated_transfer_reservations
                .len(),
            1
        );
        assert!(current.pending_transfer_attachments.is_empty());
    }

    #[tokio::test]
    async fn retired_positive_endpoint_frames_and_completed_projection_use_one_exact_cut() {
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            TransferSettlementSide, UiTransferSettlementWriteOutcome,
            begin_historical_transfer_cut_read, finish_historical_transfer_cut_read,
            read_complete_fixed_history, read_historical_transfer_cut_page,
        };
        // Real typed export and dedicated Prepare/Accept/Commit/Activate,
        // followed by the existing lifecycle settlement/finalization/Remove
        // owners. Admission callbacks in the seed are synthetic metadata;
        // this fixture does not qualify executable browser/session grants.
        let (_directory, state, transfer, source_initial, destination_initial) =
            activated_pair_observation_fixture(
                ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 1 },
            );
        state.threads.set_scheduler_db(
            state.scheduler_db.clone(),
            state.scheduler_runtime_gate.clone(),
            state.config.app_root.clone(),
        );
        let mut node_config = state.node_config.as_ref().clone();
        let mut append_route = node_config.routes[0].clone();
        append_route.id = "ui/seat/append".into();
        append_route.path = "/ui/api/seat/append".into();
        append_route.methods = std::collections::HashSet::from(["POST".into()]);
        append_route.response.source = Some("service:ui/seat/append".into());
        append_route.response.source_config = serde_json::json!({});
        append_route.limits.body_bytes_max = 65536;
        node_config.routes.retain(|existing| {
            existing.response.source.as_deref() != Some("service:ui/seat/append")
        });
        node_config.routes.push(append_route);
        // The stock helper has only launch/dispatch routes. Bind an explicit
        // bounded append route through the real lifecycle startup owner.
        state
            .threads
            .set_ui_seat_append_route_from_node_config(&node_config)
            .unwrap();
        for (seat, side) in [
            (&transfer.destination, TransferSettlementSide::Destination),
            (&transfer.source, TransferSettlementSide::Source),
        ] {
            assert!(
                state
                    .state_store
                    .claim_expired_seat_lease(&seat.thread_id, i64::MAX)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .reserve_claimed_ui_seat_transfer_discovery(&seat.thread_id)
                    .unwrap()
            );
            assert!(
                state
                    .threads
                    .queue_claimed_ui_seat_transfer(&seat.thread_id, &transfer, side)
                    .unwrap()
            );
            let mut committed = false;
            for _ in 0..512 {
                if matches!(
                    state
                        .threads
                        .advance_claimed_ui_transfer_settlement_once()
                        .await
                        .unwrap(),
                    Some(
                        UiTransferSettlementWriteOutcome::Committed(_)
                            | UiTransferSettlementWriteOutcome::AlreadyRecorded(_)
                    )
                ) {
                    committed = true;
                    break;
                }
            }
            assert!(
                committed,
                "finite signed fixture must settle through bounded turns"
            );
        }
        assert!(
            read_complete_fixed_history(state.state_store.clone(), &transfer)
                .unwrap()
                .lifecycle()
                .settlement_observed()
                .is_some()
        );
        let mut finalized = false;
        for _ in 0..512 {
            if state
                .threads
                .finalize_claimed_ui_seat_if_cleared(&transfer.source.thread_id)
                .await
                .unwrap()
            {
                finalized = true;
                break;
            }
        }
        assert!(finalized);
        let mut discovery = state
            .state_store
            .begin_terminal_seat_transfer_discovery(&transfer.source.thread_id)
            .unwrap()
            .unwrap();
        while !state
            .state_store
            .read_terminal_seat_transfer_discovery_page(&mut discovery, 64, 2 * 1024 * 1024)
            .unwrap()
        {}
        let scope = state
            .state_store
            .finish_terminal_seat_transfer_discovery(discovery)
            .unwrap();
        let pair = read_complete_fixed_history(state.state_store.clone(), &transfer).unwrap();
        let candidate = state
            .state_store
            .qualify_terminal_seat_transfer_retirement(scope, &[pair], &[])
            .unwrap();
        assert!(
            state
                .state_store
                .retire_qualified_terminal_seat(&candidate, "2100-01-01T00:00:00Z", false, |_| Ok(
                    0
                ))
                .unwrap()
                .is_some()
        );
        assert!(
            state
                .state_store
                .capture_thread_event_read_anchor(
                    &transfer.source.chain_root_id,
                    &transfer.source.thread_id
                )
                .unwrap()
                .is_none()
        );
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(
                &transfer.destination.chain_root_id,
                &transfer.destination.thread_id,
            )
            .unwrap()
            .unwrap();
        let incomplete = begin_historical_transfer_cut_read(
            state.state_store.clone(),
            &anchor,
            std::slice::from_ref(&transfer),
        )
        .unwrap();
        assert!(finish_historical_transfer_cut_read(incomplete).is_err());
        let mut history = begin_historical_transfer_cut_read(
            state.state_store.clone(),
            &anchor,
            std::slice::from_ref(&transfer),
        )
        .unwrap();
        while !read_historical_transfer_cut_page(&mut history).unwrap() {}
        let proof = Arc::new(finish_historical_transfer_cut_read(history).unwrap());
        assert_eq!(proof.retired_prefixes().len(), 1);
        assert_eq!(
            proof.retired_prefixes()[0].chain_root_id(),
            transfer.source.chain_root_id
        );
        assert_eq!(proof.current_anchors(), std::slice::from_ref(&anchor));
        let member = |initial: UiPresentationInitialization, seat: &TransferSeat| {
            crate::browser_session::PresentationMembership {
                presentation_id: initial.presentation_id.clone(),
                creation_request_id: initial.creation_request_id.clone(),
                root_coordinate: crate::browser_session::BindingAttachmentCoordinate {
                    binding_attachment_id: initial.root_binding.binding_attachment_id.clone(),
                    binding_generation: initial.root_binding.binding_generation,
                    binding_digest: initial.root_binding.binding_digest.clone(),
                },
                thread_id: Some(seat.thread_id.clone()),
                chain_root_id: Some(seat.chain_root_id.clone()),
                surfaces: Default::default(),
                activated_transfer_reservations: Default::default(),
                next_mount_id: initial.next_mount_id,
                initialization_digest: seat_payload_digest(
                    &serde_json::to_value(&initial).unwrap(),
                )
                .unwrap(),
                initialization: initial,
            }
        };
        let source_member = member(source_initial.clone(), &transfer.source);
        let destination_member = member(destination_initial.clone(), &transfer.destination);
        let mut changed = source_member.clone();
        changed.initialization.next_mount_id += 1;
        changed.initialization_digest =
            seat_payload_digest(&serde_json::to_value(&changed.initialization).unwrap()).unwrap();
        assert_eq!(changed.root_coordinate, source_member.root_coordinate);
        assert!(
            transfer_ingress::test_cut_from_completed_historical_proof(
                &state.state_store,
                proof.clone(),
                vec![changed, destination_member.clone()]
            )
            .is_err()
        );
        let cut = transfer_ingress::test_cut_from_completed_historical_proof(
            &state.state_store,
            proof.clone(),
            vec![source_member, destination_member],
        )
        .unwrap();
        let frame = cut
            .proof_frame(&state.state_store, &transfer.operation_id, 65536)
            .unwrap();
        assert_eq!(
            frame.source_anchor.event_hash,
            proof.retired_prefixes()[0].tip().unwrap().0
        );
        assert_eq!(
            frame.source_anchor.thread_seq,
            proof.retired_prefixes()[0].tip().unwrap().1
        );
        assert_eq!(
            frame.source_anchor.chain_seq,
            proof.retired_prefixes()[0].tip().unwrap().2
        );
        assert_eq!(frame.destination_anchor.event_hash, anchor.tip().unwrap().0);
        let retired_prefix = proof.retired_prefixes()[0].clone();
        let mut retired_replay = FixedAnchorReplay::begin_at_retired_prefix(
            &state,
            &transfer.source.presentation_id,
            retired_prefix.clone(),
            &source_initial,
            Some(cut.clone()),
        )
        .unwrap();
        loop {
            match retired_replay.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("completed historical cut must already cover retired testimony")
                }
            }
        }
        retired_replay
            .completed_composition_frontier(&source_initial)
            .unwrap();
        assert!(retired_replay.anchor.current().is_err());
        let consumed_index = retired_replay.retired_read.as_ref().unwrap();
        assert!(consumed_index.positions.is_empty());
        assert_eq!(consumed_index.positions.capacity(), 0);
        assert_eq!(consumed_index.remaining, 0);
        let terminal_hash = retired_prefix.tip().unwrap().0;
        let terminal = persisted(
            terminal_hash.to_owned(),
            state
                .state_store
                .retained_thread_event_by_hash(terminal_hash)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        // These reducer negatives start from a genuinely completed retired
        // prefix; altered DTOs are not admissions or alternate signed cuts.
        let reject_terminal = |candidate: PersistedEventRecord, current: bool| {
            let mut attempted = retired_replay.clone();
            if current {
                attempted.anchor = anchor.clone().into();
            }
            let mut published = Vec::new();
            assert!(attempted.apply_record(candidate, &mut published).is_err());
            assert!(published.is_empty());
        };
        reject_terminal(terminal.clone(), true);
        let mut wrong_hash = terminal.clone();
        wrong_hash.event_hash = Some("0".repeat(64));
        reject_terminal(wrong_hash, false);
        let mut suffix = terminal.clone();
        suffix.thread_seq += 1;
        suffix.chain_seq += 1;
        reject_terminal(suffix, false);
        let mut malformed = terminal.clone();
        malformed.payload["extra"] = serde_json::json!(true);
        reject_terminal(malformed, false);
        let mut wrong_status = terminal;
        wrong_status.payload["status"] = serde_json::json!("cancelled");
        reject_terminal(wrong_status, false);
        let mut replay = FixedAnchorReplay::begin_at_anchor(
            &state,
            &transfer.destination.presentation_id,
            &transfer.destination.chain_root_id,
            &transfer.destination.thread_id,
            anchor.clone(),
        )
        .unwrap();
        loop {
            match replay.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::RequiresPairedTransfer { anchor: requested } => {
                    assert_eq!(requested, anchor);
                    replay.attach_paired_cut(cut.clone()).unwrap();
                }
                FixedAnchorReplayStep::Complete { .. } => break,
            }
        }
        let completed = replay
            .completed_composition_frontier(&destination_initial)
            .unwrap();
        assert!(completed.transfer_testimony_seen);
        assert!(completed.require_ordinary_mutation().is_err());
        let mut called = false;
        assert_eq!(
            state
                .state_store
                .with_thread_event_read_mixed_cut(
                    &transfer.destination.chain_root_id,
                    proof.current_anchors(),
                    proof.retired_prefixes(),
                    || {
                        called = true;
                        Ok(())
                    }
                )
                .unwrap(),
            Some(())
        );
        assert!(called);
        // A changed surviving head cannot publish the historical projection;
        // the retired marker is still the original immutable dependency.
        state
            .state_store
            .append_events(
                &transfer.destination.chain_root_id,
                &transfer.destination.thread_id,
                &[ryeos_app::state_store::NewEventRecord {
                    event_type: "fixture.head_advance".into(),
                    storage_class: "indexed".into(),
                    payload: serde_json::json!({}),
                }],
            )
            .unwrap();
        called = false;
        assert!(
            state
                .state_store
                .with_thread_event_read_mixed_cut(
                    &transfer.destination.chain_root_id,
                    proof.current_anchors(),
                    proof.retired_prefixes(),
                    || {
                        called = true;
                        Ok(())
                    }
                )
                .unwrap()
                .is_none()
        );
        assert!(!called);
    }

    pub(crate) fn completed_cancellation_fixture() -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        Arc<VerifiedTransferSeatPrefix>,
        UiCompositionReplay,
        ryeos_client_base::ui::binding::UiSurfaceMembership,
        Arc<crate::browser_session::AdmittedBindingAttachment>,
    ) {
        completed_cancellation_fixture_with_retention(
            ryeos_state::objects::ThreadHistoryRetention::Durable,
        )
    }

    pub(crate) fn completed_cancellation_fixture_with_retention(
        retention: ryeos_state::objects::ThreadHistoryRetention,
    ) -> (
        tempfile::TempDir,
        AppState,
        Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
        Arc<VerifiedTransferSeatPrefix>,
        UiCompositionReplay,
        ryeos_client_base::ui::binding::UiSurfaceMembership,
        Arc<crate::browser_session::AdmittedBindingAttachment>,
    ) {
        let (directory, state, proof, digest) =
            freeze_fixture_options_with_retention(true, true, true, retention);
        let seat = proof.transfer().source.clone();
        let mut read = FixedAnchorReplay::begin_move_freeze_cancellation_admission(
            &state,
            &seat,
            &digest,
            proof.clone(),
        )
        .unwrap();
        loop {
            match read.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("scoped cancellation unexpectedly requires a positive phase")
                }
            }
        }
        let prefix = read.finish_transfer_admission().unwrap();
        let surface = prefix.replay.initialization.as_ref().unwrap().surfaces[0].clone();
        let projection = prefix.replay.composition.as_ref().unwrap().clone();
        let attachment = crate::browser_session::tests::test_surface_attachment();
        (
            directory, state, proof, prefix, projection, surface, attachment,
        )
    }

    #[test]
    fn scoped_cancellation_replays_real_paired_cut_and_cannot_seed_ordinary_admission() {
        let (_directory, state, proof, digest) = cancellation_fixture();
        let seat = proof.transfer().source.clone();
        let mut read = FixedAnchorReplay::begin_move_freeze_cancellation_admission(
            &state,
            &seat,
            &digest,
            proof.clone(),
        )
        .unwrap();
        let anchor = read.anchor.clone();
        loop {
            match read.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("scoped settlement is not an unpaired transfer")
                }
            }
        }
        assert_eq!(read.anchor, anchor);
        assert!(read.cancellation_seen);
        let prefix = read.finish_transfer_admission().unwrap();
        assert!(
            prefix
                .replay
                .composition
                .as_ref()
                .unwrap()
                .move_freezes
                .is_empty()
        );
        assert!(prefix.freeze_event(&proof.freeze().preparation).is_some());
        assert!(
            prefix
                .freeze_event(proof.freeze().completion.as_ref().unwrap())
                .is_some()
        );
        let response =
            super::super::super::transfer_ingress::retained_freeze_cancellation_response(
                &proof, &prefix,
            )
            .unwrap()
            .unwrap();
        assert_eq!(response["outcome"], "freeze_cancelled");
        assert_eq!(
            response["cancellation"],
            serde_json::to_value(proof.retained_cancellation().unwrap().0).unwrap()
        );
        assert_eq!(
            state
                .state_store
                .capture_thread_event_read_anchor("T-seat", "T-seat")
                .unwrap()
                .unwrap(),
            anchor.current().unwrap().clone()
        );
        let ordinary = FixedAnchorReplay::begin_transfer_admission(
            &state,
            &seat,
            anchor.current().unwrap().clone(),
            &digest,
            &[],
            Some(prefix),
        )
        .unwrap();
        assert!(!ordinary.initialization_seen);
        assert!(ordinary.cancellation_scope.is_none());
    }

    #[test]
    fn scoped_cancellation_refuses_wrong_hash_position_and_missing_receipted_witness() {
        let (_directory, state, proof, digest) = cancellation_fixture();
        let seat = proof.transfer().source.clone();
        let mut read = FixedAnchorReplay::begin_move_freeze_cancellation_admission(
            &state,
            &seat,
            &digest,
            proof.clone(),
        )
        .unwrap();
        let (evidence, _) = proof.retained_cancellation().unwrap();
        let page = state
            .state_store
            .replay_events("T-seat", Some("T-seat"), None, 64, 1024 * 1024)
            .unwrap();
        let mut cancellation = None;
        for record in page.events {
            if record.event_hash.as_deref() == Some(evidence.event_hash.as_str()) {
                cancellation = Some(record);
                break;
            }
            let thread_seq = u64::try_from(record.thread_seq).unwrap();
            read.apply_record(record, &mut Vec::new()).unwrap();
            read.previous_thread_seq = thread_seq;
        }
        let cancellation = cancellation.unwrap();
        assert_eq!(read.composition.as_ref().unwrap().move_freezes.len(), 1);
        for coordinate in ["hash", "position", "witness"] {
            let mut candidate = read.clone();
            let mut record = cancellation.clone();
            match coordinate {
                "hash" => record.event_hash = Some("f".repeat(64)),
                "position" => record.chain_seq += 1,
                "witness" => {
                    candidate
                        .verified_freeze_events
                        .remove(&proof.freeze().completion.as_ref().unwrap().event_hash);
                }
                _ => unreachable!(),
            }
            assert!(
                candidate.apply_record(record, &mut Vec::new()).is_err(),
                "{coordinate}"
            );
            assert!(!candidate.cancellation_seen);
            assert_eq!(
                candidate.composition.as_ref().unwrap().move_freezes.len(),
                1
            );
        }
        let mut missing_outcome = read.clone();
        missing_outcome.reached_tip = true;
        assert!(missing_outcome.finish_transfer_admission().is_err());
        read.apply_record(cancellation, &mut Vec::new()).unwrap();
        assert!(read.cancellation_seen);
        assert!(read.composition.as_ref().unwrap().move_freezes.is_empty());
    }

    #[test]
    fn scoped_pending_cancellation_releases_receipted_preparation_without_inventing_completion() {
        let (_directory, state, proof, digest) = cancellation_fixture_with_completion(false);
        assert!(proof.freeze().completion.is_none());
        let mut read = FixedAnchorReplay::begin_move_freeze_cancellation_admission(
            &state,
            &proof.transfer().source,
            &digest,
            proof.clone(),
        )
        .unwrap();
        assert_eq!(read.requested_freeze_events.len(), 1);
        loop {
            match read.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Batch { .. } => {}
                FixedAnchorReplayStep::Complete { .. } => break,
                FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("pending cancellation must use its exact settlement scope")
                }
            }
        }
        let prefix = read.finish_transfer_admission().unwrap();
        let replay = prefix.replay.composition.as_ref().unwrap();
        assert!(replay.move_freezes.is_empty());
        assert!(replay.pending.is_empty());
        assert_eq!(replay.settled.len(), 2); // subject completion plus cancelled intent, not invented freeze completion
        assert_eq!(prefix.replay.next_engine_seq, 3);
        let response =
            super::super::super::transfer_ingress::retained_freeze_cancellation_response(
                &proof, &prefix,
            )
            .unwrap()
            .unwrap();
        assert_eq!(response["outcome"], "freeze_cancelled");
        assert_eq!(
            response["cancellation"],
            serde_json::to_value(proof.retained_cancellation().unwrap().0).unwrap()
        );
    }

    #[tokio::test]
    async fn next_step_checks_real_transfer_family_link_before_typed_handoff() {
        let (_directory, state, mut replay, _transfer) = paged_transfer_fixture(false);
        assert!(matches!(
            replay.next_step(&state).unwrap(),
            FixedAnchorReplayStep::RequiresPairedTransfer { .. }
        ));
        // A pending handoff re-emits its fixed anchor without reading or
        // publishing anything. Schedule a fresh read at the still-unconsumed
        // transfer cursor, rather than testing that intentional early return.
        replay.awaiting_paired_handoff = false;
        replay.pending_handoff_events.clear();
        // Changing the predecessor must fail before the located transfer type
        // can request a new paired handoff.
        replay.previous_hash = Some("f".repeat(64));
        let error = replay
            .next_step(&state)
            .err()
            .expect("broken link must refuse");
        assert!(format!("{error}").contains("omits or changes signed thread history"));
        assert!(!replay.reached_tip);
    }

    #[tokio::test]
    async fn handed_off_unknown_family_is_not_a_discoverable_transfer_proof() {
        let (_directory, state, mut replay, _) = paged_transfer_fixture(false);
        let FixedAnchorReplayStep::RequiresPairedTransfer { anchor } =
            replay.next_step(&state).unwrap()
        else {
            panic!("expected exact-cut handoff");
        };
        let error = ryeos_app::thread_lifecycle::presentation_transfer::read_signed_seat_transfer_identity_page(
            &state.state_store, "T-seat", &anchor, None, 64, 2 * 1024 * 1024,
        ).err().expect("unknown testimony must refuse positive discovery");
        assert!(error.to_string().contains("unsupported testimony"));
        assert!(!replay.reached_tip);
    }

    #[tokio::test]
    async fn next_step_refuses_real_unreceipted_interval_before_transfer_family_handoff() {
        let (_directory, state, mut replay, transfer) = paged_transfer_fixture(true);
        let original_anchor = replay.anchor.clone();
        let error = replay
            .next_step(&state)
            .err()
            .expect("missing receipt must refuse");
        assert!(format!("{error}").contains("unreceipted producer interval"));
        assert_eq!(replay.anchor, original_anchor);
        assert_eq!(replay.pending_producer_events.len(), 1);
        assert_eq!(replay.pending_producer_events[0].event_type, "seat.facet");
        assert_eq!(replay.previous_hash, transfer.prev_thread_event_hash);
        assert!(!replay.reached_tip);
        assert_eq!(replay.next_engine_seq, 0);
    }

    #[test]
    fn paired_prefix_scope_cannot_be_replaced_by_another_operation() {
        let first = TransferEvidence {
            event_hash: "a".repeat(64),
            chain_seq: 3,
        };
        let second = TransferEvidence {
            event_hash: "b".repeat(64),
            chain_seq: 4,
        };
        assert!(paired_scope_covers_prefix(
            std::slice::from_ref(&first),
            &[first.clone(), second.clone()],
        ));
        assert!(!paired_scope_covers_prefix(
            std::slice::from_ref(&first),
            std::slice::from_ref(&second),
        ));
        let same_hash_wrong_position = TransferEvidence {
            chain_seq: 5,
            ..first.clone()
        };
        assert!(!paired_scope_covers_prefix(
            std::slice::from_ref(&first),
            &[same_hash_wrong_position],
        ));
    }

    #[test]
    fn private_admission_refuses_unproved_transfer_without_mutating_frontier() {
        let (_directory, mut replay) = initialized_replay();
        replay.transfer_admission = true;
        let event = transfer_record(&replay);
        let frontier = replay.next_engine_seq;
        let revision = replay.composition.as_ref().unwrap().revision;
        let mut publishable = Vec::new();
        let error = replay.apply_record(event, &mut publishable).unwrap_err();
        assert!(format!("{error}").contains("outside its paired proof"));
        assert!(publishable.is_empty());
        assert!(replay.transfer_witnesses.is_empty());
        assert!(!replay.transfer_testimony_seen);
        assert_eq!(replay.next_engine_seq, frontier);
        assert_eq!(replay.composition.as_ref().unwrap().revision, revision);
    }

    #[test]
    fn private_transfer_witness_does_not_advance_producer_frontier() {
        let (_directory, mut replay) = initialized_replay();
        replay.transfer_admission = true;
        let event = transfer_record(&replay);
        replay.required_transfer_witnesses.push(TransferEvidence {
            event_hash: event.event_hash.clone().unwrap(),
            chain_seq: event.chain_seq as u64,
        });
        let frontier = replay.next_engine_seq;
        let revision = replay.composition.as_ref().unwrap().revision;
        let mut publishable = Vec::new();
        replay.apply_record(event, &mut publishable).unwrap();
        assert_eq!(replay.next_engine_seq, frontier);
        assert_eq!(replay.composition.as_ref().unwrap().revision, revision);
        assert!(replay.transfer_testimony_seen);
        assert_eq!(publishable.len(), 1);

        let mut stale = transfer_record(&replay);
        stale.payload["seq"] = serde_json::json!(frontier + 1);
        assert!(replay.apply_record(stale, &mut Vec::new()).is_err());
        assert_eq!(replay.next_engine_seq, frontier);
    }

    #[test]
    fn required_transfer_witness_survives_acceleration_ring_eviction() {
        // Synthetic fold-only coverage of the acceleration budget. The app
        // phase reader separately rejects repeated inbound commitments; this
        // fixture is not a valid multi-phase pair history or admission proof.
        let (_directory, mut replay) = initialized_replay();
        replay.transfer_admission = true;
        let event = transfer_record(&replay);
        let required = TransferEvidence {
            event_hash: event.event_hash.clone().unwrap(),
            chain_seq: event.chain_seq as u64,
        };
        replay.required_transfer_witnesses.push(required.clone());
        replay.apply_record(event, &mut Vec::new()).unwrap();
        for offset in 0..TRANSFER_PREFIX_WITNESSES {
            let mut later = transfer_record(&replay);
            later.event_hash = Some(format!("{:064x}", offset + 1));
            later.chain_seq = 8 + offset as i64;
            replay.required_transfer_witnesses.push(TransferEvidence {
                event_hash: later.event_hash.clone().unwrap(),
                chain_seq: later.chain_seq as u64,
            });
            replay.apply_record(later, &mut Vec::new()).unwrap();
        }
        assert_eq!(replay.transfer_witnesses.len(), TRANSFER_PREFIX_WITNESSES);
        assert!(!replay.transfer_witnesses.contains(&required));
        assert!(replay.verified_required_witnesses.contains(&required));
    }

    #[test]
    fn transfer_prefix_requires_completion_and_every_exact_witness() {
        let (_directory, mut replay) = initialized_replay();
        replay.transfer_admission = true;
        let initial = replay.initialization.as_ref().unwrap();
        replay.admission_initialization_digest = Some(
            ryeos_client_base::ui::seat_payload_digest(&serde_json::to_value(initial).unwrap())
                .unwrap(),
        );
        assert!(replay.clone().finish_transfer_admission().is_err());
        replay.reached_tip = true;
        let witness = TransferEvidence {
            event_hash: "e".repeat(64),
            chain_seq: 7,
        };
        replay.required_transfer_witnesses.push(witness.clone());
        assert!(replay.clone().finish_transfer_admission().is_err());
        replay.verified_required_witnesses.push(witness);
        let mut unfinished_batch = replay.clone();
        unfinished_batch.pending_producer_events.push(record(
            &"d".repeat(64),
            6,
            Some(&"c".repeat(64)),
        ));
        assert!(unfinished_batch.finish_transfer_admission().is_err());
        let mut changed_initialization = replay.clone();
        changed_initialization.admission_initialization_digest = Some("0".repeat(64));
        assert!(changed_initialization.finish_transfer_admission().is_err());
        let mut missing_freeze = replay.clone();
        missing_freeze
            .requested_freeze_events
            .insert("f".repeat(64), 8);
        assert!(missing_freeze.finish_transfer_admission().is_err());
        let prefix = replay.finish_transfer_admission().unwrap();
        assert_eq!(prefix.replay.required_transfer_witnesses.len(), 1);
        assert!(prefix.replay.verified_required_witnesses.is_empty());
    }

    #[test]
    fn freeze_witness_request_refuses_unbounded_repeated_and_malformed_coordinates() {
        use ryeos_ui_presentation_contract::transfer::TransferSeat;
        let (_directory, state, replay, _transfer) = paged_transfer_fixture(false);
        let seat = TransferSeat {
            presentation_id: PRESENTATION.into(),
            thread_id: "T-seat".into(),
            chain_root_id: "T-seat".into(),
            producer_incarnation: "a".repeat(64),
            revision: 0,
        };
        let evidence = TransferEvidence {
            event_hash: "a".repeat(64),
            chain_seq: 1,
        };
        let begin = |freeze_events: &[TransferEvidence]| {
            FixedAnchorReplay::begin_transfer_admission_with_freeze(
                &state,
                &seat,
                replay.anchor.current().unwrap().clone(),
                &"b".repeat(64),
                &[],
                freeze_events,
                None,
            )
        };
        assert!(begin(&vec![evidence.clone(); MAX_REQUESTED_FREEZE_EVENTS + 1]).is_err());
        assert!(begin(&[evidence.clone(), evidence.clone()]).is_err());
        let malformed = TransferEvidence {
            event_hash: "A".repeat(64),
            ..evidence
        };
        assert!(begin(&[malformed]).is_err());
    }

    #[test]
    fn unsupported_freeze_cancellation_cannot_be_skipped_by_private_replay() {
        let (_directory, mut replay) = initialized_replay();
        replay.transfer_admission = true;
        let mut cancellation = record(&"f".repeat(64), 7, Some(&"e".repeat(64)));
        cancellation.event_type =
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
                .into();
        let mut publishable = Vec::new();
        assert!(replay.apply_record(cancellation, &mut publishable).is_err());
        assert!(publishable.is_empty());
    }

    #[test]
    fn exact_unpaired_transfer_is_refused_without_publication() {
        let (_directory, mut replay) = initialized_replay();
        assert!(replay.completed_paired_projection().is_err());
        let event = transfer_record(&replay);
        let mut publishable = Vec::new();
        let error = replay.apply_record(event, &mut publishable).unwrap_err();
        assert!(format!("{error}").contains("complete two-seat replay proof"));
        assert!(publishable.is_empty());
        assert!(!replay.reached_tip);
        replay.reached_tip = true;
        assert!(
            replay.completed_paired_projection().is_err(),
            "a forged local tip without a complete paired cut cannot authorize cleanup"
        );
    }

    #[test]
    fn live_reproof_wake_requires_closed_owned_transfer_shape() {
        let (_directory, replay) = initialized_replay();
        let event = transfer_record(&replay);
        assert!(validate_live_transfer_wake(&event).is_ok());
        let mut unknown = event.clone();
        unknown.event_type = "seat.presentation_transfer.v999".into();
        assert!(validate_live_transfer_wake(&unknown).is_err());
        let mut foreign = event.clone();
        foreign.thread_id = "T-foreign".into();
        assert!(validate_live_transfer_wake(&foreign).is_err());
        let mut malformed = event;
        malformed.payload = serde_json::json!({});
        assert!(validate_live_transfer_wake(&malformed).is_err());
    }

    #[test]
    fn paired_handoff_preserves_cut_without_completion_or_frontier_advance() {
        let (_directory, mut replay) = initialized_replay();
        let event = transfer_record(&replay);
        let frontier = replay.next_engine_seq;
        let position = replay.after_chain_seq;
        let handoff = replay.paired_handoff(&event.event_type).unwrap().unwrap();
        let FixedAnchorReplayStep::RequiresPairedTransfer { anchor } = handoff else {
            panic!("expected paired reconstruction, not publication");
        };
        assert_eq!(anchor.tip(), replay.anchor.tip());
        assert_eq!(anchor.chain_root_id(), replay.chain_root_id.as_str());
        assert_eq!(anchor.thread_id(), replay.thread_id.as_str());
        assert_eq!(replay.next_engine_seq, frontier);
        assert_eq!(replay.after_chain_seq, position);
        assert!(!replay.reached_tip);
        assert!(replay.paired_handoff("thread_notice").unwrap().is_none());
        replay.transfer_admission = true;
        assert!(replay.paired_handoff(&event.event_type).unwrap().is_none());
    }

    #[test]
    fn paired_handoff_refuses_an_unreceipted_producer_interval() {
        let (_directory, mut replay) = initialized_replay();
        let event = transfer_record(&replay);
        replay
            .pending_producer_events
            .push(record(&"d".repeat(64), 6, Some(&"c".repeat(64))));
        assert!(replay.paired_handoff(&event.event_type).is_err());
        assert_eq!(replay.pending_producer_events.len(), 1);
        assert!(!replay.reached_tip);
    }

    #[test]
    fn receipt_releases_structural_edit_before_transfer_witness_is_checked() {
        use ryeos_client_base::ui::admission_operation::{
            COMPLETED_EVENT, PREPARED_EVENT, UiPreparedPresentationOperation,
            UiPreparedPresentationOperationBody, UiSeatAppendIdentity,
        };
        use ryeos_client_base::ui::attachment::SelectionAttachment;
        use ryeos_client_base::ui::binding::UiPresentationProducer;
        use ryeos_client_base::ui::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
        };
        use sha2::{Digest, Sha256};
        let (_directory, mut replay) = initialized_replay();
        let composition = replay.composition.as_ref().unwrap();
        let initial = replay.initialization.as_ref().unwrap();
        let mount = composition
            .composition
            .dialogs()
            .values()
            .next()
            .unwrap()
            .view()
            .instance_key()
            .clone();
        let attachment = SelectionAttachment::Pinned {
            values: std::collections::BTreeMap::new(),
            fingerprint: format!("{:x}", Sha256::digest(b"{}")),
        };
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: String::new(),
            producer: UiPresentationProducer {
                presentation_id: PRESENTATION.into(),
                thread_id: "T-seat".into(),
                producer_incarnation: replay
                    .control
                    .as_ref()
                    .unwrap()
                    .producer_incarnation
                    .clone(),
            },
            root_binding: initial.root_binding.clone(),
            request_bounds: initial.binding_attachments[0].binding_request_bounds,
            body: UiPreparedPresentationOperationBody::Composition {
                request: UiCompositionRequest {
                    schema: 1,
                    presentation_id: composition.composition.id().clone(),
                    operation_id: String::new(),
                    expected_revision: 0,
                    expected_next_mount_id: composition.next_mount_id,
                    owner: CompositionOwner::Shared {
                        binding: initial.root_binding.clone(),
                    },
                    mutation: CompositionMutation::SetSubject {
                        mount: mount.clone(),
                        attachment,
                    },
                },
            },
        };
        prepared.assign_identity().unwrap();
        let completed = composition
            .expected_composition_completion(&prepared, prepared.request_bounds.max_request_bytes)
            .unwrap();
        let semantic = serde_json::json!([
            {"engine_seq":0,"event_type":PREPARED_EVENT,"payload":prepared},
            {"engine_seq":1,"event_type":COMPLETED_EVENT,"payload":completed},
        ]);
        let mut publishable = Vec::new();
        for (index, kind) in [PREPARED_EVENT, COMPLETED_EVENT].into_iter().enumerate() {
            let mut event = record(
                &format!("{:064x}", index + 10),
                4 + index as i64,
                Some(&format!("{:064x}", index + 9)),
            );
            event.chain_root_id = "T-seat".into();
            event.storage_class = "durable".into();
            event.event_type = kind.into();
            event.payload = serde_json::json!({"seq":index,"payload":semantic[index]["payload"]});
            replay.apply_record(event, &mut publishable).unwrap();
        }
        assert!(publishable.is_empty());
        assert_eq!(replay.next_engine_seq, 0);
        assert_eq!(replay.composition.as_ref().unwrap().revision, 0);
        let mut interrupted = replay.clone();
        let error = interrupted
            .apply_record(transfer_record(&replay), &mut Vec::new())
            .unwrap_err();
        assert!(format!("{error}").contains("unreceipted producer batch"));
        let digest =
            ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&semantic).unwrap();
        let identity = UiSeatAppendIdentity {
            presentation_id: PRESENTATION.into(),
            chain_root_id: "T-seat".into(),
            thread_id: "T-seat".into(),
            producer_incarnation: replay
                .control
                .as_ref()
                .unwrap()
                .producer_incarnation
                .clone(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest.clone(),
        };
        let receipt = SeatAppendReceipt {
            schema_version:
                ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: identity.producer_incarnation.clone(),
            operation_id: identity.derive_identity().unwrap(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest,
        };
        let mut receipt_record = record(&"d".repeat(64), 6, Some(&format!("{:064x}", 11)));
        receipt_record.chain_root_id = "T-seat".into();
        receipt_record.event_type = SEAT_APPEND_RECEIPT_EVENT.into();
        receipt_record.payload = serde_json::to_value(receipt).unwrap();
        replay
            .apply_record(receipt_record, &mut publishable)
            .unwrap();
        assert_eq!(publishable.len(), 3);
        assert_eq!(publishable[0].event_type, PREPARED_EVENT);
        assert_eq!(publishable[1].event_type, COMPLETED_EVENT);
        assert_eq!(publishable[2].event_type, SEAT_APPEND_RECEIPT_EVENT);
        assert!(replay.pending_producer_events.is_empty());
        assert_eq!(replay.next_engine_seq, 2);
        assert_eq!(replay.composition.as_ref().unwrap().revision, 1);
        assert!(
            replay
                .composition
                .as_ref()
                .unwrap()
                .subjects
                .contains_key(&mount)
        );
        let mut stale = transfer_record(&replay);
        stale.payload["payload"]["transfer"]["destination"]["revision"] = serde_json::json!(0);
        let error = replay
            .clone()
            .apply_record(stale, &mut Vec::new())
            .unwrap_err();
        assert!(format!("{error}").contains("transfer witness differs"));
        let mut transferred = Vec::new();
        let error = replay
            .apply_record(transfer_record(&replay), &mut transferred)
            .unwrap_err();
        assert!(format!("{error}").contains("complete two-seat replay proof"));
        assert!(transferred.is_empty());
    }

    #[test]
    fn transfer_rejects_stale_sequence_revision_and_incarnation_before_pairing() {
        let (_directory, replay) = initialized_replay();
        for field in ["sequence", "revision", "incarnation"] {
            let mut candidate = replay.clone();
            let mut event = transfer_record(&candidate);
            match field {
                "sequence" => event.payload["seq"] = serde_json::json!(1),
                "revision" => {
                    event.payload["payload"]["transfer"]["destination"]["revision"] =
                        serde_json::json!(1)
                }
                _ => {
                    event.payload["payload"]["transfer"]["destination"]["producer_incarnation"] =
                        serde_json::json!("c".repeat(64))
                }
            }
            let mut publishable = Vec::new();
            let error = candidate.apply_record(event, &mut publishable).unwrap_err();
            assert!(
                format!("{error}").contains("transfer witness differs"),
                "{field}: {error}"
            );
            assert!(publishable.is_empty());
        }
    }

    #[test]
    fn lifecycle_versions_and_unknown_transfer_family_never_publish() {
        let (_directory, replay) = initialized_replay();
        for kind in [
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
            ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT,
            "contract.ryeos_ui.presentation_transfer.future",
        ] {
            let mut candidate = replay.clone();
            let mut event = transfer_record(&candidate);
            event.event_type = kind.into();
            let mut publishable = Vec::new();
            let error = candidate.apply_record(event, &mut publishable).unwrap_err();
            assert!(format!("{error}").contains("complete two-seat replay proof"));
            assert!(publishable.is_empty());
        }
    }

    fn record(hash: &str, sequence: i64, previous: Option<&str>) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 0,
            event_hash: Some(hash.to_owned()),
            chain_root_id: "T-root".into(),
            chain_seq: sequence,
            thread_id: "T-seat".into(),
            thread_seq: sequence,
            event_type: "thread_created".into(),
            storage_class: "indexed".into(),
            ts: "2026-10-06T00:00:00Z".into(),
            prev_chain_event_hash: previous.map(str::to_owned),
            prev_thread_event_hash: previous.map(str::to_owned),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn missing_projection_row_cannot_become_a_noop_gap() {
        let retained = record("b", 3, Some("a"));
        let locator = retained.clone();
        assert!(!fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
    }

    #[test]
    fn projection_payload_and_event_type_are_not_membership() {
        let retained = record("b", 2, Some("a"));
        let mut locator = retained.clone();
        locator.payload = serde_json::json!({"forged": true});
        locator.event_type = "seat.facet".into();
        assert!(fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
        locator.thread_seq = 3;
        assert!(!fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
    }

    #[test]
    fn live_suffix_never_treats_control_as_own_append() {
        let mut event = record("a", 1, None);
        event.event_type = SEAT_PRODUCER_EVENT.into();
        assert!(live_receipt(&event).is_err());
        event.event_type = SEAT_APPEND_RECEIPT_EVENT.into();
        assert!(live_receipt(&event).unwrap());
        event.event_type = format!("{SEAT_EVENT_PREFIX}test");
        assert!(!live_receipt(&event).unwrap());
        event.event_type = ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT.into();
        assert!(live_receipt(&event).is_err());
        event.event_type =
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT.into();
        assert!(live_receipt(&event).is_err());
        event.event_type =
            ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT
                .into();
        assert!(live_receipt(&event).is_err());
    }
}
