//! Prospective structural validation under the existing seat transition owner.
//! This is a temporary journal-derived projection, never a second registry.

use anyhow::Result;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_app::state_store::ThreadDetail;
use ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_client_base::ui::seat::{SeatEvent, SeatEventKind, retained_seat_event};

use crate::browser_session::{BindingAttachmentCoordinate, BrowserSession, PresentationMembership};
use crate::state::{CompositionCheckpoint, CompositionCheckpointKey, get_ui_state};
use ryeos_ui_presentation_contract::seat_control::{
    UiSeatControlPredecessor, UiSeatProducer, UiSeatProducerState, authenticate_control_transition,
};

/// Called only after the current receipt and producer branches. All other
/// ui_seat vocabulary is closed except the separately validated initialization;
/// unknown versions cannot be skipped as foreign activity before mutation.
fn validate_auxiliary_seat_event_type(event_type: &str) -> Result<()> {
    if event_type.starts_with("ui_seat.")
        && event_type
            != ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT
    {
        return Err(conflict("unsupported owned seat history event"));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct VerifiedHistory {
    pub replay: UiCompositionReplay,
    pub chain_seq: u64,
    pub tip_hash: String,
    pub thread_seq: u64,
    pub next_engine_seq: u64,
    pub control: Option<UiSeatProducer>,
    pub control_predecessor: UiSeatControlPredecessor,
    pub transfer_testimony_seen: bool,
}

/// Fold a private, receipt-released seat record without cloning the entire
/// accumulated composition for each event. A failed fold consumes only the
/// caller's unpublished candidate; it must never be used on a live cache.
pub(super) fn fold_private_composition_event(
    replay: &mut Option<UiCompositionReplay>,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
    event: &SeatEvent,
) -> Result<(), String> {
    let next = match replay.take() {
        Some(current) => current.apply_ordered_event_owned(event)?,
        None => UiCompositionReplay::begin(initial, event)?,
    };
    *replay = Some(next);
    Ok(())
}

/// The receipt authenticates the producer interval, but a structural
/// preparation also records its original producer and engine position. Both
/// historical readers must check that provenance before folding the event.
pub(super) fn fold_private_receipted_composition_event(
    replay: &mut Option<UiCompositionReplay>,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
    event: &SeatEvent,
    thread_id: &str,
    producer_incarnation: &str,
) -> Result<(), String> {
    validate_receipted_preparation(
        event,
        &initial.presentation_id,
        thread_id,
        producer_incarnation,
    )?;
    fold_private_composition_event(replay, initial, event)
}

fn validate_receipted_preparation(
    event: &SeatEvent,
    presentation_id: &str,
    thread_id: &str,
    producer_incarnation: &str,
) -> Result<(), String> {
    if let SeatEventKind::PresentationOperationPrepared(prepared) = &event.kind {
        if prepared.producer.presentation_id != presentation_id
            || prepared.producer.thread_id != thread_id
            || prepared.producer.producer_incarnation != producer_incarnation
            || event.producer_engine_seq != Some(prepared.prepared_engine_seq)
        {
            return Err("preparation differs from its authenticated original producer".into());
        }
        prepared.validate_identity()?;
    }
    Ok(())
}

impl VerifiedHistory {
    pub(super) fn require_ordinary_mutation(&self) -> Result<()> {
        if self.transfer_testimony_seen {
            return Err(conflict(
                "transfer testimony requires phase-aware two-seat mutation admission",
            ));
        }
        Ok(())
    }

    fn checkpoint(&self) -> CompositionCheckpoint {
        CompositionCheckpoint {
            tip_hash: self.tip_hash.clone(),
            thread_seq: self.thread_seq,
            chain_seq: self.chain_seq,
            replay: self.replay.clone(),
            next_engine_seq: self.next_engine_seq,
            control: self.control.clone(),
            control_predecessor: self.control_predecessor.clone(),
            transfer_testimony_seen: self.transfer_testimony_seen,
        }
    }
}

fn conflict(message: impl Into<String>) -> anyhow::Error {
    HandlerError::Conflict(message.into()).into()
}

/// Caller holds the existing transition lock and has checked exact receipt and
/// CURRENT writer authority. No lease, journal, store or layout mutation occurs.
pub(super) fn require_completed_initialization(
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    detail: &ThreadDetail,
    verified: &VerifiedHistory,
) -> Result<()> {
    let (current_detail, current) = super::require_presentation_seat_for_read(
        state,
        session,
        &membership.presentation_id,
        Some(&detail.thread_id),
        Some(&detail.chain_root_id),
    )?;
    if current != *membership
        || current_detail.thread_id != detail.thread_id
        || current_detail.chain_root_id != detail.chain_root_id
        || !verified
            .replay
            .matches_initialization(&membership.initialization)
        || membership.initialization.digest().map_err(conflict)? != membership.initialization_digest
    {
        return Err(conflict(
            "completed append initialization differs from current retained admission",
        ));
    }
    Ok(())
}

pub(super) fn validate_prospective_append(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    detail: &ThreadDetail,
    request: &super::AppendRequest,
    verified: &VerifiedHistory,
) -> Result<UiCompositionReplay> {
    // The sole caller already completed ordinary or paired signed replay and
    // fences that same cut during publication. Re-running singleton history
    // here would discard authenticated transfer effects before a later edit.
    require_completed_initialization(state, session, membership, detail, verified)?;
    if request.presentation_id != membership.presentation_id
        || request.thread_id != detail.thread_id
        || membership.thread_id.as_deref() != Some(detail.thread_id.as_str())
        || membership.chain_root_id.as_deref() != Some(detail.chain_root_id.as_str())
    {
        return Err(conflict("prospective composition names another seat"));
    }
    let original = membership
        .initialization
        .binding_attachments
        .iter()
        .find(|attachment| {
            attachment.binding_attachment_id
                == membership.initialization.root_binding.binding_attachment_id
                && attachment.binding_generation
                    == membership.initialization.root_binding.binding_generation
                && attachment.binding_digest
                    == membership.initialization.root_binding.binding_digest
        })
        .ok_or_else(|| conflict("initial composition root bound is missing"))?
        .binding_request_bounds;
    let current = super::super::ui_launch_mint::binding_request_bounds(state)?;
    authenticate_binding(
        generation,
        state,
        session,
        membership,
        &membership.initialization.root_binding,
    )?;
    let append_maximum = original
        .max_seat_append_bytes
        .min(current.max_seat_append_bytes);
    let envelope = serde_json::json!({
        "presentation_id":request.presentation_id,"thread_id":request.thread_id,
        "producer_incarnation":request.producer_incarnation,"operation_id":request.operation_id,
        "first_engine_seq":request.first_engine_seq.to_string(),
        "last_engine_seq":request.last_engine_seq.to_string(),
        "event_count":request.event_count,"payload_digest":request.payload_digest,
        "events":request.events.iter().map(|event|serde_json::json!({
            "engine_seq":event.engine_seq.to_string(),"event_type":event.event_type,"payload":event.payload,
        })).collect::<Vec<_>>(),
    });
    if serde_json::to_vec(&envelope)?.len() as u64 > append_maximum {
        return Err(conflict(
            "complete composition append exceeds original/current signed bounds",
        ));
    }
    // The caller already proved this fold against the exact signed anchor it
    // will condition at publication. Re-reading history here can repeat a
    // cold/suffix CAS walk under the process-wide seat transition lock.
    let (mut replay, mut last_chain_seq) = (verified.replay.clone(), verified.chain_seq);
    for event in &request.events {
        let kind: SeatEventKind = serde_json::from_value(serde_json::json!({
            "event_type":event.event_type,"payload":event.payload,
        }))?;
        if let SeatEventKind::PresentationOperationPrepared(operation) = &kind
            && let Some(composition) = operation.composition()
        {
            if operation.request_bounds != original {
                return Err(conflict(
                    "composition preparation bounds differ from original root admission",
                ));
            }
            composition
                .validate(original.max_request_bytes.min(current.max_request_bytes))
                .map_err(conflict)?;
            authenticate_composition(generation, state, session, membership, composition)?;
            for binding in replay
                .affected_composition_bindings(
                    composition,
                    original.max_request_bytes.min(current.max_request_bytes),
                )
                .map_err(conflict)?
            {
                authenticate_binding(generation, state, session, membership, &binding)?;
            }
            let predicted = replay
                .expected_composition_completion(
                    operation,
                    original.max_request_bytes.min(current.max_request_bytes),
                )
                .map_err(conflict)?;
            let predicted_receipt: ryeos_client_base::ui::composition_completion::UiCompositionCompletion =
                    serde_json::from_value(predicted.result.clone())?;
            if predicted_receipt.next_mount_id > replay.next_mount_id
                && membership.next_mount_id > replay.next_mount_id
            {
                return Err(conflict(
                    "a published admission remains unsettled; mount allocation must wait",
                ));
            }
            validate_completed_bytes(
                &predicted,
                original.max_request_bytes.min(current.max_request_bytes),
            )?;
            let current_writer = ryeos_client_base::ui::UiPresentationProducer {
                presentation_id: request.presentation_id.clone(),
                thread_id: request.thread_id.clone(),
                producer_incarnation: request.producer_incarnation.clone(),
            };
            if !ryeos_client_base::ui::admission_operation::single_seat_append_fits(
                &current_writer,
                ryeos_client_base::ui::admission_operation::COMPLETED_EVENT,
                &predicted,
                append_maximum,
            ) {
                return Err(conflict(
                    "predicted composition completion exceeds original/current append bounds",
                ));
            }
        }
        if let SeatEventKind::PresentationOperationCompleted(completed) = &kind {
            let prepared = replay
                .pending
                .get(&completed.operation_id)
                .ok_or_else(|| conflict("completion has no exact retained preparation"))?;
            if let Some(composition) = prepared.composition() {
                if prepared.request_bounds != original {
                    return Err(conflict(
                        "retained composition bounds differ from original root admission",
                    ));
                }
                validate_completed_bytes(
                    completed,
                    original.max_request_bytes.min(current.max_request_bytes),
                )?;
                composition
                    .validate(original.max_request_bytes.min(current.max_request_bytes))
                    .map_err(conflict)?;
                // Preparation does not reserve authority across time. Recheck
                // exact owner and every declared affected binding at settlement.
                authenticate_composition(generation, state, session, membership, composition)?;
                for binding in replay
                    .affected_composition_bindings(
                        composition,
                        original.max_request_bytes.min(current.max_request_bytes),
                    )
                    .map_err(conflict)?
                {
                    authenticate_binding(generation, state, session, membership, &binding)?;
                }
            }
        }
        last_chain_seq = last_chain_seq
            .checked_add(1)
            .filter(|sequence| *sequence <= i64::MAX as u64)
            .ok_or_else(|| conflict("composition chain sequence is exhausted"))?;
        replay
            .apply_ordered_event(&SeatEvent {
                seq: last_chain_seq,
                producer_engine_seq: Some(event.engine_seq),
                kind,
            })
            .map_err(conflict)?;
    }
    Ok(replay)
}

fn validate_completed_bytes(
    completed: &ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation,
    maximum: u64,
) -> Result<()> {
    if serde_json::to_vec(completed)?.len() as u64 > maximum {
        return Err(conflict(
            "complete composition settlement exceeds original/current request bounds",
        ));
    }
    Ok(())
}

fn require_live_composition_surface(
    surface: Option<&crate::browser_session::SurfaceMembership>,
    surface_id: &str,
    binding: &UiBindingAttachmentCoordinate,
) -> Result<()> {
    let surface = surface
        .filter(|surface| !surface.closed && surface.surface_instance_id == surface_id)
        .ok_or_else(|| conflict("composition Surface is no longer an exact live membership"))?;
    if surface.coordinate.binding_attachment_id != binding.binding_attachment_id
        || surface.coordinate.binding_generation != binding.binding_generation
        || surface.coordinate.binding_digest != binding.binding_digest
    {
        return Err(conflict(
            "composition Surface binding differs from retained membership",
        ));
    }
    Ok(())
}

fn authenticate_composition(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    composition: &ryeos_client_base::ui::composition_operation::UiCompositionRequest,
) -> Result<()> {
    if let Some(surface_id) = composition.owner.surface_instance_id() {
        require_live_composition_surface(
            membership.surfaces.get(surface_id.as_str()),
            surface_id.as_str(),
            composition.owner.binding(),
        )?;
    } else if composition.owner.binding().binding_attachment_id
        != membership.root_coordinate.binding_attachment_id
        || composition.owner.binding().binding_generation
            != membership.root_coordinate.binding_generation
        || composition.owner.binding().binding_digest != membership.root_coordinate.binding_digest
    {
        return Err(conflict(
            "shared subject owner differs from presentation root",
        ));
    }
    if let ryeos_client_base::ui::composition_operation::CompositionMutation::MoveViewAcrossSurfaces {
        destination,
        destination_binding,
        ..
    } = &composition.mutation
    {
        // Attachment authorization does not prove this exact Surface is live.
        require_live_composition_surface(
            membership.surfaces.get(destination.surface_instance_id.as_str()),
            destination.surface_instance_id.as_str(),
            destination_binding,
        )?;
        authenticate_binding(generation, state, session, membership, destination_binding)?;
    }
    authenticate_binding(
        generation,
        state,
        session,
        membership,
        composition.owner.binding(),
    )
}

pub(super) fn authenticate_binding(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    coordinate: &UiBindingAttachmentCoordinate,
) -> Result<()> {
    let attachment = crate::state::get_ui_state(state)
        .ok_or_else(|| conflict("UI state is absent"))?
        .browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &membership.presentation_id,
            &BindingAttachmentCoordinate {
                binding_attachment_id: coordinate.binding_attachment_id.clone(),
                binding_generation: coordinate.binding_generation,
                binding_digest: coordinate.binding_digest.clone(),
            },
            state.node_policy.generation_digest(),
        )
        .map_err(|error| conflict(error.to_string()))?;
    if attachment
        .compiled_binding
        .binding
        .request_engine_generation_identity
        != generation.request_engine_generation_identity()
        || attachment
            .compiled_binding
            .binding
            .node_policy_generation_digest
            != state.node_policy.generation_digest()
    {
        return Err(conflict("affected composition binding generation is stale"));
    }
    let project = crate::seat_auth::attachment_project_access(&attachment)?;
    match (
        attachment.compiled_binding.binding.project_root.as_deref(),
        attachment.project_query_identity.as_deref(),
        attachment.project_authority.as_ref(),
    ) {
        (None, None, None) => {}
        (Some(root), Some(query), Some(authority))
            if root == query && authority.path() == std::path::Path::new(root) => {}
        _ => {
            return Err(conflict(
                "affected composition project authority is inconsistent",
            ));
        }
    }
    super::super::ui_launch_mint::revalidate_binding_authority(
        generation,
        attachment.compiled_binding.as_ref(),
        project.as_ref().map(|project| project.path()),
        attachment.project_authority.as_deref(),
    )
}

pub(super) fn read_history(
    state: &AppState,
    detail: &ThreadDetail,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
) -> Result<VerifiedHistory> {
    let initialization_digest =
        ryeos_client_base::ui::seat_payload_digest(&serde_json::to_value(initial)?)?;
    let key = CompositionCheckpointKey {
        chain_root_id: detail.chain_root_id.clone(),
        thread_id: detail.thread_id.clone(),
        presentation_id: initial.presentation_id.clone(),
        initialization_digest,
    };
    let ui = get_ui_state(state);
    if let Some(seed) = ui
        .as_ref()
        .and_then(|ui| ui.composition_checkpoint_get(&key))
    {
        if let Ok(verified) = read_history_attempt(state, detail, initial, Some(seed)) {
            if let Some(ui) = &ui {
                ui.composition_checkpoint_put(key, verified.checkpoint());
            }
            return Ok(verified);
        }
        if let Some(ui) = &ui {
            ui.composition_checkpoint_invalidate(&key);
        }
    }
    let verified = read_history_attempt(state, detail, initial, None)?;
    if let Some(ui) = &ui {
        ui.composition_checkpoint_put(key, verified.checkpoint());
    }
    Ok(verified)
}

/// Recheck the freshly folded lifetime frontier against the exact signed
/// anchor that the mutation owner will condition. A changed head is a retry,
/// never permission to mix one fold with a different append predecessor.
pub(super) fn verified_frontier(
    state: &AppState,
    detail: &ThreadDetail,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
) -> Result<(
    VerifiedHistory,
    ryeos_state::AuthoritativeThreadEventReadAnchor,
)> {
    let verified = read_history(state, detail, initial)?;
    verified.require_ordinary_mutation()?;
    let anchor = state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)?
        .ok_or_else(|| conflict("authenticated frontier lost its signed anchor"))?;
    if anchor.tip() != Some((verified.tip_hash.as_str(), verified.thread_seq)) {
        return Err(conflict(
            "authenticated frontier changed before conditional append",
        ));
    }
    Ok((verified, anchor))
}

fn read_history_attempt(
    state: &AppState,
    detail: &ThreadDetail,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
    seed: Option<CompositionCheckpoint>,
) -> Result<VerifiedHistory> {
    let anchor = state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)?
        .ok_or_else(|| conflict("composition history has no signed thread anchor"))?;
    let (tip_hash, tip_seq) = anchor
        .tip()
        .map(|(hash, sequence)| (hash.to_owned(), sequence))
        .ok_or_else(|| conflict("composition history has no signed initialization"))?;
    let tip_event = state
        .state_store
        .retained_thread_event_by_hash(&tip_hash)?
        .ok_or_else(|| conflict("composition signed tip is absent"))?;
    if tip_event.chain_root_id != detail.chain_root_id
        || tip_event.thread_id != detail.thread_id
        || tip_event.thread_seq != tip_seq
    {
        return Err(conflict(
            "composition signed tip differs from the selected seat",
        ));
    }
    let through_seq = i64::try_from(tip_event.chain_seq)
        .map_err(|_| conflict("composition signed tip chain sequence overflows"))?;
    if let Some(seed) = &seed
        && seed.tip_hash == tip_hash
        && seed.thread_seq == tip_seq
        && seed.chain_seq == tip_event.chain_seq
    {
        return Ok(VerifiedHistory {
            replay: seed.replay.clone(),
            chain_seq: seed.chain_seq,
            tip_hash,
            thread_seq: tip_seq,
            next_engine_seq: seed.next_engine_seq,
            control: seed.control.clone(),
            control_predecessor: seed.control_predecessor.clone(),
            transfer_testimony_seen: seed.transfer_testimony_seen,
        });
    }
    let mut replay: Option<UiCompositionReplay> = seed.as_ref().map(|seed| seed.replay.clone());
    let mut cursor = seed
        .as_ref()
        .map(|seed| i64::try_from(seed.chain_seq))
        .transpose()
        .map_err(|_| conflict("cached composition chain sequence overflows"))?;
    let mut previous_hash = seed.as_ref().map(|seed| seed.tip_hash.clone());
    let mut previous_thread_seq = seed.as_ref().map_or(0, |seed| seed.thread_seq);
    let mut last_chain_seq = seed.as_ref().map(|seed| seed.chain_seq);
    let mut next_engine_seq = seed.as_ref().map_or(0, |seed| seed.next_engine_seq);
    let mut control = seed.as_ref().and_then(|seed| seed.control.clone());
    let mut control_predecessor = seed.as_ref().map(|seed| seed.control_predecessor.clone());
    let mut transfer_testimony_seen = seed
        .as_ref()
        .is_some_and(|seed| seed.transfer_testimony_seen);
    let mut reached_tip = false;
    let mut pending_producer_events: Vec<PersistedEventRecord> = Vec::new();
    loop {
        let page = state.state_store.replay_events_through(
            &detail.chain_root_id,
            Some(&detail.thread_id),
            cursor,
            through_seq,
            500,
            32 * 1024 * 1024,
        )?;
        if page.events.is_empty() && page.has_more {
            return Err(conflict("composition locator page made no progress"));
        }
        for row in page.events {
            let hash = row
                .event_hash
                .as_deref()
                .ok_or_else(|| conflict("composition locator has no CAS event hash"))?;
            let retained = state
                .state_store
                .retained_thread_event_by_hash(hash)?
                .ok_or_else(|| conflict("composition locator CAS event is absent"))?;
            let expected_thread_seq = previous_thread_seq
                .checked_add(1)
                .ok_or_else(|| conflict("composition thread sequence exhausted"))?;
            if retained.chain_root_id != detail.chain_root_id
                || retained.thread_id != detail.thread_id
                || retained.thread_seq != expected_thread_seq
                || retained.prev_thread_event_hash != previous_hash
                || i64::try_from(retained.chain_seq).ok() != Some(row.chain_seq)
                || i64::try_from(retained.thread_seq).ok() != Some(row.thread_seq)
                || row.thread_id != detail.thread_id
                || row.chain_root_id != detail.chain_root_id
                || last_chain_seq.is_some_and(|previous| retained.chain_seq <= previous)
            {
                return Err(conflict(
                    "composition locator differs from signed thread history",
                ));
            }
            let value = serde_json::to_value(&retained)?;
            if retained.event_type.starts_with(super::SEAT_EVENT_PREFIX) {
                if pending_producer_events.len() >= super::MAX_SEAT_APPEND_EVENTS {
                    return Err(conflict(
                        "composition producer interval exceeds append bound",
                    ));
                }
                pending_producer_events.push(
                    super::authenticated_history::persisted(hash.to_owned(), retained.clone())
                        .map_err(|error| conflict(error.to_string()))?,
                );
            } else if retained.event_type == super::SEAT_APPEND_RECEIPT_EVENT {
                let receipt: super::SeatAppendReceipt =
                    serde_json::from_value(retained.payload.clone()).map_err(|error| {
                        conflict(format!("invalid composition receipt: {error}"))
                    })?;
                if receipt.schema_version
                    != ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA
                    || receipt.event_count == 0
                    || receipt.event_count > super::MAX_SEAT_APPEND_EVENTS
                    || receipt.event_count != pending_producer_events.len()
                {
                    return Err(conflict("composition receipt interval is invalid"));
                }
                let expected_id =
                    ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
                        presentation_id: initial.presentation_id.clone(),
                        chain_root_id: detail.chain_root_id.clone(),
                        thread_id: detail.thread_id.clone(),
                        producer_incarnation: receipt.producer_incarnation.clone(),
                        first_engine_seq: receipt.first_engine_seq,
                        last_engine_seq: receipt.last_engine_seq,
                        event_count: receipt.event_count as u64,
                        payload_digest: receipt.payload_digest.clone(),
                    }
                    .derive_identity()
                    .map_err(conflict)?;
                if receipt.operation_id != expected_id {
                    return Err(conflict("composition receipt identity is not current"));
                }
                if receipt.first_engine_seq != next_engine_seq
                    || !control.as_ref().is_some_and(|current| {
                        current.state == UiSeatProducerState::Active
                            && current.producer_incarnation == receipt.producer_incarnation
                    })
                {
                    return Err(conflict(
                        "composition receipt breaks authenticated producer frontier",
                    ));
                }
                super::append_ack::validate_projected_interval(
                    &detail.chain_root_id,
                    &detail.thread_id,
                    &receipt,
                    i64::try_from(retained.chain_seq)
                        .map_err(|_| conflict("composition receipt sequence overflows"))?,
                    &pending_producer_events,
                )
                .map_err(|error| conflict(error.to_string()))?;
                next_engine_seq = receipt
                    .last_engine_seq
                    .checked_add(1)
                    .ok_or_else(|| conflict("composition producer sequence is exhausted"))?;
                for producer_record in pending_producer_events.drain(..) {
                    let event = retained_seat_event(&serde_json::to_value(&producer_record)?)
                        .map_err(conflict)?
                        .ok_or_else(|| conflict("composition receipt covers no seat event"))?;
                    fold_private_receipted_composition_event(
                        &mut replay,
                        initial,
                        &event,
                        &detail.thread_id,
                        &receipt.producer_incarnation,
                    )
                    .map_err(conflict)?;
                }
            } else if retained.event_type
                == ryeos_ui_presentation_contract::seat_control::SEAT_PRODUCER_EVENT
            {
                if !pending_producer_events.is_empty() {
                    return Err(conflict("control interrupts an unreceipted producer batch"));
                }
                let next: UiSeatProducer = serde_json::from_value(retained.payload.clone())
                    .map_err(|error| {
                        conflict(format!("invalid retained producer control: {error}"))
                    })?;
                let predecessor = control_predecessor
                    .as_ref()
                    .ok_or_else(|| conflict("producer control precedes signed initialization"))?;
                authenticate_control_transition(
                    &next,
                    control.as_ref(),
                    predecessor,
                    &initial.presentation_id,
                    &detail.chain_root_id,
                    &detail.thread_id,
                    next_engine_seq,
                )
                .map_err(conflict)?;
                control_predecessor = Some(UiSeatControlPredecessor::Control {
                    event_hash: hash.to_owned(),
                });
                control = Some(next);
            } else {
                if !pending_producer_events.is_empty() {
                    return Err(conflict(
                        "composition producer events lack adjacent receipt",
                    ));
                }
                validate_auxiliary_seat_event_type(&retained.event_type)?;
                if ryeos_ui_presentation_contract::transfer::is_transfer_testimony(
                    &retained.event_type,
                ) {
                    transfer_testimony_seen = true;
                }
                if retained.event_type == ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT {
                    let transfer =
                        ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                            .map_err(conflict)?;
                    // Transfer testimony witnesses the current authenticated
                    // frontier without consuming a receipted producer sequence.
                    // The transfer identity retains its original preparation
                    // revision/incarnation; later phases may follow unrelated
                    // edits or a separately authenticated producer replacement.
                    let current = control
                        .as_ref()
                        .filter(|current| current.state == UiSeatProducerState::Active)
                        .ok_or_else(|| conflict("transfer testimony has no active producer"))?;
                    let revision = replay
                        .as_ref()
                        .ok_or_else(|| conflict("transfer testimony has no composition"))?
                        .revision;
                    transfer
                        .validate_current_witness(
                            next_engine_seq,
                            revision,
                            &initial.presentation_id,
                            &current.producer_incarnation,
                        )
                        .map_err(conflict)?;
                }
                if retained.event_type == ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT {
                    if replay.is_some() || control_predecessor.is_some() {
                        return Err(conflict("composition history has duplicate initialization"));
                    }
                    control_predecessor = Some(UiSeatControlPredecessor::Initialization {
                        event_hash: hash.to_owned(),
                    });
                }
                if let Some(event) = retained_seat_event(&value).map_err(conflict)? {
                    fold_private_composition_event(&mut replay, initial, &event)
                        .map_err(conflict)?;
                }
            }
            previous_hash = Some(hash.to_owned());
            previous_thread_seq = retained.thread_seq;
            last_chain_seq = Some(retained.chain_seq);
            if hash == tip_hash && retained.thread_seq == tip_seq {
                reached_tip = true;
                break;
            }
        }
        if reached_tip {
            break;
        }
        let next_cursor = if page.has_more {
            last_chain_seq.and_then(|seq| i64::try_from(seq).ok())
        } else {
            None
        };
        match next_cursor {
            None => break,
            Some(next)
                if next >= 0
                    && cursor.is_none_or(|previous| next > previous)
                    && last_chain_seq.and_then(|seq| i64::try_from(seq).ok()) == Some(next) =>
            {
                cursor = Some(next);
            }
            Some(_) => return Err(conflict("composition locator cursor did not advance")),
        }
    }
    if !reached_tip {
        return Err(conflict(
            "composition locator omitted signed thread history",
        ));
    }
    if !pending_producer_events.is_empty() {
        return Err(conflict(
            "composition history ends with unreceipted producer events",
        ));
    }
    Ok(VerifiedHistory {
        replay: replay.ok_or_else(|| conflict("composition initialization is absent"))?,
        chain_seq: last_chain_seq.unwrap_or(0),
        tip_hash,
        thread_seq: tip_seq,
        next_engine_seq,
        control,
        control_predecessor: control_predecessor
            .ok_or_else(|| conflict("composition control has no signed initialization"))?,
        transfer_testimony_seen,
    })
}

/// Page-bounded streaming seam. The existing event service owns page limits;
/// no new lifetime ceiling or structural history vector is introduced.
#[cfg(test)]
fn scan_history(
    chain: &str,
    thread: &str,
    mut page: impl FnMut(Option<i64>) -> Result<(serde_json::Value, Option<i64>)>,
    mut consume: impl FnMut(SeatEvent) -> Result<()>,
) -> Result<u64> {
    let mut cursor = None;
    let mut last = None;
    loop {
        let (rows, next) = page(cursor)?;
        for row in rows
            .as_array()
            .ok_or_else(|| conflict("composition history page is not an array"))?
        {
            if row.get("chain_root_id").and_then(serde_json::Value::as_str) != Some(chain)
                || row.get("thread_id").and_then(serde_json::Value::as_str) != Some(thread)
            {
                return Err(conflict("composition history belongs to another seat"));
            }
            let sequence = row
                .get("chain_seq")
                .and_then(serde_json::Value::as_i64)
                .filter(|sequence| {
                    *sequence >= 0 && last.is_none_or(|previous| *sequence > previous)
                })
                .ok_or_else(|| conflict("composition history is not in increasing chain order"))?;
            last = Some(sequence);
            if let Some(event) = retained_seat_event(row).map_err(conflict)? {
                consume(event)?;
            }
        }
        match next {
            None => return Ok(last.unwrap_or(0) as u64),
            Some(next)
                if next >= 0
                    && cursor.is_none_or(|previous| next > previous)
                    && last.is_some_and(|last| next == last) =>
            {
                cursor = Some(next)
            }
            Some(_) => {
                return Err(conflict(
                    "composition history cursor does not advance exactly",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::{UiSeatControlRequest, UiSeatProducerClaim};

    #[test]
    fn composition_surface_owner_requires_live_exact_instance_and_binding() {
        use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
        let binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "surface-binding".into(),
            binding_generation: 2,
            binding_digest: "ab".repeat(32),
        };
        let id = "44444444-4444-4444-8444-444444444444";
        let surface = crate::browser_session::SurfaceMembership {
            surface_instance_id: id.into(),
            creation_request_id: "55555555-5555-4555-8555-555555555555".into(),
            ordinal: 1,
            title: "Destination".into(),
            coordinate: BindingAttachmentCoordinate {
                binding_attachment_id: binding.binding_attachment_id.clone(),
                binding_generation: binding.binding_generation,
                binding_digest: binding.binding_digest.clone(),
            },
            mount_manifest:
                ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
                    PresentationId::try_new("33333333-3333-4333-8333-333333333333").unwrap(),
                    SurfaceInstanceId::try_new(id).unwrap(),
                    1,
                    Vec::new(),
                    None,
                )
                .unwrap(),
            initial_seed: None,
            closed: false,
            particular_resume: None,
            saved_resume: None,
            tab_openings: std::collections::BTreeMap::new(),
        };
        // This is a membership-shape regression, not a compiled grant fixture.
        assert!(require_live_composition_surface(Some(&surface), id, &binding).is_ok());
        assert!(require_live_composition_surface(None, id, &binding).is_err());
        let mut closed = surface.clone();
        closed.closed = true;
        assert!(require_live_composition_surface(Some(&closed), id, &binding).is_err());
        assert!(
            require_live_composition_surface(Some(&surface), "another-Surface", &binding).is_err()
        );
        for field in 0..3 {
            let mut changed = binding.clone();
            match field {
                0 => changed.binding_attachment_id.push_str("-other"),
                1 => changed.binding_generation += 1,
                _ => changed.binding_digest = "cd".repeat(32),
            }
            assert!(require_live_composition_surface(Some(&surface), id, &changed).is_err());
        }
    }

    #[test]
    fn auxiliary_history_cannot_skip_unknown_control_receipt_or_initialization() {
        for event_type in [
            "ui_seat.producer.v2",
            "ui_seat.producer.v999",
            "ui_seat.producer.v3.future",
            "ui_seat.append_receipt.v1",
            "ui_seat.append_receipt.v999",
            "ui_seat.presentation_initialized.v999",
            "ui_seat.unknown.v1",
        ] {
            assert!(
                validate_auxiliary_seat_event_type(event_type).is_err(),
                "{event_type}"
            );
        }
        for event_type in [
            ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT,
            ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
            "thread_notice",
        ] {
            assert!(
                validate_auxiliary_seat_event_type(event_type).is_ok(),
                "{event_type}"
            );
        }
    }

    #[test]
    fn receipted_preparation_keeps_its_original_producer_and_position() {
        let presentation = "22222222-2222-4222-8222-222222222222";
        let incarnation = "ab".repeat(32);
        let mut prepared: ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation =
            serde_json::from_value(serde_json::json!({
                "schema":2,"operation_id":"","prepared_engine_seq":8192,
                "producer":{"presentation_id":presentation,"thread_id":"seat","producer_incarnation":incarnation.clone()},
                "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32)},
                "request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
                "body":{"kind":"admission","purpose":"resume_particular_surface","surface_open_target":null,
                    "particular_selection":{"kind":"surface","id":"workspace","expected_revision":4},
                    "invocation_origin":format!("seat:{presentation}:dock:left"),
                    "request":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32),
                        "coordinate":{"kind":"affordance","view_ref":"view:test/open","affordance_id":"open"},
                        "payload":{"kind":"selection","record":{"creation_request_id":"untrusted-self-id","selected":"work"}}},
                    "success_notice":null}
            }))
            .unwrap();
        prepared.assign_identity().unwrap();
        let mut event = SeatEvent {
            seq: 9,
            producer_engine_seq: Some(8192),
            kind: SeatEventKind::PresentationOperationPrepared(prepared),
        };
        assert!(validate_receipted_preparation(&event, presentation, "seat", &incarnation).is_ok());
        assert!(validate_receipted_preparation(&event, "another", "seat", &incarnation).is_err());
        assert!(
            validate_receipted_preparation(&event, presentation, "another", &incarnation).is_err()
        );
        assert!(
            validate_receipted_preparation(&event, presentation, "seat", &"ef".repeat(32)).is_err()
        );
        event.producer_engine_seq = Some(8193);
        assert!(
            validate_receipted_preparation(&event, presentation, "seat", &incarnation).is_err()
        );
        event.producer_engine_seq = Some(8192);
        if let SeatEventKind::PresentationOperationPrepared(prepared) = &mut event.kind {
            prepared.operation_id = "0".repeat(36);
        }
        assert!(
            validate_receipted_preparation(&event, presentation, "seat", &incarnation).is_err()
        );
    }

    fn fresh_control() -> (UiSeatControlPredecessor, UiSeatProducer) {
        let predecessor = UiSeatControlPredecessor::Initialization {
            event_hash: "a".repeat(64),
        };
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            thread_id: "thread".into(),
            chain_root_id: "root".into(),
            predecessor: predecessor.clone(),
            intent_nonce: "22222222-2222-4222-8222-222222222222".into(),
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
        (predecessor, control)
    }

    #[test]
    fn lifetime_control_requires_exact_initialization_and_engine_frontier() {
        let (predecessor, control) = fresh_control();
        assert!(
            authenticate_control_transition(
                &control,
                None,
                &predecessor,
                "11111111-1111-4111-8111-111111111111",
                "root",
                "thread",
                0,
            )
            .is_ok()
        );
        assert!(
            authenticate_control_transition(
                &control,
                None,
                &predecessor,
                "11111111-1111-4111-8111-111111111111",
                "root",
                "thread",
                1,
            )
            .is_err()
        );
        assert!(
            authenticate_control_transition(
                &control,
                None,
                &UiSeatControlPredecessor::Initialization {
                    event_hash: "c".repeat(64)
                },
                "11111111-1111-4111-8111-111111111111",
                "root",
                "thread",
                0,
            )
            .is_err()
        );
        let mut malformed = control;
        malformed.producer_incarnation.clear();
        assert!(
            authenticate_control_transition(
                &malformed,
                None,
                &predecessor,
                "11111111-1111-4111-8111-111111111111",
                "root",
                "thread",
                0,
            )
            .is_err()
        );
    }
    #[test]
    fn completed_bound_charges_the_full_record_not_only_result() {
        let completed =
            ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation {
                operation_id: "11111111-1111-4111-8111-111111111111".into(),
                result: serde_json::json!({"escaped":"\\\"\\\"\\\""}),
            };
        let exact = serde_json::to_vec(&completed).unwrap().len() as u64;
        assert!(validate_completed_bytes(&completed, exact).is_ok());
        assert!(validate_completed_bytes(&completed, exact - 1).is_err());
        assert!(
            validate_completed_bytes(
                &completed,
                serde_json::to_vec(&completed.result).unwrap().len() as u64
            )
            .is_err()
        );
    }
    fn row(seq: i64, event_type: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"chain_root_id":"chain","thread_id":"thread","chain_seq":seq,"event_type":event_type,"payload":payload})
    }
    #[test]
    fn unrelated_pages_still_advance_to_owned_history() {
        let mut calls = 0;
        let mut events = Vec::new();
        let last = scan_history(
            "chain",
            "thread",
            |cursor| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(cursor, None);
                        Ok((
                            serde_json::json!([row(1, "thread.started", serde_json::json!({}))]),
                            Some(1),
                        ))
                    }
                    2 => {
                        assert_eq!(cursor, Some(1));
                        Ok((
                            serde_json::json!([row(
                                2,
                                "seat.facet",
                                serde_json::json!({"seq":9,"payload":{"key":"test","value":1}})
                            )]),
                            None,
                        ))
                    }
                    _ => panic!("extra page"),
                }
            },
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 2);
        assert_eq!(last, 2);
    }
    #[test]
    fn malformed_owned_foreign_and_nonadvancing_history_fail_closed() {
        assert!(
            scan_history(
                "chain",
                "thread",
                |_| Ok((
                    serde_json::json!([row(1, "seat.facet", serde_json::json!({}))]),
                    None
                )),
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            scan_history(
                "other",
                "thread",
                |_| Ok((
                    serde_json::json!([row(1, "thread.started", serde_json::json!({}))]),
                    None
                )),
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            scan_history(
                "chain",
                "thread",
                |_| Ok((serde_json::json!([]), Some(1))),
                |_| Ok(())
            )
            .is_err()
        );
    }
    #[test]
    fn fold_failure_stops_stream_without_publishing_another_page() {
        let mut calls = 0;
        assert!(
            scan_history(
                "chain",
                "thread",
                |_| {
                    calls += 1;
                    Ok((
                        serde_json::json!([row(
                            1,
                            "seat.facet",
                            serde_json::json!({"seq":1,"payload":{"key":"test","value":1}})
                        )]),
                        Some(1),
                    ))
                },
                |_| Err(conflict("invalid causal prefix"))
            )
            .is_err()
        );
        assert_eq!(calls, 1);
    }
}
