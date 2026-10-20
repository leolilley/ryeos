//! Synchronous exact-instance MOVE restriction preparation. The ordinary seat
//! preparation is write-ahead; this module grants no transfer ingress or release.

use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use crate::ui::admission_operation::{
    COMPLETED_EVENT, PREPARED_EVENT, UiCompletedPresentationOperation,
    UiPreparedPresentationOperation, UiPreparedPresentationOperationBody, single_seat_append_fits,
};
use crate::ui::model::RyeOsCore;
use crate::ui::seat::SeatEventKind;
use crate::ui::transfer_freeze::UiMoveFreezeRequest;

fn transfer_prepare_fits(
    transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
    export: &crate::ui::transfer_export::UiTransferExport,
    freeze: Option<&ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence>,
    max_bytes: u64,
) -> bool {
    serde_json::to_vec(&serde_json::json!({
        "action": "prepare", "transfer": transfer, "export": export, "freeze": freeze,
    }))
    .is_ok_and(|bytes| bytes.len() as u64 <= max_bytes)
}

impl RyeOsCore {
    pub(super) fn begin_transfer(
        &mut self,
        mode: crate::ui::transfer_export::UiTransferExportMode,
        source: crate::ui::event::UiTransferSourceSelection,
        destination_presentation_id: String,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        use crate::ui::effect::RyeOsEffectKind;
        let result = (|| -> Result<_, String> {
            if !self.composition_write_available()
                || self.pending_effects.values().any(|effect| {
                    matches!(effect, RyeOsEffectKind::ObserveTransferDestination { .. })
                })
            {
                return Err("transfer source requires a settled seat and one observation".into());
            }
            let producer = self
                .presentation_operations
                .producer
                .as_ref()
                .filter(|producer| self.producer_matches_membership(producer))
                .ok_or("transfer source has no current producer")?
                .clone();
            let presentation = self.presentation.as_ref().ok_or("transfer source absent")?;
            let session = self
                .data
                .session
                .as_ref()
                .ok_or("transfer session absent")?;
            let source_member = session
                .presentations
                .iter()
                .find(|member| member.presentation_id == producer.presentation_id)
                .ok_or("source membership absent")?;
            let destination = session
                .presentations
                .iter()
                .filter(|member| member.presentation_id == destination_presentation_id)
                .collect::<Vec<_>>();
            let [destination] = destination.as_slice() else {
                return Err("destination membership is absent or duplicated".into());
            };
            if destination.presentation_id == source_member.presentation_id {
                return Err("transfer requires another presentation".into());
            }
            let destination_thread_id = destination
                .thread_id
                .clone()
                .ok_or("destination seat thread absent")?;
            let destination_chain_root_id = destination
                .chain_root_id
                .clone()
                .ok_or("destination seat root absent")?;
            let destination_root_binding = destination.root_binding.clone();
            let destination_initialization_digest = destination.initialization_digest.clone();
            let source_chain_root_id = source_member
                .chain_root_id
                .clone()
                .ok_or("source seat root absent")?;
            let root_binding = presentation.root_binding().clone();
            let bounds = self
                .binding_attachment(&root_binding.binding_attachment_id)
                .filter(|descriptor| {
                    descriptor.binding_generation == root_binding.binding_generation
                        && descriptor.binding_digest == root_binding.binding_digest
                })
                .ok_or("source root binding unavailable")?
                .binding_request_bounds;
            let replay = self.acknowledged_composition_replay()?;
            let source_export = crate::ui::transfer_capture::capture_selected_transfer_export(
                self,
                mode,
                &source,
                bounds.max_request_bytes,
            )?;
            Ok(vec![self.emit(
                RyeOsEffectKind::ObserveTransferDestination {
                    destination_presentation_id,
                    destination_thread_id,
                    destination_chain_root_id,
                    destination_root_binding,
                    destination_initialization_digest,
                    source_producer: producer,
                    source_chain_root_id,
                    source_root_binding: root_binding,
                    source_revision: replay.revision,
                    source_export,
                    request_bounds: bounds,
                },
            )])
        })();
        match result {
            Ok(effects) => effects,
            Err(reason) => {
                self.notice(reason, crate::ui::view_model::RyeOsTone::Warn);
                Vec::new()
            }
        }
    }

    pub(super) fn contact_transfer(
        &mut self,
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        action: crate::ui::event::UiTransferContactIntent,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        use crate::ui::effect::{RyeOsEffectKind, UiTransferContactRequest as Request};
        use crate::ui::event::UiTransferContactIntent as Intent;
        let result = (|| -> Result<_, String> {
            let source_action = matches!(
                action,
                Intent::Commit | Intent::Abort | Intent::CancelFreeze
            );
            let actor = if source_action {
                &transfer.source
            } else {
                &transfer.destination
            };
            let producer = self
                .presentation_operations
                .producer
                .as_ref()
                .ok_or("transfer contact requires a current seat producer")?;
            let session = self
                .data
                .session
                .as_ref()
                .ok_or("transfer contact has no session")?;
            if self.presentation_operations.replay_blocked
                || !self.producer_matches_membership(producer)
                || producer.presentation_id != actor.presentation_id
                || producer.thread_id != actor.thread_id
                || self
                    .presentation
                    .as_ref()
                    .is_none_or(|p| p.id().as_str() != actor.presentation_id)
                || ![&transfer.source, &transfer.destination]
                    .into_iter()
                    .all(|seat| {
                        session.presentations.iter().any(|member| {
                            member.presentation_id == seat.presentation_id
                                && member.thread_id.as_deref() == Some(seat.thread_id.as_str())
                                && member.chain_root_id.as_deref()
                                    == Some(seat.chain_root_id.as_str())
                        })
                    })
            {
                return Err("transfer contact differs from current exact paired membership".into());
            }
            let replay = self.acknowledged_composition_replay()?;
            let bounds = self
                .binding_attachment(
                    &self
                        .presentation
                        .as_ref()
                        .unwrap()
                        .root_binding()
                        .binding_attachment_id,
                )
                .ok_or("transfer contact root descriptor absent")?
                .binding_request_bounds;
            let lifecycle = if matches!(action, Intent::CancelFreeze) {
                None
            } else {
                let (_, lifecycle) = self
                    .authenticated_transfer_proofs
                    .get(&transfer.operation_id)
                    .filter(|(frame, lifecycle)| {
                        frame.transfer == transfer && lifecycle.transfer() == &transfer
                    })
                    .ok_or("transfer contact has no completed paired proof")?;
                Some(lifecycle)
            };
            let (name, request) = match action {
                Intent::AcceptView {
                    surface_instance_id,
                    view_set_id,
                    placement,
                } => {
                    let proof = lifecycle.as_ref().unwrap();
                    let prepared = proof
                        .prepared()
                        .ok_or("accept has no authenticated preparation")?;
                    let inbound = proof.inbound();
                    if proof.accepted().is_some()
                        || proof.aborted().is_some()
                        || proof.committed().is_some()
                        || proof.retained().is_some()
                        || proof.released().is_some()
                    {
                        return Err("transfer acceptance already settled or closed".into());
                    }
                    (
                        "accept_view",
                        Request::AcceptView {
                            transfer,
                            inbound: inbound.evidence.clone(),
                            preparation: prepared.evidence.clone(),
                            surface_instance_id,
                            view_set_id,
                            placement,
                        },
                    )
                }
                Intent::AcceptTabs {
                    surface_instance_id,
                } => {
                    let proof = lifecycle.as_ref().unwrap();
                    let prepared = proof
                        .prepared()
                        .ok_or("accept has no authenticated preparation")?;
                    let inbound = proof.inbound();
                    if proof.accepted().is_some()
                        || proof.aborted().is_some()
                        || proof.committed().is_some()
                        || proof.retained().is_some()
                        || proof.released().is_some()
                    {
                        return Err("transfer acceptance already settled or closed".into());
                    }
                    (
                        "accept_tabs",
                        Request::AcceptTabs {
                            transfer,
                            inbound: inbound.evidence.clone(),
                            preparation: prepared.evidence.clone(),
                            surface_instance_id,
                        },
                    )
                }
                Intent::AcceptSharedDock { edge } => {
                    let proof = lifecycle.as_ref().unwrap();
                    let prepared = proof
                        .prepared()
                        .ok_or("accept has no authenticated preparation")?;
                    let inbound = proof.inbound();
                    if proof.accepted().is_some()
                        || proof.aborted().is_some()
                        || proof.committed().is_some()
                        || proof.retained().is_some()
                        || proof.released().is_some()
                    {
                        return Err("transfer acceptance already settled or closed".into());
                    }
                    (
                        "accept_shared_dock",
                        Request::AcceptSharedDock {
                            transfer,
                            inbound: inbound.evidence.clone(),
                            preparation: prepared.evidence.clone(),
                            edge,
                        },
                    )
                }
                Intent::Commit => {
                    let proof = lifecycle.as_ref().unwrap();
                    if proof.prepared().is_none()
                        || proof.accepted().is_none()
                        || proof.committed().is_some()
                        || proof.aborted().is_some()
                    {
                        return Err("commit has no exact unsettled accepted phase".into());
                    }
                    ("commit", Request::Commit { transfer })
                }
                Intent::Abort => {
                    let proof = lifecycle.as_ref().unwrap();
                    if proof.prepared().is_none()
                        || proof.committed().is_some()
                        || proof.aborted().is_some()
                    {
                        return Err("abort has no exact unsettled preparation".into());
                    }
                    ("abort", Request::Abort { transfer })
                }
                Intent::Activate => {
                    let proof = lifecycle.as_ref().unwrap();
                    if proof.accepted().is_none()
                        || proof.committed().is_none()
                        || proof.activated().is_some()
                        || proof.retained().is_some()
                        || proof.released().is_some()
                    {
                        return Err("activation has no exact committed hidden reservation".into());
                    }
                    ("activate", Request::Activate { transfer })
                }
                Intent::CancelFreeze => {
                    if !replay
                        .move_freezes
                        .values()
                        .any(|freeze| freeze.transfer == transfer)
                        || self
                            .authenticated_cancellation_proofs
                            .contains_key(&transfer.operation_id)
                    {
                        return Err("cancellation has no active exact source freeze".into());
                    }
                    ("cancel_freeze", Request::CancelFreeze { transfer })
                }
            };
            let body = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
            if bounds.max_request_bytes == 0 || body.len() as u64 > bounds.max_request_bytes {
                return Err("transfer contact exceeds admitted request bound".into());
            }
            let operation_id = request.transfer().operation_id.clone();
            if self.pending_effects.values().any(|effect| matches!(effect,
                RyeOsEffectKind::ContactTransfer { request, .. } if request.transfer().operation_id == operation_id
            )) || !self.contacted_transfer_actions.insert((operation_id, name.into())) {
                return Err("transfer contact requires fresh authenticated observation".into());
            }
            Ok(vec![self.emit(RyeOsEffectKind::ContactTransfer {
                request,
                request_bounds: bounds,
            })])
        })();
        match result {
            Ok(effects) => effects,
            Err(reason) => {
                self.notice(reason, crate::ui::view_model::RyeOsTone::Warn);
                Vec::new()
            }
        }
    }

    pub(super) fn prepare_transfer(
        &mut self,
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        export: crate::ui::transfer_export::UiTransferExport,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        let result = (|| -> Result<Vec<crate::ui::effect::RyeOsEffect>, String> {
            ryeos_ui_presentation_contract::transfer::TransferRecord {
                schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
                transfer: transfer.clone(),
                phase: ryeos_ui_presentation_contract::transfer::TransferPhase::InboundCommitment,
            }
            .validate_shape()?;
            if transfer.source.presentation_id == transfer.destination.presentation_id {
                return Err("transfer requires distinct presentations".into());
            }
            let root = self
                .presentation
                .as_ref()
                .ok_or("transfer presentation absent")?
                .root_binding()
                .clone();
            if export.mode == crate::ui::transfer_export::UiTransferExportMode::Move {
                let instances = export
                    .mounts()
                    .into_iter()
                    .map(|mount| mount.source_instance.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let view_sets = match &export.content {
                    crate::ui::transfer_export::UiTransferExportContent::ViewSets {
                        sets, ..
                    } => sets.iter().map(|set| set.source_view_set_id).collect(),
                    crate::ui::transfer_export::UiTransferExportContent::SurfaceMount {
                        source_view_set_id,
                        ..
                    } => std::collections::BTreeSet::from([*source_view_set_id]),
                    crate::ui::transfer_export::UiTransferExportContent::SharedDock { .. } => {
                        std::collections::BTreeSet::new()
                    }
                }
                .into_iter()
                .collect();
                self.prepare_move_freeze(UiMoveFreezeRequest {
                    schema: 1,
                    operation_id: String::new(),
                    transfer,
                    root_binding: root,
                    export,
                    instances,
                    view_sets,
                })?;
                return Ok(Vec::new());
            }
            if !self.composition_write_available() || self.any_move_freeze(|freeze| {
                export.mounts().into_iter().any(|mount| freeze.instances.contains(&mount.source_instance))
                    || export.sets().is_some_and(|sets| sets.iter().any(|set| freeze.view_sets.contains(&set.source_view_set_id)))
                    || matches!(&export.content,
                        crate::ui::transfer_export::UiTransferExportContent::SurfaceMount { source_view_set_id, .. }
                            if freeze.view_sets.contains(source_view_set_id))
            }) {
                return Err("COPY requires a settled unfrozen source".into());
            }
            let producer = self
                .presentation_operations
                .producer
                .as_ref()
                .ok_or("COPY requires an authenticated producer")?;
            let replay = self.acknowledged_composition_replay()?;
            let member = self
                .data
                .session
                .as_ref()
                .and_then(|session| {
                    session
                        .presentations
                        .iter()
                        .find(|member| member.presentation_id == producer.presentation_id)
                })
                .ok_or("COPY membership absent")?;
            if !self.producer_matches_membership(producer)
                || transfer.source.presentation_id != producer.presentation_id
                || transfer.source.thread_id != producer.thread_id
                || member.chain_root_id.as_deref() != Some(transfer.source.chain_root_id.as_str())
                || transfer.source.producer_incarnation != producer.producer_incarnation
                || transfer.source.revision != replay.revision
            {
                return Err("COPY differs from exact source identity".into());
            }
            let bounds = self
                .binding_attachment(&root.binding_attachment_id)
                .ok_or("COPY root descriptor absent")?
                .binding_request_bounds;
            export.validate_for(&transfer, bounds.max_request_bytes)?;
            let captured = crate::ui::transfer_capture::capture_transfer_export_from_core(
                self,
                &export,
                bounds.max_request_bytes,
            )?;
            captured.validate_for(&transfer, bounds.max_request_bytes)?;
            captured.validate_snapshot(&transfer, &replay, bounds.max_request_bytes)?;
            if !transfer_prepare_fits(&transfer, &captured, None, bounds.max_request_bytes) {
                return Err("COPY transfer contact exceeds admitted request bound".into());
            }
            if self.pending_effects.values().any(|effect| {
                matches!(effect,
                crate::ui::effect::RyeOsEffectKind::PrepareTransfer { transfer: previous, .. }
                    if previous.operation_id == transfer.operation_id)
            }) {
                return Err("transfer contact already pending".into());
            }
            if !self
                .presentation_operations
                .dispatched
                .insert(transfer.operation_id.clone())
            {
                return Err(
                    "transfer contact requires authenticated observation before retry".into(),
                );
            }
            Ok(vec![self.emit(
                crate::ui::effect::RyeOsEffectKind::PrepareTransfer {
                    transfer,
                    export: captured,
                    freeze: None,
                    request_bounds: bounds,
                },
            )])
        })();
        match result {
            Ok(effects) => effects,
            Err(reason) => {
                self.notice(reason, crate::ui::view_model::RyeOsTone::Warn);
                Vec::new()
            }
        }
    }

    /// ACK alone has no canonical hashes. Wait for the authenticated original
    /// rows as well, and reuse the existing dispatched index for exact contact.
    pub(super) fn drive_acknowledged_move_transfers(
        &mut self,
        replay: &crate::ui::composition_replay::UiCompositionReplay,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        let mut effects = Vec::new();
        for (id, request) in &replay.move_freezes {
            if !replay.settled.contains(id)
                || self.presentation_operations.dispatched.contains(id)
                || self.presentation_operations.invalid.contains(id)
            {
                continue;
            }
            let Some(prepared_seq) = self.presentation_operations.prepared.get(id).copied() else {
                continue;
            };
            let Some(completed_seq) = self.presentation_operations.completed.get(id).copied()
            else {
                continue;
            };
            let Some(prepared) = self.seat.event_at_sequence(prepared_seq) else {
                continue;
            };
            let Some(completed) = self.seat.event_at_sequence(completed_seq) else {
                continue;
            };
            let SeatEventKind::PresentationOperationPrepared(operation) = &prepared.kind else {
                continue;
            };
            let SeatEventKind::PresentationOperationCompleted(completion) = &completed.kind else {
                continue;
            };
            if operation.move_freeze() != Some(request)
                || completion.validate_for(operation).is_err()
                || self.presentation_operations.producer.as_ref() != Some(&operation.producer)
                || !self.presentation_operation_is_live(operation)
            {
                continue;
            }
            let Some(preparation) = self.seat.evidence_at_sequence(prepared_seq).cloned() else {
                continue;
            };
            let Some(completion) = self.seat.evidence_at_sequence(completed_seq).cloned() else {
                continue;
            };
            if preparation.chain_seq >= completion.chain_seq
                || preparation.event_hash == completion.event_hash
            {
                continue;
            }
            let freeze = ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                preparation,
                completion,
            };
            if !transfer_prepare_fits(
                &request.transfer,
                &request.export,
                Some(&freeze),
                operation.request_bounds.max_request_bytes,
            ) {
                continue;
            }
            let effect = crate::ui::effect::RyeOsEffectKind::PrepareTransfer {
                transfer: request.transfer.clone(),
                export: request.export.clone(),
                freeze: Some(freeze),
                request_bounds: operation.request_bounds,
            };
            self.presentation_operations.dispatched.insert(id.clone());
            effects.push(self.emit(effect));
        }
        effects
    }

    /// Capture and restrict in one actor turn, before any subsequent event can
    /// mutate the source. The caller's immutable transfer identity is never
    /// rebased to a fresh revision, incarnation or export commitment.
    pub fn prepare_move_freeze(
        &mut self,
        mut request: UiMoveFreezeRequest,
    ) -> Result<UiPreparedPresentationOperation, String> {
        ryeos_ui_presentation_contract::transfer::TransferRecord {
            schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
            transfer: request.transfer.clone(),
            phase: ryeos_ui_presentation_contract::transfer::TransferPhase::InboundCommitment,
        }
        .validate_shape()?;
        if request.export.mode != crate::ui::transfer_export::UiTransferExportMode::Move {
            return Err("MOVE freeze requires MOVE export mode".into());
        }
        if self.presentation_operations.replay_blocked {
            return Err("MOVE freeze requires unblocked authenticated replay".into());
        }
        let producer = self
            .presentation_operations
            .producer
            .clone()
            .ok_or("MOVE freeze requires an authenticated producer")?;
        if !self.producer_matches_membership(&producer) {
            return Err("MOVE freeze producer differs from current membership".into());
        }
        let (prefix, replay) = self
            .acknowledged_composition_cache
            .as_ref()
            .filter(|(prefix, _)| *prefix == self.presentation_operations.acknowledged_prefix_len)
            .ok_or("MOVE freeze requires the checked acknowledged composition")?;
        let suffix = self
            .seat
            .events()
            .get(*prefix..)
            .ok_or("MOVE freeze acknowledged prefix is outside the seat")?;
        if !replay.pending.is_empty()
            || suffix.iter().any(|event| {
                matches!(
                    event.kind,
                    SeatEventKind::PresentationOperationPrepared(_)
                        | SeatEventKind::PresentationOperationCompleted(_)
                )
            })
            || !self.composition_write_available()
        {
            return Err("MOVE freeze conflicts with an unsettled presentation preparation".into());
        }
        let presentation = self
            .presentation
            .as_ref()
            .ok_or("MOVE freeze presentation absent")?;
        let root = presentation.root_binding().clone();
        let membership = self
            .data
            .session
            .as_ref()
            .and_then(|session| {
                session
                    .presentations
                    .iter()
                    .find(|member| member.presentation_id == producer.presentation_id)
            })
            .ok_or("MOVE freeze exact membership absent")?;
        let source = &request.transfer.source;
        if source.presentation_id != producer.presentation_id
            || source.thread_id != producer.thread_id
            || membership.chain_root_id.as_deref() != Some(source.chain_root_id.as_str())
            || source.producer_incarnation != producer.producer_incarnation
            || source.revision != replay.revision
            || request.root_binding != root
        {
            return Err("MOVE freeze differs from exact source identity or root".into());
        }
        let bounds = self
            .binding_attachment(&root.binding_attachment_id)
            .ok_or("MOVE freeze root descriptor absent")?
            .binding_request_bounds;
        request
            .export
            .validate_for(&request.transfer, bounds.max_request_bytes)?;
        if self.any_move_freeze(|prior| {
            prior.transfer.operation_id == request.transfer.operation_id
                || prior
                    .instances
                    .iter()
                    .any(|instance| request.instances.contains(instance))
                || prior
                    .view_sets
                    .iter()
                    .any(|set| request.view_sets.contains(set))
        }) {
            return Err("MOVE freeze overlaps an existing exact restriction".into());
        }
        // The capture helper independently refuses relevant pending effects
        // and retains the exact live set/mount/draft/focus inventory.
        let captured = crate::ui::transfer_capture::capture_transfer_export_from_core(
            self,
            &request.export,
            bounds.max_request_bytes,
        )?;
        let digest = crate::ui::seat_digest::seat_payload_digest(
            &serde_json::to_value(&captured).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if digest != request.transfer.export_digest {
            return Err("MOVE freeze live capture differs from immutable export commitment".into());
        }
        request.export = captured;
        let prepared_engine_seq = self
            .presentation_operations
            .reserved_engine_seq(self.seat.events().len())
            .ok_or("MOVE freeze authenticated engine frontier unavailable")?;
        prepared_engine_seq
            .checked_add(2)
            .ok_or("MOVE freeze completion engine position exhausted")?;
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            operation_id: String::new(),
            producer,
            prepared_engine_seq,
            root_binding: root,
            request_bounds: bounds,
            body: UiPreparedPresentationOperationBody::MoveFreeze { request },
        };
        operation.assign_identity().map_err(str::to_owned)?;
        operation.validate().map_err(str::to_owned)?;
        let request = operation
            .move_freeze()
            .ok_or("MOVE freeze preparation changed kind")?;
        request.validate_snapshot(replay, bounds.max_request_bytes)?;
        let completion = UiCompletedPresentationOperation {
            operation_id: operation.operation_id.clone(),
            result: serde_json::to_value(request.completion())
                .map_err(|error| error.to_string())?,
        };
        completion.validate_for(&operation).map_err(str::to_owned)?;
        if serde_json::to_vec(&operation)
            .map_err(|error| error.to_string())?
            .len() as u64
            > bounds.max_request_bytes
            || !single_seat_append_fits(
                &operation.producer,
                PREPARED_EVENT,
                &operation,
                bounds.max_seat_append_bytes,
            )
            || !single_seat_append_fits(
                &operation.producer,
                COMPLETED_EVENT,
                &completion,
                bounds.max_seat_append_bytes,
            )
        {
            return Err("MOVE freeze preparation or completion exceeds admitted bounds".into());
        }
        // Every fallible check precedes the first mutation. No await or emitted
        // platform work separates capture from this provisional restriction.
        self.seat
            .append(SeatEventKind::PresentationOperationPrepared(
                operation.clone(),
            ));
        self.bump_generation();
        Ok(operation)
    }

    pub(super) fn any_move_freeze(&self, matches: impl Fn(&UiMoveFreezeRequest) -> bool) -> bool {
        if self.presentation_operations.replay_blocked {
            return true;
        }
        // An ordinary fresh Core has no authenticated prefix, producer or
        // engine frontier. Missing a replay cache there is not a freeze. Any
        // retained initialization or authenticated state instead stays on the
        // fail-closed path below. The only walk here is the local unACKed tail.
        if self.acknowledged_composition_cache.is_none()
            && self.presentation_operations.acknowledged_prefix_len == 0
            && self.presentation_operations.producer.is_none()
            && self
                .presentation_operations
                .acknowledged_engine_frontier
                .is_none()
            && !self
                .seat
                .events()
                .iter()
                .any(|event| matches!(&event.kind, SeatEventKind::PresentationInitialized(_)))
        {
            return self.seat.events().iter().any(|event| match &event.kind {
                SeatEventKind::PresentationOperationPrepared(operation) => {
                    operation.move_freeze().is_some_and(&matches)
                }
                _ => false,
            });
        }
        let Some((prefix, replay)) = self
            .acknowledged_composition_cache
            .as_ref()
            .filter(|(prefix, _)| *prefix == self.presentation_operations.acknowledged_prefix_len)
        else {
            return true;
        };
        let Some(suffix) = self.seat.events().get(*prefix..) else {
            return true;
        };
        replay.move_freezes.values().any(&matches)
            || suffix.iter().any(|event| match &event.kind {
                SeatEventKind::PresentationOperationPrepared(operation) => {
                    operation.move_freeze().is_some_and(&matches)
                }
                _ => false,
            })
    }

    /// Includes exact unACKed preparations and checked retained obligations;
    /// missing checked state fails closed, without reconstructing history.
    pub(crate) fn move_freeze_restricts_instance(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.any_move_freeze(|request| request.instances.contains(instance))
    }

    pub(crate) fn move_freeze_restricts_view_set(&self, set: ViewSetId) -> bool {
        self.any_move_freeze(|request| request.view_sets.contains(&set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::reducer::test_support;
    use crate::ui::transfer_export::{
        UiTransferExport, UiTransferExportContent, UiTransferExportMode, UiTransferMountExport,
        UiTransferSetExport,
    };
    use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};

    fn fixture() -> (RyeOsCore, UiMoveFreezeRequest) {
        fixture_with_session(test_support::writable_session())
    }

    #[test]
    fn exact_tree_fold_cannot_mutate_frozen_cursor_or_branch_state() {
        let browser = test_support::session_with_surface(serde_json::json!({
            "name":"frozen-tree", "tiles":["view:test/tree"],
            "views":{"view:test/tree":{
                "widget":"table",
                "sources":{"default":{"ref":"service:test/tree","collection":"rows"}},
                "projections":{"hierarchy":{"id":"id","parent":"parent"},
                    "columns":[{"label":"Node","field":"id"}]}
            }}
        }));
        let (mut core, request) = fixture_with_session(browser);
        let instance = request.instances[0].clone();
        let source =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                .encode();
        core.data.sources.insert(
            source,
            serde_json::json!({"rows":[
                {"id":"other"}, {"id":"parent"}, {"id":"child","parent":"parent"}
            ]}),
        );
        assert!(
            crate::ui::view_model::view_pointer_tree_fold(
                &core,
                &instance,
                "view:test/tree#parent"
            )
            .is_some()
        );
        core.prepare_move_freeze(request).unwrap();
        let before = core.mounted_local_state(&instance).unwrap().clone();
        let generation = core.generation;
        let events = core.seat.events().to_vec();
        assert!(
            core.dispatch(crate::ui::event::RyeOsEvent::Ui {
                event: crate::ui::event::RyeOsUiEvent::SetViewTreeItemCollapsed {
                    instance_key: instance.clone(),
                    item_id: "view:test/tree#parent".into(),
                    collapsed: true,
                }
            })
            .is_empty()
        );
        assert_eq!(core.mounted_local_state(&instance), Some(&before));
        assert_eq!(core.generation, generation);
        assert_eq!(core.seat.events(), events);
    }

    #[test]
    fn selected_copy_observes_exact_destination_then_prepares_captured_source() {
        use crate::ui::{
            binding::{
                UiObservedPresentationProducer, UiPresentationProducerObservation,
                UiSeatControlPredecessor,
            },
            effect::{
                RyeOsEffectResult, RyeOsEffectResultKind, UiTransferDestinationObservationResult,
            },
            event::UiTransferSourceSelection,
        };
        let mut session = test_support::session();
        let mut destination = session.presentations[0].clone();
        destination.presentation_id = "22222222-2222-4222-8222-222222222222".into();
        destination.thread_id = Some("destination-seat".into());
        destination.chain_root_id = Some("destination-root".into());
        session.presentations.push(destination.clone());
        let mut core = test_support::core(session);
        test_support::claim_fixture_producer(&mut core);
        let surface = core.active_surface().unwrap().id().clone();
        let observed = core.dispatch_intent(crate::ui::event::RyeOsUiIntent::BeginTransfer {
            mode: UiTransferExportMode::Copy,
            source: UiTransferSourceSelection::SurfaceViewSets {
                surface_instance_id: surface,
            },
            destination_presentation_id: destination.presentation_id.clone(),
        });
        assert_eq!(observed.len(), 1);
        assert!(matches!(&observed[0].kind,
            RyeOsEffectKind::ObserveTransferDestination {
                destination_presentation_id, source_export, ..
            } if destination_presentation_id == &destination.presentation_id
                && source_export.mode == UiTransferExportMode::Copy
        ));
        let observation = UiPresentationProducerObservation {
            presentation_id: destination.presentation_id.clone(),
            thread_id: destination.thread_id.clone().unwrap(),
            chain_root_id: destination.chain_root_id.clone().unwrap(),
            root_binding: destination.root_binding.clone(),
            revision: 0,
            control_predecessor: UiSeatControlPredecessor::Control {
                event_hash: "ab".repeat(32),
            },
            producer: UiObservedPresentationProducer::Active {
                producer_incarnation: "cd".repeat(32),
            },
        };
        let prepared = core.dispatch(crate::ui::event::RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: observed[0].id,
                ok: true,
                kind: RyeOsEffectResultKind::TransferDestinationObserved,
                data: Some(
                    serde_json::to_value(UiTransferDestinationObservationResult {
                        operation_id: "33333333-3333-4333-8333-333333333333".into(),
                        observation,
                    })
                    .unwrap(),
                ),
                error: None,
            },
        });
        assert_eq!(prepared.len(), 1);
        assert!(
            matches!(&prepared[0].kind, RyeOsEffectKind::PrepareTransfer { transfer, export, freeze: None, .. }
            if transfer.destination.revision == 0
                && transfer.destination.producer_incarnation == "cd".repeat(32)
                && export.mode == UiTransferExportMode::Copy)
        );
    }

    #[test]
    fn cancellation_contact_uses_current_producer_without_rebasing_original_identity() {
        use crate::ui::{
            effect::RyeOsEffectKind,
            event::{RyeOsUiIntent, UiTransferContactIntent},
        };
        let (mut core, request) = fixture();
        let original = core.presentation_operations.producer.clone().unwrap();
        let transfer = request.transfer.clone();
        // Contact requires both exact memberships even though cancellation
        // does not mount or claim the destination in this document.
        let session = core.data.session.as_mut().unwrap();
        let mut destination = session.presentations[0].clone();
        destination.presentation_id = transfer.destination.presentation_id.clone();
        destination.thread_id = Some(transfer.destination.thread_id.clone());
        destination.chain_root_id = Some(transfer.destination.chain_root_id.clone());
        session.presentations.push(destination);
        assert!(
            core.dispatch_intent(RyeOsUiIntent::PrepareTransfer {
                transfer: transfer.clone(),
                export: request.export.clone(),
            })
            .is_empty()
        );
        let prefix = test_support::pending_prefix(&core);
        assert!(core.acknowledge_seat_prefix(&original, prefix).is_empty());
        let prefix = test_support::pending_prefix(&core);
        assert!(core.acknowledge_seat_prefix(&original, prefix).is_empty());
        // Model the already-authenticated successor control in this pure Core
        // test. Original transfer provenance remains unchanged; the server
        // still rechecks the actual current signed control and paired cut.
        core.presentation_operations
            .producer
            .as_mut()
            .unwrap()
            .producer_incarnation = "d".repeat(64);
        let effects = core.dispatch_intent(RyeOsUiIntent::ContactTransfer {
            transfer: transfer.clone(),
            action: UiTransferContactIntent::CancelFreeze,
        });
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0].kind,
            RyeOsEffectKind::ContactTransfer { request: crate::ui::effect::UiTransferContactRequest::CancelFreeze { transfer: retained }, .. }
                if retained == &transfer));
        assert!(
            core.dispatch_intent(RyeOsUiIntent::ContactTransfer {
                transfer,
                action: UiTransferContactIntent::CancelFreeze,
            })
            .is_empty()
        );
    }

    #[test]
    fn actor_move_prepare_waits_for_both_acks_and_original_signed_rows() {
        use crate::ui::{effect::RyeOsEffectKind, event::RyeOsUiIntent, seat::RetainedSeatEvent};
        use ryeos_ui_presentation_contract::transfer::TransferEvidence;
        let (mut core, request) = fixture();
        let producer = core.presentation_operations.producer.clone().unwrap();
        let identity = request.transfer.clone();
        assert!(
            core.dispatch_intent(RyeOsUiIntent::PrepareTransfer {
                transfer: identity.clone(),
                export: request.export,
            })
            .is_empty()
        );
        assert!(core.move_freeze_restricts_instance(&request.instances[0]));
        let mut prepared = core.seat.events().last().unwrap().clone();
        assert!(matches!(
            prepared.kind,
            SeatEventKind::PresentationOperationPrepared(_)
        ));
        let prepared_engine_seq = match &prepared.kind {
            SeatEventKind::PresentationOperationPrepared(operation) => {
                operation.prepared_engine_seq
            }
            _ => unreachable!(),
        };
        prepared.producer_engine_seq = Some(prepared_engine_seq);
        // Explicit synthetic authenticated-transport scope, not CAS/signature
        // qualification: original rows and coordinates are supplied together.
        let retained = |event: crate::ui::seat::SeatEvent, hash: &str| RetainedSeatEvent {
            evidence: TransferEvidence {
                event_hash: hash.repeat(32),
                chain_seq: event.seq,
            },
            event,
            thread_id: identity.source.thread_id.clone(),
            chain_root_id: identity.source.chain_root_id.clone(),
        };
        assert!(
            core.acknowledge_canonical_seat_prefix(
                &producer,
                test_support::pending_prefix(&core),
                vec![retained(prepared.clone(), "ab")]
            )
            .unwrap()
            .is_empty()
        );
        let mut completed = core.seat.events().last().unwrap().clone();
        assert!(matches!(
            completed.kind,
            SeatEventKind::PresentationOperationCompleted(_)
        ));
        completed.producer_engine_seq = Some(prepared_engine_seq + 1);
        let effects = core
            .acknowledge_canonical_seat_prefix(
                &producer,
                test_support::pending_prefix(&core),
                vec![retained(completed.clone(), "cd")],
            )
            .unwrap();
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0].kind, RyeOsEffectKind::PrepareTransfer {
            transfer, freeze: Some(links), ..
        } if transfer == &identity && links.preparation.chain_seq == prepared.seq
            && links.completion.chain_seq == completed.seq)
        );
        assert!(core.drive_presentation_operations().is_empty());
        assert!(core.move_freeze_restricts_instance(&request.instances[0]));
    }

    #[test]
    fn actor_copy_uses_exact_capture_without_freeze_and_refuses_blind_repeat() {
        use crate::ui::{effect::RyeOsEffectKind, event::RyeOsUiIntent};
        let (mut core, mut request) = fixture();
        request.export = crate::ui::transfer_capture::capture_selected_transfer_export(
            &core,
            UiTransferExportMode::Copy,
            &crate::ui::event::UiTransferSourceSelection::SurfaceViewSets {
                surface_instance_id: core.active_surface().unwrap().id().clone(),
            },
            64 * 1024,
        )
        .unwrap();
        request.transfer.export_digest = crate::ui::seat_digest::seat_payload_digest(
            &serde_json::to_value(&request.export).unwrap(),
        )
        .unwrap();
        let before = core.seat.events().len();
        let intent = RyeOsUiIntent::PrepareTransfer {
            transfer: request.transfer.clone(),
            export: request.export.clone(),
        };
        let effects = core.dispatch_intent(intent.clone());
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0].kind, RyeOsEffectKind::PrepareTransfer {
            transfer, export, freeze: None, ..
        } if transfer == &request.transfer && export == &request.export)
        );
        assert_eq!(core.seat.events().len(), before);
        assert!(!core.move_freeze_restricts_instance(&request.instances[0]));
        assert!(core.dispatch_intent(intent.clone()).is_empty());
        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: effects[0].id,
            ok: false,
            kind: crate::ui::effect::RyeOsEffectResultKind::BindingInvoked,
            data: None,
            error: Some(crate::ui::effect::RyeOsUiError::outcome_unknown(
                "transfer_contact_unknown",
                "Observe the exact retained outcome.",
            )),
        });
        assert!(core.dispatch_intent(intent).is_empty());
        assert_eq!(core.seat.events().len(), before);
    }

    #[test]
    fn uncertain_transfer_contact_is_not_silent_or_retry_authority() {
        for (ok, kind) in [
            (true, crate::ui::effect::RyeOsEffectResultKind::BrowserOnly),
            (
                false,
                crate::ui::effect::RyeOsEffectResultKind::BindingInvoked,
            ),
        ] {
            let (mut core, mut request) = fixture();
            request.export = crate::ui::transfer_capture::capture_selected_transfer_export(
                &core,
                UiTransferExportMode::Copy,
                &crate::ui::event::UiTransferSourceSelection::SurfaceViewSets {
                    surface_instance_id: core.active_surface().unwrap().id().clone(),
                },
                64 * 1024,
            )
            .unwrap();
            request.transfer.export_digest = crate::ui::seat_digest::seat_payload_digest(
                &serde_json::to_value(&request.export).unwrap(),
            )
            .unwrap();
            let intent = crate::ui::event::RyeOsUiIntent::PrepareTransfer {
                transfer: request.transfer,
                export: request.export,
            };
            let effects = core.dispatch_intent(intent.clone());
            assert_eq!(effects.len(), 1);
            let before = core.seat.events().to_vec();
            assert!(
                core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
                    id: effects[0].id,
                    ok,
                    kind,
                    data: Some(serde_json::json!({"activated": true})),
                    error: None,
                })
                .is_empty()
            );
            assert!(core.ui.effect_failures.contains_key(&effects[0].id));
            assert_eq!(
                core.ui.effect_failures[&effects[0].id].outcome,
                crate::ui::effect::RyeOsEffectOutcome::Unknown,
            );
            assert_eq!(core.seat.events(), before.as_slice());
            assert!(core.dispatch_intent(intent).is_empty());
        }
    }

    #[test]
    fn canonical_ack_adopts_real_native_gaps_without_changing_preparation_identity() {
        use crate::ui::seat::RetainedSeatEvent;
        use ryeos_ui_presentation_contract::transfer::TransferEvidence;
        let (mut core, request) = fixture();
        let producer = core.presentation_operations.producer.clone().unwrap();
        let operation = core.prepare_move_freeze(request).unwrap();
        let captured = test_support::pending_prefix(&core);
        let mut original = captured[0].clone();
        let native_preparation = original.seq + 7; // retained control/receipt gap
        original.seq = native_preparation;
        original.producer_engine_seq = Some(operation.prepared_engine_seq);
        let retained = |event: crate::ui::seat::SeatEvent, hash: &str| RetainedSeatEvent {
            evidence: TransferEvidence {
                event_hash: hash.repeat(32),
                chain_seq: event.seq,
            },
            event,
            thread_id: producer.thread_id.clone(),
            chain_root_id: operation
                .move_freeze()
                .unwrap()
                .transfer
                .source
                .chain_root_id
                .clone(),
        };
        assert!(
            core.acknowledge_canonical_seat_prefix(
                &producer,
                captured,
                vec![retained(original.clone(), "ab")]
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            core.presentation_operations.prepared[&operation.operation_id],
            native_preparation
        );
        assert_eq!(
            core.seat
                .evidence_at_sequence(native_preparation)
                .unwrap()
                .event_hash,
            "ab".repeat(32)
        );
        let captured = test_support::pending_prefix(&core);
        assert_eq!(captured.len(), 1);
        let mut completion = captured[0].clone();
        let native_completion = native_preparation + 3; // receipt/control gap
        completion.seq = native_completion;
        completion.producer_engine_seq = Some(operation.prepared_engine_seq + 1);
        // A later local facet remains a proof-free provisional tail and keeps
        // its ordinal/body when canonical positions advance underneath it.
        core.seat
            .append_facet("unrelated.local", serde_json::json!("keep"));
        let effects = core
            .acknowledge_canonical_seat_prefix(
                &producer,
                captured,
                vec![retained(completion, "cd")],
            )
            .unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(
            core.presentation_operations.completed[&operation.operation_id],
            native_completion
        );
        assert!(
            matches!(&effects[0].kind, crate::ui::effect::RyeOsEffectKind::PrepareTransfer {
            transfer, freeze: Some(links), ..
        } if transfer == &operation.move_freeze().unwrap().transfer
            && links.preparation.chain_seq == native_preparation && links.completion.chain_seq == native_completion)
        );
        assert!(matches!(&core.seat.events().last().unwrap().kind,
            SeatEventKind::Facet { key, value } if key == "unrelated.local" && value == "keep"));
        assert_eq!(
            core.seat.events().last().unwrap().seq,
            native_completion + 1
        );
        assert_eq!(core.seat.events().last().unwrap().producer_engine_seq, None);
        assert_eq!(
            core.seat
                .event_at_sequence(native_preparation)
                .unwrap()
                .kind,
            SeatEventKind::PresentationOperationPrepared(operation.clone())
        );
        assert_eq!(
            core.seat
                .event_at_sequence(native_preparation)
                .unwrap()
                .producer_engine_seq,
            Some(operation.prepared_engine_seq)
        );
    }

    #[test]
    fn canonical_ack_refuses_changed_body_engine_owner_and_evidence_atomically() {
        use crate::ui::seat::RetainedSeatEvent;
        use ryeos_ui_presentation_contract::transfer::TransferEvidence;
        let (mut core, request) = fixture();
        let producer = core.presentation_operations.producer.clone().unwrap();
        let operation = core.prepare_move_freeze(request).unwrap();
        let captured = test_support::pending_prefix(&core);
        let mut event = captured[0].clone();
        event.seq += 7;
        event.producer_engine_seq = Some(operation.prepared_engine_seq);
        let good = RetainedSeatEvent {
            evidence: TransferEvidence {
                event_hash: "ab".repeat(32),
                chain_seq: event.seq,
            },
            event,
            thread_id: producer.thread_id.clone(),
            chain_root_id: operation
                .move_freeze()
                .unwrap()
                .transfer
                .source
                .chain_root_id
                .clone(),
        };
        let mut variants = Vec::new();
        let mut bad = good.clone();
        bad.event.producer_engine_seq = Some(operation.prepared_engine_seq + 1);
        variants.push(bad);
        let mut bad = good.clone();
        bad.event.kind = SeatEventKind::Facet {
            key: "changed".into(),
            value: serde_json::Value::Null,
        };
        variants.push(bad);
        let mut bad = good.clone();
        bad.thread_id = "another-thread".into();
        variants.push(bad);
        let mut bad = good.clone();
        bad.evidence.chain_seq += 1;
        variants.push(bad);
        let mut bad = good;
        bad.evidence.event_hash = "not-a-hash".into();
        variants.push(bad);
        for bad in variants {
            let before = core.seat.events().to_vec();
            let prefix = core.presentation_operations.acknowledged_prefix_len;
            assert!(
                core.acknowledge_canonical_seat_prefix(&producer, captured.clone(), vec![bad])
                    .is_err()
            );
            assert_eq!(core.seat.events(), before);
            assert_eq!(core.presentation_operations.acknowledged_prefix_len, prefix);
            assert!(core.presentation_operations.completed.is_empty());
        }
    }

    fn fixture_with_session(
        session: crate::ui::model::BrowserSession,
    ) -> (RyeOsCore, UiMoveFreezeRequest) {
        let mut core = test_support::core(session);
        let producer = test_support::claim_fixture_producer(&mut core);
        let surface = core.active_surface().unwrap();
        let set = surface.active_view_set().unwrap();
        let set_id = set.id;
        let source_owner = crate::ids::UiViewMountOwner::Surface {
            presentation_id: core.presentation.as_ref().unwrap().id().clone(),
            surface_instance_id: surface.id().clone(),
        };
        let binding = surface.binding().clone();
        let replay = core.acknowledged_composition_replay().unwrap();
        let canonical_template_id = replay.template_ids.get(&set_id).unwrap().clone();
        let seed =
            crate::ui::layout_preferences::capture_view_set(set, canonical_template_id).unwrap();
        let mounts = set
            .root
            .as_ref()
            .unwrap()
            .tile_ids()
            .into_iter()
            .map(|tile_id| {
                let tile = &set.tiles[&tile_id];
                let mounted = core
                    .presentation
                    .as_ref()
                    .unwrap()
                    .mount(&tile.instance_key)
                    .unwrap();
                UiTransferMountExport {
                    source_instance: tile.instance_key.clone(),
                    view_ref: mounted.view_ref().into(),
                    binding: mounted.binding().clone(),
                    local: mounted.local().clone(),
                    subject: core
                        .selection_attachment_for_instance(&tile.instance_key)
                        .unwrap(),
                    route: crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
                    current_label: None,
                    drafts: Vec::new(),
                    lens_frames: Vec::new(),
                }
            })
            .collect();
        let requested = UiTransferExport {
            schema: 1,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::ViewSets {
                active_set: Some("frozen".into()),
                sets: vec![UiTransferSetExport {
                    handle: "frozen".into(),
                    source_view_set_id: set_id,
                    source_owner,
                    binding,
                    seed,
                    focused_view: Some(0),
                    mounts,
                }],
            },
        };
        let export = crate::ui::transfer_capture::capture_transfer_export_from_core(
            &core,
            &requested,
            64 * 1024,
        )
        .unwrap();
        let export_digest =
            crate::ui::seat_digest::seat_payload_digest(&serde_json::to_value(&export).unwrap())
                .unwrap();
        let request = UiMoveFreezeRequest {
            schema: 1,
            operation_id: String::new(),
            transfer: TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: TransferSeat {
                    presentation_id: producer.presentation_id,
                    thread_id: producer.thread_id,
                    chain_root_id: "fixture-chain".into(),
                    producer_incarnation: producer.producer_incarnation,
                    revision: core
                        .acknowledged_composition_cache
                        .as_ref()
                        .unwrap()
                        .1
                        .revision,
                },
                destination: TransferSeat {
                    presentation_id: "44444444-4444-4444-8444-444444444444".into(),
                    thread_id: "destination".into(),
                    chain_root_id: "destination".into(),
                    producer_incarnation: "cd".repeat(32),
                    revision: 0,
                },
                export_digest,
            },
            root_binding: core.presentation.as_ref().unwrap().root_binding().clone(),
            instances: export
                .mounts()
                .into_iter()
                .map(|mount| mount.source_instance.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            view_sets: vec![set_id],
            export,
        };
        (core, request)
    }

    fn input_source_fixture() -> (RyeOsCore, UiMoveFreezeRequest, RyeOsViewInstanceKey) {
        let view = serde_json::json!({"widget":"text","input":{"id":"line","submit":"route"},
            "sources":{"default":{"ref":"service:test/read","collection":"rows"}}});
        input_source_fixture_with_view(view)
    }

    fn input_source_fixture_with_view(
        view: serde_json::Value,
    ) -> (RyeOsCore, UiMoveFreezeRequest, RyeOsViewInstanceKey) {
        let shared = serde_json::json!({"widget":"text","input":{"id":"line","submit":"route"},
            "sources":{"default":{"ref":"service:test/read","collection":"rows"}}});
        let session = test_support::session_with_surface_and_docks(
            serde_json::json!({"name":"freeze-input-source","tiles":["view:test/frozen"],
                "views":{"view:test/frozen":view,"view:ryeos/input":shared}}),
            serde_json::json!({"bottom":{"content":"view:ryeos/input","open":true,"size":7}}),
        );
        let (mut core, mut request) = fixture_with_session(session);
        let frozen = request.instances[0].clone();
        core.focused_input = Some(frozen);
        core.focused_input_buffer_mut()
            .unwrap()
            .set_text("keep frozen draft".into(), 4);
        // Choose this fixture's commitment AFTER establishing its private draft;
        // production prepare_move_freeze must refuse, never rebase, a mismatch.
        request.export = crate::ui::transfer_capture::capture_transfer_export_from_core(
            &core,
            &request.export,
            64 * 1024,
        )
        .unwrap();
        request.transfer.export_digest = crate::ui::seat_digest::seat_payload_digest(
            &serde_json::to_value(&request.export).unwrap(),
        )
        .unwrap();
        let unaffected = core.presentation.as_ref().unwrap().docks()["bottom"]
            .view()
            .instance_key()
            .clone();
        (core, request, unaffected)
    }

    #[test]
    fn exact_frozen_input_refuses_addressed_edits_clear_submit_and_cycles() {
        use crate::ui::event::RyeOsInputAction;
        let (mut core, request, unaffected) = input_source_fixture();
        let (key, _) = core.focused_input_instance().unwrap();
        let address = core.input_address_for(key.clone()).unwrap();
        core.prepare_move_freeze(request).unwrap();
        let events_before = core.seat.events().to_vec();
        let effects_before = core.pending_effects.len();
        let draft = key.storage_key();
        assert!(core.focused_input_buffer_mut().is_none());
        assert!(core.submit_focused_input(false).is_empty());
        assert!(core.cycle_input_target(true).is_empty());
        assert!(core.cycle_filter_field(true).is_empty());
        assert!(core.effects_for_focused_feeds().is_empty());
        assert!(!core.clear_addressed_input(&address));
        core.focused_input = Some(unaffected.clone());
        assert!(
            core.dispatch_addressed_input(
                address,
                RyeOsInputAction::SetText {
                    text: "must not replace frozen draft".into(),
                    cursor: 0,
                }
            )
            .is_empty()
        );
        assert_eq!(core.focused_input, Some(unaffected));
        assert_eq!(core.input_buffers[&draft].text, "keep frozen draft");
        assert_eq!(core.input_buffers[&draft].cursor, 4);
        assert_eq!(core.seat.events(), events_before.as_slice());
        assert_eq!(core.pending_effects.len(), effects_before);
    }

    #[test]
    fn frozen_multi_field_filter_preserves_field_and_draft_that_normally_cycle() {
        let view = serde_json::json!({"widget":"rows", "input":{"id":"filter","feeds":{"fields":[
            {"param":"query","label":"Query"}, {"param":"status","label":"Status"}
        ]}}, "sources":{"default":{"ref":"service:test/read","collection":"rows"}}});
        let (mut ordinary, _, _) = input_source_fixture_with_view(view.clone());
        ordinary.cycle_filter_field(true);
        let (ordinary_key, _) = ordinary.focused_input_instance().unwrap();
        assert_eq!(
            ordinary.input_buffers[&ordinary_key.storage_key()].filter_field,
            1
        );
        assert!(
            ordinary.input_buffers[&ordinary_key.storage_key()]
                .text
                .is_empty()
        );
        let (mut frozen, request, _) = input_source_fixture_with_view(view);
        let (key, _) = frozen.focused_input_instance().unwrap();
        frozen.prepare_move_freeze(request).unwrap();
        assert!(frozen.cycle_filter_field(true).is_empty());
        assert_eq!(frozen.input_buffers[&key.storage_key()].filter_field, 0);
        assert_eq!(
            frozen.input_buffers[&key.storage_key()].text,
            "keep frozen draft"
        );
    }

    #[test]
    fn unrelated_shared_input_remains_editable_during_exact_surface_freeze() {
        use crate::ui::event::RyeOsInputAction;
        let (mut core, request, unaffected) = input_source_fixture();
        core.prepare_move_freeze(request).unwrap();
        core.focused_input = Some(unaffected.clone());
        core.focused_input_buffer_mut()
            .expect("unaffected input is not globally frozen")
            .set_text("unaffected draft".into(), 5);
        let (key, _) = core.focused_input_instance().unwrap();
        let address = core.input_address_for(key.clone()).unwrap();
        assert!(
            core.dispatch_addressed_input(
                address.clone(),
                RyeOsInputAction::SetText {
                    text: "updated unaffected draft".into(),
                    cursor: 8,
                }
            )
            .is_empty()
        );
        assert_eq!(
            core.input_buffers[&key.storage_key()].text,
            "updated unaffected draft"
        );
        assert!(core.clear_addressed_input(&address));
        assert!(core.input_buffers[&key.storage_key()].text.is_empty());
        assert!(!core.move_freeze_restricts_instance(&unaffected));
    }

    #[test]
    fn frozen_deferred_observation_preserves_parked_request_error_and_epoch() {
        use crate::ui::model::DeferredSourceFetch;
        use crate::ui::source_key::RyeOsSourceInstanceKey;
        let (mut core, request, unaffected) = input_source_fixture();
        let frozen_key =
            RyeOsSourceInstanceKey::named(request.instances[0].clone(), "default").encode();
        let unaffected_key = RyeOsSourceInstanceKey::named(unaffected, "default").encode();
        for (key, view_ref) in [
            (&frozen_key, "view:test/frozen"),
            (&unaffected_key, "view:ryeos/input"),
        ] {
            core.deferred_source_fetches.insert(
                key.clone(),
                DeferredSourceFetch {
                    view_ref: view_ref.into(),
                    channel: "default".into(),
                    params: serde_json::json!({"query":"parked observation"}),
                },
            );
            core.data.source_errors.insert(
                key.clone(),
                crate::ui::effect::RyeOsUiError::definite(
                    "prior_observation_failed",
                    "Keep the last source error while frozen.",
                ),
            );
            core.data.source_epoch.insert(key.clone(), 17);
        }
        core.next_effect_id = 17;
        core.prepare_move_freeze(request).unwrap();
        let frozen_error = core.data.source_errors[&frozen_key].clone();
        let pending_before = core.pending_effects.len();
        assert!(core.release_deferred_source_fetch(&frozen_key).is_none());
        let parked = &core.deferred_source_fetches[&frozen_key];
        assert_eq!(parked.view_ref, "view:test/frozen");
        assert_eq!(parked.channel, "default");
        assert_eq!(
            parked.params,
            serde_json::json!({"query":"parked observation"})
        );
        assert_eq!(core.data.source_errors[&frozen_key], frozen_error);
        assert_eq!(core.data.source_epoch[&frozen_key], 17);
        assert_eq!(core.pending_effects.len(), pending_before);
        let released = core
            .release_deferred_source_fetch(&unaffected_key)
            .expect("unaffected observation may reconcile normally");
        assert!(
            matches!(&released.kind, crate::ui::effect::RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &unaffected_key)
        );
        assert!(!core.deferred_source_fetches.contains_key(&unaffected_key));
        assert!(!core.data.source_errors.contains_key(&unaffected_key));
        assert_eq!(core.data.source_epoch[&unaffected_key], released.id);
        assert!(released.id > 17);
        assert!(core.deferred_source_fetches.contains_key(&frozen_key));
        assert_eq!(core.data.source_errors[&frozen_key], frozen_error);
        assert_eq!(core.data.source_epoch[&frozen_key], 17);
    }

    #[test]
    fn frozen_source_launch_is_inert_while_unaffected_shared_source_can_launch() {
        let (mut core, request, unaffected) = input_source_fixture();
        let frozen = request.instances[0].clone();
        core.prepare_move_freeze(request).unwrap();
        let pending_before = core.pending_effects.len();
        let epochs_before = core.data.source_epoch.clone();
        assert!(
            core.emit_fetch_source_for_instance(frozen, "view:test/frozen")
                .is_empty()
        );
        assert_eq!(core.pending_effects.len(), pending_before);
        assert_eq!(core.data.source_epoch, epochs_before);
        let effects = core.emit_fetch_source_for_instance(unaffected, "view:ryeos/input");
        assert!(
            !effects.is_empty(),
            "unaffected source is not globally frozen"
        );
        assert!(effects.iter().all(|effect| matches!(
            &effect.kind,
            crate::ui::effect::RyeOsEffectKind::FetchSource { .. }
        )));
    }

    #[test]
    fn fresh_initialized_ordinary_core_without_transfers_is_not_write_fenced() {
        // The ordinary path includes its exact retained initialization. A raw
        // pre-initialization constructor is deliberately fail-closed, not a
        // usable presentation with evidence that no transfer exists.
        let core = test_support::core(test_support::session());
        let sets = core.active_surface_view_sets().unwrap();
        assert!(!sets.is_empty());
        for set in sets {
            assert!(!core.move_freeze_restricts_view_set(set.id));
            for tile in set.tiles.values() {
                assert!(!core.move_freeze_restricts_instance(&tile.instance_key));
            }
        }
    }

    #[test]
    fn synchronous_freeze_is_write_ahead_and_restricts_exact_unacknowledged_inventory() {
        let (mut core, request) = fixture();
        let before = core.seat.events().len();
        let instance = request.instances[0].clone();
        let set = request.view_sets[0];
        let revision = request.transfer.source.revision;
        assert!(!core.move_freeze_restricts_instance(&instance));
        let operation = core.prepare_move_freeze(request.clone()).unwrap();
        assert_eq!(core.seat.events().len(), before + 1);
        assert!(
            matches!(&core.seat.events()[before].kind, SeatEventKind::PresentationOperationPrepared(retained) if retained == &operation)
        );
        assert_eq!(operation.move_freeze().unwrap().transfer, request.transfer);
        assert_eq!(
            core.acknowledged_composition_cache
                .as_ref()
                .unwrap()
                .1
                .revision,
            revision
        );
        assert!(core.move_freeze_restricts_instance(&instance));
        assert!(core.move_freeze_restricts_view_set(set));
        assert!(!core.move_freeze_restricts_view_set(ViewSetId::new(u64::MAX)));
        assert!(core.prepare_move_freeze(request).is_err());
        assert_eq!(core.seat.events().len(), before + 1);
    }

    #[test]
    fn move_snapshot_requires_the_exact_ordered_return_stack() {
        let (core, mut request) = fixture();
        let mut replay = core
            .acknowledged_composition_cache
            .as_ref()
            .unwrap()
            .1
            .clone();
        let mount = request.export.mounts()[0];
        let instance = mount.source_instance.clone();
        let frames = vec![
            crate::ui::composition_lens::RetainedLensFrame {
                binding: mount.binding.clone(),
                view_ref: mount.view_ref.clone(),
                attachment: mount.subject.clone(),
                input_route: None,
                label: Some("First".into()),
            },
            crate::ui::composition_lens::RetainedLensFrame {
                binding: mount.binding.clone(),
                view_ref: mount.view_ref.clone(),
                attachment: mount.subject.clone(),
                input_route: None,
                label: Some("Second".into()),
            },
        ];
        replay.lens_frames.insert(instance, frames.clone());
        let crate::ui::transfer_export::UiTransferExportContent::ViewSets { sets, .. } =
            &mut request.export.content
        else {
            unreachable!()
        };
        sets[0].mounts[0].lens_frames = frames;
        request.transfer.export_digest = crate::ui::seat_digest::seat_payload_digest(
            &serde_json::to_value(&request.export).unwrap(),
        )
        .unwrap();
        request
            .export
            .validate_snapshot(&request.transfer, &replay, u64::MAX)
            .unwrap();
        let crate::ui::transfer_export::UiTransferExportContent::ViewSets { sets, .. } =
            &mut request.export.content
        else {
            unreachable!()
        };
        sets[0].mounts[0].lens_frames.reverse();
        request.transfer.export_digest = crate::ui::seat_digest::seat_payload_digest(
            &serde_json::to_value(&request.export).unwrap(),
        )
        .unwrap();
        let error = request
            .export
            .validate_snapshot(&request.transfer, &replay, u64::MAX)
            .unwrap_err();
        assert!(error.contains("exact canonical source state"));
    }

    #[test]
    fn authenticated_cancellation_requires_exact_settled_freeze_positions() {
        use ryeos_ui_presentation_contract::{
            move_freeze_lifecycle::{
                MoveFreezeCancellation, MoveFreezeCancellationEvidence,
                MoveFreezeCancellationWriter,
            },
            transfer::TransferEvidence,
        };
        let (mut core, request) = fixture();
        let operation = core.prepare_move_freeze(request.clone()).unwrap();
        let preparation = core.seat.events().last().unwrap().clone();
        let completion = crate::ui::seat::SeatEvent {
            producer_engine_seq: Some(operation.prepared_engine_seq + 1),
            seq: preparation.seq + 1,
            kind: SeatEventKind::PresentationOperationCompleted(UiCompletedPresentationOperation {
                operation_id: operation.operation_id.clone(),
                result: serde_json::to_value(operation.move_freeze().unwrap().completion())
                    .unwrap(),
            }),
        };
        let cancellation = MoveFreezeCancellation::new(
            request.transfer.clone(),
            MoveFreezeCancellationEvidence {
                preparation: TransferEvidence {
                    event_hash: "ab".repeat(32),
                    chain_seq: preparation.seq,
                },
                completion: Some(TransferEvidence {
                    event_hash: "cd".repeat(32),
                    chain_seq: completion.seq,
                }),
            },
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation: operation.producer.producer_incarnation.clone(),
                next_engine_seq: operation.prepared_engine_seq + 2,
            },
        )
        .unwrap();
        let mut replay = core
            .acknowledged_composition_cache
            .as_ref()
            .unwrap()
            .1
            .clone();
        replay.apply_ordered_event(&preparation).unwrap();
        assert!(
            replay
                .apply_authenticated_move_freeze_cancellation(
                    &cancellation,
                    &preparation,
                    Some(&completion),
                    completion.seq + 1,
                )
                .is_err()
        );
        assert!(replay.move_freezes.contains_key(&operation.operation_id));
        let mut pending_replay = replay.clone();
        let pending_cancellation = MoveFreezeCancellation::new(
            cancellation.transfer.clone(),
            MoveFreezeCancellationEvidence {
                preparation: cancellation.freeze.preparation.clone(),
                completion: None,
            },
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation: operation.producer.producer_incarnation.clone(),
                next_engine_seq: operation.prepared_engine_seq + 1,
            },
        )
        .unwrap();
        let pending_revision = pending_replay.revision;
        pending_replay
            .apply_authenticated_move_freeze_cancellation(
                &pending_cancellation,
                &preparation,
                None,
                preparation.seq + 1,
            )
            .unwrap();
        assert!(!pending_replay.pending.contains_key(&operation.operation_id));
        assert!(
            !pending_replay
                .move_freezes
                .contains_key(&operation.operation_id)
        );
        assert!(pending_replay.settled.contains(&operation.operation_id));
        assert_eq!(pending_replay.revision, pending_revision);
        let mut late_completion = completion.clone();
        late_completion.seq = preparation.seq + 2;
        assert!(
            pending_replay
                .apply_ordered_event(&late_completion)
                .is_err()
        );
        replay.apply_ordered_event(&completion).unwrap();
        let mut wrong_completion = completion.clone();
        wrong_completion.seq += 1;
        assert!(
            replay
                .apply_authenticated_move_freeze_cancellation(
                    &cancellation,
                    &preparation,
                    Some(&wrong_completion),
                    completion.seq + 2,
                )
                .is_err()
        );
        let mut wrong_transfer = cancellation.transfer.clone();
        wrong_transfer.destination.revision += 1;
        let wrong_identity = MoveFreezeCancellation::new(
            wrong_transfer,
            cancellation.freeze.clone(),
            cancellation.writer.clone(),
        )
        .unwrap();
        wrong_identity.validate_shape().unwrap();
        assert!(
            replay
                .apply_authenticated_move_freeze_cancellation(
                    &wrong_identity,
                    &preparation,
                    Some(&completion),
                    completion.seq + 1,
                )
                .is_err()
        );
        assert!(replay.move_freezes.contains_key(&operation.operation_id));
        let revision = replay.revision;
        let next_mount_id = replay.next_mount_id;
        replay
            .apply_authenticated_move_freeze_cancellation(
                &cancellation,
                &preparation,
                Some(&completion),
                completion.seq + 1,
            )
            .unwrap();
        assert!(!replay.move_freezes.contains_key(&operation.operation_id));
        assert_eq!(replay.revision, revision);
        assert_eq!(replay.next_mount_id, next_mount_id);
        assert!(
            replay
                .apply_authenticated_move_freeze_cancellation(
                    &cancellation,
                    &preparation,
                    Some(&completion),
                    completion.seq + 2,
                )
                .is_err()
        );
    }

    #[test]
    fn replay_refuses_self_declared_freeze_bounds_before_installing_obligation() {
        let (mut core, request) = fixture();
        let mut operation = core.prepare_move_freeze(request).unwrap();
        let mut replay = core
            .acknowledged_composition_cache
            .as_ref()
            .unwrap()
            .1
            .clone();
        operation.request_bounds.max_seat_append_bytes = operation
            .request_bounds
            .max_seat_append_bytes
            .checked_add(1)
            .unwrap();
        operation.assign_identity().unwrap();
        operation.validate().unwrap();
        let event = crate::ui::seat::SeatEvent {
            producer_engine_seq: None,
            seq: core.seat.events().last().unwrap().seq,
            kind: SeatEventKind::PresentationOperationPrepared(operation),
        };
        let error = replay.apply_ordered_event(&event).unwrap_err();
        assert!(error.contains("retained root request bounds"));
        assert!(replay.move_freezes.is_empty());
        assert!(replay.pending.is_empty());
    }

    #[test]
    fn relevant_pending_binding_release_refuses_without_installing_restriction() {
        let (mut core, request) = fixture();
        let before = core.seat.events().len();
        let binding = request.export.mounts()[0].binding.clone();
        core.pending_effects.insert(
            99,
            crate::ui::effect::RyeOsEffectKind::ReleaseBindingAttachment {
                binding_attachment_id: binding.binding_attachment_id,
                binding_generation: binding.binding_generation,
                binding_digest: binding.binding_digest,
            },
        );
        let error = core.prepare_move_freeze(request.clone()).unwrap_err();
        assert!(error.contains("pending local effect"));
        assert_eq!(core.seat.events().len(), before);
        assert!(!core.move_freeze_restricts_instance(&request.instances[0]));
    }

    #[test]
    fn stale_source_identity_and_commitment_refuse_without_write_ahead() {
        for field in ["revision", "incarnation", "root", "digest"] {
            let (mut core, mut request) = fixture();
            let before = core.seat.events().len();
            match field {
                "revision" => request.transfer.source.revision += 1,
                "incarnation" => request.transfer.source.producer_incarnation = "ef".repeat(32),
                "root" => request.transfer.source.chain_root_id = "other-root".into(),
                "digest" => request.transfer.export_digest = "ff".repeat(32),
                _ => unreachable!(),
            }
            assert!(core.prepare_move_freeze(request).is_err(), "{field}");
            assert_eq!(core.seat.events().len(), before, "{field}");
        }
    }

    #[test]
    fn restriction_queries_require_checked_cache_and_include_retained_obligations() {
        let (mut core, request) = fixture();
        let instance = request.instances[0].clone();
        let set = request.view_sets[0];
        core.acknowledged_composition_cache
            .as_mut()
            .unwrap()
            .1
            .move_freezes
            .insert("retained-query-fixture".into(), request);
        assert!(core.move_freeze_restricts_instance(&instance));
        assert!(core.move_freeze_restricts_view_set(set));
        assert!(!core.move_freeze_restricts_view_set(ViewSetId::new(u64::MAX)));
        core.acknowledged_composition_cache = None;
        assert!(core.move_freeze_restricts_instance(&instance));
        assert!(core.move_freeze_restricts_view_set(ViewSetId::new(u64::MAX)));
    }
}
