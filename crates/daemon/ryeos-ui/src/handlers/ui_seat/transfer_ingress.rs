//! Narrow browser-authenticated ingress for the destination-first preparation
//! pair. Ordinary seat append remains unable to publish transfer testimony.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, anyhow, bail};
use ryeos_app::state_store::{
    ThreadDetail, UiMoveFreezeCancellationAppendResult, UiTransferAcceptedAppendResult,
    UiTransferAcceptedReadback, UiTransferPairReadback, UiTransferPairSeat,
};
use ryeos_app::{handler_context::HandlerContext, handler_error::HandlerError, state::AppState};
use ryeos_client_base::ids::{SurfaceInstanceId, ViewSetId};
use ryeos_client_base::ui::{
    binding::UiBindingAttachmentCoordinate,
    composition_operation::CompositionPlacement,
    transfer_export::{UiTransferExport, UiTransferExportMode},
    transfer_reservation::{UiTransferDestinationPlacement, UiTransferReservation},
};
use ryeos_state::state_db::ConditionalThreadEventPairAppendResult;
use ryeos_ui_presentation_contract::transfer::{
    TRANSFER_SCHEMA, TransferEvidence, TransferIdentity, TransferPhase, TransferRecord,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(test)]
use super::read_completed_seat_history;
use super::{
    BrowserSession, browser_session, composition, get_ui_state, require_presentation_seat,
    require_presentation_seat_for_read, seat_owner,
};
use crate::browser_session::{
    BrowserSessionStore, HiddenTransferReservationOutcome, PresentationMembership,
};

mod freeze_evidence;
mod paired_read;

#[cfg(test)]
pub(crate) use paired_read::test_cut_from_completed_historical_proof;
#[cfg(test)]
pub(crate) use paired_read::test_cut_from_completed_positive_proof;
pub(crate) use paired_read::{DiscoveredRootTransferCut, FixedTransferCut};

/// Internal read-only consumer of the replay's original cut. It does not
/// renew a session, claim a producer or authorize stream completion.
pub(crate) async fn read_replay_transfer_cut_at_anchor(
    state: Arc<AppState>,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
) -> Result<DiscoveredRootTransferCut> {
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    paired_read::read_root_transfer_cut_at_anchor(
        state,
        &session,
        presentation_id,
        chain_root_id,
        thread_id,
        anchor,
    )
    .await
}

/// Historical bootstrap supplies the immutable marker-bound endpoint. The
/// session association is reauthorized before discovery; a missing transfer
/// locator does not certify composition or grant a live writer.
pub(crate) async fn read_replay_transfer_cut_at_retired_prefix(
    state: Arc<AppState>,
    session_id: &str,
    presentation_id: &str,
    prefix: ryeos_state::RetiredSeatPositivePrefix,
) -> Result<(
    PresentationMembership,
    Vec<TransferIdentity>,
    Option<FixedTransferCut>,
)> {
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    paired_read::read_root_transfer_cut_at_retired_prefix(state, &session, presentation_id, prefix)
        .await
}

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
        freeze: Option<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence>,
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
) -> std::result::Result<
    Option<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence>,
    D::Error,
> {
    Option::deserialize(deserializer)
}

struct AuthenticatedSeat {
    detail: ThreadDetail,
    membership: PresentationMembership,
    verified: composition::VerifiedHistory,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    max_append_bytes: u64,
}

struct TransferAdmissionProof {
    history: ryeos_app::thread_lifecycle::presentation_transfer::UiTransferFixedHistoryProof,
    cut: Arc<FixedTransferCut>,
    source: Arc<super::VerifiedTransferSeatPrefix>,
    destination: Arc<super::VerifiedTransferSeatPrefix>,
}

impl TransferAdmissionProof {
    fn seat(
        &self,
        expected: &ryeos_ui_presentation_contract::transfer::TransferSeat,
    ) -> Result<(
        &super::VerifiedTransferSeatPrefix,
        &ryeos_state::AuthoritativeThreadEventReadAnchor,
    )> {
        let transfer = self.history.lifecycle().transfer();
        if expected == &transfer.source {
            Ok((&self.source, self.history.source_anchor()))
        } else if expected == &transfer.destination {
            Ok((&self.destination, self.history.destination_anchor()))
        } else {
            Err(HandlerError::Conflict("transfer proof belongs to another seat".into()).into())
        }
    }
}

fn authenticated_seat(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    expected: &ryeos_ui_presentation_contract::transfer::TransferSeat,
    require_preparation_frontier: bool,
    transfer_proof: Option<&TransferAdmissionProof>,
) -> Result<AuthenticatedSeat> {
    authenticated_seat_with_frontier(
        generation,
        state,
        session,
        expected,
        require_preparation_frontier,
        transfer_proof,
        None,
    )
}

fn authenticated_seat_with_frontier(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    expected: &ryeos_ui_presentation_contract::transfer::TransferSeat,
    require_preparation_frontier: bool,
    transfer_proof: Option<&TransferAdmissionProof>,
    completed: Option<&super::CompletedSeatIngressRead>,
) -> Result<AuthenticatedSeat> {
    // Transfer-bearing history is validated by the completed paired proof
    // below. A singleton membership repair can omit phase materialization
    // after Activated/Commit and must not preempt that exact-cut admission.
    let detail = if transfer_proof.is_some() || completed.is_some() {
        require_presentation_seat_for_read(
            state,
            session,
            &expected.presentation_id,
            Some(&expected.thread_id),
            Some(&expected.chain_root_id),
        )?
        .0
    } else {
        // The initial preparation path has no paired proof. Keep its ordinary
        // membership repair and fail-closed transfer-history fence intact.
        require_presentation_seat(
            state,
            session,
            &expected.presentation_id,
            Some(&expected.thread_id),
            Some(&expected.chain_root_id),
        )?
    };
    if detail.thread_id != detail.chain_root_id || detail.status != "running" {
        return Err(
            HandlerError::Conflict("transfer requires an exact running seat root".into()).into(),
        );
    }
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &expected.presentation_id,
            Some(&expected.thread_id),
            Some(&expected.chain_root_id),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let (verified, anchor) = if let Some(completed) = completed {
        completed.authorize_history(
            state,
            session,
            &expected.presentation_id,
            &expected.thread_id,
        )?;
        let current = state
            .state_store
            .capture_thread_event_read_anchor(&expected.chain_root_id, &expected.thread_id)?;
        if current.as_ref() != Some(&completed.anchor) {
            return Err(HandlerError::Conflict("preparation completed head changed".into()).into());
        }
        (completed.verified.clone(), completed.anchor.clone())
    } else if let Some(proof) = transfer_proof {
        let (prefix, anchor) = proof.seat(expected)?;
        let verified = prefix.history(&membership.initialization)?;
        let current = state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)?;
        if current.as_ref() != Some(anchor)
            || anchor.tip() != Some((verified.tip_hash.as_str(), verified.thread_seq))
        {
            return Err(
                HandlerError::Conflict("transfer admission signed head changed".into()).into(),
            );
        }
        (verified, anchor.clone())
    } else {
        composition::verified_frontier(state, &detail, &membership.initialization)?
    };
    if verified.replay.revision < expected.revision
        || (require_preparation_frontier && verified.replay.revision != expected.revision)
        || !verified.control.as_ref().is_some_and(|control| {
            control.state == super::ProducerState::Active
                && (!require_preparation_frontier
                    || control.producer_incarnation == expected.producer_incarnation)
        })
    {
        return Err(HandlerError::Conflict(
            "transfer seat has no admitted current active producer frontier".into(),
        )
        .into());
    }
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
        .ok_or_else(|| {
            HandlerError::Conflict(
                "transfer root binding is absent from signed initialization".into(),
            )
        })?;
    let current = super::super::ui_launch_mint::binding_request_bounds(state)?;
    let max_append_bytes = root
        .binding_request_bounds
        .max_seat_append_bytes
        .min(current.max_seat_append_bytes);
    Ok(AuthenticatedSeat {
        detail,
        membership,
        verified,
        anchor,
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
            return Err(HandlerError::Conflict(
                "retained activation admission generation is stale".into(),
            )
            .into());
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
                return Err(HandlerError::Conflict(
                    "retained activation project authority is inconsistent".into(),
                )
                .into());
            }
        }
        super::super::ui_launch_mint::revalidate_binding_authority(
            generation,
            state,
            attachment.compiled_binding.as_ref(),
            project.as_ref().map(|project| project.path()),
            attachment.project_authority.as_deref(),
        )?;
    }
    Ok(())
}

fn pair_seat<'a>(
    seat: &'a AuthenticatedSeat,
    owner: &'a str,
    expected: &'a ryeos_ui_presentation_contract::transfer::TransferSeat,
) -> UiTransferPairSeat<'a> {
    UiTransferPairSeat {
        anchor: &seat.anchor,
        owner,
        surface: &seat.detail.item_ref,
        client_ref: &seat.detail.executor_ref,
        presentation_id: &expected.presentation_id,
        producer_incarnation: &seat
            .verified
            .control
            .as_ref()
            .expect("authenticated seat has active producer")
            .producer_incarnation,
        revision: seat.verified.replay.revision,
        next_engine_seq: seat.verified.next_engine_seq,
        max_append_bytes: seat.max_append_bytes,
    }
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

/// Read the exact pair outside the seat-transition lock. Every blocking turn
/// has an owner-defined page budget; neither a partial read nor task
/// cancellation is evidence that the counterpart lacks a phase. The caller
/// reauthenticates session membership and both signed heads after completion.
async fn read_transfer_proof(
    state: Arc<AppState>,
    session: &BrowserSession,
    transfer: TransferIdentity,
) -> Result<TransferAdmissionProof> {
    // The requested operation selects the admission testimony, while the
    // completed dependency cut also materializes preceding transfer effects.
    // Later receipted edits must see their real composition frontier, not an
    // evidence-only replay that silently ignores earlier activation.
    let mut selected_cut = paired_read::read_fixed_transfer_cut(
        state.clone(),
        session,
        std::slice::from_ref(&transfer),
    )
    .await?;
    let (history, source, destination) = selected_cut.take_operation(&transfer.operation_id)?;
    // The explicit operation reader does not discover older dependencies.
    // Expand through the existing root-cut owner, holding the selected source
    // anchor fixed. A changed destination is a retry, never a substituted cut.
    let DiscoveredRootTransferCut::Transfers { cut, requested, .. } =
        paired_read::read_root_transfer_cut_at_anchor(
            state.clone(),
            session,
            &transfer.source.presentation_id,
            &transfer.source.chain_root_id,
            &transfer.source.thread_id,
            history.source_anchor().clone(),
        )
        .await?
    else {
        bail!("selected transfer is absent from its completed root cut");
    };
    if !requested.contains(&transfer)
        || !cut.covers_seat_anchor(
            &transfer.destination.presentation_id,
            &transfer.destination.chain_root_id,
            &transfer.destination.thread_id,
            history.destination_anchor(),
        )
    {
        bail!("completed transfer dependencies changed the selected pair cut");
    }
    let cut = Arc::new(cut);
    let freeze_events = match history
        .lifecycle()
        .prepared()
        .map(|phase| &phase.record.phase)
    {
        Some(TransferPhase::Prepared {
            freeze: Some(links),
            ..
        }) => vec![links.preparation.clone(), links.completion.clone()],
        _ => Vec::new(),
    };
    let source = read_materialized_transfer_seat(
        state.clone(),
        transfer.source,
        history.source_anchor().clone(),
        source.initialization,
        freeze_events,
        cut.clone(),
    )
    .await?;
    let destination = read_materialized_transfer_seat(
        state,
        transfer.destination,
        history.destination_anchor().clone(),
        destination.initialization,
        Vec::new(),
        cut.clone(),
    )
    .await?;
    Ok(TransferAdmissionProof {
        history,
        cut,
        source,
        destination,
    })
}

fn transfer_checkpoint_key(
    seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
) -> Result<crate::state::CompositionCheckpointKey> {
    Ok(crate::state::CompositionCheckpointKey {
        presentation_id: seat.presentation_id.clone(),
        chain_root_id: seat.chain_root_id.clone(),
        thread_id: seat.thread_id.clone(),
        initialization_digest: ryeos_client_base::ui::seat_payload_digest(&serde_json::to_value(
            initial,
        )?)?,
    })
}

async fn read_fixed_transfer_seat(
    state: Arc<AppState>,
    seat: ryeos_ui_presentation_contract::transfer::TransferSeat,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    initial: &ryeos_client_base::ui::binding::UiPresentationInitialization,
    requested: Vec<TransferIdentity>,
) -> Result<Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiTransferFixedSeatProof>> {
    use ryeos_app::thread_lifecycle::presentation_transfer::reuse_fixed_seat_proof;
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let key = transfer_checkpoint_key(&seat, initial)?;
    let seed = ui.transfer_history_proof_get(&key);
    if let Some(cached) = &seed {
        if let Some(proof) = reuse_fixed_seat_proof(
            state.state_store.as_ref(),
            &anchor,
            &seat,
            &requested,
            cached,
        )? {
            return Ok(proof);
        }
    }
    let attempt = read_fixed_transfer_seat_attempt(
        state.clone(),
        seat.clone(),
        anchor.clone(),
        requested.clone(),
        seed,
    )
    .await;
    let proof = match attempt {
        Ok(proof) => proof,
        Err(failure) if failure.suffix_attempted => {
            // Discard a failed cached lineage/coverage candidate. Reconstruct
            // once from genesis at the SAME captured cut; never switch heads or
            // turn an incomplete suffix into absence/admission evidence.
            ui.composition_checkpoint_invalidate(&key);
            read_fixed_transfer_seat_attempt(state, seat, anchor, requested, None)
                .await
                .map_err(|failure| failure.error)?
        }
        Err(failure) => return Err(failure.error),
    };
    ui.transfer_history_proof_put(key, proof.clone());
    Ok(proof)
}

struct FixedTransferSeatReadFailure {
    error: anyhow::Error,
    /// Scheduling fact only: an ineligible cached seed already caused a cold
    /// read and must not cause another full scan when that cold read fails.
    suffix_attempted: bool,
}

async fn read_fixed_transfer_seat_attempt(
    state: Arc<AppState>,
    seat: ryeos_ui_presentation_contract::transfer::TransferSeat,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    requested: Vec<TransferIdentity>,
    seed: Option<Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiTransferFixedSeatProof>>,
) -> std::result::Result<
    Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiTransferFixedSeatProof>,
    FixedTransferSeatReadFailure,
> {
    use ryeos_app::thread_lifecycle::presentation_transfer::{
        begin_fixed_seat_read, begin_fixed_seat_read_from_proof, finish_fixed_seat_read,
        read_fixed_seat_page,
    };
    let store = state.state_store.clone();
    let read = tokio::task::spawn_blocking(move || match seed {
        Some(proof) => begin_fixed_seat_read_from_proof(store, anchor, &seat, &requested, proof),
        None => begin_fixed_seat_read(store, anchor, &seat, &requested),
    })
    .await
    .map_err(|error| FixedTransferSeatReadFailure {
        error: error.into(),
        suffix_attempted: false,
    })?
    .map_err(|error| FixedTransferSeatReadFailure {
        error,
        suffix_attempted: false,
    })?;
    let suffix_attempted = read.is_suffix_read();
    let attempt: Result<_> = async move {
        let mut read = read;
        loop {
            let (next, result) = tokio::task::spawn_blocking(move || {
                let result = read_fixed_seat_page(&mut read, 64, 2 * 1024 * 1024);
                (read, result)
            })
            .await?;
            read = next;
            if result? {
                break;
            }
        }
        Ok(Arc::new(
            tokio::task::spawn_blocking(move || finish_fixed_seat_read(read)).await??,
        ))
    }
    .await;
    attempt.map_err(|error| FixedTransferSeatReadFailure {
        error,
        suffix_attempted,
    })
}

async fn read_materialized_transfer_seat(
    state: Arc<AppState>,
    seat: ryeos_ui_presentation_contract::transfer::TransferSeat,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    initial: ryeos_client_base::ui::binding::UiPresentationInitialization,
    freeze_events: Vec<TransferEvidence>,
    cut: Arc<FixedTransferCut>,
) -> Result<Arc<super::VerifiedTransferSeatPrefix>> {
    let key = transfer_checkpoint_key(&seat, &initial)?;
    // An evidence-only cached prefix cannot attest materialized composition.
    // Replay at the captured cut through the existing paired phase owner;
    // original freeze receipt intervals remain mandatory witnesses.
    let read_state = state.clone();
    let mut read = tokio::task::spawn_blocking(move || {
        super::FixedAnchorReplay::begin_preparation_freeze(
            &read_state,
            &seat,
            anchor,
            &key.initialization_digest,
            &freeze_events,
            Some(cut),
        )
    })
    .await??;
    loop {
        let read_state = state.clone();
        let (next, result) = tokio::task::spawn_blocking(move || {
            let result = read.next_step(&read_state);
            (read, result)
        })
        .await?;
        read = next;
        match result? {
            super::FixedAnchorReplayStep::Complete { .. } => break,
            super::FixedAnchorReplayStep::Batch { .. } => {}
            super::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                bail!("completed transfer cut omitted a required paired dependency");
            }
        }
    }
    Ok(tokio::task::spawn_blocking(move || read.finish_preparation_freeze(&initial)).await??)
}

async fn finish_transfer_prefix(
    state: Arc<AppState>,
    mut read: super::FixedAnchorReplay,
) -> Result<Arc<super::VerifiedTransferSeatPrefix>> {
    loop {
        let read_state = state.clone();
        let (next, result) = tokio::task::spawn_blocking(move || {
            let result = read.next_step(&read_state);
            (read, result)
        })
        .await?;
        read = next;
        match result? {
            super::FixedAnchorReplayStep::Complete { .. } => break,
            super::FixedAnchorReplayStep::Batch { .. } => {}
            super::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                bail!("private transfer admission unexpectedly requested public paired replay");
            }
        }
    }
    Ok(tokio::task::spawn_blocking(move || read.finish_transfer_admission()).await??)
}

async fn cancel_freeze_ingress(
    ctx: &HandlerContext,
    state: Arc<AppState>,
    transfer: &TransferIdentity,
) -> Result<Value> {
    use ryeos_app::thread_lifecycle::presentation_transfer::assemble_move_freeze_cancellation_proof;
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let session = browser_session(ctx, &state)?;
    // BOTH memberships precede any seat CAS capture/read. Cancellation retry
    // deliberately does not enter ordinary composition::verified_frontier.
    let source_membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &transfer.source.presentation_id,
            Some(&transfer.source.thread_id),
            Some(&transfer.source.chain_root_id),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let destination_membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &transfer.destination.presentation_id,
            Some(&transfer.destination.thread_id),
            Some(&transfer.destination.chain_root_id),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let (source_anchor, destination_anchor) =
        state
            .engine
            .with_checked_bundle_generation(|_generation| -> Result<_> {
                for (seat, _membership) in [
                    (&transfer.source, &source_membership),
                    (&transfer.destination, &destination_membership),
                ] {
                    super::require_presentation_seat_for_read(
                        &state,
                        &session,
                        &seat.presentation_id,
                        Some(&seat.thread_id),
                        Some(&seat.chain_root_id),
                    )?;
                }
                Ok((
                    state
                        .state_store
                        .capture_thread_event_read_anchor(
                            &transfer.source.chain_root_id,
                            &transfer.source.thread_id,
                        )?
                        .ok_or_else(|| {
                            HandlerError::Conflict("freeze source signed head absent".into())
                        })?,
                    state
                        .state_store
                        .capture_thread_event_read_anchor(
                            &transfer.destination.chain_root_id,
                            &transfer.destination.thread_id,
                        )?
                        .ok_or_else(|| {
                            HandlerError::Conflict("freeze counterpart signed head absent".into())
                        })?,
                ))
            })?;
    let source_cut = read_fixed_transfer_seat(
        state.clone(),
        transfer.source.clone(),
        source_anchor.clone(),
        &source_membership.initialization,
        vec![transfer.clone()],
    )
    .await?;
    let destination_cut = read_fixed_transfer_seat(
        state.clone(),
        transfer.destination.clone(),
        destination_anchor.clone(),
        &destination_membership.initialization,
        vec![transfer.clone()],
    )
    .await?;
    let proof = Arc::new(assemble_move_freeze_cancellation_proof(
        &source_cut,
        &destination_cut,
        transfer,
    )?);
    let digest = transfer_checkpoint_key(&transfer.source, &source_membership.initialization)?
        .initialization_digest;
    let read_state = state.clone();
    let read_proof = proof.clone();
    let seat = transfer.source.clone();
    let read = tokio::task::spawn_blocking(move || {
        if read_proof.retained_cancellation().is_some() {
            super::FixedAnchorReplay::begin_move_freeze_cancellation_admission(
                &read_state,
                &seat,
                &digest,
                read_proof,
            )
        } else {
            let links = read_proof.freeze();
            super::FixedAnchorReplay::begin_transfer_admission_with_freeze(
                &read_state,
                &seat,
                read_proof.source_anchor().clone(),
                &digest,
                &[],
                &std::iter::once(links.preparation.clone())
                    .chain(links.completion.clone())
                    .collect::<Vec<_>>(),
                None,
            )
        }
    })
    .await??;
    // Scoped retry prefixes are not exported to the ordinary replay cache.
    let prefix = finish_transfer_prefix(state.clone(), read).await?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(ctx, &state)?;
    state.engine.with_checked_bundle_generation(|generation| -> Result<Value> {
        let current_source = ui.browser_sessions.require_presentation(&session.session_id, &transfer.source.presentation_id, Some(&transfer.source.thread_id), Some(&transfer.source.chain_root_id), state.node_policy.generation_digest())
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        let current_destination = ui.browser_sessions.require_presentation(&session.session_id, &transfer.destination.presentation_id, Some(&transfer.destination.thread_id), Some(&transfer.destination.chain_root_id), state.node_policy.generation_digest())
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        if current_source != source_membership || current_destination != destination_membership
            || state.state_store.capture_thread_event_read_anchor(&transfer.source.chain_root_id, &transfer.source.thread_id)?.as_ref() != Some(&source_anchor)
            || state.state_store.capture_thread_event_read_anchor(&transfer.destination.chain_root_id, &transfer.destination.thread_id)?.as_ref() != Some(&destination_anchor)
        { return Ok(transfer_response(&transfer.operation_id, "conflict")); }
        let (detail, _) = super::require_presentation_seat_for_read(&state, &session, &transfer.source.presentation_id, Some(&transfer.source.thread_id), Some(&transfer.source.chain_root_id))?;
        let verified = prefix.history(&current_source.initialization)?;
        if detail.thread_id != detail.chain_root_id
            || prefix.anchor() != &source_anchor || source_anchor.tip() != Some((verified.tip_hash.as_str(), verified.thread_seq))
            || verified.replay.revision < transfer.source.revision
        { return Ok(transfer_response(&transfer.operation_id,"conflict")); }
        let root = current_source.initialization.binding_attachments.iter().find(|binding| binding.binding_attachment_id == current_source.initialization.root_binding.binding_attachment_id
            && binding.binding_generation == current_source.initialization.root_binding.binding_generation && binding.binding_digest == current_source.initialization.root_binding.binding_digest)
            .ok_or_else(|| HandlerError::Conflict("freeze source root descriptor absent".into()))?;
        let max_append_bytes = root.binding_request_bounds.max_seat_append_bytes;
        let mut source = AuthenticatedSeat { detail, membership:current_source, verified, anchor:source_anchor.clone(), max_append_bytes };
        let export = freeze_evidence::verify_source_cancellation_freeze(&source,&prefix,transfer,proof.freeze())?;
        // An exact already-recorded outcome is historical testimony, not a
        // new write or a binding/producer grant. Typed scoped replay and both
        // current membership/cut checks above still precede this readback.
        if let Some(response) = retained_freeze_cancellation_response(&proof, &prefix)? {
            return Ok(response);
        }
        if source.detail.status != "running"
            || !source.verified.control.as_ref().is_some_and(|control| control.state == super::ProducerState::Active)
        { return Ok(transfer_response(&transfer.operation_id,"conflict")); }
        composition::authenticate_binding(generation,&state,&session,&source.membership,&source.membership.initialization.root_binding)?;
        let current_bound = super::super::ui_launch_mint::binding_request_bounds(&state)?.max_seat_append_bytes;
        source.max_append_bytes = source.max_append_bytes.min(current_bound);
        revalidate_export_bindings(generation,&state,&session,&source.membership,&export)?;
        let owner = seat_owner(&session); let pair = pair_seat(&source,&owner,&transfer.source);
        let next_generation = session.next_attachment_generation; let policy = state.node_policy.generation_digest().to_owned();
        let outcome = state.state_store.append_ui_move_freeze_cancellation(&proof,&pair,|| Ok(ui.browser_sessions.matches_transfer_preparation_memberships(
            &session.session_id,&source.membership,&current_destination,next_generation,&policy)))?;
        Ok(match outcome {
            UiMoveFreezeCancellationAppendResult::Committed(records) => json!({"operation_id":transfer.operation_id,"outcome":"freeze_cancelled","cancellation":cancellation_commit_evidence(&records, transfer)?}),
            UiMoveFreezeCancellationAppendResult::AlreadyRecorded(evidence) => json!({"operation_id":transfer.operation_id,"outcome":"freeze_cancelled","cancellation":evidence}),
            UiMoveFreezeCancellationAppendResult::Conflict => transfer_response(&transfer.operation_id,"conflict"),
        })
    })
}

pub(super) fn retained_freeze_cancellation_response(
    proof: &ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof,
    prefix: &super::VerifiedTransferSeatPrefix,
) -> Result<Option<Value>> {
    let Some((evidence, _)) = proof.retained_cancellation() else {
        return Ok(None);
    };
    if prefix.anchor() != proof.source_anchor()
        || prefix.freeze_event(&proof.freeze().preparation).is_none()
        || proof
            .freeze()
            .completion
            .as_ref()
            .is_some_and(|evidence| prefix.freeze_event(evidence).is_none())
    {
        bail!("historical cancellation differs from complete exact typed source prefix");
    }
    Ok(Some(
        json!({"operation_id":proof.transfer().operation_id,"outcome":"freeze_cancelled","cancellation":evidence}),
    ))
}

fn cancellation_commit_evidence(
    records: &[ryeos_app::state_store::PersistedEventRecord],
    transfer: &TransferIdentity,
) -> Result<TransferEvidence> {
    let [record] = records else {
        return Err(HandlerError::Internal(
            "freeze cancellation appended another event count".into(),
        )
        .into());
    };
    if record.chain_root_id != transfer.source.chain_root_id
        || record.thread_id != transfer.source.thread_id
        || record.event_type
            != ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        return Err(HandlerError::Internal(
            "freeze cancellation append returned another event owner".into(),
        )
        .into());
    }
    Ok(TransferEvidence {
        event_hash: record.event_hash.clone().ok_or_else(|| {
            HandlerError::Internal("freeze cancellation signed hash absent".into())
        })?,
        chain_seq: u64::try_from(record.chain_seq).map_err(|_| {
            HandlerError::Internal("freeze cancellation chain sequence invalid".into())
        })?,
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

fn committed_evidence(
    committed: &ryeos_state::state_db::CommittedWrite<ryeos_state::chain::AppendResult>,
) -> Result<TransferEvidence> {
    if committed.value.event_count != 1 {
        return Err(
            HandlerError::Internal("transfer phase appended another event count".into()).into(),
        );
    }
    let [event] = committed.value.events.as_slice() else {
        return Err(
            HandlerError::Internal("transfer phase appended another event count".into()).into(),
        );
    };
    Ok(TransferEvidence {
        event_hash: ryeos_state::objects::thread_event::hash_event(event)?,
        chain_seq: event.chain_seq,
    })
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
        .ok_or_else(|| HandlerError::Conflict("signed transfer tip object is absent".into()))?;
    if event.chain_root_id != chain_root_id
        || event.thread_id != thread_id
        || event.chain_seq != evidence.chain_seq
        || ryeos_state::objects::thread_event::hash_event(&event)? != evidence.event_hash
    {
        return Err(HandlerError::Conflict(
            "signed transfer tip differs from retained object".into(),
        )
        .into());
    }
    Ok(event)
}

/// Reserve fresh, hidden destination bindings before the signed Accepted
/// append. Nothing here exposes those bindings as a live destination mount.
#[allow(clippy::too_many_arguments)]
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
    let source = authenticated_seat(
        generation,
        state,
        session,
        &transfer.source,
        false,
        Some(proof),
    )?;
    let destination = authenticated_seat(
        generation,
        state,
        session,
        &transfer.destination,
        false,
        Some(proof),
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
        return Err(
            HandlerError::Conflict("another transfer owns source preparation".into()).into(),
        );
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
            return Err(HandlerError::Conflict(
                "MOVE preparation has no exact freeze links".into(),
            )
            .into());
        };
        freeze_evidence::verify_retained_move_freeze(
            &source,
            &proof.source,
            proof,
            transfer,
            &export,
            links,
        )?;
    }
    export
        .validate_affected_snapshot(transfer, &source.verified.replay, max_record_bytes)
        .map_err(HandlerError::Conflict)?;
    revalidate_export_bindings(generation, state, session, &source.membership, &export)?;
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
    )?;
    let max_attachments = usize::try_from(policy.max_live_binding_attachments_per_session)
        .map_err(|_| HandlerError::Internal("browser attachment bound exceeds usize".into()))?;
    let policy_digest = state.node_policy.generation_digest();
    if proof.history.source_anchor() != &source.anchor
        || proof.history.destination_anchor() != &destination.anchor
        || proof.history.lifecycle().transfer() != transfer
        || proof.history.lifecycle().inbound().evidence != inbound
        || proof
            .history
            .lifecycle()
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
    if let Some(retained) = proof.history.lifecycle().accepted() {
        if proof.history.lifecycle().source_state()
            != ryeos_ui_presentation_contract::transfer_state::TransferSourceState::Frozen
            || proof.history.lifecycle().destination_state()
                != ryeos_ui_presentation_contract::transfer_state::TransferDestinationState::Reserved
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
    if proof.history.lifecycle().source_state()
        != ryeos_ui_presentation_contract::transfer_state::TransferSourceState::Frozen
        || proof.history.lifecycle().destination_state()
            != ryeos_ui_presentation_contract::transfer_state::TransferDestinationState::Unreserved
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
    let source_pair = pair_seat(&source, &owner, &transfer.source);
    let destination_pair = pair_seat(&destination, &owner, &transfer.destination);
    let created = std::cell::Cell::new(false);
    let (anchors, retired) = proof.cut.publication_cut(state, session)?;
    let expected_memberships = session.presentations.values().cloned().collect::<Vec<_>>();
    let result = ui
        .browser_sessions
        .with_exact_transfer_reservation_snapshot(
            &session.session_id,
            &expected_memberships,
            policy_digest,
            |live_session| {
                state.state_store.append_ui_transfer_accepted_if_mixed_cut(
                    transfer,
                    &proof.history,
                    preparation,
                    &inbound_event,
                    &prepared_event,
                    &reservation_value,
                    &destination_pair,
                    &source_pair,
                    &anchors,
                    &retired,
                    || {
                        if live_session.expires_at <= std::time::Instant::now()
                            || !proof.cut.publication_memberships_match(live_session) {
                            return Ok(false);
                        }
                        let outcome = BrowserSessionStore::reserve_hidden_transfer_attachment_batch_in_session(
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
    Ok(match result {
        UiTransferAcceptedAppendResult::Committed(committed) => {
            let acceptance = committed_evidence(&committed)?;
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
                .is_ok()
            {
                accepted_response(&transfer.operation_id, acceptance)
            } else {
                transfer_response(&transfer.operation_id, "uncertain")
            }
        }
        UiTransferAcceptedAppendResult::Conflict => {
            if created.get() {
                ui.browser_sessions
                    .discard_new_hidden_transfer_attachment_batch(&session.session_id, &staged)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?;
            }
            transfer_response(&transfer.operation_id, "conflict")
        }
        UiTransferAcceptedAppendResult::Uncertain { .. } => {
            match state.state_store.read_ui_transfer_accepted_successor(
                transfer,
                preparation,
                &import_digest,
                &destination_pair,
            ) {
                Ok(UiTransferAcceptedReadback::Observed { acceptance })
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
                        .is_ok() =>
                {
                    accepted_response(&transfer.operation_id, acceptance)
                }
                _ => transfer_response(&transfer.operation_id, "uncertain"),
            }
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
    let proof = read_transfer_proof(state.clone(), &session, transfer.clone()).await?;
    let written = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = browser_session(ctx, &state)?;
        state.engine.with_checked_bundle_generation(|generation| -> Result<WriteOutcome> {
            let source = authenticated_seat(generation, &state, &session, &transfer.source, false, Some(&proof))?;
            let destination = authenticated_seat(generation, &state, &session, &transfer.destination, false, Some(&proof))?;
            let lifecycle = proof.history.lifecycle();
            if let Some(activated) = lifecycle.activated() {
                return Ok(WriteOutcome::Evidence((*activated.evidence).clone()));
            }
            if lifecycle.source_state() != ryeos_ui_presentation_contract::transfer_state::TransferSourceState::Committed
                || lifecycle.destination_state() != ryeos_ui_presentation_contract::transfer_state::TransferDestinationState::Reserved
            {
                return Ok(WriteOutcome::Conflict);
            }
            let prepared = lifecycle.prepared().ok_or_else(|| HandlerError::Conflict("activation has no preparation".into()))?;
            let accepted = lifecycle.accepted().ok_or_else(|| HandlerError::Conflict("activation has no acceptance".into()))?;
            let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
            let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
                .map_err(HandlerError::Conflict)?;
            let reservation = UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
                .map_err(HandlerError::Conflict)?;
            reservation.validate_export(transfer, &export, max_record_bytes).map_err(HandlerError::Conflict)?;
            let policy_digest = state.node_policy.generation_digest();
            let staged = ui.browser_sessions.stage_activation_transfer_attachment_batch(
                &session.session_id, &proof.history, &export, &reservation, max_record_bytes, policy_digest,
            ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
            revalidate_activation_bindings(generation, &state, &session, staged.destination_admissions())?;
            // All path/pin checks happen before StateStore's ordered chain
            // exclusion. The later publication callback is in-memory only.
            ui.browser_sessions.prevalidate_transfer_attachment_batch(
                &staged, &export, &reservation, max_record_bytes, policy_digest,
            ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
            let owner = seat_owner(&session);
            let source_pair = pair_seat(&source, &owner, &transfer.source);
            let destination_pair = pair_seat(&destination, &owner, &transfer.destination);
            let (anchors, retired) = proof.cut.publication_cut(&state, &session)?;
            let expected_memberships = session.presentations.values().cloned().collect::<Vec<_>>();
            let result = ui.browser_sessions.with_exact_presentation_read_snapshot(
                &session.session_id, &expected_memberships, policy_digest, |live_session| {
                state.state_store.append_ui_transfer_activated_if_mixed_cut(
                &proof.history, &destination_pair, &source_pair, &anchors, &retired, || {
                    Ok(live_session.expires_at > std::time::Instant::now()
                        && live_session.next_attachment_generation == session.next_attachment_generation
                        && proof.cut.publication_memberships_match(live_session))
                },
            )}).map_err(|error| HandlerError::Conflict(error.to_string()))??;
            Ok(match result {
                UiTransferAcceptedAppendResult::Committed(committed) =>
                    WriteOutcome::Evidence(committed_evidence(&committed)?),
                UiTransferAcceptedAppendResult::Conflict => WriteOutcome::Conflict,
                UiTransferAcceptedAppendResult::Uncertain { .. } => WriteOutcome::Uncertain,
            })
        })?
    };
    if matches!(written, WriteOutcome::Conflict) {
        return Ok(transfer_response(&transfer.operation_id, "conflict"));
    }
    let session = browser_session(ctx, &state)?;
    let completed = match read_transfer_proof(state.clone(), &session, transfer.clone()).await {
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
    state.engine.with_checked_bundle_generation(|generation| -> Result<Value> {
        let source = authenticated_seat(generation, &state, &session, &transfer.source, false, Some(&completed))?;
        let destination = authenticated_seat(generation, &state, &session, &transfer.destination, false, Some(&completed))?;
        let prepared = completed.history.lifecycle().prepared().ok_or_else(|| HandlerError::Conflict("activation has no preparation".into()))?;
        let accepted = completed.history.lifecycle().accepted().ok_or_else(|| HandlerError::Conflict("activation has no acceptance".into()))?;
        let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
        let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
            .map_err(HandlerError::Conflict)?;
        let reservation = UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
            .map_err(HandlerError::Conflict)?;
        let policy_digest = state.node_policy.generation_digest();
        let policy = state.node_policy.require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
        let max_attachments = usize::try_from(policy.max_live_binding_attachments_per_session)
            .map_err(|_| HandlerError::Internal("browser attachment bound exceeds usize".into()))?;
        let staged = ui.browser_sessions.stage_activation_transfer_attachment_batch(
            &session.session_id, &completed.history, &export, &reservation, max_record_bytes, policy_digest,
        ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
        revalidate_activation_bindings(generation, &state, &session, staged.destination_admissions())?;
        let prevalidated = ui.browser_sessions.prevalidate_transfer_attachment_batch(
            &staged, &export, &reservation, max_record_bytes, policy_digest,
        ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
        let (anchors, retired) = completed.cut.publication_cut(&state, &session)?;
        let expected_memberships = session.presentations.values().cloned().collect::<Vec<_>>();
        let published = ui.browser_sessions.activate_transfer_attachment_batch_at_mixed_cut(
            &state.state_store,
            &anchors,
            &retired,
            &expected_memberships,
            &completed.cut,
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

/// A browser cannot choose a pair of stale seat heads as authority. This
/// handler holds the existing transition and checked-engine guards through
/// the exact two-chain append; the bounded session callback performs only an
/// in-memory membership check after StateStore and ordered chain locks.
pub(super) async fn handle_transfer(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let request: TransferRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid transfer request: {error}")))?;
    let transfer = match &request {
        TransferRequest::CancelFreeze { transfer } => transfer,
        TransferRequest::Commit { transfer }
        | TransferRequest::Abort { transfer }
        | TransferRequest::Activate { transfer } => transfer,
        TransferRequest::Prepare { transfer, .. }
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
    // read. That read holds no seat-transition or StateStore lock. The live
    // membership and both signed heads are rechecked after it completes.
    let session = browser_session(&ctx, &state)?;
    let proof = if matches!(
        &request,
        TransferRequest::AcceptView { .. }
            | TransferRequest::AcceptTabs { .. }
            | TransferRequest::AcceptSharedDock { .. }
            | TransferRequest::Commit { .. }
            | TransferRequest::Abort { .. }
    ) {
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
        Some(read_transfer_proof(state.clone(), &session, transfer.clone()).await?)
    } else {
        None
    };
    // Existing operations reconstruct the actual current composition. They
    // never replace this new operation's immutable identity or absence proof.
    let preparation_frontiers = if matches!(&request, TransferRequest::Prepare { .. }) {
        Some((
            super::read_completed_seat_history(
                state.clone(),
                &session,
                &transfer.source.presentation_id,
                &transfer.source.thread_id,
            )
            .await?,
            super::read_completed_seat_history(
                state.clone(),
                &session,
                &transfer.destination.presentation_id,
                &transfer.destination.thread_id,
            )
            .await?,
        ))
    } else {
        None
    };
    // Complete selected source coverage keeps cancellation permanent across
    // producer replacement. Publication rechecks the same cut, never rebases it.
    let preparation_proofs = if matches!(&request, TransferRequest::Prepare { .. }) {
        let ((source_anchor, source_initial), (destination_anchor, destination_initial)) = state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<_> {
                let frontiers = preparation_frontiers.as_ref().expect("Prepare frontiers");
                let source = authenticated_seat_with_frontier(
                    generation,
                    &state,
                    &session,
                    &transfer.source,
                    true,
                    None,
                    Some(&frontiers.0),
                )?;
                let destination = authenticated_seat_with_frontier(
                    generation,
                    &state,
                    &session,
                    &transfer.destination,
                    true,
                    None,
                    Some(&frontiers.1),
                )?;
                Ok((
                    (source.anchor, source.membership.initialization),
                    (destination.anchor, destination.membership.initialization),
                ))
            })?;
        let source_proof = read_fixed_transfer_seat(
            state.clone(),
            transfer.source.clone(),
            source_anchor.clone(),
            &source_initial,
            vec![transfer.clone()],
        )
        .await?;
        let destination_proof = read_fixed_transfer_seat(
            state.clone(),
            transfer.destination.clone(),
            destination_anchor,
            &destination_initial,
            vec![transfer.clone()],
        )
        .await?;
        let freeze_prefix = if matches!(&request, TransferRequest::Prepare { export, .. }
            if export.get("mode").and_then(Value::as_str) == Some("move"))
        {
            let links = source_proof.preparation_freeze_links(
                &state.state_store,
                &source_anchor,
                transfer,
            )?;
            let frontier = &preparation_frontiers.as_ref().expect("Prepare frontiers").0;
            let prefix_key =
                transfer_checkpoint_key(&transfer.source, &frontier.membership.initialization)?;
            let mut read = super::FixedAnchorReplay::begin_preparation_freeze(
                &state,
                &transfer.source,
                source_anchor.clone(),
                &prefix_key.initialization_digest,
                &[links.preparation.clone(), links.completion.clone()],
                frontier.cut.clone(),
            )?;
            loop {
                let task_state = state.clone();
                let (returned, step) = tokio::task::spawn_blocking(move || {
                    let step = read.next_step(&task_state);
                    (read, step)
                })
                .await?;
                read = returned;
                match step? {
                    super::FixedAnchorReplayStep::Batch { .. } => {}
                    super::FixedAnchorReplayStep::Complete { .. } => break,
                    super::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        bail!("preparation freeze omitted completed root dependency cut")
                    }
                }
            }
            let prefix = read.finish_preparation_freeze(&source_initial)?;
            Some((links, prefix))
        } else {
            None
        };
        Some((source_proof, destination_proof, freeze_prefix))
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
            if matches!(&request, TransferRequest::Commit { .. } | TransferRequest::Abort { .. }) {
                let proof = proof.as_ref().expect("source decision has completed paired proof");
                let source = authenticated_seat(generation, &state, &session, &transfer.source, false, Some(proof))?;
                let destination = authenticated_seat(generation, &state, &session, &transfer.destination, false, Some(proof))?;
                let recorded = if matches!(&request, TransferRequest::Commit { .. }) {
                    proof.history.lifecycle().committed()
                } else { proof.history.lifecycle().aborted() };
                if let Some(recorded) = recorded {
                    // Exact completed paired readback, not a second decision.
                    return Ok(json!({"operation_id":transfer.operation_id,
                        "outcome":"recorded", "decision":recorded.evidence}));
                }
                let prepared = proof.history.lifecycle().prepared()
                    .ok_or_else(|| HandlerError::Conflict("source decision has no retained preparation".into()))?;
                let max_record_bytes = source.max_append_bytes.min(destination.max_append_bytes);
                let export = UiTransferExport::from_preparation(prepared.record, max_record_bytes)
                    .map_err(HandlerError::Conflict)?;
                export.validate_affected_snapshot(transfer, &source.verified.replay, max_record_bytes)
                    .map_err(HandlerError::Conflict)?;
                revalidate_export_bindings(generation, &state, &session, &source.membership, &export)?;
                if export.mode == UiTransferExportMode::Move {
                    let TransferPhase::Prepared { freeze: Some(links), .. } = &prepared.record.phase else {
                        return Err(HandlerError::Conflict("MOVE decision has no retained freeze".into()).into());
                    };
                    freeze_evidence::verify_retained_move_freeze(
                        &source, &proof.source, proof, transfer, &export, links,
                    )?;
                }
                if let Some(accepted) = proof.history.lifecycle().accepted() {
                    let reservation = UiTransferReservation::from_acceptance(accepted.record, max_record_bytes)
                        .map_err(HandlerError::Conflict)?;
                    reservation.validate_export(transfer, &export, max_record_bytes)
                        .map_err(HandlerError::Conflict)?;
                    if matches!(&request, TransferRequest::Commit { .. }) {
                        ui.browser_sessions.stage_retained_transfer_attachment_batch(
                            &session.session_id, transfer, &export, &reservation,
                            max_record_bytes, state.node_policy.generation_digest(),
                        ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
                    }
                }
                let decision = if matches!(&request, TransferRequest::Commit { .. }) {
                    ryeos_app::state_store::UiTransferSourceDecision::Commit
                } else { ryeos_app::state_store::UiTransferSourceDecision::Abort };
                let owner = seat_owner(&session);
                let source_pair = pair_seat(&source, &owner, &transfer.source);
                let destination_pair = pair_seat(&destination, &owner, &transfer.destination);
                let policy_digest = state.node_policy.generation_digest();
                let (anchors, retired) = proof.cut.publication_cut(&state, &session)?;
                let expected_memberships = session.presentations.values().cloned().collect::<Vec<_>>();
                let result = ui.browser_sessions.with_exact_presentation_read_snapshot(
                    &session.session_id, &expected_memberships, policy_digest, |live_session| {
                    state.state_store.append_ui_transfer_source_decision_if_mixed_cut(
                    &proof.history, decision, &source_pair, &destination_pair, &anchors, &retired, || {
                        Ok(live_session.expires_at > std::time::Instant::now()
                            && live_session.next_attachment_generation == session.next_attachment_generation
                            && proof.cut.publication_memberships_match(live_session))
                    })}).map_err(|error| HandlerError::Conflict(error.to_string()))??;
                return Ok(match result {
                    UiTransferAcceptedAppendResult::Committed(committed) => json!({
                        "operation_id":transfer.operation_id, "outcome":"recorded", "decision":committed_evidence(&committed)?
                    }),
                    UiTransferAcceptedAppendResult::Conflict => transfer_response(&transfer.operation_id, "conflict"),
                    UiTransferAcceptedAppendResult::Uncertain { .. } => transfer_response(&transfer.operation_id, "uncertain"),
                });
            }
            let acceptance = match &request {
                TransferRequest::AcceptView { inbound, preparation, surface_instance_id, view_set_id, placement, .. } =>
                    Some((inbound, preparation, TransferAcceptanceTarget::View {
                        surface_instance_id, view_set_id: *view_set_id, placement,
                    })),
                TransferRequest::AcceptTabs { inbound, preparation, surface_instance_id, .. } =>
                    Some((inbound, preparation, TransferAcceptanceTarget::Tabs { surface_instance_id })),
                TransferRequest::AcceptSharedDock { inbound, preparation, edge, .. } =>
                    Some((inbound, preparation, TransferAcceptanceTarget::SharedDock { edge })),
                _ => None,
            };
            if let Some((inbound, preparation, target)) = acceptance {
                return accept_transfer(
                    generation, &state, &session, transfer,
                    proof.as_ref().expect("acceptance has fixed-history proof"),
                    inbound, preparation, target,
                );
            }
            let TransferRequest::Prepare { export, freeze, .. } = &request else {
                unreachable!("acceptance returned above")
            };
            let frontiers = preparation_frontiers.as_ref().expect("Prepare frontiers");
            let source = authenticated_seat_with_frontier(
                generation, &state, &session, &transfer.source, true, None, Some(&frontiers.0))?;
            let destination = authenticated_seat_with_frontier(
                generation,
                &state,
                &session,
                &transfer.destination,
                true,
                None,
                Some(&frontiers.1),
            )?;
            let decoded: UiTransferExport =
                serde_json::from_value(export.clone()).map_err(|error| {
                    HandlerError::BadRequest(format!("invalid source export: {error}"))
                })?;
            if decoded.mode == UiTransferExportMode::Move {
                let (links, prefix) = preparation_proofs
                    .as_ref()
                    .and_then(|proofs| proofs.2.as_ref())
                    .ok_or_else(|| {
                        HandlerError::Conflict(
                            "MOVE has no completed receipted freeze prefix".into(),
                        )
                    })?;
                let retained =
                    freeze_evidence::verify_source_move_freeze(&source, prefix, transfer, links)?;
                if freeze.as_ref() != Some(links) {
                    return Err(HandlerError::Conflict(
                        "MOVE request links differ from completed source testimony".into(),
                    )
                    .into());
                }
                if retained != decoded {
                    return Err(HandlerError::Conflict(
                        "MOVE export differs from exact Core freeze".into(),
                    )
                    .into());
                }
            }
            if decoded.mode == UiTransferExportMode::Copy && freeze.is_some() {
                return Err(
                    HandlerError::BadRequest("COPY cannot carry MOVE freeze links".into()).into(),
                );
            }
            decoded
                .validate_snapshot(&transfer, &source.verified.replay, source.max_append_bytes)
                .map_err(HandlerError::Conflict)?;
            revalidate_export_bindings(generation, &state, &session, &source.membership, &decoded)?;
            let owner = seat_owner(&session);
            let source_pair = pair_seat(&source, &owner, &transfer.source);
            let destination_pair = pair_seat(&destination, &owner, &transfer.destination);
            let next_attachment_generation = session.next_attachment_generation;
            let policy_digest = state.node_policy.generation_digest().to_owned();
            let source_cut = frontiers.0.authorize_history(&state, &session,
                &transfer.source.presentation_id, &transfer.source.thread_id)?;
            let destination_cut = frontiers.1.authorize_history(&state, &session,
                &transfer.destination.presentation_id, &transfer.destination.thread_id)?;
            let mut current_anchors = source_cut.anchors;
            current_anchors.extend(destination_cut.anchors);
            let mut retired_prefixes = source_cut.retired;
            retired_prefixes.extend(destination_cut.retired);
            let expected_memberships = session.presentations.values().cloned().collect::<Vec<_>>();
            // UI transition -> session owner -> StateStore -> sorted chains.
            // Keep the whole original membership snapshot live throughout
            // publication, including prior-operation dependency seats.
            let pair = ui.browser_sessions.with_exact_presentation_read_snapshot(
                &session.session_id, &expected_memberships, &policy_digest, |live_session| {
                state.state_store.append_ui_transfer_preparation_pair_if_mixed_cut(
                &transfer,
                &export,
                &destination_pair,
                &source_pair,
                preparation_proofs
                    .as_ref()
                    .expect("Prepare has completed paired cuts")
                    .0
                    .as_ref(),
                preparation_proofs
                    .as_ref()
                    .expect("Prepare has completed paired cuts")
                    .1
                    .as_ref(),
                &current_anchors,
                &retired_prefixes,
                || {
                    // Pure borrowed-session predicates: no lock/store reentry.
                    Ok(live_session.expires_at > std::time::Instant::now()
                        && live_session.next_attachment_generation == next_attachment_generation
                        && frontiers.0.cut.as_ref().is_none_or(|cut|
                            cut.publication_memberships_match(live_session))
                        && frontiers.1.cut.as_ref().is_none_or(|cut|
                            cut.publication_memberships_match(live_session)))
                },
            )
            }).map_err(|error| HandlerError::Conflict(error.to_string()))??;
            Ok(match pair {
                ConditionalThreadEventPairAppendResult::Committed {
                    destination,
                    source,
                } => prepared_response(
                    &transfer.operation_id,
                    committed_evidence(&destination)?,
                    committed_evidence(&source)?,
                ),
                ConditionalThreadEventPairAppendResult::Conflict => {
                    transfer_response(&transfer.operation_id, "conflict")
                }
                ConditionalThreadEventPairAppendResult::DestinationUncertain { .. }
                | ConditionalThreadEventPairAppendResult::DestinationCommittedSourceUncertain {
                    ..
                } => {
                    // This is observation, never an absence proof or retry permit.
                    // A linked pair can be reported; every other state remains
                    // uncertain and requires ordered two-head reconciliation.
                    match state.state_store.read_ui_transfer_preparation_pair(
                        &transfer,
                        &export,
                        &destination_pair,
                        &source_pair,
                    ) {
                        Ok(UiTransferPairReadback::LinkedPairObserved { inbound, prepared }) => {
                            prepared_response(&transfer.operation_id, inbound, prepared)
                        }
                        _ => transfer_response(&transfer.operation_id, "uncertain"),
                    }
                }
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admitted_session_replacement_preserves_native_document_support_fact() {
        use ryeos_client_base::ui::effect::{
            RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
        };
        use ryeos_client_base::ui::{RyeOsCore, RyeOsEvent};
        let (_directory, state, id, seat) =
            super::super::authenticated_history::ordinary_session_replacement_fixture();
        let state = Arc::new(state);
        let projected = crate::handlers::ui_session_current::handle(
            json!({}),
            HandlerContext::new(format!("session:{id}"), Vec::new(), false),
            state.clone(),
        )
        .await
        .unwrap();
        let session: ryeos_client_base::ui::BrowserSession =
            serde_json::from_value(projected).unwrap();
        let mut read = super::super::FixedAnchorReplay::begin(
            &state,
            &seat.presentation_id,
            &seat.chain_root_id,
            &seat.thread_id,
        )
        .unwrap();
        let mut replayed = Vec::new();
        loop {
            match read.next_step(&state).unwrap() {
                super::super::FixedAnchorReplayStep::Batch { events } => {
                    for event in events {
                        if let Some(retained) =
                            ryeos_client_base::ui::seat::retained_seat_event_with_evidence(
                                &serde_json::to_value(event).unwrap(),
                            )
                            .unwrap()
                        {
                            replayed.push(retained);
                        }
                    }
                }
                super::super::FixedAnchorReplayStep::Complete { .. } => break,
                super::super::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("ordinary fixture has no phase testimony")
                }
            }
        }
        assert!(replayed.iter().any(|row| matches!(
            row.event.kind,
            ryeos_client_base::ui::seat::SeatEventKind::PresentationInitialized(_)
        )));
        let mut admitted = RyeOsCore::new(
            session.clone(),
            &seat.presentation_id,
            Default::default(),
            0,
        );
        admitted
            .validate_replayed_initialization(
                &replayed
                    .iter()
                    .map(|row| row.event.clone())
                    .collect::<Vec<_>>(),
            )
            .expect("real replacement fixture initialization must match its retained membership");
        admitted
            .replay_authenticated_seat_events(replayed.clone())
            .unwrap_or_else(|error| panic!("real replacement fixture must cold-fold through Core before adoption: {error}; notices: {:?}", admitted.ui.notices));
        admitted
            .require_replayed_initialization()
            .expect("replacement initialization folded");
        // Only the adapter implementation fact and pending-effect correlation
        // are synthetic. All replacement initialization hashes/native rows
        // and admitted binding descriptors come from the real daemon owners.
        for supported in [false, true] {
            let mut core = RyeOsCore::default();
            core.supports_independent_documents = supported;
            core.pending_effects.insert(
                42,
                RyeOsEffectKind::ReplaceSession {
                    session_id: "predecessor-fixture".into(),
                    launch_url: "/ui/launch/one-shot-fixture".into(),
                },
            );
            core.dispatch(RyeOsEvent::EffectResult{result:RyeOsEffectResult{
                id:42,ok:true,kind:RyeOsEffectResultKind::BrowserOnly,error:None,
                data:Some(json!({"session":session,"presentation_id":seat.presentation_id,"replayed":replayed}))}});
            assert_eq!(
                core.data.session.as_ref().unwrap().session_id,
                id,
                "the real admitted replacement must be adopted rather than failing closed"
            );
            assert_eq!(core.supports_independent_documents, supported);
            assert!(!core.pending_effects.contains_key(&42));
            core.require_replayed_initialization().unwrap();
        }
    }

    #[tokio::test]
    async fn second_move_prepare_requires_original_receipted_full_root_freeze() {
        use ryeos_client_base::ui::admission_operation::{
            UiCompletedPresentationOperation, UiPreparedPresentationOperation,
        };
        use ryeos_client_base::ui::transfer_freeze::UiMoveFreezeRequest;
        let (_directory, state, previous, _, _, id) =
            super::super::authenticated_history::activated_pair_with_compiled_authority();
        let state = Arc::new(state);
        let id = id.unwrap();
        let ui = get_ui_state(&state).unwrap();
        let session = ui.browser_sessions.get_session(&id).unwrap();
        let source = super::read_completed_seat_history(
            state.clone(),
            &session,
            &previous.destination.presentation_id,
            &previous.destination.thread_id,
        )
        .await
        .unwrap();
        let destination = super::read_completed_seat_history(
            state.clone(),
            &session,
            &previous.source.presentation_id,
            &previous.source.thread_id,
        )
        .await
        .unwrap();
        let replay = &source.verified.replay;
        let dock = &replay.composition.docks()["right"];
        let mounted = dock.view();
        let key = mounted.instance_key().clone();
        let drafts = replay
            .transfer_drafts
            .iter()
            .filter(|(draft, _)| draft.view_instance_key == key)
            .map(|(key, state)| json!({"key":key,"state":state}))
            .collect::<Vec<_>>();
        let route = replay.input_routes.get(&key).map_or(
            json!({"kind":"authored"}),
            |(_, route)| json!({"kind":"override","input_route":route}),
        );
        let export: UiTransferExport = serde_json::from_value(json!({"schema":1,"mode":"move",
            "content":{"kind":"shared_dock","edge":"right","visible":dock.visible,"size":dock.size,
            "mount":{"source_instance":key,"view_ref":mounted.view_ref(),"binding":mounted.binding(),
                "local":mounted.local,"subject":replay.subjects[&key].attachment,"route":route,
                "current_label":replay.lens_labels.get(&key).cloned().flatten(),"drafts":drafts,
                "lens_frames":replay.lens_frames.get(&key).cloned().unwrap_or_default()}}})).unwrap();
        let export_value = serde_json::to_value(&export).unwrap();
        let mut next = previous.clone();
        next.operation_id = "00000000-0000-4000-8000-000000000089".into();
        next.source = previous.destination.clone();
        next.destination = previous.source.clone();
        next.source.revision = replay.revision;
        next.source.producer_incarnation = source.control().unwrap().producer_incarnation;
        next.destination.revision = destination.verified.replay.revision;
        next.destination.producer_incarnation = destination.control().unwrap().producer_incarnation;
        next.export_digest = ryeos_client_base::ui::seat_payload_digest(&export_value).unwrap();
        export.validate_snapshot(&next, replay, 65536).unwrap();
        let initial = &source.membership.initialization;
        let root_descriptor = initial
            .binding_attachments
            .iter()
            .find(|binding| {
                binding.binding_attachment_id == initial.root_binding.binding_attachment_id
            })
            .unwrap();
        let freeze = UiMoveFreezeRequest {
            schema: 1,
            operation_id: String::new(),
            transfer: next.clone(),
            root_binding: initial.root_binding.clone(),
            export: export.clone(),
            instances: vec![key.clone()],
            view_sets: vec![],
        };
        let first = source.next_engine_seq();
        let mut preparation: UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema":2,"operation_id":"","producer":{"presentation_id":next.source.presentation_id,
                "thread_id":next.source.thread_id,"producer_incarnation":next.source.producer_incarnation},
            "prepared_engine_seq":first,"root_binding":initial.root_binding,
            "request_bounds":root_descriptor.binding_request_bounds,"body":{"kind":"move_freeze","request":freeze}})).unwrap();
        preparation.assign_identity().unwrap();
        preparation.validate().unwrap();
        let completion = UiCompletedPresentationOperation {
            operation_id: preparation.operation_id.clone(),
            result: serde_json::to_value(preparation.move_freeze().unwrap().completion()).unwrap(),
        };
        let ctx = HandlerContext::new(format!("session:{id}"), Vec::new(), false);
        // Actual seat append owner emits each ACK receipt. No test constructs
        // a positive CAS witness or grafts a hash onto a provisional Core row.
        for (offset, kind, payload) in [
            (
                0,
                "seat.presentation_operation.prepared",
                serde_json::to_value(&preparation).unwrap(),
            ),
            (
                1,
                "seat.presentation_operation.completed",
                serde_json::to_value(completion).unwrap(),
            ),
        ] {
            let seq = first + offset;
            let events = json!([{"engine_seq":seq,"event_type":kind,"payload":payload}]);
            let digest = ryeos_client_base::ui::seat_payload_digest(&events).unwrap();
            let operation_id = ryeos_ui_presentation_contract::identity::UiSeatAppendIdentity {
                presentation_id: next.source.presentation_id.clone(),
                chain_root_id: next.source.chain_root_id.clone(),
                thread_id: next.source.thread_id.clone(),
                producer_incarnation: next.source.producer_incarnation.clone(),
                first_engine_seq: seq,
                last_engine_seq: seq,
                event_count: 1,
                payload_digest: digest.clone(),
            }
            .derive_identity()
            .unwrap();
            super::super::handle_append(json!({"presentation_id":next.source.presentation_id,
                "thread_id":next.source.thread_id,"producer_incarnation":next.source.producer_incarnation,
                "operation_id":operation_id,"first_engine_seq":seq,"last_engine_seq":seq,"event_count":1,
                "payload_digest":digest,"events":events}),ctx.clone(),state.clone()).await.unwrap();
        }
        let frozen = super::read_completed_seat_history(
            state.clone(),
            &session,
            &next.source.presentation_id,
            &next.source.thread_id,
        )
        .await
        .unwrap();
        let proof = read_fixed_transfer_seat(
            state.clone(),
            next.source.clone(),
            frozen.anchor.clone(),
            initial,
            vec![next.clone()],
        )
        .await
        .unwrap();
        let links = proof
            .preparation_freeze_links(&state.state_store, &frozen.anchor, &next)
            .unwrap();
        let before_source = frozen.anchor.clone();
        let before_destination = state
            .state_store
            .capture_thread_event_read_anchor(
                &next.destination.chain_root_id,
                &next.destination.thread_id,
            )
            .unwrap()
            .unwrap();
        let mut wrong = links.clone();
        wrong.preparation.event_hash = "f".repeat(64);
        assert!(
            handle_transfer(
                json!({"action":"prepare","transfer":next,"export":export_value,"freeze":wrong}),
                ctx.clone(),
                state.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(
            state
                .state_store
                .capture_thread_event_read_anchor(
                    &next.source.chain_root_id,
                    &next.source.thread_id
                )
                .unwrap()
                .unwrap(),
            before_source
        );
        assert_eq!(
            state
                .state_store
                .capture_thread_event_read_anchor(
                    &next.destination.chain_root_id,
                    &next.destination.thread_id
                )
                .unwrap()
                .unwrap(),
            before_destination
        );
        let response = handle_transfer(
            json!({"action":"prepare","transfer":next,"export":export_value,"freeze":links}),
            ctx,
            state.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response["outcome"], "prepared");
        let retained =
            ryeos_app::thread_lifecycle::presentation_transfer::read_complete_fixed_history(
                state.state_store.clone(),
                &next,
            )
            .unwrap();
        let prepared = retained.lifecycle().prepared().unwrap();
        let TransferPhase::Prepared {
            freeze: Some(retained_links),
            inbound,
            ..
        } = &prepared.record.phase
        else {
            panic!("mandatory MOVE freeze")
        };
        assert_eq!(retained_links, &links);
        assert_eq!(inbound, retained.lifecycle().inbound().evidence);
        let after = super::read_completed_seat_history(
            state.clone(),
            &session,
            &next.source.presentation_id,
            &next.source.thread_id,
        )
        .await
        .unwrap();
        assert_eq!(after.verified.replay.revision, next.source.revision);
        assert_eq!(
            after.verified.replay.transfer_drafts,
            replay.transfer_drafts
        );
        assert_eq!(
            after
                .verified
                .replay
                .composition
                .mount(&key)
                .unwrap()
                .binding(),
            mounted.binding()
        );
    }

    #[tokio::test]
    async fn second_copy_prepare_uses_completed_prior_transfer_frontiers() {
        // The fixture admits actual pinned attachments and publishes the first
        // transfer through dedicated writers/session owners. Its compiler
        // metadata remains observation-only test context, not external grants.
        let (_directory, state, previous, _, _, session_id) =
            super::super::authenticated_history::activated_pair_with_compiled_authority();
        let state = Arc::new(state);
        let session_id = session_id.unwrap();
        let ui = get_ui_state(&state).unwrap();
        let session = ui.browser_sessions.get_session(&session_id).unwrap();
        let source = super::read_completed_seat_history(
            state.clone(),
            &session,
            &previous.destination.presentation_id,
            &previous.destination.thread_id,
        )
        .await
        .unwrap();
        let destination = super::read_completed_seat_history(
            state.clone(),
            &session,
            &previous.source.presentation_id,
            &previous.source.thread_id,
        )
        .await
        .unwrap();
        let replay = &source.verified.replay;
        let dock = &replay.composition.docks()["right"];
        let mounted = dock.view();
        let descriptor = &replay.binding_descriptors()[&mounted.binding().binding_attachment_id];
        let view: ryeos_client_base::ui::content::ViewBinding =
            serde_json::from_value(descriptor.presentation.views[mounted.view_ref()].clone())
                .expect("admitted View binding decodes");
        let facets = ryeos_client_base::ui::attachment::selection_dependencies(&view)
            .into_iter()
            .collect::<Vec<_>>();
        assert!(
            !facets.is_empty(),
            "real imported binding has portable subject dependencies"
        );
        // COPY unpins an actual pinned subject, but an already unresolved
        // source keeps its declared required-input identity rather than
        // silently renaming that input during export.
        let subject = match &replay.subjects[mounted.instance_key()].attachment {
            ryeos_client_base::ui::attachment::SelectionAttachment::Pinned { .. } => {
                json!({"mode":"required_subject","input":"selection","facets":facets})
            }
            subject => serde_json::to_value(subject).unwrap(),
        };
        let export: UiTransferExport = serde_json::from_value(json!({
            "schema":1,"mode":"copy","content":{"kind":"shared_dock","edge":"right",
                "visible":dock.visible,"size":dock.size,"mount":{
                    "source_instance":mounted.instance_key(),"view_ref":mounted.view_ref(),
                    "binding":mounted.binding(),"local":ryeos_client_base::view_set::ViewLocalState::None,
                    "subject":subject,
                    "route":{"kind":"authored"},"current_label":replay.lens_labels.get(mounted.instance_key()).cloned().flatten(),
                    "drafts":[],"lens_frames":[]}}
        })).unwrap();
        let export_value = serde_json::to_value(&export).unwrap();
        let mut next = previous.clone();
        next.operation_id = "00000000-0000-4000-8000-000000000088".into();
        next.source = previous.destination.clone();
        next.destination = previous.source.clone();
        next.source.revision = source.verified.replay.revision;
        next.source.producer_incarnation = source.control().unwrap().producer_incarnation;
        next.destination.revision = destination.verified.replay.revision;
        next.destination.producer_incarnation = destination.control().unwrap().producer_incarnation;
        next.export_digest = ryeos_client_base::ui::seat_payload_digest(&export_value).unwrap();
        export.validate_snapshot(&next, replay, 65536).unwrap();
        let original_import = mounted.instance_key().clone();
        let original_binding = mounted.binding().clone();
        let original_drafts = replay.transfer_drafts.clone();
        let memberships_before = session.presentations.clone();
        let response = handle_transfer(
            json!({"action":"prepare","transfer":next,
            "export":export_value,"freeze":null}),
            HandlerContext::new(format!("session:{session_id}"), Vec::new(), false),
            state.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response["outcome"], "prepared");
        let retained =
            ryeos_app::thread_lifecycle::presentation_transfer::read_complete_fixed_history(
                state.state_store.clone(),
                &next,
            )
            .unwrap();
        let prepared = retained.lifecycle().prepared().unwrap();
        let inbound = retained.lifecycle().inbound();
        assert_eq!(
            response["inbound"],
            serde_json::to_value(&inbound.evidence).unwrap()
        );
        assert_eq!(
            response["preparation"],
            serde_json::to_value(&prepared.evidence).unwrap()
        );
        let TransferPhase::Prepared {
            inbound: linked,
            freeze,
            export: retained_export,
        } = &prepared.record.phase
        else {
            unreachable!()
        };
        assert_eq!(linked, inbound.evidence);
        assert!(freeze.is_none());
        assert_eq!(retained_export, &serde_json::to_value(export).unwrap());
        let after_session = ui.browser_sessions.get_session(&session_id).unwrap();
        assert_eq!(
            after_session.presentations, memberships_before,
            "Prepare cannot activate or mutate retained imported membership"
        );
        let after = super::read_completed_seat_history(
            state.clone(),
            &after_session,
            &next.source.presentation_id,
            &next.source.thread_id,
        )
        .await
        .unwrap();
        let preserved = after
            .verified
            .replay
            .composition
            .mount(&original_import)
            .unwrap();
        assert_eq!(preserved.binding(), &original_binding);
        assert_eq!(after.verified.replay.transfer_drafts, original_drafts);
        assert_eq!(after.verified.replay.revision, next.source.revision);
    }

    #[tokio::test]
    async fn paired_transfer_materialization_keeps_later_edit_and_exact_initialization() {
        // Signed replay qualification only: this fixture deliberately has no
        // live authored compiler closure. The full admission tests below stay
        // separate and must not be made green by stamping engine generations.
        let (_directory, state, transfer, _, _, session_id) =
            super::super::authenticated_history::activated_pair_observation_fixture_with_session(
                ryeos_state::objects::ThreadHistoryRetention::Durable,
                true,
            );
        let state = Arc::new(state);
        let session = get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .get_session(&session_id.unwrap())
            .unwrap();
        let proof = read_transfer_proof(state.clone(), &session, transfer.clone())
            .await
            .unwrap();
        let destination = &transfer.destination;
        let completed = super::read_completed_seat_history(
            state.clone(),
            &session,
            &destination.presentation_id,
            &destination.thread_id,
        )
        .await
        .unwrap();
        let materialized = proof
            .destination
            .history(&completed.membership.initialization)
            .unwrap();
        assert_eq!(materialized.replay.revision, destination.revision + 2);
        assert_eq!(
            materialized.replay.revision,
            completed.verified.replay.revision
        );
        assert_eq!(
            materialized.replay.next_mount_id,
            completed.verified.replay.next_mount_id
        );
        assert_eq!(
            materialized.replay.subjects,
            completed.verified.replay.subjects
        );
        assert_eq!(
            materialized.replay.transfer_drafts,
            completed.verified.replay.transfer_drafts
        );
        let mounted = materialized.replay.composition.docks()["right"].view();
        let expected = completed.verified.replay.composition.docks()["right"].view();
        assert_eq!(mounted.instance_key(), expected.instance_key());
        assert_eq!(mounted.binding(), expected.binding());
        assert_eq!(materialized.next_engine_seq, completed.next_engine_seq());
        assert!(proof.cut.covers_seat_anchor(
            &destination.presentation_id,
            &destination.chain_root_id,
            &destination.thread_id,
            &completed.anchor
        ));
        let detail = state
            .state_store
            .get_thread(&destination.thread_id)
            .unwrap()
            .unwrap();
        composition::require_completed_initialization(
            &state,
            &session,
            &completed.membership,
            &detail,
            &materialized,
        )
        .unwrap();
        let mut changed = completed.membership.clone();
        changed.root_coordinate.binding_digest = "f".repeat(64);
        assert!(
            composition::require_completed_initialization(
                &state,
                &session,
                &changed,
                &detail,
                &materialized,
            )
            .is_err(),
            "different current root membership cannot reuse completed proof"
        );
        let mut changed_initial = completed.membership.initialization.clone();
        changed_initial.binding_attachments.clear();
        assert!(!materialized.replay.matches_initialization(&changed_initial));
        assert!(
            proof.destination.history(&changed_initial).is_err(),
            "a different admitted initialization closure cannot reuse this prefix"
        );
    }

    #[tokio::test]
    async fn paired_transfer_retry_authenticates_after_later_receipted_destination_edit() {
        let (_directory, state, transfer, _source_initial, _destination_initial, session_id) =
            super::super::authenticated_history::activated_pair_with_compiled_authority();
        let state = Arc::new(state);
        let session_id = session_id.expect("fixture has an admitted browser session");
        let ui = get_ui_state(&state).unwrap();
        let session = ui.browser_sessions.get_session(&session_id).unwrap();
        let proof = read_transfer_proof(state.clone(), &session, transfer.clone())
            .await
            .unwrap();
        let source = &transfer.source;
        let destination = &transfer.destination;
        let (source_revision, destination_revision) = state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<(u64, u64)> {
                let source =
                    authenticated_seat(generation, &state, &session, source, false, Some(&proof))?;
                let destination = authenticated_seat(
                    generation,
                    &state,
                    &session,
                    destination,
                    false,
                    Some(&proof),
                )?;
                Ok((
                    source.verified.replay.revision,
                    destination.verified.replay.revision,
                ))
            })
            .unwrap();
        assert!(source_revision >= transfer.source.revision);
        assert_eq!(
            destination_revision,
            transfer.destination.revision + 2,
            "retry must include both activation and the later receipted edit"
        );
        let completed = super::read_completed_seat_history(
            state.clone(),
            &session,
            &destination.presentation_id,
            &destination.thread_id,
        )
        .await
        .unwrap();
        let mut next_transfer_destination = destination.clone();
        next_transfer_destination.revision = destination_revision;
        next_transfer_destination.producer_incarnation =
            completed.control().unwrap().producer_incarnation;
        let prepared_revision = state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<u64> {
                Ok(authenticated_seat_with_frontier(
                    generation,
                    &state,
                    &session,
                    &next_transfer_destination,
                    true,
                    None,
                    Some(&completed),
                )?
                .verified
                .replay
                .revision)
            })
            .unwrap();
        assert_eq!(
            prepared_revision, destination_revision,
            "a new identity may use only the completed current paired frontier"
        );
        let mut stale_destination = next_transfer_destination.clone();
        stale_destination.revision -= 1;
        assert!(
            state
                .engine
                .with_checked_bundle_generation(|generation| {
                    authenticated_seat_with_frontier(
                        generation,
                        &state,
                        &session,
                        &stale_destination,
                        true,
                        None,
                        Some(&completed),
                    )
                })
                .is_err(),
            "completed history must not rebase a stale new-transfer revision"
        );
    }

    #[test]
    fn activate_request_is_closed_and_carries_only_exact_identity() {
        let transfer = json!({
            "operation_id": "33333333-3333-4333-8333-333333333333",
            "source": {"presentation_id":"11111111-1111-4111-8111-111111111111", "thread_id":"source", "chain_root_id":"source", "producer_incarnation":"aa".repeat(32), "revision":1},
            "destination": {"presentation_id":"22222222-2222-4222-8222-222222222222", "thread_id":"destination", "chain_root_id":"destination", "producer_incarnation":"bb".repeat(32), "revision":1},
            "export_digest":"cc".repeat(32)
        });
        let mut request = json!({"action":"activate", "transfer":transfer});
        assert!(matches!(
            serde_json::from_value::<TransferRequest>(request.clone()).unwrap(),
            TransferRequest::Activate { .. }
        ));
        request["untrusted_phase"] = json!("activated");
        assert!(serde_json::from_value::<TransferRequest>(request).is_err());
    }

    #[test]
    fn accept_tabs_and_shared_dock_requests_keep_server_allocations_closed() {
        let transfer = json!({
            "operation_id": "33333333-3333-4333-8333-333333333333",
            "source": {"presentation_id":"11111111-1111-4111-8111-111111111111", "thread_id":"source", "chain_root_id":"source", "producer_incarnation":"aa".repeat(32), "revision":1},
            "destination": {"presentation_id":"22222222-2222-4222-8222-222222222222", "thread_id":"destination", "chain_root_id":"destination", "producer_incarnation":"bb".repeat(32), "revision":1},
            "export_digest":"cc".repeat(32)
        });
        let evidence = json!({"event_hash":"dd".repeat(32), "chain_seq":1});
        let tabs = json!({"action":"accept_tabs", "transfer":transfer, "inbound":evidence,
            "preparation":evidence, "surface_instance_id":"surface"});
        assert!(matches!(
            serde_json::from_value::<TransferRequest>(tabs.clone()).unwrap(),
            TransferRequest::AcceptTabs { .. }
        ));
        let mut invented = tabs;
        invented["allocation_start"] = json!(9);
        assert!(serde_json::from_value::<TransferRequest>(invented).is_err());
        let dock = json!({"action":"accept_shared_dock", "transfer":transfer,
            "inbound":evidence, "preparation":evidence, "edge":"left"});
        assert!(matches!(
            serde_json::from_value::<TransferRequest>(dock).unwrap(),
            TransferRequest::AcceptSharedDock { .. }
        ));
    }

    #[test]
    fn cancel_freeze_request_is_closed_and_carries_full_identity() {
        let seat = json!({"presentation_id":"source", "thread_id":"thread", "chain_root_id":"thread", "producer_incarnation":"a".repeat(64), "revision":7});
        let transfer = json!({"operation_id":"b".repeat(64), "source":seat, "destination": {"presentation_id":"destination", "thread_id":"other", "chain_root_id":"other", "producer_incarnation":"c".repeat(64), "revision":9}, "export_digest":"d".repeat(64)});
        let mut request = json!({"action":"cancel_freeze", "transfer":transfer});
        let decoded: TransferRequest = serde_json::from_value(request.clone()).unwrap();
        assert!(
            matches!(decoded, TransferRequest::CancelFreeze { transfer } if transfer.source.revision == 7 && transfer.destination.revision == 9)
        );
        request["export"] = json!({});
        assert!(serde_json::from_value::<TransferRequest>(request).is_err());
    }

    #[test]
    fn prepare_requires_explicit_nullable_freeze_and_rejects_extra_authority() {
        let source = json!({"presentation_id":"source", "thread_id":"source", "chain_root_id":"source", "producer_incarnation":"a".repeat(64), "revision":7});
        let destination = json!({"presentation_id":"destination", "thread_id":"destination", "chain_root_id":"destination", "producer_incarnation":"b".repeat(64), "revision":9});
        let mut request = json!({"action":"prepare", "transfer":{"operation_id":"c".repeat(64), "source":source, "destination":destination, "export_digest":"d".repeat(64)}, "export":{}, "freeze":null});
        assert!(matches!(
            serde_json::from_value::<TransferRequest>(request.clone()).unwrap(),
            TransferRequest::Prepare { freeze: None, .. }
        ));
        request.as_object_mut().unwrap().remove("freeze");
        assert!(serde_json::from_value::<TransferRequest>(request.clone()).is_err());
        request["freeze"] = Value::Null;
        request["producer"] = json!({"authority":"operator"});
        assert!(serde_json::from_value::<TransferRequest>(request).is_err());
    }

    #[test]
    fn accepted_retry_requires_the_exact_original_view_target() {
        let surface = SurfaceInstanceId::try_new("11111111-1111-4111-8111-111111111111").unwrap();
        let allocation =
            ryeos_client_base::ui::composition_delta::UiCompositionMountAllocation::allocate(
                10, 1, 1,
            )
            .unwrap();
        let retained = UiTransferDestinationPlacement::View {
            surface_instance_id: surface.clone(),
            view_set_id: ViewSetId::new(3),
            allocation,
            placement: CompositionPlacement::First,
        };
        assert!(exact_accept_retry_target(
            &retained,
            &TransferAcceptanceTarget::View {
                surface_instance_id: &surface,
                view_set_id: ViewSetId::new(3),
                placement: &CompositionPlacement::First,
            }
        ));
        assert!(!exact_accept_retry_target(
            &retained,
            &TransferAcceptanceTarget::View {
                surface_instance_id: &surface,
                view_set_id: ViewSetId::new(4),
                placement: &CompositionPlacement::First,
            }
        ));
        assert!(!exact_accept_retry_target(
            &retained,
            &TransferAcceptanceTarget::View {
                surface_instance_id: &SurfaceInstanceId::try_new(
                    "22222222-2222-4222-8222-222222222222"
                )
                .unwrap(),
                view_set_id: ViewSetId::new(3),
                placement: &CompositionPlacement::First,
            }
        ));
        assert!(!exact_accept_retry_target(
            &retained,
            &TransferAcceptanceTarget::View {
                surface_instance_id: &surface,
                view_set_id: ViewSetId::new(3),
                placement: &CompositionPlacement::GroupTab {
                    group_id: ryeos_client_base::ids::ViewGroupId::new(2),
                    before: None
                }
            }
        ));
    }

    #[test]
    fn prepared_reply_carries_exact_both_seat_evidence() {
        let inbound = TransferEvidence {
            event_hash: "a".repeat(64),
            chain_seq: 17,
        };
        let preparation = TransferEvidence {
            event_hash: "b".repeat(64),
            chain_seq: 23,
        };
        let response = prepared_response("operation", inbound.clone(), preparation.clone());
        assert_eq!(response["outcome"], "prepared");
        assert_eq!(response["inbound"], serde_json::to_value(inbound).unwrap());
        assert_eq!(
            response["preparation"],
            serde_json::to_value(&preparation).unwrap()
        );
        let uncertain = transfer_response("operation", "uncertain");
        assert!(uncertain.get("preparation").is_none());
        assert!(uncertain.get("inbound").is_none());
        assert!(uncertain.get("acceptance").is_none());
        let accepted = accepted_response("operation", preparation.clone());
        assert_eq!(accepted["outcome"], "accepted");
        assert_eq!(
            accepted["acceptance"],
            serde_json::to_value(preparation).unwrap()
        );
    }
}
