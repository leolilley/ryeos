//! Write-ahead admission and exact local-prefix acknowledgement.
//!
//! The braid owns preparation/completion; this reducer is its local projection.
//! Only signed idempotent presentation result effects enter this path. Ordinary
//! worker/service mutations are never inferred to be safe to repeat.

use super::model::{InputBufferKey, RyeOsCore};
use crate::ui::admission_operation::{
    UiCompletedPresentationOperation, UiPreparedAdmissionOperation,
    UiPreparedPresentationOperation, UiPreparedPresentationOperationBody,
    UiPresentationOperationKind,
};
use crate::ui::binding::{
    UiBindingPayload, UiBindingRequest, UiBindingRequestBounds, UiPresentationProducer,
};
use crate::ui::effect::{InvokeIntent, RyeOsEffect, RyeOsEffectKind};
use crate::ui::seat::{SeatEvent, SeatEventKind};
use crate::ui::view_model::RyeOsTone;

impl RyeOsCore {
    pub(crate) fn surface_close_available(
        &self,
        surface_id: &crate::ids::SurfaceInstanceId,
    ) -> bool {
        if !self.composition_write_available() {
            return false;
        }
        let Some(producer) = self.presentation_operations.producer.as_ref() else {
            return false;
        };
        if !self.producer_matches_membership(producer) {
            return false;
        }
        let Some(presentation) = self.presentation.as_ref() else {
            return false;
        };
        let Some(surface) = presentation.surfaces().get(surface_id) else {
            return false;
        };
        if self.any_move_freeze(|freeze| {
            let owns_surface = |owner: &crate::ids::UiViewMountOwner| {
                matches!(owner,
                    crate::ids::UiViewMountOwner::Surface { surface_instance_id, .. }
                        if surface_instance_id == surface_id)
            };
            match &freeze.export.content {
                crate::ui::transfer_export::UiTransferExportContent::ViewSets { sets, .. } => {
                    sets.iter().any(|set| owns_surface(&set.source_owner))
                }
                crate::ui::transfer_export::UiTransferExportContent::SurfaceMount {
                    mount, ..
                } => mount
                    .source_instance
                    .mount_owner()
                    .is_some_and(|owner| owns_surface(&owner)),
                crate::ui::transfer_export::UiTransferExportContent::SharedDock { .. } => false,
            }
        }) {
            return false;
        }
        // An acknowledged close removes the Surface's mounts. Refuse it while
        // any of those mounts still owns an unsent input buffer, just as tile
        // and tab removal do. Shared mounts belong to the presentation, not
        // this Surface, and must not veto its close.
        if self.input_buffers.iter().any(|(key, input)| {
            !input.text.is_empty()
                && (surface
                    .view_sets()
                    .iter()
                    .flat_map(|set| set.tiles.values())
                    .any(|tile| InputBufferKey::storage_key_belongs_to(key, &tile.instance_key))
                    || presentation.dialogs().values().any(|dialog| {
                        dialog.surface_owner() == Some(surface_id)
                            && InputBufferKey::storage_key_belongs_to(
                                key,
                                dialog.view().instance_key(),
                            )
                    }))
        }) {
            return false;
        }
        let Some(suffix) = self
            .seat
            .events()
            .get(self.presentation_operations.acknowledged_prefix_len..)
        else {
            return false;
        };
        !suffix.iter().any(|event| {
            matches!(&event.kind, SeatEventKind::SurfaceClosed(closure) if closure.surface_instance_id == surface_id.as_str() && &closure.binding == surface.binding())
        })
    }

    pub fn request_surface_close(
        &mut self,
        surface_id: &crate::ids::SurfaceInstanceId,
    ) -> Vec<RyeOsEffect> {
        if !self.surface_close_available(surface_id) {
            return Vec::new();
        }
        let Some(producer) = self.presentation_operations.producer.as_ref() else {
            return Vec::new();
        };
        if !self.producer_matches_membership(producer) {
            return Vec::new();
        }
        let Some(membership) = self.data.session.as_ref().and_then(|session| {
            session
                .presentations
                .iter()
                .find(|entry| entry.presentation_id == producer.presentation_id)
        }) else {
            return Vec::new();
        };
        let Some(surface) = membership
            .surfaces
            .iter()
            .find(|surface| surface.surface_instance_id == surface_id.as_str())
        else {
            return Vec::new();
        };
        let closure = crate::ui::seat::UiSurfaceClosure {
            presentation_id: producer.presentation_id.clone(),
            surface_instance_id: surface.surface_instance_id.clone(),
            binding: surface.binding.clone(),
        };
        if closure.validate().is_err() {
            return Vec::new();
        }
        let Some(root) = self
            .presentation
            .as_ref()
            .and_then(|p| self.binding_attachment(&p.root_binding().binding_attachment_id))
        else {
            return Vec::new();
        };
        if !crate::ui::admission_operation::single_seat_append_fits(
            producer,
            crate::ui::seat::SURFACE_CLOSED_EVENT,
            &closure,
            root.binding_request_bounds.max_seat_append_bytes,
        ) {
            return Vec::new();
        }
        let Some(suffix) = self
            .seat
            .events()
            .get(self.presentation_operations.acknowledged_prefix_len..)
        else {
            return Vec::new();
        };
        if suffix.iter()
            .any(|event| matches!(&event.kind, SeatEventKind::SurfaceClosed(previous) if previous == &closure)) { return Vec::new(); }
        self.seat.append(SeatEventKind::SurfaceClosed(closure));
        self.bump_generation();
        Vec::new()
    }

    pub(crate) fn producer_matches_membership(&self, producer: &UiPresentationProducer) -> bool {
        let Some(presentation) = self.presentation.as_ref() else {
            return false;
        };
        let Some(session) = self.data.session.as_ref() else {
            return false;
        };
        producer.presentation_id == presentation.id().as_str()
            && producer.producer_incarnation.len() == 64
            && producer
                .producer_incarnation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && session.presentations.iter().any(|membership| {
                membership.presentation_id == producer.presentation_id
                    && membership.thread_id.as_deref() == Some(producer.thread_id.as_str())
                    && membership.root_binding == *presentation.root_binding()
            })
    }

    pub(crate) fn presentation_producer_ready(
        &mut self,
        producer: UiPresentationProducer,
        next_engine_seq: u64,
    ) -> Vec<RyeOsEffect> {
        if self.presentation_operations.replay_blocked {
            return Vec::new();
        }
        if !self.producer_matches_membership(&producer) {
            return Vec::new();
        }
        // A running core cannot silently switch writers. Replacement boot owns
        // a freshly replayed core; an ordinary UI event cannot perform takeover.
        if self
            .presentation_operations
            .producer
            .as_ref()
            .is_some_and(|retained| retained != &producer)
        {
            return Vec::new();
        }
        if next_engine_seq == u64::MAX
            || self
                .presentation_operations
                .acknowledged_engine_frontier
                .is_some_and(|frontier| frontier != next_engine_seq)
        {
            return Vec::new();
        }
        self.presentation_operations.producer = Some(producer);
        self.presentation_operations.acknowledged_engine_frontier = Some(next_engine_seq);

        self.drive_presentation_operations()
    }

    /// A readiness event is not itself an acknowledgement: the reducer may
    /// refuse it without emitting effects. Native bootstrap must prove that
    /// the exact claimed incarnation and frontier were actually installed
    /// before starting platform work.
    pub fn require_presentation_producer_ready(
        &self,
        producer: &UiPresentationProducer,
        next_engine_seq: u64,
    ) -> Result<(), String> {
        self.require_replayed_initialization()?;
        if self.presentation_operations.producer.as_ref() != Some(producer)
            || self.presentation_operations.acknowledged_engine_frontier != Some(next_engine_seq)
            || !self.producer_matches_membership(producer)
        {
            return Err("exact presentation producer readiness was not admitted".into());
        }
        Ok(())
    }

    pub(crate) fn acknowledge_seat_prefix(
        &mut self,
        producer: &UiPresentationProducer,
        events: Vec<SeatEvent>,
    ) -> Vec<RyeOsEffect> {
        if events.is_empty()
            || self.presentation_operations.producer.as_ref() != Some(producer)
            || !self.producer_matches_membership(producer)
        {
            return Vec::new();
        }
        let start = self.presentation_operations.acknowledged_prefix_len;
        let Some(end) = start.checked_add(events.len()) else {
            return Vec::new();
        };
        if self.seat.events().get(start..end) != Some(events.as_slice()) {
            // A wire last_engine_seq, sibling receipt or matching count cannot
            // consume a prefix or release an invocation.
            return Vec::new();
        }
        let Some(first_engine_seq) = self.presentation_operations.acknowledged_engine_frontier
        else {
            return Vec::new();
        };
        let Some(next_engine_seq) = u64::try_from(events.len())
            .ok()
            .and_then(|count| first_engine_seq.checked_add(count))
        else {
            return Vec::new();
        };
        if self
            .seat
            .acknowledge_producer_interval(start, events.len(), first_engine_seq)
            .is_err()
        {
            return Vec::new();
        }
        let acknowledged = self.seat.events()[start..end].to_vec();
        if let Err(reason) = self.advance_acknowledged_composition_replay(&acknowledged) {
            self.presentation_operations.replay_blocked = true;
            self.presentation_operations.producer = None;
            self.notice(
                format!("Causal composition replay refused: {reason}"),
                RyeOsTone::Danger,
            );
            return Vec::new();
        }
        for event in &events {
            self.presentation_operations.fold_acknowledged(event);
        }
        self.presentation_operations.acknowledged_prefix_len = end;
        self.presentation_operations.acknowledged_engine_frontier = Some(next_engine_seq);

        self.drive_presentation_operations()
    }

    /// Adopt original canonical ACK rows, never a hash at a provisional local
    /// position. This is the authenticated append adapter boundary, not a new
    /// producer claim or permission to trust arbitrary event envelopes.
    pub fn acknowledge_canonical_seat_prefix(
        &mut self,
        producer: &UiPresentationProducer,
        captured: Vec<SeatEvent>,
        canonical: Vec<crate::ui::seat::RetainedSeatEvent>,
    ) -> Result<Vec<RyeOsEffect>, String> {
        if self.presentation_operations.replay_blocked
            || self.presentation_operations.producer.as_ref() != Some(producer)
            || !self.producer_matches_membership(producer)
            || captured.is_empty()
            || captured.len() != canonical.len()
        {
            return Err("canonical ACK producer or captured interval differs".into());
        }
        let start = self.presentation_operations.acknowledged_prefix_len;
        let end = start
            .checked_add(captured.len())
            .ok_or("canonical ACK prefix exhausted")?;
        if self.seat.events().get(start..end) != Some(captured.as_slice()) {
            return Err("canonical ACK differs from exact captured local prefix".into());
        }
        let first_engine = self
            .presentation_operations
            .acknowledged_engine_frontier
            .ok_or("canonical ACK producer frontier absent")?;
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
            .ok_or("canonical ACK exact membership absent")?;
        let root = member
            .chain_root_id
            .as_deref()
            .ok_or("canonical ACK root absent")?
            .to_owned();
        for (offset, (local, row)) in captured.iter().zip(&canonical).enumerate() {
            let engine = first_engine
                .checked_add(offset as u64)
                .ok_or("canonical ACK engine interval exhausted")?;
            if local.kind != row.event.kind
                || row.event.producer_engine_seq != Some(engine)
                || local.producer_engine_seq.is_some_and(|seq| seq != engine)
                || row.thread_id != producer.thread_id
                || row.chain_root_id != root
                || matches!(&local.kind, SeatEventKind::PresentationInitialized(_))
                || matches!(&local.kind, SeatEventKind::PresentationOperationPrepared(operation)
                    if operation.prepared_engine_seq != engine || operation.validate_identity().is_err())
            {
                return Err(
                    "canonical ACK changes semantic body, original engine position or owner".into(),
                );
            }
        }
        let mut candidate = self.clone();
        candidate.seat.adopt_acknowledged_positions(
            start,
            &canonical,
            &producer.thread_id,
            &root,
        )?;
        let native = canonical
            .into_iter()
            .map(|row| row.event)
            .collect::<Vec<_>>();
        let effects = candidate.acknowledge_seat_prefix(producer, native);
        if candidate.presentation_operations.acknowledged_prefix_len != end
            || candidate.presentation_operations.replay_blocked
        {
            return Err("canonical ACK causal projection refused".into());
        }
        *self = candidate;
        Ok(effects)
    }

    pub(crate) fn prepare_presentation_operation(
        &mut self,
        request: UiBindingRequest,
        request_bounds: UiBindingRequestBounds,
        purpose: UiPresentationOperationKind,
        invocation_origin: crate::ids::RyeOsViewInstanceKey,
        success_notice: Option<String>,
        surface_open_target: Option<crate::surface::opening::SurfaceOpenTarget>,
        particular_selection: Option<
            crate::ui::admission_operation::UiParticularAdmissionSelection,
        >,
    ) -> Vec<RyeOsEffect> {
        if !self.composition_write_available() {
            self.notice(
                "Another structural edit awaits exact acknowledgement.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(producer) = self.presentation_operations.producer.clone() else {
            self.notice(
                "Presentation admission is awaiting its authenticated producer.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        };
        if !self.producer_matches_membership(&producer) {
            return Vec::new();
        }
        let Some(prepared_engine_seq) = self
            .presentation_operations
            .reserved_engine_seq(self.seat.events().len())
        else {
            return Vec::new();
        };
        let Some(root_binding) = self.presentation.as_ref().map(|p| p.root_binding().clone())
        else {
            return Vec::new();
        };
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            operation_id: String::new(),
            producer,
            prepared_engine_seq,
            root_binding,
            request_bounds,
            body: UiPreparedPresentationOperationBody::Admission(UiPreparedAdmissionOperation {
                purpose,
                surface_open_target,
                particular_selection,
                invocation_origin,
                request,
                success_notice,
            }),
        };
        if operation.assign_identity().is_err()
            || !self.presentation_operation_is_live(&operation)
            || !crate::ui::admission_operation::single_seat_append_fits(
                &operation.producer,
                crate::ui::admission_operation::PREPARED_EVENT,
                &operation,
                request_bounds.max_seat_append_bytes,
            )
            || serde_json::to_vec(&operation)
                .ok()
                .is_none_or(|bytes| bytes.len() as u64 > request_bounds.max_request_bytes)
        {
            self.notice(
                "Presentation admission could not retain its exact bounded request.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        self.seat
            .append(SeatEventKind::PresentationOperationPrepared(operation));
        self.bump_generation();
        // No effect is emitted until a validated receipt acknowledges this
        // exact local prefix. The adapter's existing seat pump does the append.
        Vec::new()
    }

    pub(super) fn presentation_operation_is_live(
        &self,
        operation: &UiPreparedPresentationOperation,
    ) -> bool {
        if let Some(request) = operation.move_freeze() {
            return operation.validate().is_ok()
                && self
                    .presentation_operations
                    .producer
                    .as_ref()
                    .is_some_and(|producer| {
                        self.producer_matches_membership(producer)
                            && producer.presentation_id == request.transfer.source.presentation_id
                    })
                && self.presentation.as_ref().is_some_and(|presentation| {
                    presentation.root_binding() == &operation.root_binding
                })
                && self.acknowledged_composition_replay().is_ok_and(|replay| {
                    replay.move_freezes.get(&operation.operation_id) == Some(request)
                });
        }
        if let Some(request) = operation.composition() {
            return operation.validate().is_ok()
                && self.producer_matches_membership(&operation.producer)
                && self.presentation.as_ref().is_some_and(|presentation| {
                    presentation.root_binding() == &operation.root_binding
                        && match &request.owner {
                            crate::ui::composition_operation::CompositionOwner::Shared {
                                binding,
                            } => presentation.root_binding() == binding,
                            crate::ui::composition_operation::CompositionOwner::Surface {
                                surface_instance_id,
                                binding,
                            } => presentation
                                .surfaces()
                                .get(surface_instance_id)
                                .is_some_and(|surface| surface.binding() == binding),
                        }
                })
                && self
                    .acknowledged_composition_replay()
                    .and_then(|projection| {
                        projection.expected_composition_completion(
                            operation,
                            operation.request_bounds.max_request_bytes,
                        )
                    })
                    .is_ok();
        }
        let Some(admission) = operation.admission() else {
            return false;
        };
        if operation.validate().is_err()
            || !self.producer_matches_membership(&operation.producer)
            || self
                .presentation
                .as_ref()
                .is_none_or(|p| p.root_binding() != &operation.root_binding)
        {
            return false;
        }
        let crate::ui::binding::UiBindingCoordinate::Affordance {
            view_ref,
            affordance_id,
        } = &admission.request.coordinate
        else {
            return false;
        };
        let Some(binding) = self.binding_for_instance(&admission.invocation_origin, view_ref)
        else {
            return false;
        };
        let Some(affordance) = binding.affordances.iter().find(|entry| {
            entry.get("id").and_then(serde_json::Value::as_str) == Some(affordance_id)
        }) else {
            return false;
        };
        let UiBindingPayload::Selection { record } = &admission.request.payload else {
            return false;
        };
        let Some(super::content::AffordanceInvoke::Service { args, .. }) =
            super::content::resolve_affordance_invoke(
                affordance,
                super::content::Producer::Selection,
                &super::content::Payload::Selection(record),
            )
        else {
            return false;
        };
        let Some(declaration) = affordance.get("presentation_operation") else {
            return false;
        };
        if crate::ui::admission_operation::resolve_presentation_operation(declaration, &args).ok()
            != Some((admission.purpose, admission.surface_open_target.clone()))
        {
            return false;
        }
        if crate::ui::admission_operation::UiParticularAdmissionSelection::from_resolved_params(
            admission.purpose,
            &args,
        )
        .ok()
        .as_ref()
            != Some(&admission.particular_selection)
        {
            return false;
        }
        // Exact mount binding and subject validation are shared with ordinary
        // invokes. Replay cannot borrow a different active Surface's authority.
        self.compiled_binding_operation(
            &admission.invocation_origin,
            admission.request.coordinate.clone(),
            admission.request.payload.clone(),
        ) == Some((admission.request.clone(), operation.request_bounds))
    }

    pub(crate) fn retain_presentation_completion(
        &mut self,
        operation: &UiPreparedPresentationOperation,
        result: serde_json::Value,
    ) -> bool {
        if self
            .seat
            .events()
            .get(self.presentation_operations.acknowledged_prefix_len..)
            .is_none()
        {
            return false;
        }
        if !self.presentation_operation_is_live(operation) {
            return false;
        }
        let completion = UiCompletedPresentationOperation {
            operation_id: operation.operation_id.clone(),
            result,
        };
        if let Err(error) = completion.validate_for(operation) {
            self.notice(error, RyeOsTone::Danger);
            return false;
        }
        if self.presentation_operations.invalid.contains(&completion.operation_id)
            || self.presentation_operations.completed.contains_key(&completion.operation_id)
            || self.seat.events()[self.presentation_operations.acknowledged_prefix_len..].iter().any(|event| matches!(&event.kind, SeatEventKind::PresentationOperationCompleted(previous) if previous.operation_id == completion.operation_id)) {
            return false;
        }
        self.seat
            .append(SeatEventKind::PresentationOperationCompleted(completion));
        self.bump_generation();
        true
    }

    pub(crate) fn drive_presentation_operations(&mut self) -> Vec<RyeOsEffect> {
        self.drive_presentation_operations_with_subjects(std::collections::BTreeSet::new())
    }

    pub(crate) fn drive_presentation_operations_with_subjects(
        &mut self,
        mut changed_subjects: std::collections::BTreeSet<crate::ids::RyeOsViewInstanceKey>,
    ) -> Vec<RyeOsEffect> {
        if self.presentation_operations.replay_blocked {
            return Vec::new();
        }
        // A cached projection does not establish that the captured log still
        // contains its acknowledged prefix. Refuse before reconciliation or
        // successor effects can mutate an inconsistent candidate.
        if self
            .seat
            .events()
            .get(self.presentation_operations.acknowledged_prefix_len..)
            .is_none()
        {
            self.presentation_operations.replay_blocked = true;
            self.presentation_operations.producer = None;
            self.notice(
                "Acknowledged seat prefix is outside its captured history.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        }
        let replay = match self.acknowledged_composition_replay() {
            Ok(replay) => replay,
            Err(reason) => {
                self.presentation_operations.replay_blocked = true;
                self.presentation_operations.producer = None;
                self.notice(
                    format!("Causal composition replay refused: {reason}"),
                    RyeOsTone::Danger,
                );
                return Vec::new();
            }
        };
        changed_subjects.extend(
            match self.reconcile_acknowledged_composition_with_replay(replay.clone()) {
                Ok(changed) => changed,
                Err(reason) => {
                    self.presentation_operations.replay_blocked = true;
                    self.presentation_operations.producer = None;
                    self.notice(
                        format!("Causal composition publication refused: {reason}"),
                        RyeOsTone::Danger,
                    );
                    return Vec::new();
                }
            },
        );
        // Settled operations have already been applied in exact seat order.
        // Never replay them by UUID sort or through a second layout reducer.
        // `dirty` is the exact acknowledged interval (or the one-time cold
        // index rebuild). Walking every settled identity on each ordinary
        // edit makes a long-lived seat progressively more expensive.
        let newly_settled = self
            .presentation_operations
            .dirty
            .iter()
            .filter(|id| {
                replay.settled.contains(*id) && !self.presentation_operations.applied.contains(*id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let prepared_documents = newly_settled
            .iter()
            .filter_map(|id| {
                let seq = self.presentation_operations.completed.get(id)?;
                let SeatEventKind::PresentationOperationCompleted(completed) =
                    &self.seat.event_at_sequence(*seq)?.kind
                else {
                    return None;
                };
                let receipt = serde_json::from_value::<
                    super::super::binding::UiPreparedPresentationDocumentTransition,
                >(completed.result.get("ui_transition")?.clone())
                .ok()?;
                Some(receipt.prepared.presentation_id)
            })
            .collect::<Vec<_>>();
        let changed = newly_settled.len() != prepared_documents.len();
        self.presentation_operations.applied.extend(newly_settled);
        self.presentation_operations.dirty.clear();
        self.presentation_operations.dirty_closures.clear();
        let mut effects = if changed {
            self.refresh_view_set_sources_excluding_subjects(&changed_subjects)
        } else {
            Vec::new()
        };
        effects.extend(self.refresh_settled_subjects(&changed_subjects));
        for presentation_id in prepared_documents {
            effects.push(
                self.emit(RyeOsEffectKind::ShowPreparedPresentationDocument { presentation_id }),
            );
        }
        // Observers project and read settled evidence, but cannot settle a
        // pending edit or dispatch its executable admission.
        if self.presentation_operations.producer.is_none() {
            return effects;
        }
        effects.extend(self.drive_acknowledged_move_transfers(&replay));
        for (id, operation) in replay.pending.clone() {
            if self.presentation_operations.invalid.contains(&id)
                || self.presentation_operations.refused.contains(&id)
                || !self.presentation_operation_is_live(&operation)
            {
                continue;
            }
            if let Some(request) = operation.move_freeze() {
                match serde_json::to_value(request.completion()) {
                    Ok(result) => {
                        self.retain_presentation_completion(&operation, result);
                    }
                    Err(error) => self.notice(
                        format!("MOVE freeze completion encoding refused: {error}"),
                        RyeOsTone::Danger,
                    ),
                }
                continue;
            }
            if operation.composition().is_some() {
                if self.seat.events()[self.presentation_operations.acknowledged_prefix_len..].iter()
                    .any(|event| matches!(&event.kind, SeatEventKind::PresentationOperationCompleted(completed) if completed.operation_id == id))
                { continue; }
                match replay.expected_composition_completion(
                    &operation,
                    operation.request_bounds.max_request_bytes,
                ) {
                    Ok(completion) => {
                        self.seat
                            .append(SeatEventKind::PresentationOperationCompleted(completion));
                        self.bump_generation();
                    }
                    Err(reason) => self.notice(
                        format!("Composition settlement refused: {reason}"),
                        RyeOsTone::Danger,
                    ),
                }
            } else if self.presentation_operations.dispatched.insert(id) {
                effects.push(self.emit(RyeOsEffectKind::InvokePresentationOperation { operation }));
            }
        }
        effects
    }
}

impl UiPreparedPresentationOperation {
    pub(crate) fn as_invoke_binding(&self) -> Option<RyeOsEffectKind> {
        let admission = self.admission()?;
        Some(RyeOsEffectKind::InvokeBinding {
            request: admission.request.clone(),
            request_bounds: self.request_bounds,
            intent: InvokeIntent::Service,
            success_notice: admission.success_notice.clone(),
            invocation_origin: Some(admission.invocation_origin.clone()),
            input_origin: None,
            route_seq: None,
            ratchet_on_thread_id: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_sibling_fixture(session: &mut crate::ui::model::BrowserSession, id: &str) {
        let mut sibling = session.presentations[0].surfaces[0].clone();
        sibling.surface_instance_id = id.into();
        sibling.ordinal = 2;
        let descriptor = session
            .binding_attachments
            .iter()
            .find(|entry| entry.binding_attachment_id == sibling.binding.binding_attachment_id)
            .unwrap();
        let seeds = descriptor
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().map(|seed| seed.id.clone());
        sibling.mount_manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(session.presentations[0].presentation_id.clone())
                .unwrap(),
            crate::ids::SurfaceInstanceId::try_new(id).unwrap(),
            session.presentations[0].next_mount_id,
            seeds
                .into_iter()
                .map(|seed| (seed, sibling.binding.clone()))
                .collect(),
            active.as_deref(),
        )
        .unwrap();
        session.presentations[0].next_mount_id = sibling.mount_manifest.next_mount_id;
        session.presentations[0].surfaces.push(sibling);
    }

    #[test]
    fn surface_selector_keeps_shared_state_and_close_waits_for_exact_ack() {
        let mut session = super::super::test_support::session();
        append_sibling_fixture(&mut session, "00000000-0000-4000-8000-000000000099");
        let mut core = super::super::test_support::core(session);
        let presentation = core.presentation.as_ref().unwrap();
        let first = presentation.ordered_surfaces().next().unwrap();
        let id = first.id().clone();
        let binding = first.binding().clone();
        let shared = presentation.dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        core.open_dialog("launcher");
        // Seed actual renderer-local list state; this test exercises retaining
        // that state across Surface selection/closure, not widget activation.
        *core.mounted_local_state_mut(&shared).unwrap() =
            crate::view_set::ViewSpec::bound("view:ryeos/launcher").initial_local_state();
        let crate::view_set::ViewLocalState::GenericList { cursor, .. } =
            core.mounted_local_state_mut(&shared).unwrap()
        else {
            panic!("launcher has list-local state")
        };
        *cursor = 7;
        let shared_attachment = core
            .binding_attachment_for_instance(&shared)
            .unwrap()
            .clone();
        let shared_draft = InputBufferKey::new(
            shared.clone(),
            "view:ryeos/launcher",
            "query",
            &shared_attachment,
        )
        .storage_key();
        core.input_buffers.insert(
            shared_draft.clone(),
            crate::ui::model::RyeOsInputState {
                text: "keep shared draft".into(),
                ..Default::default()
            },
        );
        let producer = UiPresentationProducer {
            presentation_id: super::super::test_support::FIXTURE_PRESENTATION_ID.into(),
            thread_id: "fixture-seat".into(),
            producer_incarnation: "a".repeat(64),
        };
        core.presentation_producer_ready(producer.clone(), 0);
        test_support::settle_pending_prefix(&mut core, &producer);
        core.select_surface_instance(&id, &binding);
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_surface().unwrap().id(), &id);
        assert!(matches!(
            core.mounted_local_state(&shared).unwrap(),
            crate::view_set::ViewLocalState::GenericList { cursor: 7, .. }
        ));
        assert_eq!(core.input_buffers[&shared_draft].text, "keep shared draft");
        let before = core.seat.events().to_vec();
        let mut stale = binding.clone();
        stale.binding_generation += 1;
        core.select_surface_instance(&id, &stale);
        assert_eq!(core.seat.events(), before.as_slice());
        assert!(core.surface_close_available(&id));
        core.request_surface_close(&id);
        assert!(!core.surface_close_available(&id));
        assert!(
            core.presentation
                .as_ref()
                .unwrap()
                .surfaces()
                .contains_key(&id),
            "unacknowledged close must not detach"
        );
        assert_eq!(
            core.presentation_operations.producer.as_ref(),
            Some(&producer)
        );
        let prefix = test_support::pending_prefix(&core);
        core.acknowledge_seat_prefix(&producer, prefix);
        assert!(
            !core
                .presentation
                .as_ref()
                .unwrap()
                .surfaces()
                .contains_key(&id)
        );
        assert!(matches!(
            core.mounted_local_state(&shared).unwrap(),
            crate::view_set::ViewLocalState::GenericList { cursor: 7, .. }
        ));
        assert_eq!(core.input_buffers[&shared_draft].text, "keep shared draft");
        assert_eq!(
            core.presentation_operations.producer.as_ref(),
            Some(&producer)
        );
    }
    use crate::ui::effect::RyeOsEffectResultKind;
    use crate::ui::event::RyeOsEvent;
    use crate::ui::reducer::test_support;
    use serde_json::json;

    #[test]
    fn shipped_admission_openers_bind_the_prepared_creation_id() {
        let projects: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/projects/list.yaml"
        ))
        .unwrap();
        let library: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/view-sets/library.yaml"
        ))
        .unwrap();
        for (view, id, purpose, service) in [
            (
                &projects,
                "open-project",
                "surface_opening",
                "service:projects/open",
            ),
            (
                &library,
                "open-reusable-surface",
                "surface_opening",
                "service:ui/session/surfaces/open",
            ),
            (
                &library,
                "resume-particular-surface",
                "resume_particular_surface",
                "service:ui/ryeos-ui/surfaces/resume",
            ),
            (
                &library,
                "duplicate-particular-surface",
                "resume_particular_surface",
                "service:ui/ryeos-ui/surfaces/resume",
            ),
            (
                &library,
                "resume-particular",
                "resume_particular_view_set",
                "service:ui/ryeos-ui/view-sets/resume",
            ),
        ] {
            let affordance = view["affordances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|affordance| affordance["id"] == id)
                .unwrap();
            assert_eq!(affordance["presentation_operation"], purpose);
            assert_eq!(affordance["invoke"]["ref"], service);
            assert_eq!(
                affordance["invoke"]["args"]["creation_request_id"],
                "{record.creation_request_id}"
            );
            let row = json!({"id":"saved", "local_id":"project-exact", "expected_revision":1,
                "view_set_expected_revision":2,
                "surface_ref":"surface:test/example", "fresh_subjects":{}});
            let Some(crate::ui::content::AffordanceInvoke::Service { args, .. }) =
                crate::ui::content::resolve_affordance_invoke(
                    affordance,
                    crate::ui::content::Producer::Selection,
                    &crate::ui::content::Payload::Selection(&row),
                )
            else {
                panic!("shipped admission affordance must resolve before ID allocation");
            };
            assert!(args["creation_request_id"].is_null());
            let mut prepared_row = row;
            prepared_row["creation_request_id"] = json!("11111111-1111-4111-8111-111111111111");
            let Some(crate::ui::content::AffordanceInvoke::Service { args, .. }) =
                crate::ui::content::resolve_affordance_invoke(
                    affordance,
                    crate::ui::content::Producer::Selection,
                    &crate::ui::content::Payload::Selection(&prepared_row),
                )
            else {
                panic!("shipped admission affordance must resolve after ID allocation");
            };
            assert_eq!(
                args["creation_request_id"],
                "11111111-1111-4111-8111-111111111111"
            );
            if matches!(
                id,
                "resume-particular-surface" | "duplicate-particular-surface"
            ) {
                assert_eq!(args["view_set_expected_revision"], 2);
            }
        }
        let project_open = projects["affordances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|affordance| affordance["id"] == "open-project")
            .unwrap();
        assert!(
            !crate::ui::content::affordance_eligibility(
                project_open,
                &json!({"local_id":"current", "registered":false}),
            )
            .enabled
        );
        assert!(
            crate::ui::content::affordance_eligibility(
                project_open,
                &json!({"local_id":"project-exact", "registered":true}),
            )
            .enabled
        );
    }

    fn fixture() -> (
        RyeOsCore,
        UiPresentationProducer,
        crate::ids::RyeOsViewInstanceKey,
    ) {
        let mut core = test_support::core(test_support::session_with_surface(json!({
            "name":"admission-fixture", "tiles":["view:test/open"],
            "views":{"view:test/open":{"widget":"rows",
                "input":{"id":"message","submit":"route"}, "affordances":[{
                "id":"open", "producer":"selection", "presentation_operation":"surface_opening",
                "invoke":{"plane":"rye","ref":"service:test/open","args":{
                    "placement":{"kind":"new_surface"},"local_id":"{record.local_id}","creation_request_id":"{record.creation_request_id}"
                    ,"selection":{"kind":"authored","surface_ref":"surface:test/new-surface"}
                }}
            }]}}
        })));
        let producer = test_support::claim_fixture_producer(&mut core);
        assert!(test_support::settle_pending_prefix(&mut core, &producer).is_empty());
        let origin = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert!(
            core.dispatch(RyeOsEvent::PresentationProducerReady {
                producer: producer.clone(),
                next_engine_seq: 0,
            })
            .is_empty()
        );
        (core, producer, origin)
    }

    #[test]
    fn transferred_draft_hydration_preserves_later_local_edits() {
        let (mut core, _, origin) = fixture();
        let descriptor = core.binding_attachment_for_instance(&origin).unwrap();
        let key = InputBufferKey::new(origin, "view:test/open", "message", descriptor);
        let mut replay = core.acknowledged_composition_replay().unwrap();
        replay.transfer_drafts.insert(
            key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "transferred".into(),
                cursor: 11,
                filter_field: 2,
            },
        );
        // This tests publication of an already checked projection, not the
        // two-seat admission proof that constructs it.
        core.reconcile_acknowledged_composition_with_replay(replay.clone())
            .unwrap();
        assert_eq!(core.input_buffers[&key.storage_key()].text, "transferred");
        assert_eq!(core.input_buffers[&key.storage_key()].filter_field, 2);
        core.input_buffers
            .get_mut(&key.storage_key())
            .unwrap()
            .set_text("later local edit".into(), 5);
        core.reconcile_acknowledged_composition_with_replay(replay.clone())
            .unwrap();
        assert_eq!(
            core.input_buffers[&key.storage_key()].text,
            "later local edit"
        );
        assert_eq!(core.input_buffers[&key.storage_key()].cursor, 5);
        core.input_buffers
            .get_mut(&key.storage_key())
            .unwrap()
            .text
            .clear();
        core.input_buffers
            .get_mut(&key.storage_key())
            .unwrap()
            .cursor = 0;
        core.reconcile_acknowledged_composition_with_replay(replay)
            .unwrap();
        assert!(core.input_buffers[&key.storage_key()].text.is_empty());
    }

    #[test]
    fn transferred_draft_hydration_rejects_invalid_cursor_before_publication() {
        let (mut core, _, origin) = fixture();
        let descriptor = core.binding_attachment_for_instance(&origin).unwrap();
        let key = InputBufferKey::new(origin, "view:test/open", "message", descriptor);
        let mut replay = core.acknowledged_composition_replay().unwrap();
        replay.transfer_drafts.insert(
            key,
            crate::ui::model::RyeOsInputState {
                text: "é".into(),
                cursor: 1,
                filter_field: 0,
            },
        );
        let generation = core.generation;
        let buffers = core.input_buffers.clone();
        assert!(
            core.reconcile_acknowledged_composition_with_replay(replay)
                .is_err()
        );
        assert_eq!(core.generation, generation);
        assert_eq!(core.input_buffers, buffers);
    }

    #[test]
    fn transferred_draft_hydration_rejects_unadmitted_binding_scope() {
        let (mut core, _, origin) = fixture();
        let descriptor = core.binding_attachment_for_instance(&origin).unwrap();
        let mut key = InputBufferKey::new(origin, "view:test/open", "message", descriptor);
        key.binding_scope = "unadmitted-binding".into();
        let mut replay = core.acknowledged_composition_replay().unwrap();
        replay.transfer_drafts.insert(key, Default::default());
        let generation = core.generation;
        let buffers = core.input_buffers.clone();
        assert!(
            core.reconcile_acknowledged_composition_with_replay(replay)
                .is_err()
        );
        assert_eq!(core.generation, generation);
        assert_eq!(core.input_buffers, buffers);
    }

    #[test]
    fn inconsistent_acknowledged_prefix_refuses_completion_and_cached_drive() {
        let (mut core, _, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        // Keep the valid checked cache, but model a shorter captured log.
        // Cache presence must not authorize indexing or derived publication.
        let prefix = core.presentation_operations.acknowledged_prefix_len;
        assert!(prefix > 0);
        core.seat = Default::default();
        assert!(core.acknowledged_composition_replay().is_err());
        assert!(core.reconcile_acknowledged_composition().is_err());
        assert!(!core.retain_presentation_completion(&operation, json!({})));
        assert!(core.seat.events().is_empty());
        assert!(core.drive_presentation_operations().is_empty());
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.presentation_operations.producer.is_none());
        assert_eq!(core.presentation_operations.acknowledged_prefix_len, prefix);
        assert!(core.seat.events().is_empty());
    }

    #[test]
    fn startup_requires_positive_exact_producer_readiness() {
        let (core, producer, _) = fixture();
        let mut before_ready = core.clone();
        before_ready.presentation_operations.producer = None;
        assert!(
            before_ready
                .require_presentation_producer_ready(&producer, 0)
                .is_err()
        );
        assert!(
            core.require_presentation_producer_ready(&producer, 0)
                .is_ok()
        );
        assert!(
            core.require_presentation_producer_ready(&producer, 1)
                .is_err()
        );
        let mut other = producer.clone();
        other.producer_incarnation = "cd".repeat(32);
        assert!(core.require_presentation_producer_ready(&other, 0).is_err());
    }

    fn prepare(
        core: &mut RyeOsCore,
        origin: &crate::ids::RyeOsViewInstanceKey,
    ) -> UiPreparedPresentationOperation {
        assert!(
            core.invoke_affordance(
                origin,
                "view:test/open",
                "open",
                &json!({"local_id":"project-exact"})
            )
            .is_empty()
        );
        match &core.seat.events().last().unwrap().kind {
            SeatEventKind::PresentationOperationPrepared(operation) => operation.clone(),
            _ => panic!("preparation must be write-ahead, not an emitted invoke"),
        }
    }

    #[test]
    fn prepared_admission_requires_its_closed_tagged_body() {
        let (mut core, _, origin) = fixture();
        let prepared = prepare(&mut core, &origin);
        let value = serde_json::to_value(&prepared).unwrap();
        assert_eq!(value["body"]["kind"], "admission");
        assert!(value.get("request").is_none());
        let round_trip: UiPreparedPresentationOperation =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(round_trip, prepared);
        assert!(round_trip.admission().is_some());
        assert!(round_trip.composition().is_none());
        assert!(round_trip.as_invoke_binding().is_some());

        let mut predecessor = value.clone();
        let body = predecessor.as_object_mut().unwrap().remove("body").unwrap();
        for field in [
            "purpose",
            "surface_open_target",
            "particular_selection",
            "invocation_origin",
            "request",
            "success_notice",
        ] {
            predecessor[field] = body[field].clone();
        }
        assert!(serde_json::from_value::<UiPreparedPresentationOperation>(predecessor).is_err());
        let mut unknown = value;
        unknown["body"]["kind"] = json!("executable_composition");
        assert!(serde_json::from_value::<UiPreparedPresentationOperation>(unknown).is_err());

        let mut composition = serde_json::to_value(&prepared).unwrap();
        composition["body"] = json!({"kind":"composition","request":{
            "schema":1,"presentation_id":prepared.producer.presentation_id,
            "operation_id":prepared.operation_id,"expected_revision":0,"expected_next_mount_id":1,
            "owner":{"kind":"surface","surface_instance_id":"00000000-0000-4000-8000-000000000001",
                "binding":prepared.root_binding},
            "mutation":{"kind":"select_tab","tab_id":1}
        }});
        let composition: UiPreparedPresentationOperation =
            serde_json::from_value(composition).unwrap();
        assert!(composition.admission().is_none());
        assert!(composition.composition().is_some());
        assert!(composition.as_invoke_binding().is_none());
    }

    #[test]
    fn oversized_preparation_is_refused_before_any_seat_log_append() {
        let (mut core, _, origin) = fixture();
        let mut probe = core.clone();
        let admitted = prepare(&mut probe, &origin);
        assert!(
            (serde_json::to_vec(&admitted).unwrap().len() as u64)
                < admitted.request_bounds.max_request_bytes
        );
        // The dispatch body is admissible, but the independently signed seat
        // append limit cannot hold its transport wrapper. Do not strand a
        // provisional preparation that the seat pump can never acknowledge.
        for attachment in core.binding_attachments.values_mut() {
            attachment
                .descriptor
                .binding_request_bounds
                .max_seat_append_bytes = 1;
        }
        let before = core.seat.events().to_vec();
        let sequence = core.seat.next_sequence();
        assert!(
            core.invoke_affordance(
                &origin,
                "view:test/open",
                "open",
                &json!({"local_id":"project-exact"})
            )
            .is_empty()
        );
        assert_eq!(core.seat.events(), before.as_slice());
        assert_eq!(core.seat.next_sequence(), sequence);
        assert!(core.pending_effects.is_empty());
        assert!(core.presentation_operations.prepared.is_empty());
    }

    #[test]
    fn structural_edit_requires_two_exact_acks_and_blocks_competing_work() {
        let (mut core, producer, origin) = fixture();
        let surface = core.active_surface().unwrap().id().clone();
        let before_count = core.active_surface().unwrap().view_sets().len();
        let before_frontier = core
            .acknowledged_composition_replay()
            .unwrap()
            .next_mount_id;
        assert!(core.new_view_set().is_empty());
        let prepared = test_support::pending_prefix(&core);
        assert!(prepared.iter().any(|event| matches!(&event.kind, SeatEventKind::PresentationOperationPrepared(operation) if operation.composition().is_some())));
        assert_eq!(
            core.active_surface().unwrap().view_sets().len(),
            before_count
        );
        let journal = core.seat.events().to_vec();
        core.new_view_set();
        core.invoke_affordance(
            &origin,
            "view:test/open",
            "open",
            &json!({"local_id":"another"}),
        );
        core.request_surface_close(&surface);
        assert_eq!(
            core.seat.events(),
            journal.as_slice(),
            "pending composition is a barrier for structural and admission writes"
        );
        let mut wrong = prepared.clone();
        wrong.last_mut().unwrap().seq += 1;
        assert!(core.acknowledge_seat_prefix(&producer, wrong).is_empty());
        assert_eq!(
            core.acknowledged_composition_replay()
                .unwrap()
                .next_mount_id,
            before_frontier
        );
        assert_eq!(
            core.active_surface().unwrap().view_sets().len(),
            before_count
        );
        assert!(core.acknowledge_seat_prefix(&producer, prepared).is_empty());
        assert_eq!(
            core.active_surface().unwrap().view_sets().len(),
            before_count,
            "prepared ACK only writes ahead the completion"
        );
        let completed = test_support::pending_prefix(&core);
        assert!(completed.iter().any(|event| matches!(
            &event.kind,
            SeatEventKind::PresentationOperationCompleted(_)
        )));
        core.acknowledge_seat_prefix(&producer, completed);
        assert_eq!(
            core.active_surface().unwrap().view_sets().len(),
            before_count + 1
        );
        assert!(
            core.acknowledged_composition_replay()
                .unwrap()
                .next_mount_id
                > before_frontier
        );
        assert!(test_support::pending_prefix(&core).is_empty());
    }

    #[test]
    fn focus_ack_targets_the_exact_input_mount_and_retains_other_drafts() {
        let mut core = test_support::core(test_support::session_with_surface(json!({
            "name":"input-focus", "tiles":["view:test/a","view:test/b"],
            "views":{
                "view:test/a":{"widget":"rows","input":{"id":"line","submit":"route"},"sources":{"default":{"ref":"service:test/a"}}},
                "view:test/b":{"widget":"rows","input":{"id":"line","submit":"route"},"sources":{"default":{"ref":"service:test/b"}}}
            }
        })));
        let producer = test_support::claim_fixture_producer(&mut core);
        test_support::settle_pending_prefix(&mut core, &producer);
        let keys = ["view:test/a", "view:test/b"].map(|view_ref| {
            core.active_view_set()
                .unwrap()
                .tiles
                .values()
                .find(|tile| tile.view.view_ref == view_ref)
                .unwrap()
                .instance_key
                .clone()
        });
        core.focused_input = Some(keys[0].clone());
        let draft = core.focused_input_instance().unwrap().0.storage_key();
        core.input_buffers.insert(
            draft.clone(),
            crate::ui::model::RyeOsInputState {
                text: "A private draft".into(),
                ..Default::default()
            },
        );
        let source_floors = core.data.source_floor.clone();
        assert!(
            core.prepare_composition_mutation(
                crate::ui::composition_operation::CompositionMutation::FocusView {
                    mount: keys[1].clone()
                }
            )
            .is_empty()
        );
        assert_eq!(core.focused_input.as_ref(), Some(&keys[0]));
        assert_eq!(
            core.data.source_floor, source_floors,
            "unacknowledged focus cannot retarget source work"
        );
        core.acknowledge_seat_prefix(&producer, test_support::pending_prefix(&core));
        assert_eq!(core.focused_input.as_ref(), Some(&keys[0]));
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_input.as_ref(), Some(&keys[1]));
        assert_eq!(core.input_buffers[&draft].text, "A private draft");

        core.new_view_set();
        test_support::settle_pending_prefix(&mut core, &producer);
        assert!(
            core.focused_input
                .as_ref()
                .is_none_or(|mount| !keys.contains(mount)),
            "hidden prior-tab inputs cannot retain interaction focus"
        );
        if let Some(mount) = core.focused_input.as_ref() {
            assert!(
                core.presentation
                    .as_ref()
                    .unwrap()
                    .mount(mount)
                    .is_some_and(|view| view.visible()),
                "only a real visible shared input may remain when the selected tab is empty"
            );
        }
        assert_eq!(core.input_buffers[&draft].text, "A private draft");
    }

    #[test]
    fn ordinary_facet_ack_does_not_restore_input_after_no_input_focus_or_blur() {
        let mut core = test_support::core(test_support::session_with_surface(json!({
            "name":"input-retirement", "tiles":["view:test/a","view:test/b"],
            "views":{
                "view:test/a":{"widget":"text","input":{"id":"line","submit":"route"}},
                "view:test/b":{"widget":"text","body":"No input here"}
            }
        })));
        let producer = test_support::claim_fixture_producer(&mut core);
        test_support::settle_pending_prefix(&mut core, &producer);
        let keys = ["view:test/a", "view:test/b"].map(|view_ref| {
            core.active_view_set()
                .unwrap()
                .tiles
                .values()
                .find(|tile| tile.view.view_ref == view_ref)
                .unwrap()
                .instance_key
                .clone()
        });
        core.focused_input = Some(keys[0].clone());
        let draft = core.focused_input_instance().unwrap().0.storage_key();
        core.input_buffers.insert(
            draft.clone(),
            crate::ui::model::RyeOsInputState {
                text: "retained A draft".into(),
                ..Default::default()
            },
        );
        core.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::FocusView {
                mount: keys[1].clone(),
            },
        );
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_view_instance_key().as_ref(), Some(&keys[1]));
        assert!(core.focused_input.is_none());
        core.seat
            .append_facet("unrelated-observation", json!({"tick":1}));
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_view_instance_key().as_ref(), Some(&keys[1]));
        assert!(
            core.focused_input.is_none(),
            "ordinary ACK cannot select a global/default input after exact no-input focus"
        );
        assert_eq!(core.input_buffers[&draft].text, "retained A draft");

        core.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::FocusView {
                mount: keys[0].clone(),
            },
        );
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_input.as_ref(), Some(&keys[0]));
        core.dispatch(RyeOsEvent::Ui {
            event: crate::ui::event::RyeOsUiEvent::BlurInput,
        });
        assert!(core.focused_input.is_none());
        core.seat
            .append_facet("unrelated-observation", json!({"tick":2}));
        test_support::settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_view_instance_key().as_ref(), Some(&keys[0]));
        assert!(
            core.focused_input.is_none(),
            "unchanged canonical focus cannot undo explicit local blur"
        );
        assert_eq!(core.input_buffers[&draft].text, "retained A draft");
    }

    #[test]
    fn conflicting_structural_replay_blocks_without_replacing_visible_composition() {
        let (mut core, producer, _) = fixture();
        core.new_view_set();
        test_support::settle_pending_prefix(&mut core, &producer);
        let before = core
            .active_surface()
            .unwrap()
            .view_sets()
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        let mut replacement = core
            .seat
            .events()
            .iter()
            .find(|event| {
                matches!(
                    &event.kind,
                    SeatEventKind::PresentationOperationCompleted(_)
                )
            })
            .unwrap()
            .clone();
        replacement.kind = SeatEventKind::Facet {
            key: "contradictory-history".into(),
            value: json!(true),
        };
        core.replay_seat_events([replacement]);
        assert!(core.presentation_operations.replay_blocked);
        assert_eq!(
            core.active_surface()
                .unwrap()
                .view_sets()
                .iter()
                .map(|set| set.id)
                .collect::<Vec<_>>(),
            before
        );
        let tail = core.seat.events().to_vec();
        core.new_view_set();
        assert_eq!(core.seat.events(), tail.as_slice());
    }

    #[test]
    fn admission_waits_for_exact_local_prefix_and_current_producer() {
        let (mut core, producer, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        operation.validate().unwrap();
        assert!(core.pending_effects.is_empty());
        let prefix = test_support::pending_prefix(&core);
        let mut foreign = producer.clone();
        foreign.producer_incarnation = "cd".repeat(32);
        assert!(
            core.acknowledge_seat_prefix(&foreign, prefix.clone())
                .is_empty()
        );
        let mut altered = prefix.clone();
        altered[0].seq += 10;
        assert!(core.acknowledge_seat_prefix(&producer, altered).is_empty());
        core.seat.append_facet("later", json!("unsaved suffix"));
        let effects = core.acknowledge_seat_prefix(&producer, prefix.clone());
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0].kind, RyeOsEffectKind::InvokePresentationOperation { operation:retained } if retained == &operation)
        );
        assert!(core.acknowledge_seat_prefix(&producer, prefix).is_empty());
        assert_eq!(
            core.presentation_operations.acknowledged_prefix_len,
            core.seat.events().len() - 1
        );
    }

    #[test]
    fn facet_acknowledgements_do_not_duplicate_history_or_rescan_operations() {
        let (mut core, producer, _) = fixture();
        for index in 0..2000 {
            core.seat.append_facet("activity", json!(index));
            let prefix =
                core.seat.events()[core.presentation_operations.acknowledged_prefix_len..].to_vec();
            assert!(core.acknowledge_seat_prefix(&producer, prefix).is_empty());
            assert!(core.presentation_operations.prepared.is_empty());
            assert!(core.presentation_operations.completed.is_empty());
            assert!(core.presentation_operations.dirty.is_empty());
        }
        assert_eq!(
            core.presentation_operations.acknowledged_prefix_len,
            core.seat.events().len()
        );
    }

    #[test]
    fn preparation_reserves_authenticated_engine_position_across_partial_local_ack() {
        let (mut core, producer, origin) = fixture();
        core.presentation_operations.acknowledged_engine_frontier = Some(4096);
        let prefix = core.presentation_operations.acknowledged_prefix_len;
        core.seat.append_facet("before-preparation", json!(1));
        core.seat.append_facet("before-preparation", json!(2));
        let operation = prepare(&mut core, &origin);
        assert_eq!(operation.prepared_engine_seq, 4098);
        assert_ne!(
            core.seat.events().last().unwrap().seq,
            operation.prepared_engine_seq
        );
        let captured = core.seat.events()[prefix..prefix + 1].to_vec();
        assert!(core.acknowledge_seat_prefix(&producer, captured).is_empty());
        assert_eq!(
            core.presentation_operations.acknowledged_engine_frontier,
            Some(4097)
        );
        assert_eq!(core.seat.events()[prefix].producer_engine_seq, Some(4096));
        assert_eq!(
            core.presentation_operations.reserved_engine_seq(prefix + 2),
            Some(4098)
        );
        let SeatEventKind::PresentationOperationPrepared(retained) =
            &core.seat.events()[prefix + 2].kind
        else {
            panic!("prepared tail")
        };
        assert_eq!(retained, &operation);
        assert!(retained.validate_identity().is_ok());
    }

    #[test]
    fn exact_prefix_ack_refuses_a_preparation_rebased_to_the_wrong_engine_interval() {
        let (mut core, producer, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        let prefix = core.presentation_operations.acknowledged_prefix_len;
        let events = test_support::pending_prefix(&core);
        core.presentation_operations.acknowledged_engine_frontier =
            Some(operation.prepared_engine_seq + 1);
        assert!(core.acknowledge_seat_prefix(&producer, events).is_empty());
        assert_eq!(core.presentation_operations.acknowledged_prefix_len, prefix);
        assert_eq!(core.acknowledged_seat_prefix_len(), prefix);
        assert_eq!(core.seat.events()[prefix].producer_engine_seq, None);
        assert!(
            !core
                .presentation_operations
                .dispatched
                .contains(&operation.operation_id)
        );
    }

    #[test]
    fn new_replay_cannot_advance_live_local_prefix_without_its_engine_frontier() {
        let (mut core, _, _) = fixture();
        let prefix = core.presentation_operations.acknowledged_prefix_len;
        let next = core
            .presentation_operations
            .acknowledged_engine_frontier
            .unwrap();
        let row = SeatEvent {
            seq: core.seat.next_sequence(),
            producer_engine_seq: Some(next),
            kind: SeatEventKind::Facet {
                key: "replayed-after-readiness".into(),
                value: json!(1),
            },
        };
        assert!(core.replay_seat_events([row]).is_empty());
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.presentation_operations.producer.is_none());
        assert_eq!(core.presentation_operations.acknowledged_prefix_len, prefix);
        assert_eq!(core.seat.events().len(), prefix);
    }

    #[test]
    fn last_surface_close_waits_for_ack_and_preserves_shared_root_and_attachments() {
        let (mut core, producer, _) = fixture();
        let id = core.active_surface().unwrap().id().clone();
        let attachments = core
            .data
            .session
            .as_ref()
            .unwrap()
            .binding_attachments
            .len();
        let bottom = core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom);
        assert!(core.request_surface_close(&id).is_empty());
        let length = core.seat.events().len();
        core.request_surface_close(&id);
        assert_eq!(
            core.seat.events().len(),
            length,
            "unknown pending close is not duplicated"
        );
        assert!(
            core.active_surface().is_some(),
            "no removal before receipt acknowledgement"
        );
        let events = test_support::pending_prefix(&core);
        core.acknowledge_seat_prefix(&producer, events);
        assert!(core.active_surface().is_none());
        assert!(
            core.data.session.as_ref().unwrap().presentations[0]
                .surfaces
                .is_empty()
        );
        assert_eq!(
            core.data
                .session
                .as_ref()
                .unwrap()
                .binding_attachments
                .len(),
            attachments
        );
        assert_eq!(
            core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom),
            bottom
        );
    }

    #[test]
    fn surface_close_refuses_unsent_draft_on_its_mount() {
        let (mut core, producer, origin) = fixture();
        let id = core.active_surface().unwrap().id().clone();
        let attachment = core
            .binding_attachment_for_instance(&origin)
            .unwrap()
            .clone();
        let draft_key =
            InputBufferKey::new(origin, "view:test/open", "message", &attachment).storage_key();
        core.input_buffers.insert(
            draft_key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "unsent".into(),
                ..Default::default()
            },
        );

        let before = core.seat.events().len();
        assert!(!core.surface_close_available(&id));
        assert!(core.request_surface_close(&id).is_empty());
        assert_eq!(core.seat.events().len(), before);

        core.input_buffers.get_mut(&draft_key).unwrap().text.clear();
        assert!(core.surface_close_available(&id));
        assert!(core.request_surface_close(&id).is_empty());
        let prefix = test_support::pending_prefix(&core);
        core.acknowledge_seat_prefix(&producer, prefix);
        assert!(
            !core
                .presentation
                .as_ref()
                .unwrap()
                .surfaces()
                .contains_key(&id)
        );
    }

    #[test]
    fn surface_close_refuses_unsent_contextual_dialog_draft() {
        let mut core = test_support::core(test_support::session_with_surface(json!({
            "name": "contextual-close",
            "tiles": ["view:test/open"],
            "views": {"view:test/open": {"widget": "rows"}},
            "dialogs": [{"id": "context", "title": "Context", "view": "view:test/open"}]
        })));
        let producer = test_support::claim_fixture_producer(&mut core);
        assert!(test_support::settle_pending_prefix(&mut core, &producer).is_empty());
        let id = core.active_surface().unwrap().id().clone();
        let dialog = core
            .presentation
            .as_ref()
            .unwrap()
            .dialogs()
            .values()
            .find(|dialog| dialog.surface_owner() == Some(&id))
            .unwrap();
        let instance = dialog.view().instance_key().clone();
        let attachment = core
            .binding_attachment_for_instance(&instance)
            .unwrap()
            .clone();
        let draft_key =
            InputBufferKey::new(instance, "view:test/open", "message", &attachment).storage_key();
        core.input_buffers.insert(
            draft_key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "unsent dialog input".into(),
                ..Default::default()
            },
        );

        assert!(!core.surface_close_available(&id));
        core.request_surface_close(&id);
        assert!(
            !core.seat.events()[core.presentation_operations.acknowledged_prefix_len..]
                .iter()
                .any(|event| matches!(&event.kind, SeatEventKind::SurfaceClosed(_)))
        );
        core.input_buffers.get_mut(&draft_key).unwrap().text.clear();
        assert!(core.surface_close_available(&id));
    }

    #[test]
    fn close_does_not_retire_sibling_using_the_same_attachment() {
        let mut session = test_support::session();
        let original_id = crate::ids::SurfaceInstanceId::try_new(
            session.presentations[0].surfaces[0]
                .surface_instance_id
                .clone(),
        )
        .unwrap();
        append_sibling_fixture(&mut session, "33333333-3333-4333-8333-333333333333");
        let sibling = session.presentations[0].surfaces[1].clone();
        let mut core = test_support::core(session);
        let producer = test_support::claim_fixture_producer(&mut core);
        test_support::settle_pending_prefix(&mut core, &producer);
        core.request_surface_close(&original_id);
        core.acknowledge_seat_prefix(&producer, test_support::pending_prefix(&core));
        let remaining = &core.data.session.as_ref().unwrap().presentations[0].surfaces;
        assert_eq!(remaining, &[sibling]);
        assert_eq!(core.presentation.as_ref().unwrap().surfaces().len(), 1);
    }

    #[test]
    fn replayed_close_fences_completed_admission_for_that_surface() {
        let (mut core, producer, origin) = fixture();
        let original_id = core.active_surface().unwrap().id().clone();
        let operation = prepare(&mut core, &origin);
        let attachment = test_support::fixture_attachment(
            "new-surface",
            3,
            "55",
            Some("/tmp/project"),
            json!({"name":"admitted","title":"Admitted","tiles":["view:test/content"],
                "views":{"view:test/content":{"widget":"text","body":"Actual admitted content"}}}),
        );
        let seeds = attachment
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active_seed = seeds.first().map(|seed| seed.id.clone());
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        };
        let closed_id =
            crate::ids::SurfaceInstanceId::try_new("44444444-4444-4444-8444-444444444444").unwrap();
        let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
            closed_id.clone(),
            core.data.session.as_ref().unwrap().presentations[0].next_mount_id,
            seeds
                .into_iter()
                .map(|seed| (seed, binding.clone()))
                .collect(),
            active_seed.as_deref(),
        )
        .unwrap();
        let member = crate::ui::binding::UiSurfaceMembership {
            surface_instance_id: closed_id.as_str().into(),
            ordinal: 2,
            title: "Admitted".into(),
            binding: binding.clone(),
            mount_manifest: manifest,
            saved_surface: None,
        };
        core.seat.append(SeatEventKind::PresentationOperationCompleted(UiCompletedPresentationOperation {
            operation_id:operation.operation_id.clone(),
            result:json!({"ui_transition":{"kind":"open_surface","presentation_id":producer.presentation_id,"creation_request_id":operation.operation_id,"opening":{"creation_request_id":operation.operation_id,"selection":operation.admission().unwrap().surface_open_target.as_ref().unwrap().selection,"placement":{"kind":"new_surface"}},"surface":member,"attachment":attachment}}),
        }));
        core.seat.append(SeatEventKind::SurfaceClosed(
            crate::ui::seat::UiSurfaceClosure {
                presentation_id: producer.presentation_id.clone(),
                surface_instance_id: closed_id.as_str().into(),
                binding,
            },
        ));
        let history = core.seat.events().to_vec();
        core.presentation_operations = Default::default();
        core.acknowledged_composition_cache = None;
        core.seat = Default::default();
        core.replay_seat_events(history);
        assert!(core.presentation_producer_ready(producer, 0).is_empty());
        let presentation = core.presentation.as_ref().unwrap();
        assert!(presentation.surfaces().contains_key(&original_id));
        assert!(!presentation.surfaces().contains_key(&closed_id));
        assert_eq!(presentation.surfaces().len(), 1);
        assert!(!core.presentation_operations.replay_blocked);
        assert!(
            core.presentation_operations
                .applied
                .contains(&operation.operation_id)
        );
    }

    #[test]
    fn contradictory_live_closure_replay_is_fenced_without_rebuilding_projection() {
        let (mut core, producer, _) = fixture();
        let id = core.active_surface().unwrap().id().clone();
        core.request_surface_close(&id);
        core.acknowledge_seat_prefix(&producer, test_support::pending_prefix(&core));
        let sequence = core.seat.events().last().unwrap().seq;
        core.replay_seat_events([SeatEvent {
            producer_engine_seq: None,
            seq: sequence,
            kind: SeatEventKind::Facet {
                key: "contradiction".into(),
                value: json!(true),
            },
        }]);
        assert!(
            core.presentation_operations
                .closed_surfaces
                .contains_key(id.as_str())
        );
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.presentation_producer_ready(producer, 0).is_empty());
        assert!(core.presentation_operations.producer.is_none());
        assert!(
            core.active_surface().is_none(),
            "contradictory history cannot silently restore a mutable mount"
        );
    }

    #[test]
    fn identical_replay_is_noop_and_duplicate_operation_identity_is_inert() {
        let (mut core, producer, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        let events = test_support::pending_prefix(&core);
        assert_eq!(
            core.acknowledge_seat_prefix(&producer, events.clone())
                .len(),
            1
        );
        assert!(
            core.replay_seat_events(core.seat.events().to_vec())
                .is_empty()
        );
        assert!(core.presentation_operations.dirty.is_empty());
        // Cold replay also rejects duplicates independently of the live fence.
        core.presentation_operations.producer = None;
        core.presentation_operations.acknowledged_engine_frontier = None;
        let duplicate = SeatEvent {
            producer_engine_seq: None,
            seq: core.seat.next_sequence(),
            kind: SeatEventKind::PresentationOperationPrepared(operation.clone()),
        };
        assert!(core.replay_seat_events([duplicate]).is_empty());
        assert!(
            core.presentation_operations.replay_blocked,
            "duplicate identity invalidates the complete causal replay before publishing its index"
        );
        assert!(core.presentation_operations.producer.is_none());
    }

    #[test]
    fn structural_replay_replacement_requires_rebootstrap_without_publishing_stale_completion() {
        let (mut core, _, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        let original = core.seat.events().to_vec();
        core.presentation_operations = Default::default();
        core.acknowledged_composition_cache = None;
        core.seat = Default::default();
        core.replay_seat_events(original.clone());
        core.presentation_operations
            .applied
            .insert(operation.operation_id.clone());
        core.presentation_operations
            .refused
            .insert(operation.operation_id.clone());
        let mut replacement = original.last().unwrap().clone();
        replacement.kind = SeatEventKind::Facet {
            key: "replacement".into(),
            value: json!(true),
        };
        let history = core.seat.events().to_vec();
        let composition = core.export_layout_preferences().unwrap();
        assert!(core.replay_seat_events([replacement]).is_empty());
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.presentation_operations.producer.is_none());
        assert_eq!(core.seat.events(), history);
        assert_eq!(core.export_layout_preferences().unwrap(), composition);
        assert!(core.presentation_operations.completed.is_empty());
        assert!(
            core.presentation_producer_ready(operation.producer, 0)
                .is_empty()
        );
    }

    #[test]
    fn replacement_replay_retains_original_operation_identity() {
        let (mut original, _, origin) = fixture();
        let operation = prepare(&mut original, &origin);
        let mut recovered = original.clone();
        recovered.presentation_operations = Default::default();
        recovered.acknowledged_composition_cache = None;
        recovered.seat = Default::default();
        let mut replayed = original.seat.events().to_vec();
        // Producer-event engine_seq and authoritative chain position are
        // distinct; replacement never re-derives the original logical UUID.
        for (index, event) in replayed.iter_mut().enumerate() {
            event.seq = 100 + index as u64;
        }
        assert!(recovered.replay_seat_events(replayed).is_empty());
        let mut replacement = operation.producer.clone();
        replacement.producer_incarnation = "cd".repeat(32);
        let effects =
            recovered.presentation_producer_ready(replacement, operation.prepared_engine_seq + 1);
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0].kind, RyeOsEffectKind::InvokePresentationOperation { operation:retained } if retained.operation_id == operation.operation_id && retained.producer == operation.producer && retained.prepared_engine_seq == operation.prepared_engine_seq)
        );
    }

    #[test]
    fn replayed_completion_revalidates_purpose_presentation_and_bound() {
        for failure in ["purpose", "presentation", "bound"] {
            let (mut original, _, origin) = fixture();
            let operation = prepare(&mut original, &origin);
            let mut result = json!({"ui_transition":{"kind":"open_surface","presentation_id":operation.producer.presentation_id,"creation_request_id":operation.operation_id}});
            match failure {
                "purpose" => result["ui_transition"]["kind"] = json!("resume_particular_view_set"),
                "presentation" => result["ui_transition"]["presentation_id"] = json!("foreign"),
                "bound" => {
                    result["oversized"] =
                        json!("x".repeat(operation.request_bounds.max_request_bytes as usize))
                }
                _ => unreachable!(),
            }
            let completion = UiCompletedPresentationOperation {
                operation_id: operation.operation_id.clone(),
                result,
            };
            assert!(completion.validate_for(&operation).is_err());
            original
                .seat
                .append(SeatEventKind::PresentationOperationCompleted(completion));
            let mut recovered = original.clone();
            recovered.presentation_operations = Default::default();
            recovered.seat = Default::default();
            recovered.replay_seat_events(original.seat.events().to_vec());
            assert!(
                recovered
                    .presentation_producer_ready(
                        operation.producer.clone(),
                        operation.prepared_engine_seq + 2
                    )
                    .is_empty()
            );
            assert!(
                !recovered
                    .presentation_operations
                    .applied
                    .contains(&operation.operation_id)
            );
            assert!(recovered.pending_effects.is_empty());
        }
    }

    #[test]
    fn mismatched_admission_success_retains_unknown_without_completion_or_retry() {
        let (mut core, producer, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        let prefix = test_support::pending_prefix(&core);
        let effect = core
            .acknowledge_seat_prefix(&producer, prefix.clone())
            .pop()
            .unwrap();
        let before = core.seat.events().to_vec();
        assert!(
            core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: crate::ui::effect::RyeOsEffectResultKind::BrowserOnly,
                data: Some(json!({"ui_transition": {"admitted": true}})),
                error: None,
            })
            .is_empty()
        );
        assert_eq!(core.seat.events(), before.as_slice());
        assert!(
            !core
                .presentation_operations
                .completed
                .contains_key(&operation.operation_id)
        );
        assert!(
            core.presentation_operations
                .dispatched
                .contains(&operation.operation_id)
        );
        assert_eq!(
            core.ui.effect_failures[&effect.id].outcome,
            crate::ui::effect::RyeOsEffectOutcome::Unknown
        );
        assert!(core.acknowledge_seat_prefix(&producer, prefix).is_empty());
    }

    #[test]
    fn admission_response_is_write_ahead_before_composition_changes() {
        let (mut core, producer, origin) = fixture();
        let operation = prepare(&mut core, &origin);
        let prefix = test_support::pending_prefix(&core);
        let effect = core
            .acknowledge_seat_prefix(&producer, prefix)
            .pop()
            .unwrap();
        let attachment = test_support::fixture_attachment(
            "new-surface",
            3,
            "55",
            Some("/tmp/project"),
            json!({
                "name":"admitted", "title":"Admitted", "tiles":["view:test/open"],
                "views":{"view:test/open":{"widget":"text","body":"actual admitted content"}}
            }),
        );
        let seeds = attachment
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active_seed = seeds.first().map(|seed| seed.id.clone());
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        };
        let start = core
            .data
            .session
            .as_ref()
            .unwrap()
            .presentations
            .iter()
            .find(|member| member.presentation_id == producer.presentation_id)
            .unwrap()
            .next_mount_id;
        let mount_manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
            crate::ids::SurfaceInstanceId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            start,
            seeds
                .into_iter()
                .map(|seed| (seed, binding.clone()))
                .collect(),
            active_seed.as_deref(),
        )
        .unwrap();
        let surface = crate::ui::binding::UiSurfaceMembership {
            mount_manifest,
            saved_surface: None,
            surface_instance_id: "22222222-2222-4222-8222-222222222222".into(),
            ordinal: 2,
            title: "Admitted".into(),
            binding: crate::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: attachment.binding_attachment_id.clone(),
                binding_generation: attachment.binding_generation,
                binding_digest: attachment.binding_digest.clone(),
            },
        };
        let surfaces_before = core.presentation.as_ref().unwrap().surfaces().len();
        assert!(core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id:effect.id, ok:true, kind:RyeOsEffectResultKind::BindingInvoked,
            data:Some(json!({"ui_transition":{"kind":"open_surface","presentation_id":producer.presentation_id,
                "creation_request_id":operation.operation_id,"opening":{"creation_request_id":operation.operation_id,"selection":operation.admission().unwrap().surface_open_target.as_ref().unwrap().selection,"placement":{"kind":"new_surface"}},"surface":surface,"attachment":attachment}})), error:None,
        }).is_empty());
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces().len(),
            surfaces_before
        );
        assert!(
            matches!(&core.seat.events().last().unwrap().kind, SeatEventKind::PresentationOperationCompleted(completion) if completion.operation_id == operation.operation_id)
        );
        let completion_prefix =
            core.seat.events()[core.presentation_operations.acknowledged_prefix_len..].to_vec();
        core.acknowledge_seat_prefix(&producer, completion_prefix);
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces().len(),
            surfaces_before + 1
        );
    }
}
