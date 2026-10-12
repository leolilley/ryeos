//! Seat state as a deterministic fold over seat events.
//!
//! The seat is a thread; its state (input route, selection, watch set) is
//! a last-writer-wins fold over `seat.facet` events on the seat thread's
//! braid, ordered by sequence. This module is the engine-side fold plus a
//! local append log used while the engine itself holds authority — the
//! event shape matches the seat-thread braid so swapping authority to the
//! daemon is a transport change, never a representation change.
//!
//! Facet keys are an open vocabulary. Engine lenses read only the
//! engine-reserved keys below; content-declared affordances may write any
//! key outside the reserved set (e.g. `tile.<id>.filter`). Unknown keys
//! fold harmlessly.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Engine-reserved facet keys. Engine lenses read only these.
pub const KEY_INPUT_ROUTE: &str = "input.route";
pub const KEY_NAVIGATION_DESTINATION: &str = "navigation.destination";
pub const KEY_SELECTION: &str = "selection";
pub const KEY_WATCH: &str = "watch";
pub const KEY_ACTIVE_SURFACE: &str = "presentation.active_surface";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSurfaceSelection {
    pub surface_instance_id: crate::ids::SurfaceInstanceId,
    pub binding: super::binding::UiBindingAttachmentCoordinate,
}

/// Storage coordinate for the selection owned by one open view set.
///
/// `selection` remains the logical name used by signed view definitions. The
/// client derives this key at the seat boundary so two open compositions can
/// inspect independently without inventing product-specific facet names.
pub fn selection_facet_key(view_set_id: crate::ids::ViewSetId) -> String {
    format!("{KEY_SELECTION}\u{1f}view-set:{}", view_set_id.0)
}

pub fn selection_storage_key(
    view_set_id: crate::ids::ViewSetId,
    logical_facet: &str,
) -> Option<String> {
    let suffix = logical_facet.strip_prefix(KEY_SELECTION)?;
    if !suffix.is_empty() && !suffix.starts_with('.') {
        return None;
    }
    Some(format!("{}{suffix}", selection_facet_key(view_set_id)))
}

pub fn parse_selection_storage_key(key: &str) -> Option<(crate::ids::ViewSetId, String)> {
    let rest = key.strip_prefix(&format!("{KEY_SELECTION}\u{1f}view-set:"))?;
    let split = rest.find('.').unwrap_or(rest.len());
    let id = rest[..split].parse::<u64>().ok()?;
    let suffix = &rest[split..];
    Some((
        crate::ids::ViewSetId::new(id),
        format!("{KEY_SELECTION}{suffix}"),
    ))
}

/// One local or retained seat event. `seq` is provisional local order until
/// authoritative replay supplies chain order. It is NEVER the producer-event
/// engine sequence used to reserve or authenticate a preparation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeatEvent {
    pub seq: u64,
    /// Original retained producer position. Provisional events reserve it via
    /// the core's authenticated frontier; initialization has no such position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_engine_seq: Option<u64>,
    #[serde(flatten)]
    pub kind: SeatEventKind,
}

/// Authoritative envelope returned by the retained journal owner. A provisional
/// producer event deliberately has no evidence coordinate: its local sequence
/// and JSON serialization cannot manufacture a canonical event hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSeatEvent {
    pub event: SeatEvent,
    pub evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence,
    pub thread_id: String,
    pub chain_root_id: String,
}

/// Decode evidence only from an authoritative history/append response. This is
/// a transport shape check, not signature or chain-membership authentication;
/// those remain with the daemon's existing retained-history owner.
pub fn retained_seat_event_with_evidence(
    value: &Value,
) -> Result<Option<RetainedSeatEvent>, String> {
    // Transfer phases are not ordinary seat events. Until the complete
    // two-seat testimony lane can derive exact-instance fences, a client
    // must refuse them rather than silently replaying past Prepared/Accepted.
    // Keep the generic retained_seat_event decoder's foreign-event behavior
    // for its existing non-transfer callers.
    if value
        .get("event_type")
        .and_then(Value::as_str)
        .is_some_and(|event_type| {
            ryeos_ui_presentation_contract::transfer::is_transfer_testimony(event_type)
                || event_type == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
        })
    {
        return Err("retained transfer testimony requires the authenticated two-seat lane".into());
    }
    let Some(event) = retained_seat_event(value)? else {
        return Ok(None);
    };
    if event.seq == u64::MAX {
        return Err("retained seat sequence leaves no append frontier".into());
    }
    let coordinate = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(|| format!("retained seat event omitted its {key}"))
    };
    let thread_id = coordinate("thread_id")?;
    let chain_root_id = coordinate("chain_root_id")?;
    let event_hash = value
        .get("event_hash")
        .and_then(Value::as_str)
        .filter(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or("retained seat event omitted its canonical event hash")?;
    let evidence = ryeos_ui_presentation_contract::transfer::TransferEvidence {
        event_hash: event_hash.to_owned(),
        chain_seq: event.seq,
    };
    Ok(Some(RetainedSeatEvent {
        event,
        evidence,
        thread_id,
        chain_root_id,
    }))
}

/// Closed presentation journal vocabulary. Foreign daemon events outside this
/// contract are ignored by the decoder; malformed owned events refuse replay.
/// Facet keys remain an open vocabulary within the typed facet payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "event_type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SeatEventKind {
    #[serde(rename = "ui_seat.presentation_initialized.v1")]
    PresentationInitialized(super::presentation_initialization::UiPresentationInitializationRecord),
    #[serde(rename = "seat.surface.closed")]
    SurfaceClosed(UiSurfaceClosure),
    #[serde(rename = "seat.facet")]
    Facet { key: String, value: Value },
    #[serde(rename = "seat.presentation_operation.prepared")]
    PresentationOperationPrepared(super::admission_operation::UiPreparedPresentationOperation),
    #[serde(rename = "seat.presentation_operation.completed")]
    PresentationOperationCompleted(super::admission_operation::UiCompletedPresentationOperation),
}

/// Decode authoritative retained history. Chain order is not the producer's
/// local sequence. Never substitute a local payload sequence for a missing
/// chain coordinate, or silently discard malformed events in our contract.
pub fn retained_seat_event(event: &Value) -> Result<Option<SeatEvent>, String> {
    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .ok_or("retained event omitted its type")?;
    if !event_type.starts_with("seat.")
        && event_type != super::presentation_initialization::PRESENTATION_INITIALIZED_EVENT
    {
        return Ok(None);
    }
    let payload = event
        .get("payload")
        .ok_or("retained seat event omitted its payload")?;
    let content =
        if event_type == super::presentation_initialization::PRESENTATION_INITIALIZED_EVENT {
            payload
        } else {
            payload
                .get("payload")
                .ok_or("retained producer event omitted its wrapper")?
        };
    let kind: SeatEventKind =
        serde_json::from_value(serde_json::json!({"event_type":event_type,"payload":content}))
            .map_err(|error| format!("invalid retained seat event: {error}"))?;
    let seq = event
        .get("chain_seq")
        .and_then(Value::as_u64)
        .ok_or("retained seat event omitted its exact chain sequence")?;
    let producer_engine_seq = if matches!(&kind, SeatEventKind::PresentationInitialized(_)) {
        None
    } else {
        Some(
            payload
                .get("seq")
                .and_then(Value::as_u64)
                .ok_or("retained producer event omitted its original engine sequence")?,
        )
    };
    if let SeatEventKind::PresentationOperationPrepared(operation) = &kind {
        operation.validate().map_err(str::to_owned)?;
        if producer_engine_seq != Some(operation.prepared_engine_seq) {
            return Err("retained preparation differs from its original engine position".into());
        }
    }
    Ok(Some(SeatEvent {
        seq,
        producer_engine_seq,
        kind,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSurfaceClosure {
    pub presentation_id: String,
    pub surface_instance_id: String,
    pub binding: super::binding::UiBindingAttachmentCoordinate,
}

pub const SURFACE_CLOSED_EVENT: &str = "seat.surface.closed";

impl UiSurfaceClosure {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !super::admission_operation::canonical_operation_id(&self.presentation_id)
            || !super::admission_operation::canonical_operation_id(&self.surface_instance_id)
            || self.binding.binding_attachment_id.is_empty()
            || self.binding.binding_generation == 0
            || self.binding.binding_digest.is_empty()
        {
            return Err("Surface closure identity is invalid");
        }
        Ok(())
    }
}

/// Append-ordered local seat log (slice-1 staging for the seat thread).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeatLog {
    events: Vec<SeatEvent>,
    next_seq: u64,
    /// Identity association only, never retained proof or writer permission.
    /// Preserve it through local serialization to prevent mixed-seat logs.
    retained_owner: Option<(String, String)>,
    /// Only authoritative transport insertion populates this derived cache.
    /// Local serialized logs cannot supply testimony for transfer settlement.
    #[serde(skip)]
    retained_evidence: BTreeMap<u64, ryeos_ui_presentation_contract::transfer::TransferEvidence>,
}

impl SeatLog {
    pub fn evidence_at_sequence(
        &self,
        seq: u64,
    ) -> Option<&ryeos_ui_presentation_contract::transfer::TransferEvidence> {
        self.retained_evidence.get(&seq)
    }

    pub fn append_retained(
        &mut self,
        expected_thread_id: &str,
        expected_chain_root_id: &str,
        retained: RetainedSeatEvent,
    ) -> Result<SeatReplayOutcome, String> {
        let owner = (
            expected_thread_id.to_owned(),
            expected_chain_root_id.to_owned(),
        );
        if self
            .retained_owner
            .as_ref()
            .is_some_and(|previous| previous != &owner)
        {
            return Err("retained log belongs to another seat".into());
        }
        if retained.thread_id != expected_thread_id
            || retained.chain_root_id != expected_chain_root_id
            || expected_thread_id.is_empty()
            || expected_chain_root_id.is_empty()
            || retained.event.seq == u64::MAX
            || retained.event.seq != retained.evidence.chain_seq
            || retained.evidence.event_hash.len() != 64
            || !retained
                .evidence
                .event_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("retained seat evidence coordinate is invalid".into());
        }
        if let Some(previous) = self.retained_evidence.get(&retained.event.seq)
            && (previous != &retained.evidence
                || self.event_at_sequence(retained.event.seq) != Some(&retained.event))
        {
            return Err("authoritative seat testimony contradicts retained history".into());
        }
        if self.retained_evidence.iter().any(|(sequence, evidence)| {
            *sequence != retained.event.seq && evidence.event_hash == retained.evidence.event_hash
        }) {
            return Err("retained event hash is reused at another chain position".into());
        }
        let sequence = retained.event.seq;
        let outcome = self.append_replayed(retained.event);
        self.retained_evidence.insert(sequence, retained.evidence);
        self.retained_owner = Some(owner);
        Ok(outcome)
    }
    pub fn event_at_sequence(&self, seq: u64) -> Option<&SeatEvent> {
        self.events
            .binary_search_by_key(&seq, |event| event.seq)
            .ok()
            .map(|index| &self.events[index])
    }
    pub fn next_sequence(&self) -> u64 {
        self.next_seq
    }
    pub fn append(&mut self, kind: SeatEventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(SeatEvent {
            seq,
            producer_engine_seq: None,
            kind,
        });
        seq
    }

    pub fn append_facet(&mut self, key: impl Into<String>, value: Value) -> u64 {
        self.append(SeatEventKind::Facet {
            key: key.into(),
            value,
        })
    }

    pub fn append_replayed(&mut self, event: SeatEvent) -> SeatReplayOutcome {
        // A caller without the authoritative envelope cannot keep an old hash
        // attached to substituted bytes. append_retained reinstalls evidence
        // only after the exact contradiction checks above.
        if self
            .event_at_sequence(event.seq)
            .is_some_and(|previous| previous != &event)
        {
            self.retained_evidence.remove(&event.seq);
        }
        self.next_seq = self.next_seq.max(event.seq.saturating_add(1));
        match self
            .events
            .binary_search_by_key(&event.seq, |retained| retained.seq)
        {
            // Durable replay is authoritative for an already-present sequence
            // (for example a startup baseline emitted before reattachment).
            Ok(index) if self.events[index] == event => SeatReplayOutcome::Identical { index },
            Ok(index) => {
                self.events[index] = event;
                SeatReplayOutcome::Replaced { index }
            }
            // Replay pages can arrive after provisional local mutations. Keep
            // the braid ordered by authority sequence rather than arrival time.
            Err(index) => {
                self.events.insert(index, event);
                SeatReplayOutcome::Inserted { index }
            }
        }
    }

    pub fn events(&self) -> &[SeatEvent] {
        &self.events
    }

    /// Called only by the exact ACK owner after semantic/engine validation.
    /// Canonical positions replace the captured interval; only an unretained
    /// provisional suffix may move to keep local order after that interval.
    pub(crate) fn adopt_acknowledged_positions(
        &mut self,
        start: usize,
        canonical: &[RetainedSeatEvent],
        thread_id: &str,
        root_id: &str,
    ) -> Result<(), String> {
        if canonical.is_empty() {
            return Err("canonical ACK interval empty".into());
        }
        let end = start
            .checked_add(canonical.len())
            .ok_or("canonical ACK interval exhausted")?;
        if end > self.events.len() {
            return Err("canonical ACK local interval absent".into());
        }
        let first = canonical[0].event.seq;
        let last = canonical.last().unwrap().event.seq;
        if start > 0 && self.events[start - 1].seq >= first {
            return Err("canonical ACK does not follow acknowledged history".into());
        }
        if canonical
            .windows(2)
            .any(|pair| pair[0].event.seq.checked_add(1) != Some(pair[1].event.seq))
        {
            return Err("canonical ACK interval is not contiguous".into());
        }
        if self.events[start..]
            .iter()
            .any(|event| self.retained_evidence.contains_key(&event.seq))
        {
            return Err("canonical ACK cannot reposition retained testimony".into());
        }
        let tail_count = u64::try_from(self.events.len() - end)
            .map_err(|_| "canonical ACK local tail exhausted")?;
        let next = last
            .checked_add(tail_count)
            .and_then(|seq| seq.checked_add(1))
            .ok_or("canonical ACK native frontier exhausted")?;
        let mut candidate = self.clone();
        candidate.events[start..end].clone_from_slice(
            &canonical
                .iter()
                .map(|row| row.event.clone())
                .collect::<Vec<_>>(),
        );
        for (offset, event) in candidate.events[end..].iter_mut().enumerate() {
            event.seq = last + 1 + offset as u64;
        }
        candidate.next_seq = next;
        for row in canonical {
            candidate.append_retained(thread_id, root_id, row.clone())?;
        }
        *self = candidate;
        Ok(())
    }

    /// Install receipt-acknowledged producer positions only after validating
    /// the entire exact captured prefix. Partial mutation cannot rebase a tail.
    pub(crate) fn acknowledge_producer_interval(
        &mut self,
        start: usize,
        count: usize,
        first_engine_seq: u64,
    ) -> Result<(), &'static str> {
        let end = start.checked_add(count).ok_or("local prefix exhausted")?;
        let events = self
            .events
            .get_mut(start..end)
            .ok_or("local prefix absent")?;
        for (offset, event) in events.iter().enumerate() {
            let engine_seq = first_engine_seq
                .checked_add(u64::try_from(offset).map_err(|_| "engine interval exhausted")?)
                .ok_or("engine interval exhausted")?;
            if matches!(&event.kind, SeatEventKind::PresentationInitialized(_))
                || event
                    .producer_engine_seq
                    .is_some_and(|original| original != engine_seq)
                || matches!(&event.kind, SeatEventKind::PresentationOperationPrepared(operation)
                    if operation.prepared_engine_seq != engine_seq || operation.validate_identity().is_err())
            {
                return Err("acknowledged event differs from its original producer position");
            }
        }
        for (offset, event) in events.iter_mut().enumerate() {
            event.producer_engine_seq = Some(first_engine_seq + offset as u64);
        }
        Ok(())
    }

    /// Fold the log into facet state: last-writer-wins per key, ordered
    /// by seq. Returns the folded map and the seq of the last event that
    /// wrote each key.
    pub fn fold(&self) -> SeatFold {
        let mut fold = SeatFold::default();
        for event in &self.events {
            fold.apply(event);
        }
        fold
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatReplayOutcome {
    Identical { index: usize },
    Inserted { index: usize },
    Replaced { index: usize },
}

/// Deterministic facet fold: key -> (last write seq, value).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeatFold {
    facets: BTreeMap<String, (u64, Value)>,
}

impl SeatFold {
    pub fn apply(&mut self, event: &SeatEvent) {
        match &event.kind {
            SeatEventKind::Facet { key, value } => {
                self.facets.insert(key.clone(), (event.seq, value.clone()));
            }
            SeatEventKind::PresentationOperationPrepared(_)
            | SeatEventKind::PresentationOperationCompleted(_)
            | SeatEventKind::PresentationInitialized(_)
            | SeatEventKind::SurfaceClosed(_) => {}
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.facets.get(key).map(|(_, value)| value)
    }

    /// Snapshot every facet's current value (dropping the write seqs). Used by
    /// the lens stack to capture the facet context a step-in leaves, so a pop
    /// can re-append the ones that changed.
    pub fn snapshot(&self) -> BTreeMap<String, Value> {
        self.facets
            .iter()
            .map(|(key, (_, value))| (key.clone(), value.clone()))
            .collect()
    }

    /// Seq of the last event that wrote `key`; None if never written.
    pub fn seq_of(&self, key: &str) -> Option<u64> {
        self.facets.get(key).map(|(seq, _)| *seq)
    }

    /// Typed lens for one mounted input's exact route facet. The caller owns
    /// the mounted-instance coordinate; there is deliberately no seat-global
    /// route fallback.
    pub fn input_route(&self, facet_key: &str) -> Option<InputRoute> {
        self.get(facet_key)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }
}

/// Where plain text lands on submit. Live, retargetable seat state —
/// never a permanent binding. The surface's `input:` block declares the
/// starting value; launches ratchet `thread`; retargeting is grammar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputRoute {
    /// The invocation template plain text fills. `None` = no target: the
    /// submit path warns instead of guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke: Option<InvokeTemplate>,
    /// Conversation ratchet: the chain head a follow-up continues from.
    /// Moves with each turn (the latest successor) so a submit braids onto
    /// the newest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The conversation's chain root — constant for the life of the chain.
    /// The feed follows THIS (the whole braid), not `thread` (the moving
    /// head): a follow-up retargets the head but keeps showing the
    /// conversation from its root. Set on the first turn (root == head),
    /// preserved across continuations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_root: Option<String>,
    /// Which node (reserved for the multi-node phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Fixed extras merged into the invocation parameters
    /// (e.g. `directive: <profile ref>` for the thread-input service).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl InputRoute {
    /// Resolve the route declared by a surface's `input:` block, if any.
    pub fn from_surface_input(input: Option<&Value>) -> Option<Self> {
        let route = input?.get("route")?;
        serde_json::from_value(route.clone()).ok()
    }

    pub fn has_target(&self) -> bool {
        self.invoke.is_some()
    }
}

/// Seat-facet identity for one mounted input's subject route. View instance
/// identity survives placement changes. Every source, control and composer in
/// that view resolves the same subject; changing the view binding cannot create
/// a parallel route authority.
pub fn input_route_facet_key(view_instance: &crate::ids::RyeOsViewInstanceKey) -> String {
    format!("{KEY_INPUT_ROUTE}\u{1f}{view_instance}")
}

/// The pinned invocation template. The client never interprets refs or
/// kinds — it constructs invocations and the substrate decides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvokeTemplate {
    /// Rye plane, direct service invocation (the ground-verb default:
    /// `service:threads/input` with text bound to its `input` field).
    Service {
        #[serde(rename = "ref")]
        item_ref: String,
    },
    /// Rye plane, pinned command token prefix (text appended as ONE tail
    /// argument, never shell-split).
    Command { tokens: Vec<String> },
    /// Ui plane: text written to a facet key (e.g. a tile's filter).
    UiFacet { key: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retained_seat_replay_refuses_transfer_phases_until_two_seat_proof_is_connected() {
        for event_type in [
            ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
            ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT,
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT,
            "contract.ryeos_ui.presentation_transfer_lifecycle.v3",
        ] {
            let row = json!({"event_type": event_type});
            assert_eq!(
                retained_seat_event_with_evidence(&row).unwrap_err(),
                "retained transfer testimony requires the authenticated two-seat lane"
            );
        }
        assert!(
            retained_seat_event_with_evidence(&json!({"event_type":"thread_created"}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retained_evidence_conflicts_are_atomic_and_local_replacement_drops_proof() {
        let value = json!({
            "event_type":"seat.facet", "thread_id":"seat-one", "chain_root_id":"root",
            "chain_seq":7, "event_hash":"ab".repeat(32),
            "payload":{"seq":1,"operation_id":"op","payload":{"key":"selection","value":1}}
        });
        let retained = retained_seat_event_with_evidence(&value).unwrap().unwrap();
        let mut log = SeatLog::default();
        log.append_retained("seat-one", "root", retained.clone())
            .unwrap();
        let original = log.clone();
        let mut conflicting = retained.clone();
        conflicting.evidence.event_hash = "bc".repeat(32);
        assert!(
            log.append_retained("seat-one", "root", conflicting)
                .is_err()
        );
        assert_eq!(log, original);
        let mut reused = retained.clone();
        reused.event.seq = 8;
        reused.evidence.chain_seq = 8;
        assert!(log.append_retained("seat-one", "root", reused).is_err());
        assert_eq!(log, original);
        let mut replacement = retained.event;
        replacement.kind = SeatEventKind::Facet {
            key: "selection".into(),
            value: json!(2),
        };
        log.append_replayed(replacement);
        assert!(log.evidence_at_sequence(7).is_none());
    }

    #[test]
    fn retained_evidence_never_uses_producer_sequence_or_a_derived_hash() {
        let mut value = json!({
            "event_type": "seat.facet",
            "thread_id": "seat-one", "chain_root_id": "seat-root",
            "chain_seq": 19,
            "event_hash": "ab".repeat(32),
            "payload": {"seq": 3, "operation_id": "operation",
                "payload": {"key": "selection", "value": null}}
        });
        let retained = retained_seat_event_with_evidence(&value).unwrap().unwrap();
        assert_eq!(retained.event.seq, 19);
        assert_eq!(retained.evidence.chain_seq, 19);
        assert_eq!(retained.evidence.event_hash, "ab".repeat(32));
        let mut log = SeatLog::default();
        assert!(
            log.append_retained("seat-two", "seat-root", retained.clone())
                .is_err()
        );
        assert!(log.events().is_empty());
        log.append_retained("seat-one", "seat-root", retained)
            .unwrap();
        let mut sibling = retained_seat_event_with_evidence(&value).unwrap().unwrap();
        sibling.thread_id = "seat-two".into();
        sibling.event.seq = 20;
        sibling.evidence.chain_seq = 20;
        assert!(
            log.append_retained("seat-two", "seat-root", sibling.clone())
                .is_err()
        );
        let mut restored: SeatLog =
            serde_json::from_value(serde_json::to_value(&log).unwrap()).unwrap();
        assert!(restored.evidence_at_sequence(19).is_none());
        assert!(
            restored
                .append_retained("seat-two", "seat-root", sibling)
                .is_err()
        );
        value["chain_seq"] = json!(u64::MAX);
        assert!(retained_seat_event_with_evidence(&value).is_err());
        value["chain_seq"] = json!(19);
        value.as_object_mut().unwrap().remove("event_hash");
        assert!(retained_seat_event_with_evidence(&value).is_err());
        value["event_hash"] = json!("AB".repeat(32));
        assert!(retained_seat_event_with_evidence(&value).is_err());
        value["event_hash"] = json!("ab".repeat(32));
        value.as_object_mut().unwrap().remove("chain_seq");
        assert!(retained_seat_event_with_evidence(&value).is_err());
    }

    #[test]
    fn fold_is_last_writer_wins_per_key() {
        let mut log = SeatLog::default();
        log.append_facet(KEY_SELECTION, json!({"item": "a"}));
        let route_seq = log.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-1"}));
        log.append_facet(KEY_SELECTION, json!({"item": "b"}));

        let fold = log.fold();
        assert_eq!(fold.get(KEY_SELECTION), Some(&json!({"item": "b"})));
        assert_eq!(fold.seq_of(KEY_INPUT_ROUTE), Some(route_seq));
        assert_eq!(
            fold.input_route(KEY_INPUT_ROUTE).unwrap().thread.as_deref(),
            Some("T-1")
        );
    }

    #[test]
    fn replay_fold_equals_live_fold_full_cycle() {
        // The proof that "the seat is a thread": fold-from-braid (reattach)
        // must equal the live fold. Open, append facets, persist the braid,
        // simulate a restart with a fresh log, replay, assert the folds match.
        let mut live = SeatLog::default();
        live.append_facet(KEY_SELECTION, json!({"item": "a"}));
        live.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-7"}));
        live.append_facet(KEY_SELECTION, json!({"item": "b"}));
        let before = live.fold();

        // Persist the braid exactly as the daemon would (seat.facet events).
        let braid: Vec<Value> = live
            .events()
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();

        // Restart: a fresh, empty log reattaches by replaying the braid.
        let mut reattached = SeatLog::default();
        for event in &braid {
            let seat_event: SeatEvent = serde_json::from_value(event.clone()).unwrap();
            reattached.append_replayed(seat_event);
        }

        assert_eq!(
            reattached.fold(),
            before,
            "replay fold must equal the live fold"
        );
        // Appending after reattach continues the sequence, not restarts it.
        let seq = reattached.append_facet(KEY_SELECTION, json!({"item": "c"}));
        assert_eq!(seq, 3);
        assert_eq!(
            reattached.fold().get(KEY_SELECTION),
            Some(&json!({"item": "c"}))
        );
    }

    #[test]
    fn replayed_events_advance_next_seq() {
        let mut log = SeatLog::default();
        log.append_replayed(SeatEvent {
            producer_engine_seq: None,
            seq: 7,
            kind: SeatEventKind::Facet {
                key: KEY_SELECTION.to_string(),
                value: json!({"item": "a"}),
            },
        });

        let seq = log.append_facet(KEY_SELECTION, json!({"item": "b"}));

        assert_eq!(seq, 8);
        assert_eq!(log.fold().get(KEY_SELECTION), Some(&json!({"item": "b"})));
    }

    #[test]
    fn replay_orders_older_authority_before_newer_local_state_and_deduplicates_sequence() {
        let mut log = SeatLog::default();
        log.append_replayed(SeatEvent {
            producer_engine_seq: None,
            seq: 7,
            kind: SeatEventKind::Facet {
                key: KEY_SELECTION.to_string(),
                value: json!({"item": "local-newer"}),
            },
        });
        log.append_replayed(SeatEvent {
            producer_engine_seq: None,
            seq: 6,
            kind: SeatEventKind::Facet {
                key: KEY_SELECTION.to_string(),
                value: json!({"item": "durable-older"}),
            },
        });
        assert_eq!(
            log.fold().get(KEY_SELECTION),
            Some(&json!({"item": "local-newer"})),
            "older replay must not win merely because it arrived later"
        );

        log.append_replayed(SeatEvent {
            producer_engine_seq: None,
            seq: 7,
            kind: SeatEventKind::Facet {
                key: KEY_SELECTION.to_string(),
                value: json!({"item": "durable-authoritative"}),
            },
        });
        assert_eq!(log.events().len(), 2);
        assert_eq!(
            log.fold().get(KEY_SELECTION),
            Some(&json!({"item": "durable-authoritative"}))
        );
    }

    #[test]
    fn unknown_keys_fold_harmlessly() {
        let mut log = SeatLog::default();
        log.append_facet("tile.t3.filter", json!("schedules"));
        let fold = log.fold();
        assert_eq!(fold.get("tile.t3.filter"), Some(&json!("schedules")));
        assert!(
            !fold
                .input_route(KEY_INPUT_ROUTE)
                .unwrap_or_default()
                .has_target()
        );
    }

    #[test]
    fn route_lens_defaults_on_absent_or_invalid() {
        let fold = SeatLog::default().fold();
        assert_eq!(
            fold.input_route(KEY_INPUT_ROUTE).unwrap_or_default(),
            InputRoute::default()
        );

        let mut log = SeatLog::default();
        log.append_facet(KEY_INPUT_ROUTE, json!("not a route"));
        assert_eq!(log.fold().input_route(KEY_INPUT_ROUTE), None);
    }

    #[test]
    fn surface_input_block_declares_service_route() {
        let input = json!({
            "route": {
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": {
                    "target": {
                        "kind": "fresh",
                        "item_ref": "directive:ryeos/ops/base",
                        "project_path": "/tmp/project",
                        "ref_bindings": { "model": "directive:ryeos/ops/base" }
                    }
                }
            }
        });
        let route = InputRoute::from_surface_input(Some(&input)).expect("route parses");
        assert_eq!(
            route.invoke,
            Some(InvokeTemplate::Service {
                item_ref: "service:threads/input".to_string()
            })
        );
        assert_eq!(
            route
                .params
                .pointer("/target/item_ref")
                .and_then(Value::as_str),
            Some("directive:ryeos/ops/base")
        );
        assert!(route.thread.is_none());
    }

    #[test]
    fn surface_input_block_command_and_ui_forms() {
        let command =
            json!({ "route": { "invoke": { "type": "command", "tokens": ["thread", "list"] } } });
        assert_eq!(
            InputRoute::from_surface_input(Some(&command))
                .unwrap()
                .invoke,
            Some(InvokeTemplate::Command {
                tokens: vec!["thread".into(), "list".into()]
            })
        );

        let ui = json!({ "route": { "invoke": { "type": "ui_facet", "key": "tile.t1.filter" } } });
        assert_eq!(
            InputRoute::from_surface_input(Some(&ui)).unwrap().invoke,
            Some(InvokeTemplate::UiFacet {
                key: "tile.t1.filter".into()
            })
        );
    }

    #[test]
    fn guard_old_route_enum_form_is_rejected() {
        // The pre-seat RyeOsInputRoute serde forms must NOT parse as a
        // route. No dual forms: the old tagged-enum shapes are dead.
        let old_ryeos_context = json!({ "route": { "type": "ryeos_context" } });
        let parsed = InputRoute::from_surface_input(Some(&old_ryeos_context));
        assert!(
            parsed.is_none() || !parsed.unwrap().has_target(),
            "old ryeos_context form must not yield a targeted route"
        );

        let old_directive = json!({
            "route": { "type": "directive", "directive_ref": "directive:x" }
        });
        let parsed = InputRoute::from_surface_input(Some(&old_directive));
        assert!(
            parsed.is_none() || !parsed.unwrap().has_target(),
            "old directive form must not yield a targeted route"
        );
    }

    #[test]
    fn seat_event_serde_matches_braid_shape() {
        let mut log = SeatLog::default();
        log.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-9"}));
        let event_json = serde_json::to_value(&log.events()[0]).unwrap();
        assert_eq!(event_json["event_type"], "seat.facet");
        assert_eq!(event_json["payload"]["key"], KEY_INPUT_ROUTE);
        assert_eq!(event_json["seq"], 0);
    }
}
