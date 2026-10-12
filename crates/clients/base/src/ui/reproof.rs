//! Same-producer replacement of an authenticated paired replay candidate.
//! The candidate is private until its exact completed tip and this live Core's
//! unchanged local writer boundary have both been checked.

use std::collections::BTreeSet;

use super::binding::{UiPresentationMembership, UiPresentationProducer};
use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::model::{InputBufferKey, RyeOsCore};
use super::seat::SeatEvent;
use super::source_key::{RyeOsSourceInstanceKey, RyeOsSurfaceSourceKey};
use crate::ids::RyeOsViewInstanceKey;

#[derive(Debug, Clone)]
pub struct AuthenticatedReproofGuard {
    session_id: String,
    membership: UiPresentationMembership,
    memberships: Vec<UiPresentationMembership>,
    producer: UiPresentationProducer,
    acknowledged_prefix: usize,
    acknowledged_frontier: Option<u64>,
    seat_tip: Option<SeatEvent>,
}

impl RyeOsCore {
    fn reproof_membership(&self) -> Result<&UiPresentationMembership, String> {
        let presentation = self.presentation.as_ref().ok_or("presentation absent")?;
        let session = self.data.session.as_ref().ok_or("session absent")?;
        let matches = session
            .presentations
            .iter()
            .filter(|member| member.presentation_id == presentation.id().as_str())
            .collect::<Vec<_>>();
        let [member] = matches.as_slice() else {
            return Err("reproof seat membership is absent or duplicated".into());
        };
        if member.thread_id.is_none() || member.chain_root_id.is_none() {
            return Err("reproof seat has no retained chain".into());
        }
        Ok(member)
    }

    /// Begin only after the adapter has reconciled the live producer tail.
    /// This is a race detector, not an admission grant or a substitute for the
    /// server's exact all-head paired completion check.
    pub fn begin_authenticated_reproof(&self) -> Result<AuthenticatedReproofGuard, String> {
        let producer = self
            .presentation_operations
            .producer
            .as_ref()
            .ok_or("reproof requires a current producer")?;
        if self.presentation_operations.replay_blocked
            || self.presentation_operations.acknowledged_prefix_len != self.seat.events().len()
            || !self.producer_matches_membership(producer)
        {
            return Err("reproof requires a fully acknowledged live seat".into());
        }
        let membership = self.reproof_membership()?.clone();
        let session_id = self.data.session.as_ref().unwrap().session_id.clone();
        if session_id.is_empty() {
            return Err("reproof requires an exact session".into());
        }
        Ok(AuthenticatedReproofGuard {
            session_id,
            membership,
            memberships: self.data.session.as_ref().unwrap().presentations.clone(),
            producer: producer.clone(),
            acknowledged_prefix: self.presentation_operations.acknowledged_prefix_len,
            acknowledged_frontier: self.presentation_operations.acknowledged_engine_frontier,
            seat_tip: self.seat.events().last().cloned(),
        })
    }

    /// Promote only a finished private paired candidate. Cold replay may have
    /// enumerated startup fetches, but its effect IDs are never copied into a
    /// live Core. A candidate requiring a new write is refused rather than
    /// silently dropping or duplicating that obligation.
    pub fn promote_authenticated_reproof(
        &mut self,
        mut candidate: RyeOsCore,
        guard: &AuthenticatedReproofGuard,
    ) -> Result<Vec<RyeOsEffect>, String> {
        let current = self.begin_authenticated_reproof()?;
        if current.session_id != guard.session_id
            || current.membership != guard.membership
            || current.memberships != guard.memberships
            || current.producer != guard.producer
            || current.acknowledged_prefix != guard.acknowledged_prefix
            || current.acknowledged_frontier != guard.acknowledged_frontier
            || current.seat_tip != guard.seat_tip
        {
            return Err("live seat changed during authenticated reproof".into());
        }
        let candidate_session = candidate
            .data
            .session
            .as_ref()
            .ok_or("candidate has no session")?;
        let candidate_member = candidate.reproof_membership()?;
        if candidate_session.session_id != guard.session_id
            || candidate_member.presentation_id != guard.membership.presentation_id
            || candidate_member.thread_id != guard.membership.thread_id
            || candidate_member.chain_root_id != guard.membership.chain_root_id
            || candidate_member.root_binding != guard.membership.root_binding
            || candidate_member.initialization_digest != guard.membership.initialization_digest
            || candidate.authenticated_reproof_tip.is_none()
            || candidate.presentation_operations.replay_blocked
            || candidate.presentation_operations.acknowledged_prefix_len
                != candidate.seat.events().len()
            || candidate.seat.events() != self.seat.events()
        {
            return Err("reproof candidate is not a completed exact seat".into());
        }
        if candidate.pending_effects.values().any(|kind| match kind {
            RyeOsEffectKind::FetchSource { .. } => false,
            RyeOsEffectKind::InvokePresentationOperation { operation } => {
                self.presentation_operations
                    .prepared
                    .get(&operation.operation_id)
                    != candidate
                        .presentation_operations
                        .prepared
                        .get(&operation.operation_id)
                    || !self
                        .presentation_operations
                        .dispatched
                        .contains(&operation.operation_id)
            }
            _ => true,
        }) {
            return Err("reproof candidate has an unreconciled write effect".into());
        }

        let changed = self
            .mounted_instances()
            .chain(candidate.mounted_instances())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|instance| !same_mount_subject(self, &candidate, instance))
            .collect::<BTreeSet<_>>();
        let new_fetches = candidate
            .pending_effects
            .values()
            .filter_map(|kind| match kind {
                RyeOsEffectKind::FetchSource { tile_id, .. } => {
                    let changed_subject = RyeOsSourceInstanceKey::decode(tile_id)
                        .is_some_and(|key| changed.contains(&key.view_instance))
                        || RyeOsSurfaceSourceKey::decode(tile_id).is_some_and(|key| {
                            !guard.membership.surfaces.iter().any(|surface| {
                                surface.surface_instance_id == key.surface_instance_id.as_str()
                            })
                        });
                    changed_subject.then(|| {
                        (
                            kind.clone(),
                            candidate
                                .data
                                .source_subject_fingerprint
                                .get(tile_id)
                                .cloned(),
                            candidate.data.source_unresolved.get(tile_id).copied(),
                        )
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        let candidate_session = candidate.data.session.take();
        candidate.data = self.data.clone();
        candidate.data.session = candidate_session;
        candidate.ui = self.ui.clone();
        candidate.runtime = self.runtime.clone();
        candidate.supports_independent_documents = self.supports_independent_documents;
        candidate.pending_effects = self.pending_effects.clone();
        candidate.next_effect_id = self.next_effect_id;
        candidate.deferred_source_fetches = self.deferred_source_fetches.clone();
        candidate.field_cursor_scopes = self.field_cursor_scopes.clone();
        let retained_contacts = self
            .contacted_transfer_actions
            .iter()
            .filter(|contact| {
                !self.refused_transfer_contacts.contains(*contact)
                    && !contact_observed(&candidate, &contact.0, &contact.1)
            })
            .cloned()
            .collect();
        candidate.contacted_transfer_actions = retained_contacts;
        candidate.refused_transfer_contacts.clear();
        candidate.presentation_operations.producer = Some(guard.producer.clone());
        // The paired phase lane does not advance ordinary producer sequence.
        // Candidate cold replay has no claim and therefore no writer frontier;
        // exact equality of every ordinary row above lets us retain, not
        // synthesize, the live frontier from this unchanged producer.
        candidate
            .presentation_operations
            .acknowledged_engine_frontier = guard.acknowledged_frontier;
        // A write already dispatched by this live producer is not a new cold
        // obligation. Keep only operations with the same signed preparation
        // position in both prefixes; a candidate with a different operation
        // cannot inherit its dispatch or failure state.
        for (operation_id, sequence) in &self.presentation_operations.prepared {
            if candidate.presentation_operations.prepared.get(operation_id) == Some(sequence) {
                for (old, target) in [
                    (
                        &self.presentation_operations.dispatched,
                        &mut candidate.presentation_operations.dispatched,
                    ),
                    (
                        &self.presentation_operations.applied,
                        &mut candidate.presentation_operations.applied,
                    ),
                    (
                        &self.presentation_operations.refused,
                        &mut candidate.presentation_operations.refused,
                    ),
                ] {
                    if old.contains(operation_id) {
                        target.insert(operation_id.clone());
                    }
                }
            }
        }

        for instance in self.mounted_instances() {
            if same_mount_subject(self, &candidate, &instance) {
                let local = self
                    .presentation
                    .as_ref()
                    .and_then(|presentation| presentation.mount(&instance))
                    .map(|mount| mount.local().clone());
                if let (Some(local), Some(target)) = (
                    local,
                    candidate
                        .presentation
                        .as_mut()
                        .and_then(|presentation| presentation.mount_local_mut(&instance)),
                ) {
                    *target = local;
                }
            }
        }

        for instance in &changed {
            candidate.invalidate_view_sources(instance);
        }
        for (key, buffer) in &self.input_buffers {
            let Some(parsed) = InputBufferKey::from_storage_key(key) else {
                continue;
            };
            if !same_mount_subject(self, &candidate, &parsed.view_instance_key) {
                continue;
            }
            let Some(mount) = candidate
                .presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(&parsed.view_instance_key))
            else {
                continue;
            };
            let scope = std::iter::once((mount.view_ref(), mount.binding()))
                .chain(
                    candidate
                        .retained_lens_frames
                        .get(&parsed.view_instance_key)
                        .into_iter()
                        .flatten()
                        .map(|frame| (frame.view_ref.as_str(), &frame.binding)),
                )
                .find_map(|(view_ref, binding)| {
                    InputBufferKey::storage_key_matches_view_binding(
                        key,
                        &parsed.view_instance_key,
                        view_ref,
                        binding,
                    )
                    .then_some(binding)
                });
            if let Some(scope) = scope {
                let descriptor = candidate
                    .binding_attachment(&scope.binding_attachment_id)
                    .filter(|descriptor| {
                        descriptor.binding_generation == scope.binding_generation
                            && descriptor.binding_digest == scope.binding_digest
                    })
                    .ok_or("preserved draft has no fresh binding descriptor")?;
                buffer.validate_retained_binding(&parsed, descriptor)?;
                candidate.input_buffers.insert(key.clone(), buffer.clone());
            }
        }
        candidate.focused_input = self.focused_input.clone().filter(|instance| {
            same_mount_subject(self, &candidate, instance)
                && candidate
                    .presentation
                    .as_ref()
                    .and_then(|presentation| presentation.mount(instance))
                    .is_some_and(|mount| mount.visible())
        });
        candidate.field_query_editing = self
            .field_query_editing
            .clone()
            .filter(|instance| same_mount_subject(self, &candidate, instance));
        candidate.dialog_return_focus = self
            .dialog_return_focus
            .clone()
            .filter(|instance| same_mount_subject(self, &candidate, instance));
        candidate.selection_attachments = self
            .selection_attachments
            .iter()
            .filter(|(instance, _)| same_mount_subject(self, &candidate, instance))
            .map(|(instance, selection)| (instance.clone(), selection.clone()))
            .collect();
        candidate.generation = self.generation.saturating_add(1);
        let mut effects = Vec::new();
        for (kind, fingerprint, unresolved) in new_fetches {
            if candidate
                .pending_effects
                .values()
                .any(|pending| pending == &kind)
            {
                continue;
            }
            let RyeOsEffectKind::FetchSource { tile_id, .. } = &kind else {
                unreachable!("filtered cold fetches only")
            };
            let tile_id = tile_id.clone();
            let effect = candidate.emit(kind);
            candidate
                .data
                .source_epoch
                .insert(tile_id.clone(), effect.id);
            candidate
                .data
                .source_floor
                .insert(tile_id.clone(), effect.id);
            if let Some(fingerprint) = fingerprint {
                candidate
                    .data
                    .source_subject_fingerprint
                    .insert(tile_id.clone(), fingerprint);
            }
            if let Some(unresolved) = unresolved {
                candidate.data.source_unresolved.insert(tile_id, unresolved);
            }
            effects.push(effect);
        }
        *self = candidate;
        Ok(effects)
    }
}

fn same_mount_subject(old: &RyeOsCore, new: &RyeOsCore, instance: &RyeOsViewInstanceKey) -> bool {
    let old_mount = old.presentation.as_ref().and_then(|p| p.mount(instance));
    let new_mount = new.presentation.as_ref().and_then(|p| p.mount(instance));
    match (old_mount, new_mount) {
        (Some(old_mount), Some(new_mount)) => {
            old_mount.view_ref() == new_mount.view_ref()
                && old_mount.binding() == new_mount.binding()
                && old.retained_input_routes.get(instance)
                    == new.retained_input_routes.get(instance)
                && old.retained_lens_frames.get(instance) == new.retained_lens_frames.get(instance)
        }
        _ => false,
    }
}

fn contact_observed(core: &RyeOsCore, operation_id: &str, action: &str) -> bool {
    if action == "cancel_freeze" {
        return core
            .authenticated_cancellation_proofs
            .contains_key(operation_id);
    }
    let Some((_, lifecycle)) = core.authenticated_transfer_proofs.get(operation_id) else {
        return false;
    };
    match action {
        "accept_view" | "accept_tabs" | "accept_shared_dock" => lifecycle.accepted().is_some(),
        "commit" => lifecycle.committed().is_some() || lifecycle.aborted().is_some(),
        "abort" => lifecycle.aborted().is_some() || lifecycle.committed().is_some(),
        "activate" => lifecycle.activated().is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support;

    fn guarded() -> (RyeOsCore, AuthenticatedReproofGuard, RyeOsCore) {
        let mut live = test_support::core(test_support::session());
        test_support::claim_fixture_producer(&mut live);
        let guard = live.begin_authenticated_reproof().unwrap();
        // Only these unit tests manufacture the private completed-tip marker;
        // they exercise the merge fence, not transport/signature authority.
        let mut candidate = live.clone();
        candidate.authenticated_reproof_tip = Some(("ab".repeat(32), 1));
        candidate.presentation_operations.producer = None;
        (live, guard, candidate)
    }

    #[test]
    fn promotion_refuses_smaller_draft_budget_atomically_and_accepts_unicode_boundary() {
        for limit in [3, 4] {
            let (mut live, guard, mut candidate) = guarded();
            live.focused_input =
                live.shared_dock_instance_key(super::super::model::RyeOsDockEdge::Bottom);
            test_support::set_focused_input(&mut live, "éλ");
            let (key, _) = live.focused_input_instance().unwrap();
            let id = live
                .binding_attachment_for_instance(&key.view_instance_key)
                .unwrap()
                .binding_attachment_id
                .clone();
            candidate
                .binding_attachments
                .get_mut(&id)
                .unwrap()
                .descriptor
                .binding_request_bounds
                .max_input_bytes = limit;
            let prior_buffers = live.input_buffers.clone();
            let prior_generation = live.generation;
            let prior_seat = live.seat.events().to_vec();
            let result = live.promote_authenticated_reproof(candidate, &guard);
            assert_eq!(result.is_ok(), limit == 4);
            assert_eq!(live.input_buffers, prior_buffers);
            assert_eq!(live.seat.events(), prior_seat);
            if limit == 3 {
                assert!(result.unwrap_err().contains("input bound"));
                assert_eq!(live.generation, prior_generation);
                assert_ne!(
                    live.binding_attachments[&id]
                        .descriptor
                        .binding_request_bounds
                        .max_input_bytes,
                    limit
                );
            }
        }
    }

    #[test]
    fn hidden_return_frame_draft_survives_reproof_or_refuses_whole_promotion() {
        for limit in [3, 4] {
            let (mut live, guard, mut candidate) = guarded();
            live.focused_input =
                live.shared_dock_instance_key(super::super::model::RyeOsDockEdge::Bottom);
            let (mut key, _) = live.focused_input_instance().unwrap();
            let mut retained = live
                .binding_context_for_instance(&key.view_instance_key)
                .unwrap()
                .clone();
            retained.descriptor.binding_attachment_id = "hidden-input-binding".into();
            retained.descriptor.binding_digest = "ef".repeat(32);
            let coordinate = super::super::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: retained.descriptor.binding_attachment_id.clone(),
                binding_generation: retained.descriptor.binding_generation,
                binding_digest: retained.descriptor.binding_digest.clone(),
            };
            key.binding_scope = InputBufferKey::binding_scope(&coordinate);
            let frame = super::super::composition_lens::RetainedLensFrame {
                binding: coordinate.clone(),
                view_ref: key.view_ref.clone(),
                attachment: super::super::attachment::SelectionAttachment::Pinned {
                    values: std::collections::BTreeMap::new(),
                    fingerprint: "cd".repeat(32),
                },
                input_route: None,
                label: Some("Hidden input".into()),
            };
            live.retained_lens_frames
                .insert(key.view_instance_key.clone(), vec![frame.clone()]);
            candidate
                .retained_lens_frames
                .insert(key.view_instance_key.clone(), vec![frame]);
            live.binding_attachments
                .insert(coordinate.binding_attachment_id.clone(), retained.clone());
            retained.descriptor.binding_request_bounds.max_input_bytes = limit;
            candidate
                .binding_attachments
                .insert(coordinate.binding_attachment_id, retained);
            live.input_buffers.insert(
                key.storage_key(),
                super::super::model::RyeOsInputState {
                    text: "éλ".into(),
                    cursor: 4,
                    filter_field: 0,
                },
            );
            let prior = live.input_buffers.clone();
            let generation = live.generation;
            let result = live.promote_authenticated_reproof(candidate, &guard);
            assert_eq!(result.is_ok(), limit == 4, "{result:?}");
            assert_eq!(live.input_buffers, prior);
            if limit == 3 {
                assert_eq!(live.generation, generation);
            }
        }
    }

    #[test]
    fn promotion_preserves_adapter_document_support_not_candidate_claims() {
        for supported in [false, true] {
            let (mut live, guard, mut candidate) = guarded();
            live.supports_independent_documents = supported;
            candidate.supports_independent_documents = !supported;
            live.promote_authenticated_reproof(candidate, &guard)
                .unwrap();
            assert_eq!(live.supports_independent_documents, supported);
        }
    }

    #[test]
    fn promotion_requires_completed_candidate_and_unchanged_live_seat() {
        let (mut live, guard, mut candidate) = guarded();
        candidate.authenticated_reproof_tip = None;
        assert!(
            live.promote_authenticated_reproof(candidate, &guard)
                .is_err()
        );
        let (_, _, candidate) = guarded();
        live.seat.append(super::super::seat::SeatEventKind::Facet {
            key: "unacknowledged".into(),
            value: serde_json::Value::Null,
        });
        assert!(
            live.promote_authenticated_reproof(candidate, &guard)
                .is_err()
        );
    }

    #[test]
    fn promotion_retains_live_producer_effect_identity_and_failure_state() {
        let (mut live, guard, candidate) = guarded();
        let producer = live.presentation_operations.producer.clone();
        let pending = live.emit(RyeOsEffectKind::SetLocationHash {
            hash: "#live".into(),
        });
        live.ui.effect_failures.insert(
            pending.id,
            super::super::effect::RyeOsUiError::definite("observed_refusal", "keep exact failure"),
        );
        live.contacted_transfer_actions
            .insert(("operation".into(), "commit".into()));
        let effects = live
            .promote_authenticated_reproof(candidate, &guard)
            .unwrap();
        assert!(effects.is_empty());
        assert_eq!(live.presentation_operations.producer, producer);
        assert_eq!(live.pending_effects.get(&pending.id), Some(&pending.kind));
        assert!(live.ui.effect_failures.contains_key(&pending.id));
        assert!(
            live.contacted_transfer_actions
                .contains(&("operation".into(), "commit".into()))
        );
        assert!(live.next_effect_id >= pending.id);
    }

    #[test]
    fn candidate_write_effect_is_not_dropped_or_reissued() {
        let (mut live, guard, mut candidate) = guarded();
        candidate.emit(RyeOsEffectKind::SetLocationHash {
            hash: "#cold".into(),
        });
        let original = live.seat.events().to_vec();
        assert!(
            live.promote_authenticated_reproof(candidate, &guard)
                .is_err()
        );
        assert_eq!(live.seat.events(), original);
    }

    #[test]
    fn fresh_completed_observation_reopens_only_definitely_refused_contact() {
        let (mut live, guard, candidate) = guarded();
        let refused = ("refused-operation".into(), "commit".into());
        let unknown = ("unknown-operation".into(), "commit".into());
        live.contacted_transfer_actions.insert(refused.clone());
        live.contacted_transfer_actions.insert(unknown.clone());
        live.refused_transfer_contacts.insert(refused.clone());
        live.promote_authenticated_reproof(candidate, &guard)
            .unwrap();
        assert!(!live.contacted_transfer_actions.contains(&refused));
        assert!(live.contacted_transfer_actions.contains(&unknown));
        assert!(live.refused_transfer_contacts.is_empty());
    }
}
