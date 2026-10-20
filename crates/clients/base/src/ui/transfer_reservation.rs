//! Exact destination allocation retained by acceptance, not admission authority.
//!
//! The daemon supplies freshly admitted bindings. This decoder proves only
//! their placement/commitment consistency; copied coordinates cannot admit them.
use super::{
    attachment::SelectionAttachment,
    binding::{UiBindingAttachment, UiBindingAttachmentCoordinate},
    composition_delta::UiCompositionMountAllocation,
    composition_identity::UiSurfaceMountManifest,
    composition_operation::CompositionPlacement,
};
use crate::ids::{PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId, ViewSetId};
use ryeos_ui_presentation_contract::{
    payload_digest::seat_payload_digest, transfer::TransferIdentity,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferMountAllocation {
    pub source_instance: RyeOsViewInstanceKey,
    pub destination_instance: RyeOsViewInstanceKey,
    pub binding: UiBindingAttachmentCoordinate,
    /// Destination subject after applying the set-id projection. Copy never
    /// carries live pinned values; its exported subject is already unresolved.
    pub destination_subject: SelectionAttachment,
    /// Ordered one-for-one with the source mount's retained lens frames.
    /// These are destination admissions, never reusable source authority.
    pub lens_frames: Vec<UiTransferLensFrameAllocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferLensFrameAllocation {
    pub binding: UiBindingAttachmentCoordinate,
    pub destination_subject: SelectionAttachment,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferSetAllocation {
    pub source_handle: String,
    pub source_view_set_id: ViewSetId,
    pub destination_view_set_id: ViewSetId,
    #[serde(deserialize_with = "required_optional_tile_id")]
    pub focused_tile_id: Option<crate::ids::TileId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiTransferDestinationPlacement {
    /// Existing canonical allocator retains nested geometry and ordered IDs.
    Tabs {
        manifest: UiSurfaceMountManifest,
        sets: Vec<UiTransferSetAllocation>,
        /// Exact destination selection derived from the exported source
        /// handle. Explicit null preserves a source selection outside export.
        #[serde(deserialize_with = "required_optional_view_set_id")]
        active_view_set_id: Option<ViewSetId>,
    },
    View {
        surface_instance_id: SurfaceInstanceId,
        view_set_id: ViewSetId,
        allocation: UiCompositionMountAllocation,
        placement: CompositionPlacement,
    },
    SharedDock {
        edge: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferReservation {
    pub schema: u32,
    pub presentation_id: PresentationId,
    pub expected_revision: u64,
    pub allocation_start: u64,
    pub next_mount_id: u64,
    pub placement: UiTransferDestinationPlacement,
    pub mounts: Vec<UiTransferMountAllocation>,
    /// Signed public metadata for each freshly admitted destination binding.
    /// This is never a session grant or a replacement for private admission.
    pub descriptors: Vec<UiBindingAttachment>,
}

impl UiTransferReservation {
    /// Allocate an ordered collection into one already admitted destination
    /// Surface. Subjects and return frames use the same atomic set-ID map.
    pub fn reserve_view_sets(
        transfer: &TransferIdentity,
        export: &super::transfer_export::UiTransferExport,
        destination: &super::composition_replay::UiCompositionReplay,
        surface_instance_id: SurfaceInstanceId,
        bindings: &BTreeMap<String, UiBindingAttachmentCoordinate>,
        descriptors: Vec<UiBindingAttachment>,
        max_bytes: u64,
    ) -> Result<Self, String> {
        export.validate_for(transfer, max_bytes)?;
        let super::transfer_export::UiTransferExportContent::ViewSets { active_set, sets } =
            &export.content
        else {
            return Err("tab reservation requires an ordered view-set export".into());
        };
        if destination.composition.id().as_str() != transfer.destination.presentation_id
            || destination.revision < transfer.destination.revision
            || !destination
                .composition
                .surfaces()
                .contains_key(&surface_instance_id)
        {
            return Err("tab reservation destination Surface or frontier differs".into());
        }
        let active_seed = active_set
            .as_ref()
            .map(|handle| {
                sets.iter()
                    .find(|set| &set.handle == handle)
                    .map(|set| set.seed.id.as_str())
                    .ok_or("active exported set is absent")
            })
            .transpose()?;
        let seeds = sets
            .iter()
            .map(|set| {
                Ok((
                    set.seed.clone(),
                    bindings
                        .get(&set.binding.binding_attachment_id)
                        .ok_or("exported set has no fresh destination binding")?
                        .clone(),
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let manifest = UiSurfaceMountManifest::allocate(
            destination.composition.id().clone(),
            surface_instance_id.clone(),
            destination.next_mount_id,
            seeds,
            active_seed,
        )?;
        let allocated_sets = tab_set_allocations(sets, &manifest)?;
        let set_map = allocated_sets
            .iter()
            .map(|set| (set.source_view_set_id, set.destination_view_set_id))
            .collect::<BTreeMap<_, _>>();
        let mut mounts = Vec::new();
        for (source, tab) in sets.iter().zip(&manifest.tabs) {
            if source.mounts.len() != tab.tile_ids.len() {
                return Err("exported mount order differs from nested allocation".into());
            }
            for (mount, tile) in source.mounts.iter().zip(&tab.tile_ids) {
                let mut frames = reserve_lens_frames(&mount.lens_frames, bindings)?;
                for (frame, source_frame) in frames.iter_mut().zip(&mount.lens_frames) {
                    frame.destination_subject = remap_subject(&source_frame.attachment, &set_map)?;
                }
                mounts.push(UiTransferMountAllocation {
                    source_instance: mount.source_instance.clone(),
                    destination_instance: RyeOsViewInstanceKey::surface_tile(
                        destination.composition.id(),
                        &surface_instance_id,
                        *tile,
                    ),
                    binding: bindings
                        .get(&mount.binding.binding_attachment_id)
                        .ok_or("exported View has no fresh destination binding")?
                        .clone(),
                    destination_subject: remap_subject(&mount.subject, &set_map)?,
                    lens_frames: frames,
                });
            }
        }
        let reservation = Self {
            schema: 2,
            presentation_id: destination.composition.id().clone(),
            expected_revision: destination.revision,
            allocation_start: manifest.allocation_start,
            next_mount_id: manifest.next_mount_id,
            placement: UiTransferDestinationPlacement::Tabs {
                active_view_set_id: manifest.active_view_set_id,
                manifest,
                sets: allocated_sets,
            },
            mounts,
            descriptors,
        };
        reservation.validate_export(transfer, export, max_bytes)?;
        Ok(reservation)
    }

    /// Reserve an exported shared dock without allocating a Surface or tile.
    /// Fresh bindings are supplied by the existing admission owner; an occupied
    /// destination edge is never silently replaced.
    pub fn reserve_shared_dock(
        transfer: &TransferIdentity,
        export: &super::transfer_export::UiTransferExport,
        destination: &super::composition_replay::UiCompositionReplay,
        edge: &str,
        bindings: &BTreeMap<String, UiBindingAttachmentCoordinate>,
        descriptors: Vec<UiBindingAttachment>,
        max_bytes: u64,
    ) -> Result<Self, String> {
        export.validate_for(transfer, max_bytes)?;
        let super::transfer_export::UiTransferExportContent::SharedDock { mount, .. } =
            &export.content
        else {
            return Err("shared-dock reservation requires a shared-dock export".into());
        };
        if destination.composition.id().as_str() != transfer.destination.presentation_id
            || destination.revision < transfer.destination.revision
            || destination.composition.docks().contains_key(edge)
        {
            return Err("shared-dock destination identity, frontier or vacancy differs".into());
        }
        let destination_instance =
            RyeOsViewInstanceKey::shared_dock(destination.composition.id(), edge)
                .map_err(str::to_owned)?;
        let reservation = Self {
            schema: 2,
            presentation_id: destination.composition.id().clone(),
            expected_revision: destination.revision,
            allocation_start: destination.next_mount_id,
            next_mount_id: destination.next_mount_id,
            placement: UiTransferDestinationPlacement::SharedDock {
                edge: edge.to_owned(),
            },
            mounts: vec![UiTransferMountAllocation {
                source_instance: mount.source_instance.clone(),
                destination_instance,
                binding: bindings
                    .get(&mount.binding.binding_attachment_id)
                    .ok_or("source dock has no fresh destination binding mapping")?
                    .clone(),
                destination_subject: mount.subject.clone(),
                lens_frames: reserve_lens_frames(&mount.lens_frames, bindings)?,
            }],
            descriptors,
        };
        reservation.validate_export(transfer, export, max_bytes)?;
        Ok(reservation)
    }

    /// Reserve one exported Surface View against a checked destination replay.
    /// The binding map contains fresh admissions supplied by the caller; this
    /// pure builder neither admits bindings nor installs exported local state.
    pub fn reserve_surface_mount(
        transfer: &TransferIdentity,
        export: &super::transfer_export::UiTransferExport,
        destination: &super::composition_replay::UiCompositionReplay,
        surface_instance_id: SurfaceInstanceId,
        view_set_id: ViewSetId,
        placement: CompositionPlacement,
        bindings: &BTreeMap<String, UiBindingAttachmentCoordinate>,
        descriptors: Vec<UiBindingAttachment>,
        max_bytes: u64,
    ) -> Result<Self, String> {
        export.validate_for(transfer, max_bytes)?;
        let super::transfer_export::UiTransferExportContent::SurfaceMount { mount, .. } =
            &export.content
        else {
            return Err("single-view reservation requires a Surface mount export".into());
        };
        if destination.composition.id().as_str() != transfer.destination.presentation_id
            || destination.revision < transfer.destination.revision
        {
            return Err(
                "destination replay predates transfer preparation or has a different identity"
                    .into(),
            );
        }
        let set = destination
            .composition
            .view_set(&surface_instance_id, view_set_id)
            .ok_or("destination Surface tab is not mounted")?;
        let allocation = UiCompositionMountAllocation::allocate(
            destination.next_mount_id,
            1,
            usize::from(!matches!(&placement, CompositionPlacement::GroupTab { .. })),
        )?;
        super::composition_delta::validate_allocated_view_placement(set, &allocation, &placement)?;
        let binding = bindings
            .get(&mount.binding.binding_attachment_id)
            .ok_or("source mount has no fresh destination binding mapping")?
            .clone();
        let lens_frames = reserve_lens_frames(&mount.lens_frames, bindings)?;
        let reservation = Self {
            schema: 2,
            presentation_id: destination.composition.id().clone(),
            expected_revision: destination.revision,
            allocation_start: allocation.allocation_start,
            next_mount_id: allocation.next_mount_id,
            placement: UiTransferDestinationPlacement::View {
                surface_instance_id: surface_instance_id.clone(),
                view_set_id,
                allocation: allocation.clone(),
                placement,
            },
            mounts: vec![UiTransferMountAllocation {
                source_instance: mount.source_instance.clone(),
                destination_instance: RyeOsViewInstanceKey::surface_tile(
                    destination.composition.id(),
                    &surface_instance_id,
                    allocation.tile_ids[0],
                ),
                binding,
                destination_subject: mount.subject.clone(),
                lens_frames,
            }],
            descriptors,
        };
        reservation.validate_export(transfer, export, max_bytes)?;
        Ok(reservation)
    }

    /// Decode the mandatory acceptance body, preserving the one wire digest.
    /// Successful decoding does not prove the daemon admitted any binding.
    pub fn from_acceptance(
        record: &ryeos_ui_presentation_contract::transfer::TransferRecord,
        max_bytes: u64,
    ) -> Result<Self, String> {
        record.validate_bounds(max_bytes)?;
        let ryeos_ui_presentation_contract::transfer::TransferPhase::Accepted {
            reservation,
            import_digest,
            ..
        } = &record.phase
        else {
            return Err("transfer reservation requires destination acceptance".into());
        };
        let decoded: Self = serde_json::from_value(reservation.clone())
            .map_err(|error| format!("invalid closed destination reservation: {error}"))?;
        decoded.validate_bounds(&record.transfer, max_bytes)?;
        if decoded.digest()? != *import_digest {
            return Err("decoded destination reservation changed its commitment".into());
        }
        Ok(decoded)
    }

    /// Source/destination bijection and nested geometry consistency only.
    /// Source membership and incoming followers remain authenticated checks.
    pub fn validate_export(
        &self,
        transfer: &TransferIdentity,
        export: &super::transfer_export::UiTransferExport,
        max_bytes: u64,
    ) -> Result<(), String> {
        self.validate_bounds(transfer, max_bytes)?;
        export.validate_for(transfer, max_bytes)?;
        let mounts = export.mounts();
        if mounts.len() != self.mounts.len()
            || mounts.iter().zip(&self.mounts).any(|(source, allocated)| {
                source.source_instance != allocated.source_instance
                    || source.binding.binding_attachment_id
                        == allocated.binding.binding_attachment_id
            })
        {
            return Err(
                "reservation differs from exact export order or reuses source admission".into(),
            );
        }
        let mut binding_pairs = Vec::new();
        for (source, allocated) in mounts.iter().zip(&self.mounts) {
            binding_pairs.push((&source.binding, &allocated.binding));
            binding_pairs.extend(
                source
                    .lens_frames
                    .iter()
                    .zip(&allocated.lens_frames)
                    .map(|(source, destination)| (&source.binding, &destination.binding)),
            );
        }
        if let (Some(sets), UiTransferDestinationPlacement::Tabs { manifest, .. }) =
            (export.sets(), &self.placement)
        {
            binding_pairs.extend(
                sets.iter()
                    .zip(&manifest.tabs)
                    .map(|(source, destination)| (&source.binding, &destination.binding)),
            );
        }
        validate_binding_bijection(&binding_pairs)?;
        // This runs in reservation construction BEFORE Accepted publication,
        // as well as during replay. Never commit MOVE into a destination that
        // cannot retain the complete draft under its freshly admitted bound.
        for (source, allocated) in mounts.iter().zip(&self.mounts) {
            for draft in &source.drafts {
                let fresh = super::composition_replay::mapped_transfer_draft_binding(
                    &draft.key,
                    std::iter::once((
                        source.view_ref.as_str(),
                        &source.binding,
                        &allocated.binding,
                    ))
                    .chain(
                        source.lens_frames.iter().zip(&allocated.lens_frames).map(
                            |(source, destination)| {
                                (
                                    source.view_ref.as_str(),
                                    &source.binding,
                                    &destination.binding,
                                )
                            },
                        ),
                    ),
                )?;
                let descriptor = self
                    .descriptors
                    .iter()
                    .find(|descriptor| {
                        descriptor.binding_attachment_id == fresh.binding_attachment_id
                            && descriptor.binding_generation == fresh.binding_generation
                            && descriptor.binding_digest == fresh.binding_digest
                    })
                    .ok_or("reserved draft has no exact destination descriptor")?;
                draft
                    .state
                    .validate_retained_binding(&draft.key, descriptor)?;
            }
        }
        if let UiTransferDestinationPlacement::Tabs {
            manifest,
            sets: allocated_sets,
            active_view_set_id,
        } = &self.placement
        {
            let sets = export
                .sets()
                .ok_or("tab reservation requires exported view sets")?;
            if manifest.tabs.len() != sets.len()
                || manifest
                    .tabs
                    .iter()
                    .zip(sets)
                    .any(|(tab, set)| tab.seed != set.seed)
            {
                return Err("reserved nested composition differs from source export".into());
            }
            let expected_sets = tab_set_allocations(sets, manifest)?;
            if &expected_sets != allocated_sets {
                return Err("reserved set/focus projection differs from source export".into());
            }
            let expected_active = match &export.content {
                super::transfer_export::UiTransferExportContent::ViewSets {
                    active_set, ..
                } => active_set
                    .as_ref()
                    .map(|handle| {
                        expected_sets
                            .iter()
                            .find(|set| &set.source_handle == handle)
                            .map(|set| set.destination_view_set_id)
                            .ok_or_else(|| "active exported set has no destination".to_string())
                    })
                    .transpose()?,
                _ => unreachable!("sets() established collection content"),
            };
            if *active_view_set_id != expected_active {
                return Err("reserved active set differs from source selection".into());
            }

            let set_map = expected_sets
                .iter()
                .map(|set| (set.source_view_set_id, set.destination_view_set_id))
                .collect::<BTreeMap<_, _>>();
            for (source, allocated) in mounts.iter().zip(&self.mounts) {
                let expected_subject = remap_subject(&source.subject, &set_map)?;
                if allocated.destination_subject != expected_subject {
                    return Err("reserved subject differs from atomic set projection".into());
                }
                validate_lens_allocations(export.mode, source, allocated, &set_map)?;
            }
        } else {
            for (source, allocated) in mounts.iter().zip(&self.mounts) {
                if allocated.destination_subject != source.subject {
                    return Err("singular reservation changed its exported subject".into());
                }
                validate_lens_allocations(export.mode, source, allocated, &BTreeMap::new())?;
            }
            if export.sets().is_some() {
                return Err(
                    "view-set collection cannot be flattened into a singular reservation".into(),
                );
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, String> {
        seat_payload_digest(&serde_json::to_value(self).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }

    /// Shape only. The authenticated owner must separately prove source export
    /// closure, equality with the current destination replay revision, available
    /// placement and admission. Preparation's revision is only the lower bound.
    pub fn validate_for(&self, transfer: &TransferIdentity) -> Result<(), String> {
        if self.schema != 2
            || self.presentation_id.as_str() != transfer.destination.presentation_id
            || self.expected_revision < transfer.destination.revision
            || self.allocation_start == 0
            || self.next_mount_id < self.allocation_start
        {
            return Err("transfer reservation identity or frontier differs".into());
        }
        {
            let mut expected = BTreeMap::new();
            let mut remember = |binding: &UiBindingAttachmentCoordinate| -> Result<(), String> {
                if let Some(prior) =
                    expected.insert(binding.binding_attachment_id.clone(), binding.clone())
                    && prior != *binding
                {
                    return Err(
                        "destination reservation reuses a binding ID with another coordinate"
                            .into(),
                    );
                }
                Ok(())
            };
            for mount in &self.mounts {
                remember(&mount.binding)?;
                for frame in &mount.lens_frames {
                    remember(&frame.binding)?;
                }
            }
            if let UiTransferDestinationPlacement::Tabs { manifest, .. } = &self.placement {
                for tab in &manifest.tabs {
                    remember(&tab.binding)?;
                }
            }
            let mut seen = BTreeSet::new();
            for descriptor in &self.descriptors {
                descriptor.validate().map_err(str::to_owned)?;
                let coordinate = expected
                    .get(&descriptor.binding_attachment_id)
                    .ok_or("destination descriptor has no reserved binding")?;
                if !seen.insert(descriptor.binding_attachment_id.clone())
                    || descriptor.binding_generation != coordinate.binding_generation
                    || descriptor.binding_digest != coordinate.binding_digest
                {
                    return Err("destination descriptor differs from its reserved binding".into());
                }
            }
            if seen.len() != expected.len() {
                return Err(
                    "destination reservation has no exact descriptor for every binding".into(),
                );
            }
        }
        let mut sources = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        for mount in &self.mounts {
            let source_owner = mount
                .source_instance
                .mount_owner()
                .ok_or("source reservation mount has no owner")?;
            let destination_owner = mount
                .destination_instance
                .mount_owner()
                .ok_or("destination reservation mount has no owner")?;
            let presentation = |owner: crate::ids::UiViewMountOwner| match owner {
                crate::ids::UiViewMountOwner::Shared { presentation_id }
                | crate::ids::UiViewMountOwner::Surface {
                    presentation_id, ..
                } => presentation_id,
            };
            if presentation(source_owner).as_str() != transfer.source.presentation_id
                || presentation(destination_owner) != self.presentation_id
                || !sources.insert(mount.source_instance.clone())
                || !destinations.insert(mount.destination_instance.clone())
                || mount.binding.binding_attachment_id.is_empty()
                || mount.binding.binding_generation == 0
                || mount.binding.binding_digest.is_empty()
                || mount.lens_frames.iter().any(|frame| {
                    super::composition_operation::binding(&frame.binding).is_err()
                        || invalid_subject(&frame.destination_subject)
                })
            {
                return Err("transfer allocation is not a bijection of exact owned mounts".into());
            }
        }
        match &self.placement {
            UiTransferDestinationPlacement::Tabs {
                manifest,
                sets,
                active_view_set_id,
            } => {
                manifest.validate()?;
                if manifest.tabs.is_empty()
                    || manifest.presentation_id != self.presentation_id
                    || manifest.allocation_start != self.allocation_start
                    || manifest.next_mount_id != self.next_mount_id
                {
                    return Err("transfer tab manifest differs from reserved frontier".into());
                }
                let expected = manifest
                    .tabs
                    .iter()
                    .flat_map(|tab| {
                        tab.tile_ids.iter().map(|tile| {
                            RyeOsViewInstanceKey::surface_tile(
                                &self.presentation_id,
                                &manifest.surface_instance_id,
                                *tile,
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                if expected.len() != self.mounts.len()
                    || expected
                        .iter()
                        .zip(&self.mounts)
                        .any(|(key, mount)| key != &mount.destination_instance)
                    || self
                        .mounts
                        .iter()
                        .any(|mount| mount.source_instance.surface_tile_id().is_none())
                {
                    return Err(
                        "transfer mount order/bindings differ from canonical tab allocation".into(),
                    );
                }
                if sets.len() != manifest.tabs.len()
                    || {
                        let mut handles = BTreeSet::new();
                        let mut source_ids = BTreeSet::new();
                        let mut destination_ids = BTreeSet::new();
                        sets.iter().zip(&manifest.tabs).any(|(set, tab)| {
                            set.source_handle.is_empty()
                                || !handles.insert(set.source_handle.as_str())
                                || set.source_view_set_id.0 == 0
                                || !source_ids.insert(set.source_view_set_id)
                                || !destination_ids.insert(set.destination_view_set_id)
                                || set.destination_view_set_id != tab.view_set_id
                                || set
                                    .focused_tile_id
                                    .is_some_and(|tile| !tab.tile_ids.contains(&tile))
                        })
                    }
                    || active_view_set_id.is_some_and(|active| {
                        !sets.iter().any(|set| set.destination_view_set_id == active)
                    })
                {
                    return Err(
                        "transfer set selection/focus differs from canonical allocation".into(),
                    );
                }
            }
            UiTransferDestinationPlacement::View {
                surface_instance_id,
                view_set_id,
                allocation,
                placement,
            } => {
                allocation.validate()?;
                let groups =
                    usize::from(!matches!(placement, CompositionPlacement::GroupTab { .. }));
                if view_set_id.0 == 0
                    || view_set_id.0 >= self.allocation_start
                    || !placement_precedes_frontier(placement, self.allocation_start, *view_set_id)
                    || allocation.allocation_start != self.allocation_start
                    || allocation.next_mount_id != self.next_mount_id
                    || allocation.tile_ids.len() != 1
                    || allocation.group_ids.len() != groups
                    || self.mounts.len() != 1
                    || self.mounts[0].source_instance.surface_tile_id().is_none()
                    || self.mounts[0].destination_instance
                        != RyeOsViewInstanceKey::surface_tile(
                            &self.presentation_id,
                            surface_instance_id,
                            allocation.tile_ids[0],
                        )
                {
                    return Err("single-view reservation differs from exact allocated mount".into());
                }
            }
            UiTransferDestinationPlacement::SharedDock { edge } => {
                let key = RyeOsViewInstanceKey::shared_dock(&self.presentation_id, edge)
                    .map_err(str::to_string)?;
                if self.mounts.len() != 1
                    || self.mounts[0].destination_instance != key
                    || !is_shared_dock(&self.mounts[0].source_instance)
                    || self.allocation_start != self.next_mount_id
                {
                    return Err(
                        "shared-dock reservation invents a Surface or allocated tile".into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate_bounds(
        &self,
        transfer: &TransferIdentity,
        max_bytes: u64,
    ) -> Result<(), String> {
        self.validate_for(transfer)?;
        let len = serde_json::to_vec(self).map_err(|e| e.to_string())?.len() as u64;
        if max_bytes == 0 || len > max_bytes {
            return Err("transfer reservation exceeds selected seat bounds".into());
        }
        Ok(())
    }
}

/// Data projection of the already-admitted complete binding closure. Repeated
/// source bindings retain their one fresh counterpart; no admission occurs.
fn reserve_lens_frames(
    frames: &[super::composition_lens::RetainedLensFrame],
    bindings: &BTreeMap<String, UiBindingAttachmentCoordinate>,
) -> Result<Vec<UiTransferLensFrameAllocation>, String> {
    frames
        .iter()
        .map(|frame| {
            let binding = bindings
                .get(&frame.binding.binding_attachment_id)
                .ok_or("source lens frame has no fresh destination binding mapping")?
                .clone();
            Ok(UiTransferLensFrameAllocation {
                binding,
                destination_subject: frame.attachment.clone(),
            })
        })
        .collect()
}

fn tab_set_allocations(
    sets: &[super::transfer_export::UiTransferSetExport],
    manifest: &UiSurfaceMountManifest,
) -> Result<Vec<UiTransferSetAllocation>, String> {
    if sets.len() != manifest.tabs.len() {
        return Err("reserved tab count differs from source export".into());
    }
    sets.iter()
        .zip(&manifest.tabs)
        .map(|(source, destination)| {
            let focused_tile_id =
                source
                    .focused_view
                    .map(|index| {
                        destination.tile_ids.get(index).copied().ok_or_else(|| {
                            "focused export mount has no destination tile".to_string()
                        })
                    })
                    .transpose()?;
            Ok(UiTransferSetAllocation {
                source_handle: source.handle.clone(),
                source_view_set_id: source.source_view_set_id,
                destination_view_set_id: destination.view_set_id,
                focused_tile_id,
            })
        })
        .collect()
}

fn remap_subject(
    subject: &SelectionAttachment,
    set_map: &BTreeMap<ViewSetId, ViewSetId>,
) -> Result<SelectionAttachment, String> {
    match subject {
        SelectionAttachment::FollowViewSet { view_set_id } => {
            Ok(SelectionAttachment::FollowViewSet {
                view_set_id: *set_map
                    .get(view_set_id)
                    .ok_or("followed source set has no destination allocation")?,
            })
        }
        SelectionAttachment::Pinned {
            values,
            fingerprint,
        } => Ok(SelectionAttachment::Pinned {
            values: values.clone(),
            fingerprint: fingerprint.clone(),
        }),
        SelectionAttachment::RequiredSubject { input, facets } => {
            Ok(SelectionAttachment::RequiredSubject {
                input: input.clone(),
                facets: facets.clone(),
            })
        }
    }
}

/// The existing staging owner admits once per distinct source binding. Repeated
/// references to that same admission may share its fresh destination binding;
/// distinct admissions must never collapse, and no source coordinate survives.
fn validate_binding_bijection(
    pairs: &[(
        &UiBindingAttachmentCoordinate,
        &UiBindingAttachmentCoordinate,
    )],
) -> Result<(), String> {
    let source_ids = pairs
        .iter()
        .map(|(source, _)| source.binding_attachment_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut forward = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    for &(source, destination) in pairs {
        if source_ids.contains(destination.binding_attachment_id.as_str()) {
            return Err("destination binding reuses source closure admission".into());
        }
        let pair = (source, destination);
        if forward
            .insert(source.binding_attachment_id.as_str(), pair)
            .is_some_and(|previous| previous != pair)
            || reverse
                .insert(destination.binding_attachment_id.as_str(), pair)
                .is_some_and(|previous| previous != pair)
        {
            return Err("transfer binding projection is not one exact bijection".into());
        }
    }
    Ok(())
}

fn validate_lens_allocations(
    mode: super::transfer_export::UiTransferExportMode,
    source: &super::transfer_export::UiTransferMountExport,
    allocated: &UiTransferMountAllocation,
    set_map: &BTreeMap<ViewSetId, ViewSetId>,
) -> Result<(), String> {
    if source.lens_frames.len() != allocated.lens_frames.len() {
        return Err("reserved lens frame count differs from source stack".into());
    }
    for (source_frame, destination_frame) in source.lens_frames.iter().zip(&allocated.lens_frames) {
        if source_frame.binding.binding_attachment_id
            == destination_frame.binding.binding_attachment_id
        {
            return Err("reserved lens frame reuses source admission".into());
        }
        let expected = remap_subject(&source_frame.attachment, set_map)?;
        if destination_frame.destination_subject != expected {
            return Err("reserved lens subject differs from atomic set projection".into());
        }
        if mode == super::transfer_export::UiTransferExportMode::Copy
            && (matches!(&source_frame.attachment, SelectionAttachment::Pinned { .. })
                || matches!(
                    &destination_frame.destination_subject,
                    SelectionAttachment::Pinned { .. }
                ))
        {
            return Err("copy lens frame must carry an explicit portable subject".into());
        }
    }
    Ok(())
}

fn invalid_subject(subject: &SelectionAttachment) -> bool {
    match subject {
        SelectionAttachment::FollowViewSet { view_set_id } => view_set_id.0 == 0,
        SelectionAttachment::Pinned { fingerprint, .. } => {
            fingerprint.is_empty() || fingerprint.chars().any(char::is_control)
        }
        SelectionAttachment::RequiredSubject { input, facets } => {
            input.is_empty()
                || input.trim() != input
                || input.chars().any(char::is_control)
                || facets.is_empty()
        }
    }
}

fn required_optional_tile_id<'de, D>(
    deserializer: D,
) -> Result<Option<crate::ids::TileId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<crate::ids::TileId>::deserialize(deserializer)
}

fn required_optional_view_set_id<'de, D>(deserializer: D) -> Result<Option<ViewSetId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ViewSetId>::deserialize(deserializer)
}

fn is_shared_dock(instance: &RyeOsViewInstanceKey) -> bool {
    matches!(
        instance.as_str().split(':').collect::<Vec<_>>().as_slice(),
        ["seat", _, "dock", _]
    )
}

fn placement_precedes_frontier(
    placement: &CompositionPlacement,
    frontier: u64,
    view_set_id: ViewSetId,
) -> bool {
    match placement {
        CompositionPlacement::First => true,
        CompositionPlacement::GroupTab { group_id, before } => {
            group_id.0 > 0
                && group_id.0 < frontier
                && group_id.0 != view_set_id.0
                && before.is_none_or(|tile| {
                    tile.0 > 0
                        && tile.0 < frontier
                        && tile.0 != view_set_id.0
                        && tile.0 != group_id.0
                })
        }
        CompositionPlacement::Split { target, ratio, .. } => {
            target.0 > 0
                && target.0 < frontier
                && target.0 != view_set_id.0
                && ratio.is_finite()
                && *ratio >= crate::layout::MIN_SPLIT_RATIO
                && *ratio <= crate::layout::MAX_SPLIT_RATIO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{TileId, ViewGroupId},
        surface::view_sets::ViewSetSeedSpec,
    };
    use ryeos_ui_presentation_contract::transfer::TransferSeat;
    use serde_json::json;

    fn transfer() -> TransferIdentity {
        let seat = |presentation_id: &str, side: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 4,
        };
        TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: "cd".repeat(32),
        }
    }

    fn binding() -> UiBindingAttachmentCoordinate {
        UiBindingAttachmentCoordinate {
            binding_attachment_id: "binding".into(),
            binding_generation: 1,
            binding_digest: "ef".repeat(32),
        }
    }

    fn named_binding(name: &str) -> UiBindingAttachmentCoordinate {
        UiBindingAttachmentCoordinate {
            binding_attachment_id: name.into(),
            binding_generation: 1,
            binding_digest: "ef".repeat(32),
        }
    }

    fn descriptor(binding: &UiBindingAttachmentCoordinate) -> UiBindingAttachment {
        serde_json::from_value(json!({
            "binding_attachment_id": binding.binding_attachment_id,
            "binding_generation": binding.binding_generation,
            "binding_digest": binding.binding_digest,
            "root_ref": "surface:test", "root_generation": "generation-one",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "test"}}, "views": {}},
            "project_path": null, "subject": {"kind":"projectless"},
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024, "max_seat_append_bytes": 65536}
        })).unwrap()
    }

    fn subject() -> SelectionAttachment {
        SelectionAttachment::RequiredSubject {
            input: "subject".into(),
            facets: vec!["selection.work".into()],
        }
    }

    fn destination_replay() -> (
        super::super::composition_replay::UiCompositionReplay,
        SurfaceInstanceId,
    ) {
        let core =
            crate::ui::reducer::test_support::core(crate::ui::reducer::test_support::session());
        let initial = &core.seat.events()[0];
        let crate::ui::seat::SeatEventKind::PresentationInitialized(record) = &initial.kind else {
            unreachable!()
        };
        let replay = super::super::composition_replay::UiCompositionReplay::begin(
            &record.initialization,
            initial,
        )
        .unwrap();
        let surface = replay.composition.surfaces().keys().next().unwrap().clone();
        (replay, surface)
    }

    #[test]
    fn ordered_set_builder_reserves_empty_tab_and_rejects_missing_descriptor_atomically() {
        use super::super::transfer_export::{
            UiTransferExport, UiTransferExportContent, UiTransferExportMode, UiTransferSetExport,
        };
        let (destination, surface) = destination_replay();
        let mut transfer = transfer();
        transfer.destination.presentation_id = destination.composition.id().as_str().into();
        transfer.destination.revision = destination.revision;
        let (_, source, source_surface, _) = ids();
        transfer.source.presentation_id = source.as_str().into();
        let old = named_binding("source-empty-set");
        let fresh = named_binding("fresh-empty-set");
        let export = UiTransferExport {
            schema: 1,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::ViewSets {
                active_set: Some("empty".into()),
                sets: vec![UiTransferSetExport {
                    handle: "empty".into(),
                    source_view_set_id: ViewSetId::new(8),
                    source_owner: crate::ids::UiViewMountOwner::Surface {
                        presentation_id: source,
                        surface_instance_id: source_surface,
                    },
                    binding: old.clone(),
                    seed: serde_json::from_value(
                        json!({"schema":2,"id":"empty","title":"Empty","root":null}),
                    )
                    .unwrap(),
                    focused_view: None,
                    mounts: vec![],
                }],
            },
        };
        transfer.export_digest =
            crate::ui::seat_payload_digest(&serde_json::to_value(&export).unwrap()).unwrap();
        let mappings = BTreeMap::from([(old.binding_attachment_id.clone(), fresh.clone())]);
        let before = destination.next_mount_id;
        assert!(
            UiTransferReservation::reserve_view_sets(
                &transfer,
                &export,
                &destination,
                surface.clone(),
                &mappings,
                vec![],
                65536
            )
            .is_err()
        );
        assert_eq!(destination.next_mount_id, before);
        let reservation = UiTransferReservation::reserve_view_sets(
            &transfer,
            &export,
            &destination,
            surface,
            &mappings,
            vec![descriptor(&fresh)],
            65536,
        )
        .unwrap();
        let UiTransferDestinationPlacement::Tabs {
            manifest,
            sets,
            active_view_set_id,
        } = &reservation.placement
        else {
            unreachable!()
        };
        assert_eq!(manifest.tabs.len(), 1);
        assert_eq!(sets[0].source_view_set_id, ViewSetId::new(8));
        assert_eq!(*active_view_set_id, Some(sets[0].destination_view_set_id));
        assert_eq!(sets[0].focused_tile_id, None);
        assert!(reservation.mounts.is_empty());
        assert!(reservation.next_mount_id > before);
        assert_eq!(
            destination.next_mount_id, before,
            "builder never publishes its allocation"
        );
    }

    #[test]
    fn dock_builder_preserves_frames_without_ids_and_refuses_occupied_edge() {
        use super::super::transfer_export::{
            UiTransferExport, UiTransferExportContent, UiTransferExportMode, UiTransferMountExport,
        };
        let (mut destination, _) = destination_replay();
        let edge = ["top", "bottom", "left", "right"]
            .into_iter()
            .find(|edge| !destination.composition.docks().contains_key(*edge))
            .unwrap();
        let mut transfer = transfer();
        let (_, source_id, _, _) = ids();
        transfer.source.presentation_id = source_id.as_str().into();
        transfer.destination.presentation_id = destination.composition.id().as_str().into();
        transfer.destination.revision = destination.revision;
        let old = named_binding("old-dock");
        let old_frame = named_binding("old-frame");
        let fresh = named_binding("fresh-dock");
        let fresh_frame = named_binding("fresh-frame");
        let export = UiTransferExport {
            schema: 1,
            mode: UiTransferExportMode::Copy,
            content: UiTransferExportContent::SharedDock {
                edge: crate::ui::model::RyeOsDockEdge::Left,
                visible: true,
                size: 20,
                mount: UiTransferMountExport {
                    source_instance: RyeOsViewInstanceKey::shared_dock(&source_id, "left").unwrap(),
                    view_ref: "view:test/current".into(),
                    binding: old.clone(),
                    local: crate::view_set::ViewLocalState::None,
                    subject: subject(),
                    route:
                        super::super::composition_operation::UiCompositionRouteAssignment::Authored,
                    current_label: Some("Current".into()),
                    drafts: vec![],
                    lens_frames: vec![super::super::composition_lens::RetainedLensFrame {
                        binding: old_frame.clone(),
                        view_ref: "view:test/return".into(),
                        attachment: subject(),
                        input_route: None,
                        label: Some("Return".into()),
                    }],
                },
            },
        };
        transfer.export_digest =
            crate::ui::seat_payload_digest(&serde_json::to_value(&export).unwrap()).unwrap();
        let mappings = BTreeMap::from([
            (old.binding_attachment_id.clone(), fresh.clone()),
            (old_frame.binding_attachment_id.clone(), fresh_frame.clone()),
        ]);
        let descriptors = vec![descriptor(&fresh), descriptor(&fresh_frame)];
        let before = destination.next_mount_id;
        let reservation = UiTransferReservation::reserve_shared_dock(
            &transfer,
            &export,
            &destination,
            edge,
            &mappings,
            descriptors.clone(),
            65536,
        )
        .unwrap();
        assert_eq!(reservation.allocation_start, before);
        assert_eq!(reservation.next_mount_id, before);
        assert_eq!(reservation.mounts[0].binding, fresh);
        assert_eq!(reservation.mounts[0].lens_frames[0].binding, fresh_frame);
        assert_eq!(
            reservation.mounts[0].lens_frames[0].destination_subject,
            subject()
        );
        // MOVE drafts, including hidden return-frame drafts, must fit the
        // exact fresh descriptor before the reservation can become Accepted.
        for (view_ref, source_binding, descriptor_index) in [
            ("view:test/current", &old, 0),
            ("view:test/return", &old_frame, 1),
        ] {
            let mut moved = export.clone();
            moved.mode = UiTransferExportMode::Move;
            let UiTransferExportContent::SharedDock { mount, .. } = &mut moved.content else {
                unreachable!()
            };
            mount
                .drafts
                .push(super::super::transfer_export::UiTransferDraftExport {
                    key: super::super::model::InputBufferKey::from_binding_coordinate(
                        mount.source_instance.clone(),
                        view_ref,
                        "line",
                        source_binding,
                    ),
                    state: super::super::model::RyeOsInputState {
                        text: "éλ".into(),
                        cursor: 4,
                        filter_field: 0,
                    },
                });
            let mut moved_transfer = transfer.clone();
            moved_transfer.export_digest =
                crate::ui::seat_payload_digest(&serde_json::to_value(&moved).unwrap()).unwrap();
            for (limit, declared) in [(3, true), (4, true), (4, false)] {
                let mut admitted = descriptors.clone();
                admitted[descriptor_index]
                    .binding_request_bounds
                    .max_input_bytes = limit;
                if declared {
                    admitted[descriptor_index].presentation.views.insert(
                        view_ref.into(),
                        json!({
                            "widget":"text", "input":{"id":"line", "submit":"route"}
                        }),
                    );
                }
                let result = UiTransferReservation::reserve_shared_dock(
                    &moved_transfer,
                    &moved,
                    &destination,
                    edge,
                    &mappings,
                    admitted,
                    65536,
                );
                assert_eq!(
                    result.is_ok(),
                    limit == 4 && declared,
                    "{view_ref} {limit} {declared}: {result:?}"
                );
                assert_eq!(destination.next_mount_id, before);
                assert!(!destination.composition.docks().contains_key(edge));
            }
        }
        assert!(
            UiTransferReservation::reserve_shared_dock(
                &transfer,
                &export,
                &destination,
                "invalid",
                &mappings,
                descriptors.clone(),
                65536
            )
            .is_err()
        );
        destination
            .composition
            .install_shared_dock(
                edge,
                "view:test/occupied".into(),
                named_binding("occupied"),
                crate::view_set::ViewLocalState::None,
                true,
                20,
            )
            .unwrap();
        assert!(
            UiTransferReservation::reserve_shared_dock(
                &transfer,
                &export,
                &destination,
                edge,
                &mappings,
                descriptors,
                65536
            )
            .is_err()
        );
        assert_eq!(destination.next_mount_id, before);
    }

    #[test]
    fn frame_reservation_requires_all_fresh_mappings_and_preserves_order_and_aliases() {
        let old_a = named_binding("old-a");
        let old_b = named_binding("old-b");
        let fresh_a = named_binding("fresh-a");
        let fresh_b = named_binding("fresh-b");
        let frame = |binding, label: &str| super::super::composition_lens::RetainedLensFrame {
            binding,
            view_ref: "view:test/one".into(),
            attachment: subject(),
            input_route: None,
            label: Some(label.into()),
        };
        let frames = vec![
            frame(old_a.clone(), "first"),
            frame(old_b.clone(), "second"),
            frame(old_a.clone(), "third"),
        ];
        let mut mappings = BTreeMap::from([(old_a.binding_attachment_id.clone(), fresh_a.clone())]);
        assert!(reserve_lens_frames(&frames, &mappings).is_err());
        mappings.insert(old_b.binding_attachment_id.clone(), fresh_b.clone());
        let allocated = reserve_lens_frames(&frames, &mappings).unwrap();
        assert_eq!(
            allocated
                .iter()
                .map(|frame| &frame.binding)
                .collect::<Vec<_>>(),
            vec![&fresh_a, &fresh_b, &fresh_a]
        );
        assert!(
            allocated
                .iter()
                .all(|frame| frame.destination_subject == subject())
        );
        let pairs = frames
            .iter()
            .zip(&allocated)
            .map(|(source, destination)| (&source.binding, &destination.binding))
            .collect::<Vec<_>>();
        validate_binding_bijection(&pairs).unwrap();
        // The enclosing builder validates this bijection before returning:
        // unrelated sources cannot collapse and a source cannot be reused.
        mappings.insert(old_b.binding_attachment_id.clone(), fresh_a);
        let collapsed = reserve_lens_frames(&frames, &mappings).unwrap();
        let pairs = frames
            .iter()
            .zip(&collapsed)
            .map(|(source, destination)| (&source.binding, &destination.binding))
            .collect::<Vec<_>>();
        assert!(validate_binding_bijection(&pairs).is_err());
        mappings.insert(old_a.binding_attachment_id.clone(), old_a.clone());
        let reused = reserve_lens_frames(&frames, &mappings).unwrap();
        let pairs = frames
            .iter()
            .zip(&reused)
            .map(|(source, destination)| (&source.binding, &destination.binding))
            .collect::<Vec<_>>();
        assert!(validate_binding_bijection(&pairs).is_err());
    }

    fn ids() -> (
        PresentationId,
        PresentationId,
        SurfaceInstanceId,
        SurfaceInstanceId,
    ) {
        (
            PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap(),
            PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            SurfaceInstanceId::try_new("source-surface").unwrap(),
            SurfaceInstanceId::try_new("destination-surface").unwrap(),
        )
    }

    #[test]
    fn empty_tab_content_still_reserves_its_canonical_tab_identity() {
        let (_, destination, _, destination_surface) = ids();
        let seed: ViewSetSeedSpec = serde_json::from_value(json!({
            "schema":2,"id":"empty","title":"Empty","root":null
        }))
        .unwrap();
        let manifest = UiSurfaceMountManifest::allocate(
            destination.clone(),
            destination_surface,
            10,
            vec![(seed, binding())],
            Some("empty"),
        )
        .unwrap();
        let mut reservation = UiTransferReservation {
            schema: 2,
            presentation_id: destination,
            expected_revision: 4,
            allocation_start: 10,
            next_mount_id: manifest.next_mount_id,
            placement: UiTransferDestinationPlacement::Tabs {
                sets: vec![UiTransferSetAllocation {
                    source_handle: "empty".into(),
                    source_view_set_id: ViewSetId::new(2),
                    destination_view_set_id: manifest.tabs[0].view_set_id,
                    focused_tile_id: None,
                }],
                active_view_set_id: Some(manifest.tabs[0].view_set_id),
                manifest,
            },
            mounts: vec![],
            descriptors: vec![descriptor(&binding())],
        };
        reservation.validate_for(&transfer()).unwrap();
        // Preparation's destination revision is an immutable origin, not an
        // equality fence for an unrelated later destination edit. The owner
        // separately compares this accepted revision to current replay.
        reservation.expected_revision = 5;
        reservation.validate_for(&transfer()).unwrap();
        reservation.expected_revision = 3;
        assert!(reservation.validate_for(&transfer()).is_err());
        reservation.expected_revision = 4;
        assert_ne!(reservation.allocation_start, reservation.next_mount_id);
        let UiTransferDestinationPlacement::Tabs {
            active_view_set_id, ..
        } = &mut reservation.placement
        else {
            unreachable!()
        };
        *active_view_set_id = None;
        reservation.validate_for(&transfer()).unwrap();
    }

    #[test]
    fn view_reservation_rejects_fresh_or_invalid_existing_targets() {
        let (source, destination, source_surface, destination_surface) = ids();
        let mount = UiTransferMountAllocation {
            source_instance: RyeOsViewInstanceKey::surface_tile(
                &source,
                &source_surface,
                TileId::new(2),
            ),
            destination_instance: RyeOsViewInstanceKey::surface_tile(
                &destination,
                &destination_surface,
                TileId::new(20),
            ),
            binding: binding(),
            destination_subject: subject(),
            lens_frames: vec![],
        };
        let mut missing_frames = serde_json::to_value(&mount).unwrap();
        missing_frames
            .as_object_mut()
            .unwrap()
            .remove("lens_frames");
        assert!(serde_json::from_value::<UiTransferMountAllocation>(missing_frames).is_err());
        let allocation = UiCompositionMountAllocation::allocate(20, 1, 0).unwrap();
        let reservation = |placement| UiTransferReservation {
            schema: 2,
            presentation_id: destination.clone(),
            expected_revision: 4,
            allocation_start: 20,
            next_mount_id: allocation.next_mount_id,
            placement: UiTransferDestinationPlacement::View {
                surface_instance_id: destination_surface.clone(),
                view_set_id: ViewSetId::new(3),
                allocation: allocation.clone(),
                placement,
            },
            mounts: vec![mount.clone()],
            descriptors: vec![descriptor(&mount.binding)],
        };
        reservation(CompositionPlacement::GroupTab {
            group_id: ViewGroupId::new(4),
            before: None,
        })
        .validate_for(&transfer())
        .unwrap();
        assert!(
            reservation(CompositionPlacement::GroupTab {
                group_id: ViewGroupId::new(20),
                before: None,
            })
            .validate_for(&transfer())
            .is_err()
        );
    }

    #[test]
    fn shared_dock_refuses_a_dialog_disguised_as_a_dock_transfer() {
        let (source, destination, _, _) = ids();
        let mut reservation = UiTransferReservation {
            schema: 2,
            presentation_id: destination.clone(),
            expected_revision: 4,
            allocation_start: 20,
            next_mount_id: 20,
            placement: UiTransferDestinationPlacement::SharedDock {
                edge: "left".into(),
            },
            mounts: vec![UiTransferMountAllocation {
                source_instance: RyeOsViewInstanceKey::shared_dock(&source, "right").unwrap(),
                destination_instance: RyeOsViewInstanceKey::shared_dock(&destination, "left")
                    .unwrap(),
                binding: binding(),
                destination_subject: subject(),
                lens_frames: vec![],
            }],
            descriptors: vec![descriptor(&binding())],
        };
        reservation.validate_for(&transfer()).unwrap();
        reservation.mounts[0].source_instance =
            RyeOsViewInstanceKey::shared_dialog(&source, "launcher").unwrap();
        assert!(reservation.validate_for(&transfer()).is_err());
    }

    #[test]
    fn schema_two_requires_exact_public_destination_descriptor() {
        let (source, destination, _, _) = ids();
        let mut reservation = UiTransferReservation {
            schema: 2,
            presentation_id: destination.clone(),
            expected_revision: 4,
            allocation_start: 20,
            next_mount_id: 20,
            placement: UiTransferDestinationPlacement::SharedDock {
                edge: "left".into(),
            },
            mounts: vec![UiTransferMountAllocation {
                source_instance: RyeOsViewInstanceKey::shared_dock(&source, "right").unwrap(),
                destination_instance: RyeOsViewInstanceKey::shared_dock(&destination, "left")
                    .unwrap(),
                binding: binding(),
                destination_subject: subject(),
                lens_frames: vec![],
            }],
            descriptors: vec![],
        };
        assert!(reservation.validate_for(&transfer()).is_err());
        let mut descriptor: UiBindingAttachment = serde_json::from_value(json!({
            "binding_attachment_id": reservation.mounts[0].binding.binding_attachment_id,
            "binding_generation": reservation.mounts[0].binding.binding_generation,
            "binding_digest": reservation.mounts[0].binding.binding_digest,
            "root_ref": "surface:test", "root_generation": "generation-one",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "test"}}, "views": {}},
            "project_path": null, "subject": {"kind":"projectless"},
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024, "max_seat_append_bytes": 65536}
        })).unwrap();
        reservation.descriptors.push(descriptor.clone());
        reservation.validate_for(&transfer()).unwrap();
        let shared = reservation.mounts[0].binding.clone();
        reservation.mounts[0]
            .lens_frames
            .push(UiTransferLensFrameAllocation {
                binding: shared,
                destination_subject: subject(),
            });
        // One admitted binding may be referenced more than once, but retains
        // exactly one signed descriptor for its exact coordinate.
        reservation.validate_for(&transfer()).unwrap();
        let mut distinct_frame = reservation.clone();
        let frame_binding = named_binding("distinct-frame");
        distinct_frame.mounts[0].lens_frames[0].binding = frame_binding.clone();
        assert!(distinct_frame.validate_for(&transfer()).is_err());
        distinct_frame
            .descriptors
            .push(self::descriptor(&frame_binding));
        distinct_frame.validate_for(&transfer()).unwrap();
        distinct_frame.descriptors[1].binding_generation += 1;
        assert!(distinct_frame.validate_for(&transfer()).is_err());
        reservation.mounts[0].lens_frames[0]
            .binding
            .binding_generation += 1;
        assert!(reservation.validate_for(&transfer()).is_err());
        reservation.mounts[0].lens_frames[0]
            .binding
            .binding_generation -= 1;
        reservation.schema = 1;
        assert!(reservation.validate_for(&transfer()).is_err());
        reservation.schema = 2;
        descriptor.binding_generation += 1;
        reservation.descriptors[0] = descriptor.clone();
        assert!(reservation.validate_for(&transfer()).is_err());
        descriptor.binding_generation -= 1;
        reservation.descriptors[0] = descriptor;
        reservation
            .descriptors
            .push(reservation.descriptors[0].clone());
        assert!(reservation.validate_for(&transfer()).is_err());
    }

    #[test]
    fn tab_insertion_binding_does_not_replace_exact_mount_binding() {
        let (source, destination, source_surface, destination_surface) = ids();
        let seed: ViewSetSeedSpec = serde_json::from_value(json!({
            "schema":2,"id":"one","title":"One",
            "root":{"type":"group","label":null,"views":["view:test/one"],"active":0}
        }))
        .unwrap();
        let manifest = UiSurfaceMountManifest::allocate(
            destination.clone(),
            destination_surface,
            10,
            vec![(seed, named_binding("insertion"))],
            Some("one"),
        )
        .unwrap();
        let destination_instance = RyeOsViewInstanceKey::surface_tile(
            &destination,
            &manifest.surface_instance_id,
            manifest.tabs[0].tile_ids[0],
        );
        let reservation = UiTransferReservation {
            schema: 2,
            presentation_id: destination,
            expected_revision: 4,
            allocation_start: 10,
            next_mount_id: manifest.next_mount_id,
            placement: UiTransferDestinationPlacement::Tabs {
                sets: vec![UiTransferSetAllocation {
                    source_handle: "one".into(),
                    source_view_set_id: ViewSetId::new(3),
                    destination_view_set_id: manifest.tabs[0].view_set_id,
                    focused_tile_id: Some(manifest.tabs[0].tile_ids[0]),
                }],
                active_view_set_id: Some(manifest.tabs[0].view_set_id),
                manifest,
            },
            mounts: vec![UiTransferMountAllocation {
                source_instance: RyeOsViewInstanceKey::surface_tile(
                    &source,
                    &source_surface,
                    TileId::new(7),
                ),
                destination_instance,
                binding: named_binding("mount"),
                destination_subject: subject(),
                lens_frames: vec![],
            }],
            descriptors: vec![
                descriptor(&named_binding("insertion")),
                descriptor(&named_binding("mount")),
            ],
        };
        reservation.validate_for(&transfer()).unwrap();
    }

    #[test]
    fn follow_projection_and_cross_group_focus_use_allocated_ids() {
        use super::super::transfer_export::UiTransferSetExport;
        use crate::ids::UiViewMountOwner;
        use crate::surface::ViewKindSpec;
        use crate::surface::view_sets::LayoutSeedSpec;

        let (source, destination, source_surface, destination_surface) = ids();
        let seed = ViewSetSeedSpec {
            schema: 2,
            id: "nested".into(),
            title: "Nested".into(),
            root: Some(LayoutSeedSpec::Split {
                axis: crate::layout::SplitAxis::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSeedSpec::Group {
                    label: None,
                    views: vec![ViewKindSpec("view:test/one".into())],
                    active: 0,
                }),
                second: Box::new(LayoutSeedSpec::Group {
                    label: None,
                    views: vec![ViewKindSpec("view:test/two".into())],
                    active: 0,
                }),
            }),
        };
        let manifest = UiSurfaceMountManifest::allocate(
            destination,
            destination_surface,
            20,
            vec![(seed.clone(), binding())],
            Some("nested"),
        )
        .unwrap();
        let exported = UiTransferSetExport {
            handle: "nested".into(),
            source_view_set_id: ViewSetId::new(8),
            source_owner: UiViewMountOwner::Surface {
                presentation_id: source,
                surface_instance_id: source_surface,
            },
            binding: binding(),
            seed,
            focused_view: Some(1),
            mounts: vec![],
        };
        let projected = tab_set_allocations(std::slice::from_ref(&exported), &manifest).unwrap();
        assert_eq!(
            projected[0].focused_tile_id,
            Some(manifest.tabs[0].tile_ids[1])
        );
        assert_eq!(
            remap_subject(
                &SelectionAttachment::FollowViewSet {
                    view_set_id: ViewSetId::new(8),
                },
                &BTreeMap::from([(ViewSetId::new(8), manifest.tabs[0].view_set_id)]),
            )
            .unwrap(),
            SelectionAttachment::FollowViewSet {
                view_set_id: manifest.tabs[0].view_set_id,
            }
        );
        // Exercise the connected pure builder, not just the projection helper.
        let (destination, surface) = destination_replay();
        let mut transfer = transfer();
        let (_, source_id, source_surface, _) = ids();
        transfer.source.presentation_id = source_id.as_str().into();
        transfer.destination.presentation_id = destination.composition.id().as_str().into();
        transfer.destination.revision = destination.revision;
        let mut exported = exported;
        exported.source_owner = UiViewMountOwner::Surface {
            presentation_id: source_id.clone(),
            surface_instance_id: source_surface.clone(),
        };
        exported.mounts = ["view:test/one", "view:test/two"]
            .into_iter()
            .enumerate()
            .map(
                |(index, view)| super::super::transfer_export::UiTransferMountExport {
                    source_instance: RyeOsViewInstanceKey::surface_tile(
                        &source_id,
                        &source_surface,
                        TileId::new(7 + index as u64),
                    ),
                    view_ref: view.into(),
                    binding: binding(),
                    local: crate::view_set::ViewLocalState::None,
                    subject: SelectionAttachment::FollowViewSet {
                        view_set_id: ViewSetId::new(8),
                    },
                    route:
                        super::super::composition_operation::UiCompositionRouteAssignment::Authored,
                    current_label: None,
                    drafts: vec![],
                    lens_frames: vec![super::super::composition_lens::RetainedLensFrame {
                        binding: binding(),
                        view_ref: "view:test/return".into(),
                        attachment: SelectionAttachment::FollowViewSet {
                            view_set_id: ViewSetId::new(8),
                        },
                        input_route: None,
                        label: None,
                    }],
                },
            )
            .collect();
        let export = super::super::transfer_export::UiTransferExport {
            schema: 1,
            mode: super::super::transfer_export::UiTransferExportMode::Move,
            content: super::super::transfer_export::UiTransferExportContent::ViewSets {
                active_set: Some("nested".into()),
                sets: vec![exported],
            },
        };
        transfer.export_digest =
            crate::ui::seat_payload_digest(&serde_json::to_value(&export).unwrap()).unwrap();
        let fresh = named_binding("fresh-nested");
        let reserved = UiTransferReservation::reserve_view_sets(
            &transfer,
            &export,
            &destination,
            surface,
            &BTreeMap::from([(binding().binding_attachment_id, fresh.clone())]),
            vec![descriptor(&fresh)],
            65536,
        )
        .unwrap();
        let UiTransferDestinationPlacement::Tabs {
            manifest,
            sets,
            active_view_set_id,
        } = &reserved.placement
        else {
            unreachable!()
        };
        assert_eq!(manifest.tabs[0].tile_ids.len(), 2);
        assert_eq!(manifest.tabs[0].group_ids.len(), 2);
        assert_eq!(sets[0].focused_tile_id, Some(manifest.tabs[0].tile_ids[1]));
        assert_eq!(*active_view_set_id, Some(manifest.tabs[0].view_set_id));
        for mount in &reserved.mounts {
            assert_eq!(
                mount.destination_subject,
                SelectionAttachment::FollowViewSet {
                    view_set_id: manifest.tabs[0].view_set_id
                }
            );
            assert_eq!(
                mount.lens_frames[0].destination_subject,
                mount.destination_subject
            );
            assert_eq!(mount.lens_frames[0].binding, fresh);
        }
    }

    #[test]
    fn binding_bijection_allows_repeated_exact_source_to_share_destination() {
        let source = named_binding("source");
        let destination = named_binding("destination");
        validate_binding_bijection(&[
            (&source, &destination),
            (&source, &destination),
            (&source, &destination),
        ])
        .unwrap();
    }

    #[test]
    fn binding_bijection_rejects_nonfunctional_or_conflated_projection() {
        let source_one = named_binding("source-one");
        let source_two = named_binding("source-two");
        let destination_one = named_binding("destination-one");
        let destination_two = named_binding("destination-two");

        assert!(
            validate_binding_bijection(&[
                (&source_one, &destination_one),
                (&source_one, &destination_two),
            ])
            .is_err()
        );
        assert!(
            validate_binding_bijection(&[
                (&source_one, &destination_one),
                (&source_two, &destination_one),
            ])
            .is_err()
        );
    }

    #[test]
    fn binding_bijection_rejects_any_destination_from_source_closure() {
        let source_one = named_binding("source-one");
        let source_two = named_binding("source-two");
        let destination = named_binding("destination");

        assert!(
            validate_binding_bijection(&[(&source_one, &source_two), (&source_two, &destination),])
                .is_err()
        );
    }
}
