//! Live pickup of revised items: on an `item.hint` the seat asks the daemon to
//! recompile each non-root attachment it holds, then swaps the successor in
//! through one rebind composition request per owner Surface. The hint carries
//! no content; a revise that finds nothing changed returns without a swap, so
//! sweeping every held attachment is correct and stays meaning-blind.

use std::collections::BTreeSet;

use super::super::binding::{UiBindingAttachment, UiBindingAttachmentCoordinate};
use super::super::composition_operation::{CompositionMutation, CompositionOwner};
use super::super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::super::model::RyeOsCore;

/// The hint kind the hint stream forwards for an item revision.
pub const ITEM_HINT_KIND: &str = "item.hint";
const MAX_SEEN_CHANGE_SETS: usize = 256;

type PendingRebind = (
    UiBindingAttachmentCoordinate,
    UiBindingAttachmentCoordinate,
    UiBindingAttachment,
);

#[derive(Debug, Clone, Default)]
pub(crate) struct ItemRevisionState {
    /// (change set, attachment) pairs already swept; later hints coalesce.
    seen_change_sets: BTreeSet<(String, String)>,
    /// Attachment ids that still need a revise request.
    pending: BTreeSet<String>,
    /// Attachment ids with a revise request outstanding.
    in_flight: BTreeSet<String>,
    /// Published successors waiting for a free composition write.
    ready: Vec<PendingRebind>,
    /// Swapped (source, successor) pairs: the source is released, and effects
    /// held for it are re-issued against the successor, once nothing names it.
    awaiting_release: Vec<(UiBindingAttachmentCoordinate, UiBindingAttachmentCoordinate)>,
    /// Invocations refused as stale, held by attachment until its successor is
    /// in place. Each is re-issued exactly once.
    stale_held: std::collections::BTreeMap<String, Vec<RyeOsEffectKind>>,
    /// Attachments whose revise found nothing changed: their held
    /// invocations are re-issued as they were.
    unchanged: BTreeSet<String>,
    /// Attachments already revised because dispatch found them stale.
    stale_retried: BTreeSet<String>,
}

impl RyeOsCore {
    fn revisable_attachment_ids(&self) -> BTreeSet<String> {
        self.presentation
            .as_ref()
            .map(|presentation| {
                presentation
                    .surface_attachment_coordinates()
                    .into_iter()
                    .map(|(_, coordinate)| coordinate.binding_attachment_id)
                    .filter(|id| *id != self.root_attachment_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record one `item.hint`: queue a revise for each held non-root
    /// attachment that names the hinted item. Hints sharing a change set add
    /// no second sweep of an attachment already swept for that set.
    pub(crate) fn note_item_hint(&mut self, payload: &serde_json::Value) {
        let Some(item_ref) = payload.get("item_ref").and_then(serde_json::Value::as_str) else {
            return;
        };
        let change_set = payload
            .get("change_set")
            .and_then(serde_json::Value::as_str);
        let named = self
            .revisable_attachment_ids()
            .into_iter()
            .filter(|id| {
                self.binding_attachments.get(id).is_some_and(|retained| {
                    retained
                        .descriptor
                        .named_items
                        .iter()
                        .any(|named| named == item_ref)
                })
            })
            .collect::<Vec<_>>();
        for id in named {
            if let Some(change_set) = change_set {
                if self.item_revision.seen_change_sets.len() >= MAX_SEEN_CHANGE_SETS {
                    self.item_revision.seen_change_sets.clear();
                }
                if !self
                    .item_revision
                    .seen_change_sets
                    .insert((change_set.to_owned(), id.clone()))
                {
                    continue;
                }
            }
            self.item_revision.pending.insert(id);
        }
    }

    /// Dispatch found a held attachment stale: revise it once and let the next
    /// attempt use the successor. A second stale for the same attachment id is
    /// reported as before.
    pub(crate) fn note_stale_attachment(&mut self, attachment_id: &str) -> bool {
        if attachment_id != self.root_attachment_id
            && self.revisable_attachment_ids().contains(attachment_id)
            && self
                .item_revision
                .stale_retried
                .insert(attachment_id.to_owned())
        {
            self.item_revision.pending.insert(attachment_id.to_owned());
            return true;
        }
        false
    }

    /// A refused invocation: revise its attachment and hold the invocation so
    /// it can be re-issued once against the successor. Only an invocation the
    /// user issued is held; a source fetch simply refetches on the successor.
    pub(crate) fn note_stale_effect(&mut self, effect: &RyeOsEffectKind) {
        let request = match effect {
            RyeOsEffectKind::InvokeBinding { request, .. }
            | RyeOsEffectKind::FetchSource { request, .. } => request,
            _ => return,
        };
        let id = request.binding_attachment_id.clone();
        if self.note_stale_attachment(&id)
            && matches!(effect, RyeOsEffectKind::InvokeBinding { .. })
        {
            self.item_revision
                .stale_held
                .entry(id)
                .or_default()
                .push(effect.clone());
        }
    }

    /// Re-issue held invocations against the successor, or fail them with a
    /// typed reason when the successor no longer declares their view.
    fn reissue_stale_effects(
        &mut self,
        from: &UiBindingAttachmentCoordinate,
        to: &UiBindingAttachmentCoordinate,
    ) -> Vec<RyeOsEffect> {
        let held = self
            .item_revision
            .stale_held
            .remove(&from.binding_attachment_id)
            .unwrap_or_default();
        let Some(descriptor) = self
            .binding_attachments
            .get(&to.binding_attachment_id)
            .map(|retained| retained.descriptor.clone())
        else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        for mut kind in held {
            let RyeOsEffectKind::InvokeBinding { request, .. } = &mut kind else {
                continue;
            };
            let declared = match &request.coordinate {
                super::super::binding::UiBindingCoordinate::Source { view_ref, .. }
                | super::super::binding::UiBindingCoordinate::Affordance { view_ref, .. } => {
                    descriptor.presentation.views.contains_key(view_ref)
                }
                super::super::binding::UiBindingCoordinate::SurfaceRoute => true,
            };
            if !declared {
                self.notice(
                    "That action is gone (ui_binding_view_removed): the revised view no longer declares it.",
                    super::super::view_model::RyeOsTone::Warn,
                );
                continue;
            }
            request.binding_attachment_id = to.binding_attachment_id.clone();
            request.binding_generation = to.binding_generation;
            request.binding_digest = to.binding_digest.clone();
            effects.push(self.emit(kind));
        }
        effects
    }

    /// Settle one revise request. `data` is the daemon's answer, absent when
    /// the request failed.
    pub(crate) fn finish_attachment_revision(
        &mut self,
        attachment_id: &str,
        data: Option<serde_json::Value>,
    ) {
        self.item_revision.in_flight.remove(attachment_id);
        let Some(data) = data else {
            self.item_revision.stale_held.remove(attachment_id);
            return;
        };
        if data.get("revised").and_then(serde_json::Value::as_bool) != Some(true) {
            self.item_revision
                .unchanged
                .insert(attachment_id.to_owned());
            return;
        }
        let from = data
            .get("from")
            .cloned()
            .and_then(|value| serde_json::from_value::<UiBindingAttachmentCoordinate>(value).ok());
        let descriptor = data
            .get("attachment")
            .cloned()
            .and_then(|value| serde_json::from_value::<UiBindingAttachment>(value).ok());
        let (Some(from), Some(descriptor)) = (from, descriptor) else {
            return;
        };
        if from.binding_attachment_id != attachment_id || descriptor.validate().is_err() {
            return;
        }
        let to = UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        if !self
            .binding_attachments
            .contains_key(&to.binding_attachment_id)
            && let Ok(retained) =
                super::super::binding_context::RetainedUiBindingAttachment::from_descriptor(
                    descriptor.clone(),
                )
        {
            self.binding_attachments
                .insert(to.binding_attachment_id.clone(), retained);
        }
        self.item_revision.ready.push((from, to, descriptor));
    }

    /// Emit due revise requests, prepare ready rebinds and release swapped
    /// sources. Called after every dispatched event.
    pub(crate) fn pump_item_revisions(&mut self) -> Vec<RyeOsEffect> {
        let mut effects = Vec::new();
        let state = &self.item_revision;
        if state.pending.is_empty()
            && state.ready.is_empty()
            && state.awaiting_release.is_empty()
            && state.unchanged.is_empty()
        {
            return effects;
        }
        let Some(presentation_id) = self
            .presentation
            .as_ref()
            .map(|presentation| presentation.id().as_str().to_owned())
        else {
            return effects;
        };
        let due = self
            .item_revision
            .pending
            .iter()
            .filter(|id| !self.item_revision.in_flight.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in due {
            self.item_revision.pending.remove(&id);
            let Some(descriptor) = self
                .binding_attachments
                .get(&id)
                .map(|retained| retained.descriptor.clone())
            else {
                continue;
            };
            self.item_revision.in_flight.insert(id);
            effects.push(self.emit(RyeOsEffectKind::ReviseBindingAttachment {
                presentation_id: presentation_id.clone(),
                binding_attachment_id: descriptor.binding_attachment_id,
                binding_generation: descriptor.binding_generation,
                binding_digest: descriptor.binding_digest,
            }));
        }
        for id in std::mem::take(&mut self.item_revision.unchanged) {
            for kind in self
                .item_revision
                .stale_held
                .remove(&id)
                .unwrap_or_default()
            {
                effects.push(self.emit(kind));
            }
        }
        while !self.item_revision.ready.is_empty() && self.composition_write_available() {
            let (from, to, descriptor) = self.item_revision.ready.remove(0);
            let owner = self.presentation.as_ref().and_then(|presentation| {
                let (surface, _) = presentation
                    .surface_attachment_coordinates()
                    .into_iter()
                    .find(|(_, coordinate)| *coordinate == from)?;
                let binding = presentation.surfaces().get(&surface)?.binding().clone();
                Some(CompositionOwner::Surface {
                    surface_instance_id: surface,
                    binding,
                })
            });
            let Some(owner) = owner else {
                continue;
            };
            self.item_revision
                .awaiting_release
                .push((from.clone(), to.clone()));
            effects.extend(self.prepare_composition_owned(
                CompositionMutation::RebindAttachment {
                    from,
                    to,
                    descriptor: Box::new(descriptor),
                },
                owner,
            ));
        }
        let awaiting = std::mem::take(&mut self.item_revision.awaiting_release);
        for (from, to) in awaiting {
            let named = self.presentation.as_ref().is_some_and(|presentation| {
                presentation
                    .surface_attachment_coordinates()
                    .iter()
                    .any(|(_, coordinate)| *coordinate == from)
            });
            if named {
                self.item_revision.awaiting_release.push((from, to));
                continue;
            }
            effects.extend(self.reissue_stale_effects(&from, &to));
            if self.binding_attachment_is_locally_unused(
                &from.binding_attachment_id,
                from.binding_generation,
                &from.binding_digest,
            ) {
                effects.extend(self.release_binding_attachment(
                    from.binding_attachment_id,
                    from.binding_generation,
                    from.binding_digest,
                ));
            } else {
                self.item_revision.awaiting_release.push((from, to));
            }
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::InputBufferKey;
    use crate::ui::reducer::test_support::*;

    const HELD: &str = "fixture-attachment";

    fn hint(core: &mut RyeOsCore, change_set: Option<&str>) -> Vec<RyeOsEffect> {
        core.dispatch(RyeOsEvent::HintReceived {
            kind: ITEM_HINT_KIND.into(),
            payload: serde_json::json!({
                "item_ref": "view:test/home", "digest": "d", "revision": 1,
                "change_set": change_set,
            }),
        })
    }

    fn revisions(effects: &[RyeOsEffect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::ReviseBindingAttachment {
                    binding_attachment_id,
                    ..
                } => Some(binding_attachment_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn revised_answer(core: &RyeOsCore) -> (serde_json::Value, UiBindingAttachmentCoordinate) {
        let source = core.binding_attachments[HELD].descriptor.clone();
        let from = UiBindingAttachmentCoordinate {
            binding_attachment_id: source.binding_attachment_id.clone(),
            binding_generation: source.binding_generation,
            binding_digest: source.binding_digest.clone(),
        };
        let mut successor = source;
        successor.binding_attachment_id = "successor".into();
        successor.binding_generation += 1;
        let to = UiBindingAttachmentCoordinate {
            binding_attachment_id: "successor".into(),
            binding_generation: successor.binding_generation,
            binding_digest: successor.binding_digest.clone(),
        };
        (
            serde_json::json!({"revised": true, "from": from, "attachment": successor}),
            to,
        )
    }

    #[test]
    fn twenty_hints_of_one_change_set_request_one_revise_per_attachment() {
        let mut core = core(writable_session());
        let mut all = Vec::new();
        for _ in 0..20 {
            all.extend(hint(&mut core, Some("set-1")));
        }
        assert_eq!(revisions(&all), vec![HELD.to_string()]);
        // Without a change set each hint is its own, but one request stays in
        // flight per attachment.
        let more = hint(&mut core, None);
        assert!(revisions(&more).is_empty());
    }

    #[test]
    fn the_root_attachment_is_never_revised() {
        let mut core = core(writable_session());
        let root = core.root_attachment_id.clone();
        assert!(!core.revisable_attachment_ids().contains(&root));
        core.note_stale_attachment(&root);
        assert!(core.pump_item_revisions().is_empty());
    }

    #[test]
    fn a_stale_dispatch_revises_once_and_a_second_stale_is_not_retried() {
        let mut core = core(writable_session());
        core.note_stale_attachment(HELD);
        assert_eq!(revisions(&core.pump_item_revisions()), vec![HELD]);
        core.finish_attachment_revision(HELD, None);
        core.note_stale_attachment(HELD);
        assert!(core.pump_item_revisions().is_empty());
    }

    #[test]
    fn an_unchanged_answer_swaps_nothing() {
        let mut core = core(writable_session());
        core.note_stale_attachment(HELD);
        core.pump_item_revisions();
        core.finish_attachment_revision(
            HELD,
            Some(serde_json::json!({"revised": false, "attachment": {}})),
        );
        assert!(core.pump_item_revisions().is_empty());
        assert!(pending_prefix(&core).is_empty());
    }

    #[test]
    fn a_swap_keeps_tile_state_and_drafts_and_releases_the_source() {
        let mut core = core(writable_session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let surface = core.active_surface().unwrap().id().clone();
        let (set_id, tile_id, instance, view_ref) = {
            let set = core.active_view_set().unwrap();
            let tile = &set.tiles[&set.focused_tile];
            (
                set.id,
                set.focused_tile,
                tile.instance_key.clone(),
                tile.view.view_ref.clone(),
            )
        };
        let from_scope = InputBufferKey::binding_scope(&UiBindingAttachmentCoordinate {
            binding_attachment_id: HELD.into(),
            binding_generation: core.binding_attachments[HELD].descriptor.binding_generation,
            binding_digest: core.binding_attachments[HELD]
                .descriptor
                .binding_digest
                .clone(),
        });
        let draft = crate::ui::model::RyeOsInputState {
            text: "draft".into(),
            cursor: 5,
            filter_field: 0,
        };
        let key = InputBufferKey {
            view_instance_key: instance.clone(),
            binding_scope: from_scope,
            view_ref: view_ref.clone(),
            input_id: "line".into(),
            target_scope: None,
        };
        core.input_buffers.insert(key.storage_key(), draft.clone());
        let local_before = format!(
            "{:?}",
            core.active_view_set().unwrap().tiles[&tile_id].local
        );

        let effects = hint(&mut core, Some("set-1"));
        assert_eq!(revisions(&effects), vec![HELD.to_string()]);
        let (answer, to) = revised_answer(&core);
        core.finish_attachment_revision(HELD, Some(answer));
        assert!(core.pump_item_revisions().is_empty());
        let prepared = pending_prefix(&core);
        assert!(prepared.iter().any(|event| matches!(&event.kind,
            crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation)
                if operation.composition().is_some_and(|request| matches!(
                    request.mutation,
                    crate::ui::composition_operation::CompositionMutation::RebindAttachment { .. })))));
        let mut later = settle_pending_prefix(&mut core, &producer);
        later.extend(core.pump_item_revisions());

        assert_eq!(core.active_surface().unwrap().binding(), &to);
        let set = core.active_view_set().unwrap();
        assert_eq!(set.id, set_id);
        assert_eq!(set.tiles[&tile_id].instance_key, instance);
        assert_eq!(format!("{:?}", set.tiles[&tile_id].local), local_before);
        assert!(core.active_surface().unwrap().id() == &surface);
        let to_key = InputBufferKey {
            binding_scope: InputBufferKey::binding_scope(&to),
            ..key.clone()
        };
        assert_eq!(core.input_buffers[&to_key.storage_key()].text, "draft");
        assert!(!core.input_buffers.contains_key(&key.storage_key()));
        // Nothing names the source any longer, so it is released exactly once.
        let released = later
            .iter()
            .filter(|effect| {
                matches!(&effect.kind,
                    RyeOsEffectKind::ReleaseBindingAttachment { binding_attachment_id, .. }
                        if binding_attachment_id == HELD)
            })
            .count();
        assert_eq!(released, 1);
    }

    fn invocation(
        core: &RyeOsCore,
        coordinate: crate::ui::binding::UiBindingCoordinate,
    ) -> RyeOsEffectKind {
        let held = &core.binding_attachments[HELD].descriptor;
        RyeOsEffectKind::InvokeBinding {
            request: crate::ui::binding::UiBindingRequest {
                binding_attachment_id: held.binding_attachment_id.clone(),
                binding_generation: held.binding_generation,
                binding_digest: held.binding_digest.clone(),
                coordinate,
                payload: crate::ui::binding::UiBindingPayload::Tokens {
                    tokens: vec!["go".into()],
                    arguments: serde_json::Value::Null,
                },
            },
            request_bounds: held.binding_request_bounds,
            intent: crate::ui::effect::InvokeIntent::Service,
            success_notice: None,
            invocation_origin: None,
            input_origin: None,
            route_seq: None,
            ratchet_on_thread_id: false,
        }
    }

    fn swap(
        core: &mut RyeOsCore,
        producer: &crate::ui::binding::UiPresentationProducer,
    ) -> Vec<RyeOsEffect> {
        let (answer, _) = revised_answer(core);
        core.finish_attachment_revision(HELD, Some(answer));
        core.pump_item_revisions();
        let mut later = settle_pending_prefix(core, producer);
        later.extend(core.pump_item_revisions());
        later
    }

    #[test]
    fn a_refused_invocation_is_reissued_once_against_the_successor() {
        let mut core = core(writable_session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let kind = invocation(&core, crate::ui::binding::UiBindingCoordinate::SurfaceRoute);
        core.note_stale_effect(&kind);
        assert_eq!(revisions(&core.pump_item_revisions()), vec![HELD]);
        let later = swap(&mut core, &producer);
        let reissued = later
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::InvokeBinding { request, .. } => Some(request.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reissued.len(), 1);
        assert_eq!(reissued[0].binding_attachment_id, "successor");
        assert_eq!(
            reissued[0].binding_generation,
            core.binding_attachments["successor"]
                .descriptor
                .binding_generation
        );
        // A second stale for the same attachment is not held again.
        core.note_stale_effect(&kind);
        assert!(core.pump_item_revisions().is_empty());
    }

    #[test]
    fn an_invocation_whose_view_left_the_successor_is_not_retried() {
        let mut core = core(writable_session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let kind = invocation(
            &core,
            crate::ui::binding::UiBindingCoordinate::Affordance {
                view_ref: "view:gone".into(),
                affordance_id: "x".into(),
            },
        );
        core.note_stale_effect(&kind);
        core.pump_item_revisions();
        let later = swap(&mut core, &producer);
        assert!(
            later
                .iter()
                .all(|effect| !matches!(effect.kind, RyeOsEffectKind::InvokeBinding { .. }))
        );
        assert!(
            core.ui
                .notices
                .iter()
                .any(|notice| notice.message.contains("ui_binding_view_removed"))
        );
    }

    #[test]
    fn a_hint_naming_no_held_item_requests_nothing() {
        let mut core = core(writable_session());
        let effects = core.dispatch(RyeOsEvent::HintReceived {
            kind: ITEM_HINT_KIND.into(),
            payload: serde_json::json!({
                "item_ref": "view:unrelated", "digest": "d", "revision": 1,
                "change_set": null,
            }),
        });
        assert!(revisions(&effects).is_empty());
        assert!(core.pump_item_revisions().is_empty());
    }

    #[test]
    fn a_missed_hint_then_the_catch_up_hint_gives_the_same_single_swap() {
        let mut core = core(writable_session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        // The first announcement never arrived. The daemon's catch-up re-read
        // later delivers the same revision once, and exactly one swap follows.
        let effects = hint(&mut core, Some("set-9"));
        assert_eq!(revisions(&effects), vec![HELD.to_string()]);
        let _ = swap(&mut core, &producer);
        assert_eq!(
            core.active_surface()
                .unwrap()
                .binding()
                .binding_attachment_id,
            "successor"
        );
        let again = hint(&mut core, Some("set-9"));
        assert!(revisions(&again).is_empty());
    }
}
