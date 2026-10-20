//! Narrow browser-authenticated ingress for transfer phases. Ordinary seat
//! append remains unable to publish transfer testimony: every phase here is
//! written by the typed transfer layer from a completed read of both seats,
//! conditioned on every anchor and fence those reads depended on, and
//! admitted last against the session's own memberships.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, anyhow, bail};
use ryeos_app::state_store::{CutPairAppendResult, PersistedEventRecord, ThreadDetail};
use ryeos_app::{handler_context::HandlerContext, handler_error::HandlerError, state::AppState};
use ryeos_client_base::ids::{SurfaceInstanceId, ViewSetId};
use ryeos_client_base::ui::{
    binding::UiBindingAttachmentCoordinate,
    composition_operation::CompositionPlacement,
    transfer_export::{UiTransferExport, UiTransferExportMode},
    transfer_reservation::{UiTransferDestinationPlacement, UiTransferReservation},
};
use ryeos_state::{AuthoritativeThreadEventReadAnchor, RetiredChainDependency};
use ryeos_ui_presentation_contract::transfer::{
    MoveFreezeEvidence, TRANSFER_SCHEMA, TransferEvidence, TransferIdentity, TransferPhase,
    TransferRecord, TransferSeat,
};
use ryeos_ui_presentation_contract::transfer_state::{
    TransferDestinationState, TransferSourceState,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::composition::{self, VerifiedHistory};
use super::paired_read::CutSet;
use super::{
    BrowserSession, ProducerState, SeatFrontier, browser_session, get_ui_state, read_seat_frontier,
    require_presentation_seat_for_read, seat_kind, seat_owner,
};
use crate::browser_session::transfer_reservation::CompletedPairedCut;
use crate::browser_session::{
    BrowserSessionStore, HiddenTransferReservationOutcome, PresentationMembership,
};
use crate::transfer_store::appends::{
    AcceptanceInputs, MoveFreezeCancellationAppendResult, PreparationInputs,
    TransferAcceptedReadback, TransferAppendResult, TransferCutDependencies,
    TransferPreparationReadback, TransferSeatContext, TransferSeatPair, TransferSourceDecision,
    append_move_freeze_cancellation, append_transfer_accepted, append_transfer_activated,
    append_transfer_preparation_pair, append_transfer_source_decision,
    read_transfer_accepted_successor, read_transfer_preparation_pair,
};
use crate::transfer_store::reader::{
    READ_PAGE_BYTES, READ_PAGE_EVENTS, UiTransferFixedHistoryProof, UiTransferFixedSeatProof,
    assemble_fixed_history_proof, assemble_move_freeze_cancellation_proof, begin_fixed_seat_read,
    finish_fixed_seat_read, read_fixed_seat_page,
};

mod freeze_evidence;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum TransferRequest {
    CancelFreeze {
        transfer: TransferIdentity,
    },
    Commit {
        transfer: TransferIdentity,
    },
    Abort {
        transfer: TransferIdentity,
    },
    Activate {
        transfer: TransferIdentity,
    },
    Prepare {
        transfer: TransferIdentity,
        export: Value,
        #[serde(deserialize_with = "required_freeze")]
        freeze: Option<MoveFreezeEvidence>,
    },
    AcceptView {
        transfer: TransferIdentity,
        inbound: TransferEvidence,
        preparation: TransferEvidence,
        surface_instance_id: SurfaceInstanceId,
        view_set_id: ViewSetId,
        placement: CompositionPlacement,
    },
    AcceptTabs {
        transfer: TransferIdentity,
        inbound: TransferEvidence,
        preparation: TransferEvidence,
        surface_instance_id: SurfaceInstanceId,
    },
    AcceptSharedDock {
        transfer: TransferIdentity,
        inbound: TransferEvidence,
        preparation: TransferEvidence,
        edge: String,
    },
}

enum TransferAcceptanceTarget<'a> {
    View {
        surface_instance_id: &'a SurfaceInstanceId,
        view_set_id: ViewSetId,
        placement: &'a CompositionPlacement,
    },
    Tabs {
        surface_instance_id: &'a SurfaceInstanceId,
    },
    SharedDock {
        edge: &'a str,
    },
}

fn required_freeze<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<MoveFreezeEvidence>, D::Error> {
    Option::deserialize(deserializer)
}

fn conflict(message: &'static str) -> anyhow::Error {
    HandlerError::Conflict(message.into()).into()
}

/// What a seat must hold for a phase to be admitted on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeatRule {
    /// The seat is exactly at the revision and producer the transfer
    /// identity names: a fresh preparation.
    PreparationFrontier,
    /// A current active producer at or beyond the identity's revision.
    ActiveProducer,
    /// A seat that only has to be the identified seat at or beyond its
    /// revision: it takes no part in the write but its frontier is conditioned.
    Counterpart,
}

/// One seat of a transfer, authenticated against its completed frontier under
/// the seat-transition lock.
struct AuthenticatedSeat {
    detail: ThreadDetail,
    membership: PresentationMembership,
    verified: VerifiedHistory,
    anchor: AuthoritativeThreadEventReadAnchor,
    max_append_bytes: u64,
}

impl AuthenticatedSeat {
    fn context<'a>(
        &'a self,
        owner: &'a str,
        expected: &'a TransferSeat,
    ) -> TransferSeatContext<'a> {
        TransferSeatContext {
            anchor: &self.anchor,
            owner,
            surface: &self.detail.item_ref,
            client_ref: &self.detail.executor_ref,
            presentation_id: &expected.presentation_id,
            producer_incarnation: self
                .verified
                .control
                .as_ref()
                .map_or("", |control| control.producer_incarnation.as_str()),
            revision: self.verified.replay.revision,
            next_engine_seq: self.verified.next_engine_seq,
            max_append_bytes: self.max_append_bytes,
        }
    }
}

fn authenticate_seat(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    expected: &TransferSeat,
    frontier: &SeatFrontier,
    rule: SeatRule,
) -> Result<AuthenticatedSeat> {
    let (detail, current) = require_presentation_seat_for_read(
        state,
        session,
        &expected.presentation_id,
        Some(&expected.thread_id),
        Some(&expected.chain_root_id),
    )?;
    if detail.thread_id != detail.chain_root_id || detail.status != "running" {
        return Err(conflict("transfer requires an exact running seat root"));
    }
    if current != frontier.membership {
        return Err(conflict("transfer seat membership changed"));
    }
    let verified = &frontier.verified;
    if state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)?
        .as_ref()
        != Some(&frontier.anchor)
        || frontier.anchor.tip() != Some((verified.tip_hash.as_str(), verified.thread_seq))
    {
        return Err(conflict("transfer admission signed head changed"));
    }
    let producer_admitted = match rule {
        SeatRule::PreparationFrontier => {
            verified.replay.revision == expected.revision
                && verified.control.as_ref().is_some_and(|control| {
                    control.state == ProducerState::Active
                        && control.producer_incarnation == expected.producer_incarnation
                })
        }
        SeatRule::ActiveProducer => {
            verified.replay.revision >= expected.revision
                && verified
                    .control
                    .as_ref()
                    .is_some_and(|control| control.state == ProducerState::Active)
        }
        SeatRule::Counterpart => verified.replay.revision >= expected.revision,
    };
    if !producer_admitted {
        return Err(conflict(
            "transfer seat has no admitted current active producer frontier",
        ));
    }
    let membership = frontier.membership.clone();
    composition::authenticate_binding(
        generation,
        state,
        session,
        &membership,
        &membership.initialization.root_binding,
    )?;
    let root = membership
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
        .ok_or_else(|| conflict("transfer root binding is absent from signed initialization"))?;
    let current_bounds = super::super::ui_launch_mint::binding_request_bounds(state)?;
    let max_append_bytes = root
        .binding_request_bounds
        .max_seat_append_bytes
        .min(current_bounds.max_seat_append_bytes);
    Ok(AuthenticatedSeat {
        detail,
        membership,
        verified: frontier.verified.clone(),
        anchor: frontier.anchor.clone(),
        max_append_bytes,
    })
}

fn revalidate_export_bindings(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    export: &UiTransferExport,
) -> Result<()> {
    let mut bindings: Vec<&UiBindingAttachmentCoordinate> = Vec::new();
    if let Some(sets) = export.sets() {
        bindings.extend(sets.iter().map(|set| &set.binding));
    }
    for mount in export.mounts() {
        bindings.push(&mount.binding);
        bindings.extend(mount.lens_frames.iter().map(|frame| &frame.binding));
    }
    for binding in bindings {
        composition::authenticate_binding(generation, state, session, membership, binding)?;
    }
    Ok(())
}

fn revalidate_activation_bindings<'a>(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    attachments: impl Iterator<Item = &'a Arc<crate::browser_session::AdmittedBindingAttachment>>,
) -> Result<()> {
    for attachment in attachments {
        let binding = &attachment.compiled_binding.binding;
        if binding.request_engine_generation_identity
            != generation.request_engine_generation_identity()
            || binding.node_policy_generation_digest != state.node_policy.generation_digest()
            || binding.principal_id != session.principal_id
        {
            return Err(conflict(
                "retained activation admission generation is stale",
            ));
        }
        let project = crate::seat_auth::attachment_project_access(attachment)?;
        match (
            binding.project_root.as_deref(),
            attachment.project_query_identity.as_deref(),
            attachment.project_authority.as_ref(),
        ) {
            (None, None, None) => {}
            (Some(root), Some(query), Some(authority))
                if root == query && authority.path() == std::path::Path::new(root) => {}
            _ => {
                return Err(conflict(
                    "retained activation project authority is inconsistent",
                ));
            }
        }
        super::super::ui_launch_mint::revalidate_binding_authority(
            generation,
            attachment.compiled_binding.as_ref(),
            project.as_ref().map(|project| project.path()),
            attachment.project_authority.as_deref(),
        )?;
    }
    Ok(())
}

fn transfer_response(operation_id: &str, outcome: &str) -> Value {
    json!({ "operation_id": operation_id, "outcome": outcome })
}

fn prepared_response(
    operation_id: &str,
    inbound: TransferEvidence,
    preparation: TransferEvidence,
) -> Value {
    json!({
        "operation_id": operation_id,
        "outcome": "prepared",
        "inbound": inbound,
        "preparation": preparation,
    })
}

fn accepted_response(operation_id: &str, acceptance: TransferEvidence) -> Value {
    json!({
        "operation_id": operation_id,
        "outcome": "accepted",
        "acceptance": acceptance,
    })
}

fn activated_response(operation_id: &str, activation: TransferEvidence) -> Value {
    json!({"operation_id": operation_id, "outcome": "activated", "activation": activation})
}

/// The anchors and fences one publication conditions on: both seats' own
/// anchors and everything the completed cuts of those seats depended on.
struct Publication {
    anchors: Vec<AuthoritativeThreadEventReadAnchor>,
    retired: Vec<RetiredChainDependency>,
    cuts: CutSet,
}

impl Publication {
    fn read(state: &AppState, session: &BrowserSession, seats: [&SeatFrontier; 2]) -> Result<Self> {
        let cuts = CutSet::new(seats.iter().filter_map(|seat| seat.cut.clone()));
        let (mut anchors, retired) = cuts.publication_cut(state, session)?;
        for seat in seats {
            match anchors.iter().find(|anchor| {
                anchor.chain_root_id() == seat.anchor.chain_root_id()
                    && anchor.thread_id() == seat.anchor.thread_id()
            }) {
                Some(anchor) if *anchor != seat.anchor => {
                    bail!("a completed cut read a transfer seat at another signed head");
                }
                Some(_) => {}
                None => anchors.push(seat.anchor.clone()),
            }
        }
        if retired.iter().any(|fence| {
            anchors
                .iter()
                .any(|anchor| anchor.chain_root_id() == fence.chain_root_id)
        }) {
            bail!("a transfer cut reads one chain as both live and retired");
        }
        Ok(Self {
            anchors,
            retired,
            cuts,
        })
    }

    fn dependencies(&self) -> TransferCutDependencies<'_> {
        TransferCutDependencies {
            current_anchors: &self.anchors,
            retired: &self.retired,
        }
    }

    /// The short, in-memory admission every publication ends with: the live
    /// session is unexpired, has allocated no attachments since the read, and
    /// holds exactly the memberships the cuts were read for.
    fn admits(&self, live: &BrowserSession, next_attachment_generation: u64) -> bool {
        live.expires_at > std::time::Instant::now()
            && live.next_attachment_generation == next_attachment_generation
            && self.cuts.publication_memberships_match(live)
    }
}

/// The completed two-seat read an admission rests on.
struct TransferAdmissionProof {
    history: UiTransferFixedHistoryProof,
    source_cut: Arc<UiTransferFixedSeatProof>,
    source: SeatFrontier,
    destination: SeatFrontier,
}

/// Read one seat's fixed cut at its exact anchor, for exactly one transfer.
/// Every blocking turn has an owner-defined page budget; neither a partial
/// read nor task cancellation is evidence that the seat lacks a phase.
async fn read_fixed_transfer_seat(
    state: Arc<AppState>,
    seat: TransferSeat,
    anchor: AuthoritativeThreadEventReadAnchor,
    transfer: TransferIdentity,
) -> Result<Arc<UiTransferFixedSeatProof>> {
    let store = state.state_store.clone();
    let mut read = tokio::task::spawn_blocking(move || {
        begin_fixed_seat_read(store, anchor, &seat, std::slice::from_ref(&transfer))
    })
    .await??;
    loop {
        let (next, done) = tokio::task::spawn_blocking(move || {
            let done = read_fixed_seat_page(&mut read, READ_PAGE_EVENTS, READ_PAGE_BYTES);
            (read, done)
        })
        .await?;
        read = next;
        if done? {
            break;
        }
    }
    Ok(Arc::new(
        tokio::task::spawn_blocking(move || finish_fixed_seat_read(read)).await??,
    ))
}

/// Read both seats of an existing operation outside the seat-transition lock.
/// The caller reauthenticates session membership and both signed heads after
/// completion.
async fn read_transfer_proof(
    state: Arc<AppState>,
    session: &BrowserSession,
    transfer: &TransferIdentity,
) -> Result<TransferAdmissionProof> {
    let source = read_seat_frontier(
        state.clone(),
        session,
        &transfer.source.presentation_id,
        &transfer.source.thread_id,
        Some(&transfer.source.chain_root_id),
    )
    .await?;
    let destination = read_seat_frontier(
        state.clone(),
        session,
        &transfer.destination.presentation_id,
        &transfer.destination.thread_id,
        Some(&transfer.destination.chain_root_id),
    )
    .await?;
    let source_cut = read_fixed_transfer_seat(
        state.clone(),
        transfer.source.clone(),
        source.anchor.clone(),
        transfer.clone(),
    )
    .await?;
    let destination_cut = read_fixed_transfer_seat(
        state,
        transfer.destination.clone(),
        destination.anchor.clone(),
        transfer.clone(),
    )
    .await?;
    let history = assemble_fixed_history_proof(&source_cut, &destination_cut, transfer)?;
    Ok(TransferAdmissionProof {
        history,
        source_cut,
        source,
        destination,
    })
}

fn require_both_memberships(
    state: &AppState,
    session: &BrowserSession,
    transfer: &TransferIdentity,
) -> Result<()> {
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    for seat in [&transfer.source, &transfer.destination] {
        ui.browser_sessions
            .require_presentation(
                &session.session_id,
                &seat.presentation_id,
                Some(&seat.thread_id),
                Some(&seat.chain_root_id),
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    }
    Ok(())
}

fn committed_evidence(records: &[PersistedEventRecord]) -> Result<TransferEvidence> {
    let [record] = records else {
        return Err(
            HandlerError::Internal("transfer phase appended another event count".into()).into(),
        );
    };
    Ok(TransferEvidence {
        event_hash: record
            .event_hash
            .clone()
            .ok_or_else(|| HandlerError::Internal("transfer phase signed hash absent".into()))?,
        chain_seq: u64::try_from(record.chain_seq)
            .map_err(|_| HandlerError::Internal("transfer phase chain sequence invalid".into()))?,
    })
}

fn exact_accept_retry_target(
    retained: &UiTransferDestinationPlacement,
    target: &TransferAcceptanceTarget<'_>,
) -> bool {
    match (retained, target) {
        (
            UiTransferDestinationPlacement::View {
                surface_instance_id: retained_surface,
                view_set_id: retained_set,
                placement: retained_placement,
                ..
            },
            TransferAcceptanceTarget::View {
                surface_instance_id,
                view_set_id,
                placement,
            },
        ) => {
            retained_surface == *surface_instance_id
                && retained_set == view_set_id
                && retained_placement == *placement
        }
        (
            UiTransferDestinationPlacement::Tabs { manifest, .. },
            TransferAcceptanceTarget::Tabs {
                surface_instance_id,
            },
        ) => &manifest.surface_instance_id == *surface_instance_id,
        (
            UiTransferDestinationPlacement::SharedDock {
                edge: retained_edge,
            },
            TransferAcceptanceTarget::SharedDock { edge },
        ) => retained_edge.as_str() == *edge,
        _ => false,
    }
}

/// This verifies the retained object's identity only. It is not admission
/// testimony until a signed tip or signed successor links the object.
fn exact_transfer_evidence_event(
    state: &AppState,
    chain_root_id: &str,
    thread_id: &str,
    evidence: &TransferEvidence,
) -> Result<ryeos_state::objects::thread_event::ThreadEvent> {
    let event = state
        .state_store
        .retained_thread_event_by_hash(&evidence.event_hash)?
        .ok_or_else(|| conflict("signed transfer tip object is absent"))?;
    if event.chain_root_id != chain_root_id
        || event.thread_id != thread_id
        || event.chain_seq != evidence.chain_seq
        || ryeos_state::objects::thread_event::hash_event(&event)? != evidence.event_hash
    {
        return Err(conflict("signed transfer tip differs from retained object"));
    }
    Ok(event)
}

fn session_attachment_bound(state: &AppState) -> Result<usize> {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
    usize::try_from(policy.max_live_binding_attachments_per_session)
        .map_err(|_| HandlerError::Internal("browser attachment bound exceeds usize".into()).into())
}

fn expected_memberships(session: &BrowserSession) -> Vec<PresentationMembership> {
    session.presentations.values().cloned().collect()
}

/// Reserve fresh, hidden destination bindings before the signed Accepted
/// append. Nothing here exposes those bindings as a live destination mount.
fn accept_transfer(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    transfer: &TransferIdentity,
    proof: &TransferAdmissionProof,
    inbound: &TransferEvidence,
    preparation: &TransferEvidence,
    target: TransferAcceptanceTarget<'_>,
) -> Result<Value> {
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    let source = authenticate_seat(
        generation,
        state,
        session,
        &transfer.source,
        &proof.source,
        SeatRule::ActiveProducer,
    )?;
    let destination = authenticate_seat(
        generation,
        state,
        session,
        &transfer.destination,
        &proof.destination,
        SeatRule::ActiveProducer,
    )?;
    let prepared_event = exact_transfer_evidence_event(
        state,
        &transfer.source.chain_root_id,
        &transfer.source.thread_id,
        preparation,
    )?;
    let prepared =
        ryeos_ui_presentation_contract::transfer::from_event_value(&prepared_event.to_value())
            .map_err(HandlerError::Conflict)?;
    if prepared.payload.transfer != *transfer
        || !matches!(
            &prepared.payload.phase,
            TransferPhase::Prepared { inbound: linked, .. } if linked == inbound
        )
    {
        return Err(conflict("another transfer owns source preparation"));
    }
    let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
    let export = UiTransferExport::from_preparation(&prepared.payload, max_record_bytes)
        .map_err(HandlerError::Conflict)?;
    // A MOVE is admitted only through the receipted typed Core freeze linked
    // by this exact signed preparation, never from renderer-supplied export
    // bytes alone. Ordinary seat append cannot publish these paired phases.
    if export.mode == UiTransferExportMode::Move {
        let TransferPhase::Prepared {
            freeze: Some(links),
            ..
        } = &prepared.payload.phase
        else {
            return Err(conflict("MOVE preparation has no exact freeze links"));
        };
        freeze_evidence::verify_retained_move_freeze(
            &source,
            &proof.source_cut,
            &proof.history,
            transfer,
            &export,
            links,
        )?;
    }
    export
        .validate_affected_snapshot(transfer, &source.verified.replay, max_record_bytes)
        .map_err(HandlerError::Conflict)?;
    revalidate_export_bindings(generation, state, session, &source.membership, &export)?;
    let max_attachments = session_attachment_bound(state)?;
    let policy_digest = state.node_policy.generation_digest();
    let lifecycle = proof.history.lifecycle();
    if proof.history.source_anchor() != &source.anchor
        || proof.history.destination_anchor() != &destination.anchor
        || lifecycle.transfer() != transfer
        || lifecycle.inbound().evidence != inbound
        || lifecycle
            .prepared()
            .is_none_or(|retained| retained.evidence != preparation)
    {
        return Ok(transfer_response(&transfer.operation_id, "conflict"));
    }
    let inbound_event = exact_transfer_evidence_event(
        state,
        &transfer.destination.chain_root_id,
        &transfer.destination.thread_id,
        inbound,
    )?;
    if let Some(retained) = lifecycle.accepted() {
        if lifecycle.source_state() != TransferSourceState::Frozen
            || lifecycle.destination_state() != TransferDestinationState::Reserved
        {
            return Ok(transfer_response(&transfer.operation_id, "conflict"));
        }
        let Ok(reservation) =
            UiTransferReservation::from_acceptance(retained.record, max_record_bytes)
        else {
            return Ok(transfer_response(&transfer.operation_id, "uncertain"));
        };
        if !exact_accept_retry_target(&reservation.placement, &target) {
            return Ok(transfer_response(&transfer.operation_id, "conflict"));
        }
        if ui
            .browser_sessions
            .stage_retained_transfer_attachment_batch(
                &session.session_id,
                transfer,
                &export,
                &reservation,
                max_record_bytes,
                policy_digest,
            )
            .is_err()
        {
            return Ok(transfer_response(&transfer.operation_id, "uncertain"));
        }
        return Ok(accepted_response(
            &transfer.operation_id,
            retained.evidence.clone(),
        ));
    }
    if lifecycle.source_state() != TransferSourceState::Frozen
        || lifecycle.destination_state() != TransferDestinationState::Unreserved
    {
        return Ok(transfer_response(&transfer.operation_id, "conflict"));
    }
    let staged = ui
        .browser_sessions
        .stage_fresh_transfer_attachment_batch(
            &session.session_id,
            transfer,
            &export,
            max_attachments,
            policy_digest,
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let source_bindings = export
        .sets()
        .into_iter()
        .flatten()
        .map(|set| &set.binding)
        .chain(export.mounts().into_iter().flat_map(|mount| {
            std::iter::once(&mount.binding)
                .chain(mount.lens_frames.iter().map(|frame| &frame.binding))
        }));
    let bindings = source_bindings
        .map(|source_binding| {
            staged
                .destination_binding(source_binding)
                .map(|binding| (source_binding.binding_attachment_id.clone(), binding))
                .ok_or_else(|| HandlerError::Conflict("fresh destination binding is absent".into()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let descriptors = staged
        .destination_descriptors(&source.verified.replay)
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let reservation = match target {
        TransferAcceptanceTarget::View {
            surface_instance_id,
            view_set_id,
            placement,
        } => UiTransferReservation::reserve_surface_mount(
            transfer,
            &export,
            &destination.verified.replay,
            (*surface_instance_id).clone(),
            view_set_id,
            (*placement).clone(),
            &bindings,
            descriptors,
            max_record_bytes,
        ),
        TransferAcceptanceTarget::Tabs {
            surface_instance_id,
        } => UiTransferReservation::reserve_view_sets(
            transfer,
            &export,
            &destination.verified.replay,
            (*surface_instance_id).clone(),
            &bindings,
            descriptors,
            max_record_bytes,
        ),
        TransferAcceptanceTarget::SharedDock { edge } => {
            UiTransferReservation::reserve_shared_dock(
                transfer,
                &export,
                &destination.verified.replay,
                edge,
                &bindings,
                descriptors,
                max_record_bytes,
            )
        }
    }
    .map_err(HandlerError::Conflict)?;
    let reservation_value = serde_json::to_value(&reservation)?;
    let import_digest =
        ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&reservation_value)
            .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let owner = seat_owner(session);
    let source_context = source.context(&owner, &transfer.source);
    let destination_context = destination.context(&owner, &transfer.destination);
    let publication = Publication::read(state, session, [&proof.source, &proof.destination])?;
    let created = std::cell::Cell::new(false);
    let result = ui
        .browser_sessions
        .with_exact_transfer_reservation_snapshot(
            &session.session_id,
            &expected_memberships(session),
            policy_digest,
            |live_session| {
                append_transfer_accepted(
                    &state.state_store,
                    &seat_kind(state)?,
                    &AcceptanceInputs {
                        transfer,
                        proof: &proof.history,
                        preparation,
                        inbound_event: &inbound_event,
                        prepared_event: &prepared_event,
                        reservation: &reservation_value,
                    },
                    TransferSeatPair {
                        target: &destination_context,
                        counterpart: &source_context,
                    },
                    publication.dependencies(),
                    || {
                        if !publication.admits(live_session, session.next_attachment_generation) {
                            return Ok(false);
                        }
                        let outcome =
                            BrowserSessionStore::reserve_hidden_transfer_attachment_batch_in_session(
                                live_session,
                                &staged,
                                &export,
                                &reservation,
                                max_attachments,
                                max_record_bytes,
                                policy_digest,
                            )?;
                        created.set(outcome == HiddenTransferReservationOutcome::Created);
                        Ok(true)
                    },
                )
            },
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))??;
    let retain = || {
        ui.browser_sessions
            .stage_retained_transfer_attachment_batch(
                &session.session_id,
                transfer,
                &export,
                &reservation,
                max_record_bytes,
                policy_digest,
            )
            .is_ok()
    };
    Ok(match result {
        TransferAppendResult::Committed(records) => {
            let acceptance = committed_evidence(&records)?;
            if retain() {
                accepted_response(&transfer.operation_id, acceptance)
            } else {
                transfer_response(&transfer.operation_id, "uncertain")
            }
        }
        TransferAppendResult::Conflict | TransferAppendResult::NotRunning => {
            if created.get() {
                ui.browser_sessions
                    .discard_new_hidden_transfer_attachment_batch(&session.session_id, &staged)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?;
            }
            transfer_response(&transfer.operation_id, "conflict")
        }
        TransferAppendResult::Uncertain { .. } => {
            match read_transfer_accepted_successor(
                &state.state_store,
                transfer,
                preparation,
                &import_digest,
                &destination_context,
            ) {
                Ok(TransferAcceptedReadback::Observed { acceptance }) if retain() => {
                    accepted_response(&transfer.operation_id, acceptance)
                }
                _ => transfer_response(&transfer.operation_id, "uncertain"),
            }
        }
    })
}

/// Commit or abort the source decision from a completed paired history.
fn source_decision(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    transfer: &TransferIdentity,
    proof: &TransferAdmissionProof,
    decision: TransferSourceDecision,
) -> Result<Value> {
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    let source = authenticate_seat(
        generation,
        state,
        session,
        &transfer.source,
        &proof.source,
        SeatRule::ActiveProducer,
    )?;
    let destination = authenticate_seat(
        generation,
        state,
        session,
        &transfer.destination,
        &proof.destination,
        SeatRule::ActiveProducer,
    )?;
    let lifecycle = proof.history.lifecycle();
    let recorded = match decision {
        TransferSourceDecision::Commit => lifecycle.committed(),
        TransferSourceDecision::Abort => lifecycle.aborted(),
    };
    if let Some(recorded) = recorded {
        // Exact completed paired readback, not a second decision.
        return Ok(json!({"operation_id": transfer.operation_id,
            "outcome": "recorded", "decision": recorded.evidence}));
    }
    let prepared = lifecycle
        .prepared()
        .ok_or_else(|| conflict("source decision has no retained preparation"))?;
    let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
    let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
        .map_err(HandlerError::Conflict)?;
    export
        .validate_affected_snapshot(transfer, &source.verified.replay, max_record_bytes)
        .map_err(HandlerError::Conflict)?;
    revalidate_export_bindings(generation, state, session, &source.membership, &export)?;
    if export.mode == UiTransferExportMode::Move {
        let TransferPhase::Prepared {
            freeze: Some(links),
            ..
        } = &prepared.record.phase
        else {
            return Err(conflict("MOVE decision has no retained freeze"));
        };
        freeze_evidence::verify_retained_move_freeze(
            &source,
            &proof.source_cut,
            &proof.history,
            transfer,
            &export,
            links,
        )?;
    }
    let policy_digest = state.node_policy.generation_digest();
    if let Some(accepted) = lifecycle.accepted() {
        let reservation = UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
            .map_err(HandlerError::Conflict)?;
        reservation
            .validate_export(transfer, &export, max_record_bytes)
            .map_err(HandlerError::Conflict)?;
        if decision == TransferSourceDecision::Commit {
            ui.browser_sessions
                .stage_retained_transfer_attachment_batch(
                    &session.session_id,
                    transfer,
                    &export,
                    &reservation,
                    max_record_bytes,
                    policy_digest,
                )
                .map_err(|error| HandlerError::Conflict(error.to_string()))?;
        }
    }
    let owner = seat_owner(session);
    let source_context = source.context(&owner, &transfer.source);
    let destination_context = destination.context(&owner, &transfer.destination);
    let publication = Publication::read(state, session, [&proof.source, &proof.destination])?;
    let result = ui
        .browser_sessions
        .with_exact_presentation_read_snapshot(
            &session.session_id,
            &expected_memberships(session),
            policy_digest,
            |live_session| {
                append_transfer_source_decision(
                    &state.state_store,
                    &seat_kind(state)?,
                    &proof.history,
                    decision,
                    TransferSeatPair {
                        target: &source_context,
                        counterpart: &destination_context,
                    },
                    publication.dependencies(),
                    || Ok(publication.admits(live_session, session.next_attachment_generation)),
                )
            },
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))??;
    Ok(match result {
        TransferAppendResult::Committed(records) => json!({
            "operation_id": transfer.operation_id,
            "outcome": "recorded",
            "decision": committed_evidence(&records)?,
        }),
        TransferAppendResult::Conflict | TransferAppendResult::NotRunning => {
            transfer_response(&transfer.operation_id, "conflict")
        }
        TransferAppendResult::Uncertain { .. } => {
            transfer_response(&transfer.operation_id, "uncertain")
        }
    })
}

/// Activated is first a signed destination event, then a separately completed
/// paired readback. Hidden bindings remain unpublished on conflict, uncertain
/// append, partial read, or a changed session/head during finalization.
async fn activate_ingress(
    ctx: &HandlerContext,
    state: Arc<AppState>,
    transfer: &TransferIdentity,
) -> Result<Value> {
    enum WriteOutcome {
        Evidence(TransferEvidence),
        Conflict,
        Uncertain,
    }
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let session = browser_session(ctx, &state)?;
    require_both_memberships(&state, &session, transfer)?;
    let proof = read_transfer_proof(state.clone(), &session, transfer).await?;
    let written = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = browser_session(ctx, &state)?;
        state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<WriteOutcome> {
                let source = authenticate_seat(
                    generation,
                    &state,
                    &session,
                    &transfer.source,
                    &proof.source,
                    SeatRule::ActiveProducer,
                )?;
                let destination = authenticate_seat(
                    generation,
                    &state,
                    &session,
                    &transfer.destination,
                    &proof.destination,
                    SeatRule::ActiveProducer,
                )?;
                let lifecycle = proof.history.lifecycle();
                if let Some(activated) = lifecycle.activated() {
                    return Ok(WriteOutcome::Evidence((*activated.evidence).clone()));
                }
                if lifecycle.source_state() != TransferSourceState::Committed
                    || lifecycle.destination_state() != TransferDestinationState::Reserved
                {
                    return Ok(WriteOutcome::Conflict);
                }
                let prepared = lifecycle
                    .prepared()
                    .ok_or_else(|| conflict("activation has no preparation"))?;
                let accepted = lifecycle
                    .accepted()
                    .ok_or_else(|| conflict("activation has no acceptance"))?;
                let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
                let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
                    .map_err(HandlerError::Conflict)?;
                let reservation =
                    UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
                        .map_err(HandlerError::Conflict)?;
                reservation
                    .validate_export(transfer, &export, max_record_bytes)
                    .map_err(HandlerError::Conflict)?;
                let policy_digest = state.node_policy.generation_digest();
                let staged = ui
                    .browser_sessions
                    .stage_activation_transfer_attachment_batch(
                        &session.session_id,
                        &proof.history,
                        &export,
                        &reservation,
                        max_record_bytes,
                        policy_digest,
                    )
                    .map_err(|error| HandlerError::Conflict(error.to_string()))?;
                revalidate_activation_bindings(
                    generation,
                    &state,
                    &session,
                    staged.destination_admissions(),
                )?;
                // All path and pin checks happen before the store's ordered
                // chain exclusion. The publication callback is in-memory only.
                ui.browser_sessions
                    .prevalidate_transfer_attachment_batch(
                        &staged,
                        &export,
                        &reservation,
                        max_record_bytes,
                        policy_digest,
                    )
                    .map_err(|error| HandlerError::Conflict(error.to_string()))?;
                let owner = seat_owner(&session);
                let source_context = source.context(&owner, &transfer.source);
                let destination_context = destination.context(&owner, &transfer.destination);
                let publication =
                    Publication::read(&state, &session, [&proof.source, &proof.destination])?;
                let result = ui
                    .browser_sessions
                    .with_exact_presentation_read_snapshot(
                        &session.session_id,
                        &expected_memberships(&session),
                        policy_digest,
                        |live_session| {
                            append_transfer_activated(
                                &state.state_store,
                                &seat_kind(&state)?,
                                &proof.history,
                                TransferSeatPair {
                                    target: &destination_context,
                                    counterpart: &source_context,
                                },
                                publication.dependencies(),
                                || {
                                    Ok(publication
                                        .admits(live_session, session.next_attachment_generation))
                                },
                            )
                        },
                    )
                    .map_err(|error| HandlerError::Conflict(error.to_string()))??;
                Ok(match result {
                    TransferAppendResult::Committed(records) => {
                        WriteOutcome::Evidence(committed_evidence(&records)?)
                    }
                    TransferAppendResult::Conflict | TransferAppendResult::NotRunning => {
                        WriteOutcome::Conflict
                    }
                    TransferAppendResult::Uncertain { .. } => WriteOutcome::Uncertain,
                })
            })?
    };
    if matches!(written, WriteOutcome::Conflict) {
        return Ok(transfer_response(&transfer.operation_id, "conflict"));
    }
    let session = browser_session(ctx, &state)?;
    let completed = match read_transfer_proof(state.clone(), &session, transfer).await {
        Ok(proof) => proof,
        Err(_) => return Ok(transfer_response(&transfer.operation_id, "uncertain")),
    };
    let Some(activated) = completed.history.lifecycle().activated() else {
        return Ok(transfer_response(&transfer.operation_id, "uncertain"));
    };
    let activation = (*activated.evidence).clone();
    if let WriteOutcome::Evidence(expected) = written
        && expected != activation
    {
        return Ok(transfer_response(&transfer.operation_id, "conflict"));
    }
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(ctx, &state)?;
    state
        .engine
        .with_checked_bundle_generation(|generation| -> Result<Value> {
            let source = authenticate_seat(
                generation,
                &state,
                &session,
                &transfer.source,
                &completed.source,
                SeatRule::ActiveProducer,
            )?;
            let destination = authenticate_seat(
                generation,
                &state,
                &session,
                &transfer.destination,
                &completed.destination,
                SeatRule::ActiveProducer,
            )?;
            let lifecycle = completed.history.lifecycle();
            let prepared = lifecycle
                .prepared()
                .ok_or_else(|| conflict("activation has no preparation"))?;
            let accepted = lifecycle
                .accepted()
                .ok_or_else(|| conflict("activation has no acceptance"))?;
            let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
            let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
                .map_err(HandlerError::Conflict)?;
            let reservation =
                UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
                    .map_err(HandlerError::Conflict)?;
            let policy_digest = state.node_policy.generation_digest();
            let max_attachments = session_attachment_bound(&state)?;
            let staged = ui
                .browser_sessions
                .stage_activation_transfer_attachment_batch(
                    &session.session_id,
                    &completed.history,
                    &export,
                    &reservation,
                    max_record_bytes,
                    policy_digest,
                )
                .map_err(|error| HandlerError::Conflict(error.to_string()))?;
            revalidate_activation_bindings(
                generation,
                &state,
                &session,
                staged.destination_admissions(),
            )?;
            let prevalidated = ui
                .browser_sessions
                .prevalidate_transfer_attachment_batch(
                    &staged,
                    &export,
                    &reservation,
                    max_record_bytes,
                    policy_digest,
                )
                .map_err(|error| HandlerError::Conflict(error.to_string()))?;
            let publication = Publication::read(
                &state,
                &session,
                [&completed.source, &completed.destination],
            )?;
            let published = ui
                .browser_sessions
                .activate_transfer_attachment_batch_at_mixed_cut(
                    &state.state_store,
                    &publication.anchors,
                    &publication.retired,
                    &expected_memberships(&session),
                    &publication.cuts,
                    &session.session_id,
                    &source.membership,
                    &destination.membership,
                    &prevalidated,
                    &export,
                    &reservation,
                    max_record_bytes,
                    max_attachments,
                    policy_digest,
                )?;
            if published.is_none() {
                return Ok(transfer_response(&transfer.operation_id, "conflict"));
            }
            Ok(activated_response(&transfer.operation_id, activation))
        })
}

/// Cancel a MOVE freeze whose Prepared never became a transfer. The source
/// is the writer; the destination's frontier is conditioned but takes no part.
async fn cancel_freeze_ingress(
    ctx: &HandlerContext,
    state: Arc<AppState>,
    transfer: &TransferIdentity,
) -> Result<Value> {
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let session = browser_session(ctx, &state)?;
    // BOTH memberships precede any seat read.
    require_both_memberships(&state, &session, transfer)?;
    let source = read_seat_frontier(
        state.clone(),
        &session,
        &transfer.source.presentation_id,
        &transfer.source.thread_id,
        Some(&transfer.source.chain_root_id),
    )
    .await?;
    let destination = read_seat_frontier(
        state.clone(),
        &session,
        &transfer.destination.presentation_id,
        &transfer.destination.thread_id,
        Some(&transfer.destination.chain_root_id),
    )
    .await?;
    let source_cut = read_fixed_transfer_seat(
        state.clone(),
        transfer.source.clone(),
        source.anchor.clone(),
        transfer.clone(),
    )
    .await?;
    let destination_cut = read_fixed_transfer_seat(
        state.clone(),
        transfer.destination.clone(),
        destination.anchor.clone(),
        transfer.clone(),
    )
    .await?;
    let proof = assemble_move_freeze_cancellation_proof(&source_cut, &destination_cut, transfer)?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(ctx, &state)?;
    state
        .engine
        .with_checked_bundle_generation(|generation| -> Result<Value> {
            let source_seat = authenticate_seat(
                generation,
                &state,
                &session,
                &transfer.source,
                &source,
                SeatRule::Counterpart,
            )?;
            let destination_seat = authenticate_seat(
                generation,
                &state,
                &session,
                &transfer.destination,
                &destination,
                SeatRule::Counterpart,
            )?;
            // The exact receipted freeze must be proven from the completed
            // source cut, whether the cancellation is new or already retained.
            let export = freeze_evidence::verify_source_cancellation_freeze(
                &source_seat,
                &source_cut,
                transfer,
                proof.freeze(),
            )?;
            if let Some((evidence, _)) = proof.retained_cancellation() {
                return Ok(json!({"operation_id": transfer.operation_id,
                    "outcome": "freeze_cancelled", "cancellation": evidence}));
            }
            if !source_seat
                .verified
                .control
                .as_ref()
                .is_some_and(|control| control.state == ProducerState::Active)
            {
                return Ok(transfer_response(&transfer.operation_id, "conflict"));
            }
            revalidate_export_bindings(
                generation,
                &state,
                &session,
                &source_seat.membership,
                &export,
            )?;
            let owner = seat_owner(&session);
            let source_context = source_seat.context(&owner, &transfer.source);
            let destination_context = destination_seat.context(&owner, &transfer.destination);
            let publication = Publication::read(&state, &session, [&source, &destination])?;
            let policy_digest = state.node_policy.generation_digest();
            let source_membership = source_seat.membership.clone();
            let destination_membership = destination_seat.membership.clone();
            let next_generation = session.next_attachment_generation;
            let outcome = append_move_freeze_cancellation(
                &state.state_store,
                &seat_kind(&state)?,
                &proof,
                TransferSeatPair {
                    target: &source_context,
                    counterpart: &destination_context,
                },
                publication.dependencies(),
                || {
                    Ok(ui
                        .browser_sessions
                        .matches_transfer_preparation_memberships(
                            &session.session_id,
                            &source_membership,
                            &destination_membership,
                            next_generation,
                            policy_digest,
                        ))
                },
            )?;
            Ok(match outcome {
                MoveFreezeCancellationAppendResult::Committed(records) => json!({
                    "operation_id": transfer.operation_id,
                    "outcome": "freeze_cancelled",
                    "cancellation": committed_cancellation_evidence(&records, transfer)?,
                }),
                MoveFreezeCancellationAppendResult::AlreadyRecorded(evidence) => json!({
                    "operation_id": transfer.operation_id,
                    "outcome": "freeze_cancelled",
                    "cancellation": evidence,
                }),
                MoveFreezeCancellationAppendResult::Conflict
                | MoveFreezeCancellationAppendResult::NotRunning => {
                    transfer_response(&transfer.operation_id, "conflict")
                }
            })
        })
}

fn committed_cancellation_evidence(
    records: &[PersistedEventRecord],
    transfer: &TransferIdentity,
) -> Result<TransferEvidence> {
    let evidence = committed_evidence(records)?;
    if records[0].chain_root_id != transfer.source.chain_root_id
        || records[0].thread_id != transfer.source.thread_id
        || records[0].event_type
            != ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        return Err(HandlerError::Internal(
            "freeze cancellation append returned another event owner".into(),
        )
        .into());
    }
    Ok(evidence)
}

/// A browser cannot choose a pair of stale seat heads as authority. This
/// handler holds the existing transition and checked-engine guards through
/// the exact two-chain append; the bounded session callback performs only an
/// in-memory membership check after the store and ordered chain locks.
pub(super) async fn handle_transfer(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let request: TransferRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid transfer request: {error}")))?;
    let transfer = match &request {
        TransferRequest::CancelFreeze { transfer }
        | TransferRequest::Commit { transfer }
        | TransferRequest::Abort { transfer }
        | TransferRequest::Activate { transfer }
        | TransferRequest::Prepare { transfer, .. }
        | TransferRequest::AcceptView { transfer, .. }
        | TransferRequest::AcceptTabs { transfer, .. }
        | TransferRequest::AcceptSharedDock { transfer, .. } => transfer,
    };
    TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::InboundCommitment,
    }
    .validate_shape()
    .map_err(HandlerError::BadRequest)?;
    if transfer.source.presentation_id == transfer.destination.presentation_id {
        return Err(HandlerError::BadRequest("transfer requires distinct seats".into()).into());
    }
    if matches!(&request, TransferRequest::CancelFreeze { .. }) {
        return cancel_freeze_ingress(&ctx, state.clone(), transfer).await;
    }
    if matches!(&request, TransferRequest::Activate { .. }) {
        return activate_ingress(&ctx, state.clone(), transfer).await;
    }
    // Authenticate exact session membership before any potentially long CAS
    // read. That read holds no seat-transition or store lock. The live
    // membership and both signed heads are rechecked after it completes.
    let session = browser_session(&ctx, &state)?;
    require_both_memberships(&state, &session, transfer)?;
    let proof = if matches!(&request, TransferRequest::Prepare { .. }) {
        None
    } else {
        Some(read_transfer_proof(state.clone(), &session, transfer).await?)
    };
    // A new operation's absence proof is read at each seat's own completed
    // frontier. Existing operations never replace that immutable identity.
    let preparation = if matches!(&request, TransferRequest::Prepare { .. }) {
        let source = read_seat_frontier(
            state.clone(),
            &session,
            &transfer.source.presentation_id,
            &transfer.source.thread_id,
            Some(&transfer.source.chain_root_id),
        )
        .await?;
        let destination = read_seat_frontier(
            state.clone(),
            &session,
            &transfer.destination.presentation_id,
            &transfer.destination.thread_id,
            Some(&transfer.destination.chain_root_id),
        )
        .await?;
        let source_proof = read_fixed_transfer_seat(
            state.clone(),
            transfer.source.clone(),
            source.anchor.clone(),
            transfer.clone(),
        )
        .await?;
        let destination_proof = read_fixed_transfer_seat(
            state.clone(),
            transfer.destination.clone(),
            destination.anchor.clone(),
            transfer.clone(),
        )
        .await?;
        Some((source, destination, source_proof, destination_proof))
    } else {
        None
    };
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    state
        .engine
        .with_checked_bundle_generation(|generation| -> Result<Value> {
            let acceptance = match &request {
                TransferRequest::Commit { .. } | TransferRequest::Abort { .. } => {
                    let decision = if matches!(&request, TransferRequest::Commit { .. }) {
                        TransferSourceDecision::Commit
                    } else {
                        TransferSourceDecision::Abort
                    };
                    return source_decision(
                        generation,
                        &state,
                        &session,
                        transfer,
                        proof
                            .as_ref()
                            .expect("source decision has a completed proof"),
                        decision,
                    );
                }
                TransferRequest::AcceptView {
                    inbound,
                    preparation,
                    surface_instance_id,
                    view_set_id,
                    placement,
                    ..
                } => Some((
                    inbound,
                    preparation,
                    TransferAcceptanceTarget::View {
                        surface_instance_id,
                        view_set_id: *view_set_id,
                        placement,
                    },
                )),
                TransferRequest::AcceptTabs {
                    inbound,
                    preparation,
                    surface_instance_id,
                    ..
                } => Some((
                    inbound,
                    preparation,
                    TransferAcceptanceTarget::Tabs {
                        surface_instance_id,
                    },
                )),
                TransferRequest::AcceptSharedDock {
                    inbound,
                    preparation,
                    edge,
                    ..
                } => Some((
                    inbound,
                    preparation,
                    TransferAcceptanceTarget::SharedDock { edge },
                )),
                _ => None,
            };
            if let Some((inbound, preparation, target)) = acceptance {
                return accept_transfer(
                    generation,
                    &state,
                    &session,
                    transfer,
                    proof.as_ref().expect("acceptance has a completed proof"),
                    inbound,
                    preparation,
                    target,
                );
            }
            let TransferRequest::Prepare { export, freeze, .. } = &request else {
                unreachable!("every other request returned above")
            };
            let (source_frontier, destination_frontier, source_proof, destination_proof) =
                preparation
                    .as_ref()
                    .expect("Prepare has completed absence proofs");
            prepare_transfer(
                generation,
                &state,
                &session,
                transfer,
                export,
                freeze.as_ref(),
                (source_frontier, destination_frontier),
                (source_proof, destination_proof),
            )
        })
}

/// Destination-first InboundCommitment plus source Prepared, published as one
/// ordered pair. A MOVE is admitted only against the freeze its completed
/// source cut authenticated.
fn prepare_transfer(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    transfer: &TransferIdentity,
    export: &Value,
    freeze: Option<&MoveFreezeEvidence>,
    frontiers: (&SeatFrontier, &SeatFrontier),
    proofs: (
        &Arc<UiTransferFixedSeatProof>,
        &Arc<UiTransferFixedSeatProof>,
    ),
) -> Result<Value> {
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    let (source_frontier, destination_frontier) = frontiers;
    let (source_proof, destination_proof) = proofs;
    let source = authenticate_seat(
        generation,
        state,
        session,
        &transfer.source,
        source_frontier,
        SeatRule::PreparationFrontier,
    )?;
    let destination = authenticate_seat(
        generation,
        state,
        session,
        &transfer.destination,
        destination_frontier,
        SeatRule::PreparationFrontier,
    )?;
    let decoded: UiTransferExport = serde_json::from_value(export.clone())
        .map_err(|error| HandlerError::BadRequest(format!("invalid source export: {error}")))?;
    match decoded.mode {
        UiTransferExportMode::Move => {
            let links = source_proof.preparation_freeze_links(
                &state.state_store,
                &source.anchor,
                transfer,
            )?;
            let retained = freeze_evidence::verify_source_move_freeze(
                &source,
                source_proof,
                transfer,
                &links,
            )?;
            if freeze != Some(&links) {
                return Err(conflict(
                    "MOVE request links differ from completed source testimony",
                ));
            }
            if retained != decoded {
                return Err(conflict("MOVE export differs from exact Core freeze"));
            }
        }
        UiTransferExportMode::Copy => {
            if freeze.is_some() {
                return Err(
                    HandlerError::BadRequest("COPY cannot carry MOVE freeze links".into()).into(),
                );
            }
        }
    }
    decoded
        .validate_snapshot(transfer, &source.verified.replay, source.max_append_bytes)
        .map_err(HandlerError::Conflict)?;
    revalidate_export_bindings(generation, state, session, &source.membership, &decoded)?;
    let owner = seat_owner(session);
    let source_context = source.context(&owner, &transfer.source);
    let destination_context = destination.context(&owner, &transfer.destination);
    let publication = Publication::read(state, session, [source_frontier, destination_frontier])?;
    let next_attachment_generation = session.next_attachment_generation;
    let pair = TransferSeatPair {
        target: &destination_context,
        counterpart: &source_context,
    };
    // Keep the whole original membership snapshot live throughout
    // publication, including prior-operation dependency seats.
    let result = ui
        .browser_sessions
        .with_exact_presentation_read_snapshot(
            &session.session_id,
            &expected_memberships(session),
            state.node_policy.generation_digest(),
            |live_session| {
                append_transfer_preparation_pair(
                    &state.state_store,
                    &seat_kind(state)?,
                    &PreparationInputs {
                        transfer,
                        export,
                        source_proof,
                        destination_proof,
                    },
                    pair,
                    publication.dependencies(),
                    || Ok(publication.admits(live_session, next_attachment_generation)),
                )
            },
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))??;
    Ok(match result {
        CutPairAppendResult::Committed { first, second } => prepared_response(
            &transfer.operation_id,
            committed_evidence(&first)?,
            committed_evidence(&second)?,
        ),
        CutPairAppendResult::Conflict | CutPairAppendResult::NotRunning => {
            transfer_response(&transfer.operation_id, "conflict")
        }
        CutPairAppendResult::FirstUncertain(_)
        | CutPairAppendResult::FirstCommittedSecondUncertain { .. } => {
            // This is observation, never an absence proof or retry permit. A
            // linked pair can be reported; every other state remains
            // uncertain and requires ordered two-head reconciliation.
            match read_transfer_preparation_pair(&state.state_store, transfer, export, pair) {
                Ok(TransferPreparationReadback::LinkedPairObserved { inbound, prepared }) => {
                    prepared_response(&transfer.operation_id, inbound, prepared)
                }
                _ => transfer_response(&transfer.operation_id, "uncertain"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Value {
        let seat = |presentation: &str, root: &str| {
            json!({
                "presentation_id": presentation,
                "thread_id": root,
                "chain_root_id": root,
                "producer_incarnation": "a".repeat(64),
                "revision": 0,
            })
        };
        json!({
            "operation_id": "44444444-4444-4444-8444-444444444444",
            "source": seat("11111111-1111-4111-8111-111111111111", "T-source"),
            "destination": seat("22222222-2222-4222-8222-222222222222", "T-destination"),
            "export_digest": "b".repeat(64),
        })
    }

    #[test]
    fn activate_request_is_closed_and_carries_only_exact_identity() {
        serde_json::from_value::<TransferRequest>(
            json!({"action": "activate", "transfer": identity()}),
        )
        .unwrap();
        assert!(
            serde_json::from_value::<TransferRequest>(
                json!({"action": "activate", "transfer": identity(), "freeze": null})
            )
            .is_err()
        );
    }

    #[test]
    fn cancel_freeze_request_is_closed_and_carries_full_identity() {
        serde_json::from_value::<TransferRequest>(
            json!({"action": "cancel_freeze", "transfer": identity()}),
        )
        .unwrap();
        assert!(
            serde_json::from_value::<TransferRequest>(
                json!({"action": "cancel_freeze", "transfer": identity(), "extra": 1})
            )
            .is_err()
        );
        let mut partial = identity();
        partial.as_object_mut().unwrap().remove("destination");
        assert!(
            serde_json::from_value::<TransferRequest>(
                json!({"action": "cancel_freeze", "transfer": partial})
            )
            .is_err()
        );
    }

    #[test]
    fn prepare_requires_explicit_nullable_freeze_and_rejects_extra_authority() {
        let base = json!({"action": "prepare", "transfer": identity(), "export": {}});
        assert!(serde_json::from_value::<TransferRequest>(base.clone()).is_err());
        let mut explicit = base.clone();
        explicit["freeze"] = Value::Null;
        serde_json::from_value::<TransferRequest>(explicit.clone()).unwrap();
        explicit["authority"] = json!("renderer");
        assert!(serde_json::from_value::<TransferRequest>(explicit).is_err());
    }

    #[test]
    fn accept_requests_keep_server_allocations_closed() {
        let accept = |action: &str, extra: Value| {
            let mut value = json!({
                "action": action,
                "transfer": identity(),
                "inbound": {"event_hash": "c".repeat(64), "chain_seq": 3},
                "preparation": {"event_hash": "d".repeat(64), "chain_seq": 4},
            });
            for (key, field) in extra.as_object().unwrap() {
                value[key] = field.clone();
            }
            serde_json::from_value::<TransferRequest>(value)
        };
        assert!(
            accept(
                "accept_tabs",
                json!({"surface_instance_id": "e0e0e0e0-e0e0-40e0-80e0-e0e0e0e0e0e0"})
            )
            .is_ok()
        );
        assert!(accept("accept_shared_dock", json!({"edge": "left"})).is_ok());
        // The server allocates every destination coordinate: a client cannot
        // add its own binding or view-set allocation to a tabs acceptance.
        assert!(
            accept(
                "accept_tabs",
                json!({
                    "surface_instance_id": "e0e0e0e0-e0e0-40e0-80e0-e0e0e0e0e0e0",
                    "view_set_id": 7
                })
            )
            .is_err()
        );
        assert!(
            accept(
                "accept_shared_dock",
                json!({"edge": "left", "bindings": {}})
            )
            .is_err()
        );
    }

    #[test]
    fn an_old_lifecycle_selector_is_not_a_transfer_action() {
        for action in ["settle", "reap", "transfer_lifecycle_v2"] {
            assert!(
                serde_json::from_value::<TransferRequest>(
                    json!({"action": action, "transfer": identity()})
                )
                .is_err(),
                "{action}"
            );
        }
    }
}
