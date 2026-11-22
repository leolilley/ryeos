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
    /// Change sets already swept; later hints of one set coalesce into it.
    seen_change_sets: BTreeSet<String>,
    /// Attachment ids that still need a revise request.
    pending: BTreeSet<String>,
    /// Attachment ids with a revise request outstanding.
    in_flight: BTreeSet<String>,
    /// Published successors waiting for a free composition write.
    ready: Vec<PendingRebind>,
    /// Swapped sources to release once nothing names them.
    awaiting_release: Vec<UiBindingAttachmentCoordinate>,
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

    /// Record one `item.hint`. Hints sharing a change set add no second sweep.
    pub(crate) fn note_item_hint(&mut self, payload: &serde_json::Value) {
        if let Some(change_set) = payload
            .get("change_set")
            .and_then(serde_json::Value::as_str)
        {
            if self.item_revision.seen_change_sets.contains(change_set) {
                return;
            }
            if self.item_revision.seen_change_sets.len() >= MAX_SEEN_CHANGE_SETS {
                self.item_revision.seen_change_sets.clear();
            }
            self.item_revision
                .seen_change_sets
                .insert(change_set.to_owned());
        }
        let ids = self.revisable_attachment_ids();
        self.item_revision.pending.extend(ids);
    }

    /// Dispatch found a held attachment stale: revise it once and let the next
    /// attempt use the successor. A second stale for the same attachment id is
    /// reported as before.
    pub(crate) fn note_stale_attachment(&mut self, attachment_id: &str) {
        if attachment_id != self.root_attachment_id
            && self.revisable_attachment_ids().contains(attachment_id)
            && self
                .item_revision
                .stale_retried
                .insert(attachment_id.to_owned())
        {
            self.item_revision.pending.insert(attachment_id.to_owned());
        }
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
            return;
        };
        if data.get("revised").and_then(serde_json::Value::as_bool) != Some(true) {
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
        if state.pending.is_empty() && state.ready.is_empty() && state.awaiting_release.is_empty() {
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
            self.item_revision.awaiting_release.push(from.clone());
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
        for from in awaiting {
            let named = self.presentation.as_ref().is_some_and(|presentation| {
                presentation
                    .surface_attachment_coordinates()
                    .iter()
                    .any(|(_, coordinate)| *coordinate == from)
            });
            if !named
                && self.binding_attachment_is_locally_unused(
                    &from.binding_attachment_id,
                    from.binding_generation,
                    &from.binding_digest,
                )
            {
                effects.extend(self.release_binding_attachment(
                    from.binding_attachment_id,
                    from.binding_generation,
                    from.binding_digest,
                ));
            } else {
                self.item_revision.awaiting_release.push(from);
            }
        }
        effects
    }
}
